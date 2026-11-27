//! Local segment directory with an `index.json`: an experimental dev and
//! test source.
//!
//! Spike material, not a recording format: nothing in Strom writes it, the
//! format is not stable and may go away, and it is left out of the user docs.
//! The engine's end-to-end tests use it to put segments on disk.
//!
//! - The directory must lie inside the media path. A source outside it is
//!   refused, whether it is given as an absolute path, through `..` or through
//!   a symlink.
//! - `index.json` is read once, when the flow starts, and never again
//!   ([`SourceIndex::may_grow`] is false): segments a recorder adds later are
//!   not seen until the flow restarts.
//! - Format:
//!
//!   ```json
//!   {"container": "video/mp2t", "codec": "video/h264",
//!    "frame_rate": {"numerator": 25, "denominator": 1},
//!    "absolute_time": false,
//!    "segments": [{"object": "seg_0.ts", "timerange": "[100:0_101:0)"}]}
//!   ```
//!
//!   Each segment gives its TAI time as a TAMS `timerange`, or as `start_ns`
//!   and `end_ns` (both; given alongside a `timerange` they must agree).
//!   `object` is a relative path inside the directory. Segments must not
//!   overlap, and each is at most [`MAX_SEGMENT_NS`] long. Other fields are
//!   ignored.

use super::{Location, Segment, SourceIndex, SourceInfo};
use crate::tams::client::MAX_OBJECT_BYTES;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

/// Longest segment accepted (60 s). A segment's frames are decoded in one go,
/// so a whole recording listed as one segment would decode into memory.
pub const MAX_SEGMENT_NS: i64 = 60_000_000_000;

#[derive(Deserialize)]
struct RawRate {
    numerator: u32,
    denominator: u32,
}

#[derive(Deserialize)]
struct RawSegment {
    object: String,
    /// TAMS timerange, or `start_ns` and `end_ns`; given together, they must
    /// agree.
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
    /// arbitrary TAI base.
    #[serde(default)]
    absolute_time: bool,
    segments: Vec<RawSegment>,
}

/// All segments of a directory, loaded once. Does not grow.
pub struct LocalIndex {
    dir: PathBuf,
    info: SourceInfo,
    /// Sorted by time, not overlapping.
    segments: Vec<Segment>,
    /// Largest object [`SourceIndex::fetch`] reads.
    max_object_bytes: u64,
}

/// `p` made absolute (against the working directory) with `.` and `..`
/// removed by text alone, without touching the file system.
fn absolute_lexical(p: &Path) -> Option<PathBuf> {
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// Why [`confine`] refused a path.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
    /// The root itself cannot be resolved.
    Root,
    /// The path is outside the root by its text alone.
    Outside,
    /// Inside the root by its text, but missing, unreadable or resolving
    /// (through a symlink) to a place outside it. One case, so that the
    /// error does not tell whether a path outside the root exists.
    NotFound,
}

/// `path` checked to lie inside `root`, then resolved (symlinks) and checked
/// again. A flow can name any source, so without this anyone who can edit a
/// flow could read any file the server can. The containment check comes
/// before the file system is touched, so a path outside `root` is refused
/// the same way whether it exists or not.
fn confine(path: &Path, root: &Path) -> Result<PathBuf, Refused> {
    let root_real = root.canonicalize().map_err(|_| Refused::Root)?;
    let root_abs = absolute_lexical(root).ok_or(Refused::Root)?;
    let path_abs = absolute_lexical(path).ok_or(Refused::Outside)?;
    let rel = path_abs
        .strip_prefix(&root_abs)
        .or_else(|_| path_abs.strip_prefix(&root_real))
        .map_err(|_| Refused::Outside)?;
    let resolved = root_real
        .join(rel)
        .canonicalize()
        .map_err(|_| Refused::NotFound)?;
    if resolved.starts_with(&root_real) {
        Ok(resolved)
    } else {
        Err(Refused::NotFound)
    }
}

/// `path` relative to `root` for messages, so they do not show where the
/// media directory is. Empty when `path` is not inside `root` by its text.
fn shown(path: &Path, root: &Path) -> String {
    match (absolute_lexical(path), absolute_lexical(root)) {
        (Some(p), Some(r)) => p
            .strip_prefix(&r)
            .map(|rel| rel.display().to_string())
            .unwrap_or_default(),
        _ => String::new(),
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
    /// Load the index in `dir`, which must lie inside `media_path`. Segment
    /// objects must lie inside `dir`. Errors name `dir` relative to the media
    /// path only.
    pub fn load(dir: &Path, media_path: &Path) -> Result<Self, String> {
        let name = shown(dir, media_path);
        let dir = confine(dir, media_path).map_err(|e| match e {
            Refused::Root => "the media directory is not available".to_string(),
            Refused::Outside => "a local source must be inside the media directory".to_string(),
            Refused::NotFound => format!("'{}' is not found in the media directory", name),
        })?;
        let text = std::fs::read_to_string(dir.join("index.json"))
            .map_err(|e| format!("cannot read {}/index.json: {}", name, e))?;
        Self::parse(&dir, &text).map_err(|e| format!("{}/index.json: {}", name, e))
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
            let ns = match (s.start_ns, s.end_ns) {
                (Some(start_ns), Some(end_ns)) => Some((start_ns, end_ns)),
                (None, None) => None,
                _ => {
                    return Err(format!(
                        "segment {} has only one of start_ns and end_ns",
                        s.object
                    ))
                }
            };
            let from_timerange = match &s.timerange {
                Some(tr) => Some(
                    Segment::from_tams(&s.object, tr, None, None, location.clone())
                        .ok_or_else(|| format!("bad timerange '{}'", tr))?,
                ),
                None => None,
            };
            let seg = match (ns, from_timerange) {
                (Some((start_ns, end_ns)), Some(seg)) => {
                    if (seg.start_ns, seg.end_ns) != (start_ns, end_ns) {
                        return Err(format!(
                            "segment {}: start_ns and end_ns disagree with its timerange",
                            s.object
                        ));
                    }
                    seg
                }
                (Some((start_ns, end_ns)), None) => {
                    if end_ns <= start_ns {
                        return Err(format!("segment {} is empty", s.object));
                    }
                    Segment {
                        start_ns,
                        end_ns,
                        ts_offset_ns: 0,
                        object_start_ns: None,
                        object_id: s.object.clone(),
                        location,
                    }
                }
                (None, Some(seg)) => seg,
                (None, None) => return Err(format!("segment {} has no timerange", s.object)),
            };
            if seg.duration_ns() > MAX_SEGMENT_NS {
                return Err(format!(
                    "segment {} is longer than {} s",
                    s.object,
                    MAX_SEGMENT_NS / 1_000_000_000
                ));
            }
            segments.push(seg);
        }
        if segments.is_empty() {
            return Err("no segments".to_string());
        }
        segments.sort_by_key(|s| s.start_ns);
        if let Some(w) = segments.windows(2).find(|w| w[1].start_ns < w[0].end_ns) {
            return Err(format!(
                "segments {} and {} overlap",
                w[0].object_id, w[1].object_id
            ));
        }
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
            max_object_bytes: MAX_OBJECT_BYTES,
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
                // the directory can still point out of it. Resolving touches
                // the file system: off the async runtime.
                let (path, dir) = (p.clone(), self.dir.clone());
                let object = segment.object_id.clone();
                let (resolved, size) = tokio::task::spawn_blocking(move || {
                    let resolved = confine(&path, &dir).map_err(|_| {
                        anyhow::anyhow!("segment {} not found in the index directory", object)
                    })?;
                    let size = std::fs::metadata(&resolved)
                        .with_context(|| format!("reading segment {}", object))?
                        .len();
                    anyhow::Ok((resolved, size))
                })
                .await
                .context("resolving a segment path")??;
                if size > self.max_object_bytes {
                    anyhow::bail!(
                        "segment {} is {} bytes, more than {}",
                        segment.object_id,
                        size,
                        self.max_object_bytes
                    );
                }
                tokio::fs::read(&resolved)
                    .await
                    .with_context(|| format!("reading segment {}", segment.object_id))
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

    /// Whether a path outside the media directory exists does not change
    /// the error, and no error names the media directory itself.
    #[cfg(unix)]
    #[test]
    fn errors_do_not_reveal_paths_outside_the_media_directory() {
        let (tmp, media) = media_tree();
        let root = media.canonicalize().unwrap();
        let err = |dir: &Path| LocalIndex::load(dir, &media).err().unwrap();
        let exists = err(&tmp.path().join("outside"));
        let missing = err(&tmp.path().join("nowhere"));
        assert_eq!(exists, missing);
        assert_eq!(
            err(&media.join("../outside")),
            err(&media.join("../nowhere"))
        );
        // A symlink out of the media directory: whether its target exists
        // does not show either.
        std::os::unix::fs::symlink(tmp.path().join("outside"), media.join("to_outside")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), media.join("to_nowhere")).unwrap();
        let link_exists = err(&media.join("to_outside"));
        let link_missing = err(&media.join("to_nowhere")).replace("to_nowhere", "to_outside");
        assert_eq!(link_exists, link_missing);
        for e in [exists, missing, link_exists, err(&media.join("nothing"))] {
            for root in [root.to_str().unwrap(), media.to_str().unwrap()] {
                assert!(!e.contains(root), "{e}");
            }
        }
    }

    /// `start_ns` and `end_ns` come as a pair and agree with a
    /// `timerange` given alongside.
    #[test]
    fn ns_pair_is_whole_and_agrees_with_the_timerange() {
        let one = |seg: serde_json::Value| {
            let index = serde_json::json!({ "segments": [seg] });
            LocalIndex::parse(Path::new("/m/cam1"), &index.to_string())
        };
        assert!(one(serde_json::json!({"object": "a", "start_ns": 1_000_000_000})).is_err());
        assert!(one(serde_json::json!({
            "object": "a", "timerange": "[1:0_2:0)", "end_ns": 2_000_000_000
        }))
        .is_err());
        assert!(one(serde_json::json!({
            "object": "a", "timerange": "[1:0_2:0)",
            "start_ns": 1_000_000_000, "end_ns": 3_000_000_000_i64
        }))
        .is_err());
        assert!(one(serde_json::json!({
            "object": "a", "timerange": "[1:0_2:0)",
            "start_ns": 1_000_000_000, "end_ns": 2_000_000_000
        }))
        .is_ok());
        assert!(one(serde_json::json!({
            "object": "a", "start_ns": 2_000_000_000, "end_ns": 2_000_000_000
        }))
        .is_err());
    }

    /// Overlapping or duplicate segments are refused, so the last segment
    /// is the one that ends latest.
    #[test]
    fn rejects_overlapping_segments() {
        let index = |a: &str, b: &str| {
            serde_json::json!({"segments": [
                {"object": "a", "timerange": a},
                {"object": "b", "timerange": b},
            ]})
            .to_string()
        };
        let dir = Path::new("/m/cam1");
        assert!(LocalIndex::parse(dir, &index("[0:0_10:0)", "[1:0_2:0)")).is_err());
        assert!(LocalIndex::parse(dir, &index("[1:0_2:0)", "[1:0_2:0)")).is_err());
        assert!(LocalIndex::parse(dir, &index("[1:0_2:0)", "[2:0_3:0)")).is_ok());
    }

    /// A segment longer than [`MAX_SEGMENT_NS`] is refused: its frames are
    /// decoded in one go.
    #[test]
    fn rejects_long_segments() {
        let index = |end: &str| {
            serde_json::json!({"segments": [{"object": "a", "timerange": format!("[0:0_{end})")}]})
                .to_string()
        };
        assert!(LocalIndex::parse(Path::new("/m"), &index("60:0")).is_ok());
        assert!(LocalIndex::parse(Path::new("/m"), &index("3600:0")).is_err());
    }

    /// An object over the size limit is not read.
    #[tokio::test]
    async fn fetch_refuses_objects_over_the_size_limit() {
        let (_tmp, media) = media_tree();
        let mut idx = LocalIndex::load(&media.join("cam1"), &media).unwrap();
        let seg = idx.first().await.unwrap().unwrap();
        idx.max_object_bytes = 1;
        assert!(idx.fetch(&seg).await.is_err());
        idx.max_object_bytes = 2;
        assert_eq!(idx.fetch(&seg).await.unwrap(), b"ts");
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
