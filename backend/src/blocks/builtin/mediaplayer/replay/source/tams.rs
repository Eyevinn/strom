//! TAMS flow as a replay source: `GET /flows/{id}/segments` for the index and
//! the segments' `get_urls` for the bytes.

use super::{read_newest, Location, Segment, Since, SourceIndex, SourceInfo};
use crate::client_auth::AuthMethod;
use crate::tams::client::{ListedSegment, NextPage, SegmentQuery, TamsClient};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use strom_types::tams::format_timestamp;
use tracing::warn;

/// Pages followed for one window listing. A window is seconds to minutes of
/// segments, so this only stops a runaway listing.
const MAX_PAGES: usize = 64;

pub struct TamsIndex {
    client: TamsClient,
    flow_id: String,
}

impl TamsIndex {
    pub fn new(gateway: &str, flow_id: &str, auth: AuthMethod) -> Result<Self> {
        Ok(Self {
            client: TamsClient::new(gateway, auth)?,
            flow_id: flow_id.to_string(),
        })
    }

    /// The segments of the first `max_pages` pages of a listing, and whether
    /// the store has more.
    async fn query(&self, q: SegmentQuery, max_pages: usize) -> Result<(Vec<Segment>, bool)> {
        let (listed, more) = self
            .client
            .list_segments_paged(&self.flow_id, &q, max_pages)
            .await?;
        Ok((listed.iter().filter_map(to_segment).collect(), more))
    }
}

/// A TAMS timerange covering `[start_ns, end_ns)`, signed.
fn timerange(start_ns: i64, end_ns: i64) -> String {
    format!("[{}_{})", timestamp(start_ns), timestamp(end_ns))
}

fn timestamp(ns: i64) -> String {
    if ns < 0 {
        format!("-{}", format_timestamp(ns.unsigned_abs()))
    } else {
        format_timestamp(ns as u64)
    }
}

fn to_segment(s: &ListedSegment) -> Option<Segment> {
    let urls = s.get_urls.iter().map(|u| u.url.clone()).collect();
    let seg = Segment::from_tams(
        &s.object_id,
        &s.timerange,
        s.ts_offset.as_deref(),
        s.object_timerange.as_deref(),
        Location::Urls(urls),
    );
    if seg.is_none() {
        warn!(
            "TAMS segment {} has an unusable timerange '{}'",
            s.object_id, s.timerange
        );
    }
    seg
}

#[async_trait]
impl SourceIndex for TamsIndex {
    fn describe(&self) -> String {
        format!("TAMS {}/flows/{}", self.client.base_url(), self.flow_id)
    }

    fn may_grow(&self) -> bool {
        true
    }

    async fn info(&self) -> Result<SourceInfo> {
        let flow = self.client.get_flow(&self.flow_id).await?;
        let rate = &flow["essence_parameters"]["frame_rate"];
        let frame_rate = match (rate["numerator"].as_u64(), rate["denominator"].as_u64()) {
            (Some(n), d) if n > 0 => Some((n as u32, d.filter(|d| *d > 0).unwrap_or(1) as u32)),
            _ => None,
        };
        Ok(SourceInfo {
            frame_rate,
            container: flow["container"].as_str().map(str::to_string),
            codec: video_codec(&flow),
            // TAMS timeranges are TAI by definition.
            absolute_time: true,
        })
    }

    /// Every page of the window. A listing cut off at [`MAX_PAGES`] is an
    /// error, not a partial result: the caller takes a listing as the whole
    /// truth about the window, and the missing rest would show as holes.
    async fn list(&self, start_ns: i64, end_ns: i64) -> Result<Vec<Segment>> {
        let (mut segs, more) = self
            .query(
                SegmentQuery {
                    timerange: Some(timerange(start_ns, end_ns)),
                    limit: None,
                    reverse_order: false,
                },
                MAX_PAGES,
            )
            .await?;
        if more {
            return Err(anyhow!(
                "listing of {} has more than {} pages",
                timerange(start_ns, end_ns),
                MAX_PAGES
            ));
        }
        segs.sort_by_key(|s| s.start_ns);
        Ok(segs)
    }

    async fn first(&self) -> Result<Option<Segment>> {
        let q = SegmentQuery {
            timerange: None,
            limit: Some(1),
            reverse_order: false,
        };
        Ok(self.query(q, 1).await?.0.into_iter().next())
    }

    async fn last(&self) -> Result<Option<Segment>> {
        let q = SegmentQuery {
            timerange: None,
            limit: Some(1),
            reverse_order: true,
        };
        Ok(self.query(q, 1).await?.0.into_iter().next())
    }

    /// `GET /segments?reverse_order=true&limit=page`, following the store's
    /// paging until the listing reaches `after_ns`. No time filter: a store
    /// answers this from the end of its index, where a `timerange` query may
    /// scan the flow (0.06–0.12 s against 0.8–1.0 s on a loaded gateway).
    async fn newest(&self, after_ns: i64, page: usize, max_pages: usize) -> Result<Since> {
        let q = SegmentQuery {
            timerange: None,
            limit: Some(page.max(1) as u32),
            reverse_order: true,
        };
        read_newest(after_ns, page, max_pages, |next: Option<NextPage>| {
            let q = &q;
            async move {
                let (listed, next) = self
                    .client
                    .list_segments_page(&self.flow_id, q, next.as_ref())
                    .await?;
                let segs = listed.iter().filter_map(to_segment).collect();
                Ok((segs, listed.len(), next))
            }
        })
        .await
    }

    async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>> {
        let Location::Urls(urls) = &segment.location else {
            return Err(anyhow!("TAMS segment without get_urls"));
        };
        let mut last_err = anyhow!("segment {} has no get_urls", segment.object_id);
        for url in urls {
            match self.client.fetch_object(url).await {
                Ok(b) => return Ok(b),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }
}

/// The flow's video codec, if it names one. A multi-essence flow (TAMS
/// Output's muxed mode) carries its container in `codec` (`video/mp2t`), not
/// the video inside, so it names none: replay then tries H.264, as for a
/// source that does not say, and the demux of a segment names the codec
/// when it is another.
fn video_codec(flow: &serde_json::Value) -> Option<String> {
    if flow["format"].as_str() == Some("urn:x-nmos:format:multi") {
        return None;
    }
    flow["codec"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_flow_names_no_video_codec() {
        let muxed = serde_json::json!({
            "format": "urn:x-nmos:format:multi",
            "codec": "video/mp2t",
            "container": "video/mp2t",
        });
        assert_eq!(video_codec(&muxed), None);
        let video = serde_json::json!({
            "format": "urn:x-nmos:format:video",
            "codec": "video/h264",
        });
        assert_eq!(video_codec(&video).as_deref(), Some("video/h264"));
    }

    #[test]
    fn timerange_is_signed_half_open() {
        assert_eq!(
            timerange(1_800_000_000_000_000_000, 1_800_000_001_500_000_000),
            "[1800000000:0_1800000001:500000000)"
        );
        assert_eq!(timerange(-1_500_000_000, 0), "[-1:500000000_0:0)");
    }

    #[test]
    fn listed_segment_maps_urls_and_offsets() {
        let l: ListedSegment = serde_json::from_str(
            r#"{"object_id":"b/o","timerange":"[10:0_11:0)","ts_offset":"-2:0",
                "get_urls":[{"url":"http://a.example.com/o"},{"url":"http://b.example.com/o"}]}"#,
        )
        .unwrap();
        let s = to_segment(&l).unwrap();
        assert_eq!(s.start_ns, 10_000_000_000);
        assert_eq!(s.ts_offset_ns, -2_000_000_000);
        assert_eq!(
            s.location,
            Location::Urls(vec![
                "http://a.example.com/o".into(),
                "http://b.example.com/o".into()
            ])
        );
        let bad: ListedSegment =
            serde_json::from_str(r#"{"object_id":"o","timerange":"[10:0_"}"#).unwrap();
        assert!(to_segment(&bad).is_none());
    }
}
