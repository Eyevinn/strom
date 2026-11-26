//! Source indexes for replay mode.
//!
//! The replay engine needs two things from a source: an index (which pieces of
//! media exist and which TAI time each covers) and the bytes of a piece. A
//! [`SourceIndex`] provides both, one implementation per protocol:
//!
//! - [`local::LocalIndex`]: a directory with a TAMS-like `index.json` and
//!   segment files (the spike's test material).
//! - [`tams::TamsIndex`]: a TAMS flow, via `GET /flows/{id}/segments`.
//!
//! HLS, DASH or byte ranges of a file can plug in later behind the same trait.
//! The source is chosen by URI, see [`open`].

pub mod local;
pub mod tams;

use crate::client_auth::AuthMethod;
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use strom_types::tams::{parse_timerange, parse_timestamp_signed};

/// Where the bytes of a segment are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Path(PathBuf),
    /// Alternative URLs for the same object, in preference order.
    Urls(Vec<String>),
}

/// One piece of media on the TAI timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Half-open `[start_ns, end_ns)` in TAI ns: the part of the object that
    /// belongs on the timeline.
    pub start_ns: i64,
    pub end_ns: i64,
    /// TAMS `ts_offset`: timeline time minus object time.
    pub ts_offset_ns: i64,
    /// Start of the object's own timerange (object time), when the store
    /// reports it. Needed when a segment uses only part of its object.
    pub object_start_ns: Option<i64>,
    /// Store object id, or the file name.
    pub object_id: String,
    pub location: Location,
}

impl Segment {
    /// TAI of the first sample in the object. Each decoded frame's TAI is this
    /// plus its PTS distance from the object's first PTS; frames outside
    /// `[start_ns, end_ns)` are not part of the segment.
    pub fn first_sample_tai(&self) -> i64 {
        match self.object_start_ns {
            Some(o) => o + self.ts_offset_ns,
            None => self.start_ns,
        }
    }

    pub fn duration_ns(&self) -> i64 {
        self.end_ns - self.start_ns
    }

    /// Build from TAMS wire strings. `None` if the timerange is not a bounded,
    /// non-empty range.
    pub fn from_tams(
        object_id: &str,
        timerange: &str,
        ts_offset: Option<&str>,
        object_timerange: Option<&str>,
        location: Location,
    ) -> Option<Self> {
        let (start_ns, end_ns) = parse_timerange(timerange)?.half_open()?;
        let ts_offset_ns = match ts_offset {
            Some(s) => parse_timestamp_signed(s)?,
            None => 0,
        };
        let object_start_ns = object_timerange
            .and_then(parse_timerange)
            .and_then(|r| r.half_open())
            .map(|(s, _)| s);
        Some(Self {
            start_ns,
            end_ns,
            ts_offset_ns,
            object_start_ns,
            object_id: object_id.to_string(),
            location,
        })
    }
}

/// What a source says about its essence.
#[derive(Debug, Clone, Default)]
pub struct SourceInfo {
    /// Frames per second as `(numerator, denominator)`, when known.
    pub frame_rate: Option<(u32, u32)>,
    /// Container MIME type, e.g. `video/mp2t`.
    pub container: Option<String>,
    /// Codec MIME type, e.g. `video/h264`, when the source says.
    pub codec: Option<String>,
    /// Whether the index times are real TAI (a clock on the wall), so they
    /// convert to UTC. False for material on an arbitrary TAI base.
    pub absolute_time: bool,
}

/// An index of media on the TAI timeline, and access to its bytes.
#[async_trait]
pub trait SourceIndex: Send + Sync {
    /// For logs. Must not contain secrets.
    fn describe(&self) -> String;
    /// Whether the index can grow while playing (poll the live edge).
    fn may_grow(&self) -> bool;
    async fn info(&self) -> Result<SourceInfo>;
    /// Segments overlapping `[start_ns, end_ns)`, ordered by time.
    async fn list(&self, start_ns: i64, end_ns: i64) -> Result<Vec<Segment>>;
    /// The earliest segment.
    async fn first(&self) -> Result<Option<Segment>>;
    /// The latest segment (the live edge).
    async fn last(&self) -> Result<Option<Segment>>;
    /// The newest segments, read from the live edge backwards `page` at a
    /// time until the listing reaches `after_ns` (the edge the caller
    /// knows), at most `max_pages` pages. In time order. Cheap for a store
    /// that lists newest first without a time filter (TAMS
    /// `reverse_order=true&limit=N`). [`Since::more`] says the listing
    /// stopped before it reached `after_ns`: segments between `after_ns` and
    /// the oldest one returned may be missing. The default lists
    /// `[after_ns, end)` in one go.
    async fn newest(&self, after_ns: i64, page: usize, max_pages: usize) -> Result<Since> {
        let _ = (page, max_pages);
        let segs = self.list(after_ns, i64::MAX).await?;
        Ok(Since { segs, more: false })
    }
    /// The bytes of a segment.
    async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>>;
}

/// The result of [`SourceIndex::newest`].
#[derive(Debug, Clone, Default)]
pub struct Since {
    pub segs: Vec<Segment>,
    /// The listing stopped before it reached the known edge: the source may
    /// have segments between that edge and the first of `segs`.
    pub more: bool,
}

/// Read a newest-first paged listing until it reaches `after_ns`, for
/// [`SourceIndex::newest`]. `fetch(cursor)` returns one page: its segments,
/// how many entries the store returned (before dropping unusable ones), and
/// the cursor of the next page. A page is reached when it holds a segment
/// that starts at or before `after_ns` (anything older ends before it). A
/// short page without a next cursor is the end of the store; a full one
/// without a cursor (a store that does not page) may hide more.
pub async fn read_newest<C, F, Fut>(
    after_ns: i64,
    page: usize,
    max_pages: usize,
    mut fetch: F,
) -> Result<Since>
where
    F: FnMut(Option<C>) -> Fut,
    Fut: std::future::Future<Output = Result<(Vec<Segment>, usize, Option<C>)>>,
{
    let mut segs: Vec<Segment> = Vec::new();
    let mut cursor: Option<C> = None;
    let mut more = false;
    for i in 0..max_pages.max(1) {
        let (got, returned, next) = fetch(cursor.take()).await?;
        let reached = got.iter().any(|s| s.start_ns <= after_ns);
        segs.extend(got);
        if reached {
            break;
        }
        match next {
            Some(c) if returned > 0 => {
                if i + 1 == max_pages.max(1) {
                    more = true;
                }
                cursor = Some(c);
            }
            _ => {
                more = returned >= page;
                break;
            }
        }
    }
    segs.sort_by_key(|s| s.start_ns);
    segs.dedup_by_key(|s| s.start_ns);
    Ok(Since { segs, more })
}

/// A parsed `source`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    /// Directory with `index.json`.
    Local(PathBuf),
    /// `tams+http(s)://<gateway>/flows/<flow_id>`.
    Tams { gateway: String, flow_id: String },
}

/// Parse `source`: `tams+https://host[/prefix]/flows/<id>`,
/// `tams+http://...`, `file:///dir`, or a plain directory path (absolute, or
/// relative to `media_path`).
pub fn parse_source(source: &str, media_path: &str) -> Result<SourceSpec, String> {
    let s = source.trim();
    if let Some(rest) = s.strip_prefix("tams+") {
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            return Err(format!("unsupported TAMS URI scheme in '{}'", s));
        }
        let rest = rest.trim_end_matches('/');
        let (gateway, flow_id) = rest
            .rsplit_once("/flows/")
            .ok_or_else(|| "TAMS URI must end in /flows/<flow_id>".to_string())?;
        if flow_id.is_empty() || flow_id.contains('/') || gateway.ends_with("//") {
            return Err("TAMS URI must end in /flows/<flow_id>".to_string());
        }
        return Ok(SourceSpec::Tams {
            gateway: gateway.to_string(),
            flow_id: flow_id.to_string(),
        });
    }
    let path = s.strip_prefix("file://").unwrap_or(s);
    let p = PathBuf::from(path);
    Ok(SourceSpec::Local(if p.is_absolute() {
        p
    } else {
        PathBuf::from(media_path).join(p)
    }))
}

/// Open the index for a source. `auth` is used for a TAMS gateway, already
/// checked against it (see [`crate::tams::auth::TamsAuth`]).
pub fn open(spec: &SourceSpec, auth: AuthMethod) -> Result<Arc<dyn SourceIndex>, String> {
    match spec {
        SourceSpec::Local(dir) => Ok(Arc::new(local::LocalIndex::load(dir)?)),
        SourceSpec::Tams { gateway, flow_id } => Ok(Arc::new(
            tams::TamsIndex::new(gateway, flow_id, auth).map_err(|e| e.to_string())?,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tams_uri() {
        assert_eq!(
            parse_source("tams+https://gw.example.com/flows/abc-1", "/m").unwrap(),
            SourceSpec::Tams {
                gateway: "https://gw.example.com".into(),
                flow_id: "abc-1".into()
            }
        );
        assert_eq!(
            parse_source("tams+http://localhost:18000/api/flows/x/", "/m").unwrap(),
            SourceSpec::Tams {
                gateway: "http://localhost:18000/api".into(),
                flow_id: "x".into()
            }
        );
        assert!(parse_source("tams+ftp://h/flows/x", "/m").is_err());
        assert!(parse_source("tams+http://h/x", "/m").is_err());
        assert!(parse_source("tams+http://h/flows/", "/m").is_err());
    }

    #[test]
    fn parses_local_paths() {
        assert_eq!(
            parse_source("/abs/cam1", "/m").unwrap(),
            SourceSpec::Local("/abs/cam1".into())
        );
        assert_eq!(
            parse_source("gop10/cam1", "/m").unwrap(),
            SourceSpec::Local("/m/gop10/cam1".into())
        );
        assert_eq!(
            parse_source("file:///abs/cam1", "/m").unwrap(),
            SourceSpec::Local("/abs/cam1".into())
        );
    }

    fn seg(i: i64) -> Segment {
        Segment {
            start_ns: i * 10,
            end_ns: (i + 1) * 10,
            ts_offset_ns: 0,
            object_start_ns: None,
            object_id: i.to_string(),
            location: Location::Urls(vec![]),
        }
    }

    /// `read_newest` over segments `0..n`, pages of `page`, with or without
    /// paging cursors. Returns the starts (as indexes), `more` and the pages
    /// read.
    fn newest(n: i64, after: i64, page: usize, pages: usize, paged: bool) -> (Vec<i64>, bool, u32) {
        let all: Vec<Segment> = (0..n).rev().map(seg).collect();
        let mut read = 0;
        let r = futures::executor::block_on(read_newest(after, page, pages, |c: Option<usize>| {
            read += 1;
            let off = c.unwrap_or(0);
            let got: Vec<Segment> = all.iter().skip(off).take(page).cloned().collect();
            let len = got.len();
            let next = (paged && off + len < all.len()).then_some(off + len);
            async move { Ok((got, len, next)) }
        }))
        .unwrap();
        (
            r.segs.iter().map(|s| s.start_ns / 10).collect(),
            r.more,
            read,
        )
    }

    #[test]
    fn newest_reads_back_to_the_known_edge() {
        // Edge at 17 (segment 16 ends there): one page reaches it.
        let (s, more, read) = newest(20, 170, 8, 4, true);
        assert_eq!((s, more, read), ((12..20).collect(), false, 1));
        // Edge at 5 (segment 4 ends there): two pages, 19..=12 and 11..=4.
        let (s, more, read) = newest(20, 50, 8, 4, true);
        assert_eq!((s.first().copied(), more, read), (Some(4), false, 2));
        // Not reached within the pages: more.
        let (s, more, _) = newest(100, 50, 8, 2, true);
        assert_eq!((s.len(), more), (16, true));
        // A store without paging: a full page may hide more, a short one is
        // the whole store.
        assert!(newest(100, 50, 8, 4, false).1);
        let (s, more, _) = newest(5, -100, 8, 4, false);
        assert_eq!((s, more), ((0..5).collect(), false));
    }

    #[test]
    fn segment_from_tams_respects_offsets() {
        let s = Segment::from_tams(
            "o",
            "[100:0_101:0)",
            Some("90:0"),
            Some("[10:500000000_12:0)"),
            Location::Urls(vec![]),
        )
        .unwrap();
        assert_eq!(s.start_ns, 100_000_000_000);
        assert_eq!(s.end_ns, 101_000_000_000);
        assert_eq!(s.ts_offset_ns, 90_000_000_000);
        // Object starts at 10.5 s object time = 100.5 s on the flow timeline:
        // the segment uses the middle of the object.
        assert_eq!(s.first_sample_tai(), 100_500_000_000);

        let s = Segment::from_tams("o", "[5:0_6:0)", None, None, Location::Urls(vec![])).unwrap();
        assert_eq!(s.first_sample_tai(), 5_000_000_000);
        assert!(Segment::from_tams("o", "[5:0_", None, None, Location::Urls(vec![])).is_none());
        assert!(Segment::from_tams("o", "()", None, None, Location::Urls(vec![])).is_none());
    }
}
