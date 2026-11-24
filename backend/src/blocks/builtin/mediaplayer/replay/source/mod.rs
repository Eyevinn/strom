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
    /// The bytes of a segment.
    async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>>;
}

/// How a TAMS gateway is authenticated (block properties).
#[derive(Clone)]
pub enum AuthConfig {
    Fixed(AuthMethod),
    /// OSC PAT/SAT; the PAT is looked up under the flow id.
    Osc {
        service_id: Option<String>,
    },
}

/// A parsed `replay_source`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    /// Directory with `index.json`.
    Local(PathBuf),
    /// `tams+http(s)://<gateway>/flows/<flow_id>`.
    Tams { gateway: String, flow_id: String },
}

/// Parse `replay_source`: `tams+https://host[/prefix]/flows/<id>`,
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

/// Open the index for a source.
pub fn open(
    spec: &SourceSpec,
    auth: &AuthConfig,
    credential_key: &str,
) -> Result<Arc<dyn SourceIndex>, String> {
    match spec {
        SourceSpec::Local(dir) => Ok(Arc::new(local::LocalIndex::load(dir)?)),
        SourceSpec::Tams { gateway, flow_id } => {
            let auth = match auth {
                AuthConfig::Fixed(a) => a.clone(),
                AuthConfig::Osc { service_id } => AuthMethod::Osc {
                    provider: crate::osc::sat_provider(),
                    service_id: service_id
                        .clone()
                        .or_else(|| crate::osc::derive_service_id(gateway))
                        .ok_or_else(|| {
                            "OSC auth: cannot derive the service id from the gateway URL"
                                .to_string()
                        })?,
                    credential_key: credential_key.to_string(),
                },
            };
            Ok(Arc::new(
                tams::TamsIndex::new(gateway, flow_id, auth).map_err(|e| e.to_string())?,
            ))
        }
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
