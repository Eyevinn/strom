//! Local segment directory with a TAMS-like `index.json` (spike material).

use super::{Location, Segment, SourceIndex, SourceInfo};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

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
    #[serde(default)]
    codec: Option<String>,
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

/// `path` resolved (symlinks, `..`) and checked to lie inside `root`. A flow
/// can name any source, so without this anyone who can edit a flow could read
/// any file the server can.
fn confine(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("media directory {}: {}", root.display(), e))?;
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(format!(
            "{} is outside the media directory {}",
            path.display(),
            root.display()
        ))
    }
}

/// Whether an `object` in `index.json` names a file under the index's own
/// directory: relative, with no `..` (or root/prefix) component.
fn is_contained_object(object: &str) -> bool {
    let p = Path::new(object);
    !object.is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

impl LocalIndex {
    /// Load the index in `dir`, which must lie inside `media_path` once
    /// resolved. Segment objects must lie inside `dir`.
    pub fn load(dir: &Path, media_path: &Path) -> Result<Self, String> {
        let dir = confine(dir, media_path)?;
        let path = dir.join("index.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        Self::parse(&dir, &text).map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// `dir` is already resolved (see [`Self::load`]).
    fn parse(dir: &Path, text: &str) -> Result<Self, String> {
        let raw: RawIndex = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut segments = Vec::with_capacity(raw.segments.len());
        for s in raw.segments {
            if !is_contained_object(&s.object) {
                return Err(format!(
                    "segment object '{}' is not a relative path inside the index directory",
                    s.object
                ));
            }
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
                codec: raw.codec,
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
            Location::Path(p) => {
                // `parse` refused `..` and absolute objects; a symlink in
                // the directory can still point out of it.
                let resolved = confine(p, &self.dir).map_err(anyhow::Error::msg)?;
                tokio::fs::read(&resolved)
                    .await
                    .with_context(|| format!("reading {}", p.display()))
            }
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
    fn rejects_objects_outside_the_directory() {
        for object in ["../secret.ts", "a/../../secret.ts", "/etc/passwd", "", ".."] {
            let index = serde_json::json!({
                "segments": [{"object": object, "timerange": "[1:0_2:0)"}]
            });
            assert!(
                LocalIndex::parse(Path::new("/m/cam1"), &index.to_string()).is_err(),
                "{object}"
            );
        }
        let ok = r#"{"segments": [{"object": "./sub/seg.ts", "timerange": "[1:0_2:0)"}]}"#;
        assert!(LocalIndex::parse(Path::new("/m/cam1"), ok).is_ok());
    }

    /// A media directory with `cam1/index.json` and `cam1/seg.ts`, and a
    /// secret file next to (outside) it.
    fn media_tree() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(media.join("cam1")).unwrap();
        std::fs::write(
            media.join("cam1/index.json"),
            r#"{"segments": [{"object": "seg.ts", "timerange": "[1:0_2:0)"}]}"#,
        )
        .unwrap();
        std::fs::write(media.join("cam1/seg.ts"), b"ts").unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        std::fs::write(
            tmp.path().join("outside/index.json"),
            r#"{"segments": [{"object": "seg.ts", "timerange": "[1:0_2:0)"}]}"#,
        )
        .unwrap();
        (tmp, media)
    }

    #[tokio::test]
    async fn loads_only_inside_the_media_directory() {
        let (tmp, media) = media_tree();
        let idx = LocalIndex::load(&media.join("cam1"), &media).unwrap();
        let seg = idx.first().await.unwrap().unwrap();
        assert_eq!(idx.fetch(&seg).await.unwrap(), b"ts");
        // Through `..`, by absolute path, and a media path given with `..`.
        assert!(LocalIndex::load(&media.join("../outside"), &media).is_err());
        assert!(LocalIndex::load(&tmp.path().join("outside"), &media).is_err());
        assert!(LocalIndex::load(&media.join("cam1"), &media.join("cam1/..")).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_out_of_the_media_directory_are_refused() {
        let (tmp, media) = media_tree();
        // A directory symlink out of the media directory.
        std::os::unix::fs::symlink(tmp.path().join("outside"), media.join("link")).unwrap();
        assert!(LocalIndex::load(&media.join("link"), &media).is_err());
        // A segment object symlinked out of the index directory.
        std::fs::write(tmp.path().join("secret"), b"secret").unwrap();
        std::fs::remove_file(media.join("cam1/seg.ts")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret"), media.join("cam1/seg.ts")).unwrap();
        let idx = LocalIndex::load(&media.join("cam1"), &media).unwrap();
        let seg = idx.first().await.unwrap().unwrap();
        assert!(idx.fetch(&seg).await.is_err());
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
