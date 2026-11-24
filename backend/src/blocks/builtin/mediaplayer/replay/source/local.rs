//! Local segment directory with a TAMS-like `index.json` (spike material).

use super::{Location, Segment, SourceIndex, SourceInfo};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct RawRate {
    numerator: u32,
    denominator: u32,
}

#[derive(Deserialize)]
struct RawSegment {
    object: String,
    /// TAMS timerange; preferred over the ns fields when present.
    #[serde(default)]
    timerange: Option<String>,
    #[serde(default)]
    start_ns: Option<i64>,
    #[serde(default)]
    end_ns: Option<i64>,
}

#[derive(Deserialize)]
struct RawIndex {
    #[serde(default)]
    frame_rate: Option<RawRate>,
    #[serde(default)]
    container: Option<String>,
    /// The times are real TAI. Defaults to false: test material is cut on an
    /// arbitrary `tai_base_ns`.
    #[serde(default)]
    absolute_time: bool,
    segments: Vec<RawSegment>,
}

/// All segments of a directory, loaded once. Does not grow.
pub struct LocalIndex {
    dir: PathBuf,
    info: SourceInfo,
    segments: Vec<Segment>,
}

impl LocalIndex {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join("index.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        Self::parse(dir, &text).map_err(|e| format!("{}: {}", path.display(), e))
    }

    fn parse(dir: &Path, text: &str) -> Result<Self, String> {
        let raw: RawIndex = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut segments = Vec::with_capacity(raw.segments.len());
        for s in raw.segments {
            let location = Location::Path(dir.join(&s.object));
            let seg = match (&s.timerange, s.start_ns, s.end_ns) {
                (_, Some(start_ns), Some(end_ns)) if end_ns > start_ns => Segment {
                    start_ns,
                    end_ns,
                    ts_offset_ns: 0,
                    object_start_ns: None,
                    object_id: s.object.clone(),
                    location,
                },
                (Some(tr), _, _) => Segment::from_tams(&s.object, tr, None, None, location)
                    .ok_or_else(|| format!("bad timerange '{}'", tr))?,
                _ => return Err(format!("segment {} has no timerange", s.object)),
            };
            segments.push(seg);
        }
        if segments.is_empty() {
            return Err("no segments".to_string());
        }
        segments.sort_by_key(|s| s.start_ns);
        Ok(Self {
            dir: dir.to_path_buf(),
            info: SourceInfo {
                frame_rate: raw
                    .frame_rate
                    .filter(|r| r.numerator > 0 && r.denominator > 0)
                    .map(|r| (r.numerator, r.denominator)),
                container: raw.container,
                absolute_time: raw.absolute_time,
            },
            segments,
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.segments.len()
    }
}

#[async_trait]
impl SourceIndex for LocalIndex {
    fn describe(&self) -> String {
        self.dir.display().to_string()
    }

    fn may_grow(&self) -> bool {
        false
    }

    async fn info(&self) -> Result<SourceInfo> {
        Ok(self.info.clone())
    }

    async fn list(&self, start_ns: i64, end_ns: i64) -> Result<Vec<Segment>> {
        Ok(self
            .segments
            .iter()
            .filter(|s| s.end_ns > start_ns && s.start_ns < end_ns)
            .cloned()
            .collect())
    }

    async fn first(&self) -> Result<Option<Segment>> {
        Ok(self.segments.first().cloned())
    }

    async fn last(&self) -> Result<Option<Segment>> {
        Ok(self.segments.last().cloned())
    }

    async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>> {
        match &segment.location {
            Location::Path(p) => tokio::fs::read(p)
                .await
                .with_context(|| format!("reading {}", p.display())),
            Location::Urls(_) => anyhow::bail!("local index segment without a path"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"{
        "flow_id": "cam1", "container": "video/mp2t",
        "frame_rate": {"numerator": 50, "denominator": 1},
        "tai_base_ns": 1800000000000000000,
        "segments": [
          {"object": "seg_00001.ts", "timerange": "[1800000001:0_1800000002:0)",
           "start_ns": 1800000001000000000, "end_ns": 1800000002000000000},
          {"object": "seg_00000.ts", "timerange": "[1800000000:0_1800000001:0)"}
        ]}"#;

    #[tokio::test]
    async fn parses_spike_index() {
        let idx = LocalIndex::parse(Path::new("/m/cam1"), INDEX).unwrap();
        assert_eq!(idx.len(), 2);
        let info = idx.info().await.unwrap();
        assert_eq!(info.frame_rate, Some((50, 1)));
        assert_eq!(info.container.as_deref(), Some("video/mp2t"));
        assert!(!info.absolute_time);
        let first = idx.first().await.unwrap().unwrap();
        assert_eq!(first.start_ns, 1_800_000_000_000_000_000);
        assert_eq!(
            first.location,
            Location::Path("/m/cam1/seg_00000.ts".into())
        );
        let last = idx.last().await.unwrap().unwrap();
        assert_eq!(last.end_ns, 1_800_000_002_000_000_000);
        let hit = idx
            .list(1_800_000_000_500_000_000, 1_800_000_001_000_000_000)
            .await
            .unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].object_id, "seg_00000.ts");
    }

    #[test]
    fn rejects_empty_and_bad() {
        assert!(LocalIndex::parse(Path::new("/"), r#"{"segments": []}"#).is_err());
        assert!(LocalIndex::parse(
            Path::new("/"),
            r#"{"segments": [{"object": "a", "timerange": "[1:0_"}]}"#
        )
        .is_err());
    }
}
