//! Where TAMS Output keeps its segment files, parked segments included.
//!
//! A parked segment (the store was unreachable) must survive a restart of
//! Strom, and of the machine, to be uploaded later, so the files live under
//! Strom's data directory rather than the system temp directory:
//!
//! `<root>/<tams flow id>`
//!
//! - `<root>` is `STROM_TAMS_PARK_DIR` when set, else `<data_dir>/tams-park`,
//!   else (no data directory known, e.g. a bare builder in a test)
//!   `<temp_dir>/strom-tams-park`.
//! - One directory per TAMS flow the block writes to, named by its id. The
//!   TAMS flow id is the recording's identity: it survives a restart of the
//!   Strom flow, and a production that recreates its Strom flow (new flow id,
//!   new random block ids) with the same TAMS ids finds its backlog again. A
//!   Strom flow id or block id in the path would orphan that backlog. Two
//!   running blocks never write one TAMS flow (see `crate::tams::flow_claims`).
//!
//! Directories of the old layout (`<temp_dir>/strom-tams-<block>-<suffix>`) are
//! not migrated; [`init`] warns once at startup when they still hold parked
//! segments.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::warn;

/// Overrides the root of the segment directories.
pub const PARK_DIR_ENV: &str = "STROM_TAMS_PARK_DIR";

/// The directory under the data directory that holds the segment directories.
const DATA_SUBDIR: &str = "tams-park";

/// The root under the temp directory when no data directory is known.
const TEMP_SUBDIR: &str = "strom-tams-park";

/// Prefix of the old per-block directories under the temp directory.
const LEGACY_PREFIX: &str = "strom-tams-";

/// The sidecar that marks a parked segment (see `crate::tams::uploader`).
const SIDECAR_SUFFIX: &str = ".meta";

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Record Strom's data directory, which the default root lives under, and
/// warn about parked segments left in the old temp-dir layout. Call once at
/// startup, before any flow is built.
pub fn init(data_dir: &Path) {
    let _ = DATA_DIR.set(data_dir.to_path_buf());
    let legacy = legacy_parked_dirs(&std::env::temp_dir());
    if !legacy.is_empty() {
        warn!(
            "TAMS: {} director{} in the old layout still hold parked segments ({}); they are \
             no longer uploaded. TAMS Output now keeps its segments under {}. Move a \
             directory's files to <that dir>/<TAMS flow id> to have them uploaded, or \
             delete them",
            legacy.len(),
            if legacy.len() == 1 { "y" } else { "ies" },
            legacy
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            root().display()
        );
    }
}

/// The root of all TAMS Output segment directories.
pub fn root() -> PathBuf {
    if let Some(dir) = strom_types::env::var_opt(PARK_DIR_ENV) {
        return PathBuf::from(dir);
    }
    match DATA_DIR.get() {
        Some(data_dir) => data_dir.join(DATA_SUBDIR),
        None => std::env::temp_dir().join(TEMP_SUBDIR),
    }
}

/// The segment directory of the TAMS flow `tams_flow_id`.
pub fn flow_dir(tams_flow_id: &str) -> PathBuf {
    root().join(sanitize(tams_flow_id))
}

/// A path component from an id: letters, digits, `-` and `_` are kept, all
/// else becomes `_`, so an id never escapes the root or adds a level.
fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "_".to_string()
    } else {
        s
    }
}

/// Old-layout directories under `tmp` (`strom-tams-<block>-<suffix>`) that
/// still hold a parked segment's sidecar.
fn legacy_parked_dirs(tmp: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(LEGACY_PREFIX)
                && name != TEMP_SUBDIR
                // The uploader's unit tests use these.
                && !name.starts_with("strom-tams-uploader-")
                && e.file_type().is_ok_and(|t| t.is_dir())
        })
        .map(|e| e.path())
        .filter(|dir| has_sidecar(dir))
        .collect();
    dirs.sort();
    dirs
}

fn has_sidecar(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(SIDECAR_SUFFIX))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tams_park_dir_is_one_level_per_tams_flow() {
        let id = "6f1c2a4e-0b7d-5c3e-9a1f-2d4b6c8e0a1b";
        assert_eq!(flow_dir(id), root().join(id));
        // An id never escapes the root or adds a level.
        let odd = flow_dir("../x/y");
        assert_eq!(odd.strip_prefix(root()).unwrap().iter().count(), 1);
    }

    #[test]
    fn tams_legacy_dirs_with_parked_segments_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let parked = tmp.path().join("strom-tams-cam1-mux");
        let empty = tmp.path().join("strom-tams-cam2-mux");
        let tests = tmp.path().join("strom-tams-uploader-x");
        for d in [&parked, &empty, &tests] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(parked.join("seg_r1_00001.ts.meta"), "{}").unwrap();
        std::fs::write(empty.join("seg_r1_00001.ts"), "").unwrap();
        std::fs::write(tests.join("seg_r1_00001.ts.meta"), "{}").unwrap();
        assert_eq!(legacy_parked_dirs(tmp.path()), vec![parked]);
    }
}
