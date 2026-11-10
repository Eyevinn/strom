//! Every pipeline Strom runs sets the scheduling of a streaming thread it
//! picks from GStreamer's pool.
//!
//! A WebRTC session thread (WHEP and WHIP Output) goes back to the pool with
//! the flow's priority and CPUs still on it: webrtcsink drops its Leave, so
//! nothing of ours can undo them as it leaves. Each pipeline that picks a
//! thread up sets them on Enter instead: a flow through its sync handler, a
//! block's own pipeline through `SessionThreadConfig`, and a pipeline outside
//! any flow through `thread_priority::reset_streaming_threads`. A pipeline
//! that does none of these runs its work at whatever priority the thread last
//! had.
//!
//! This test reads the backend source and fails on a file that builds a
//! pipeline outside test code without one of those calls. A file whose
//! pipeline gets its handler elsewhere is listed in [`HANDLED_ELSEWHERE`],
//! with the reason.

use std::path::{Path, PathBuf};

/// What builds a pipeline.
const CONSTRUCTORS: &[&str] = &[
    "Pipeline::new(",
    "Pipeline::builder(",
    "Pipeline::default(",
    "parse::launch(",
    "parse::bin_from_description(",
];

/// Calls that set the scheduling of the pipeline's threads on Enter.
const HANDLERS: &[&str] = &[
    "reset_streaming_threads(",
    "install_on_internal_pipeline(",
    "setup_thread_priority_handler(",
];

/// Files that build a pipeline whose handler is installed in another file.
const HANDLED_ELSEWHERE: &[(&str, &str)] = &[
    (
        "gst/pipeline/construction.rs",
        "the flow's own pipeline; PipelineManager::start installs setup_thread_priority_handler",
    ),
    (
        "blocks/builtin/mediaplayer/bridge.rs",
        "the Media Player's internal pipeline; its builder installs install_on_internal_pipeline",
    ),
    (
        "api/gst_launch.rs",
        "parses a pipeline description to show it, and never plays it",
    ),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The part of `source` outside an inline `#[cfg(test)] mod ... {`, which
/// by convention sits at the end of a file.
fn non_test_part(source: &str) -> &str {
    let lines: Vec<&str> = source.lines().collect();
    let mut offset = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines[i + 1..].iter().find(|l| !l.trim().is_empty());
            if next.is_some_and(|l| {
                let l = l.trim();
                (l.starts_with("mod ")
                    || l.starts_with("pub mod ")
                    || l.starts_with("pub(crate) mod "))
                    && l.ends_with('{')
            }) {
                return &source[..offset];
            }
        }
        offset += line.len() + 1;
    }
    source
}

#[test]
fn every_pipeline_sets_the_scheduling_of_threads_it_picks_up() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);

    let mut missing = Vec::new();
    for path in files {
        // A test module in a file of its own.
        if path.file_name().is_some_and(|n| n == "tests.rs") {
            continue;
        }
        let relative = path
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path).expect("read source file");
        let code = non_test_part(&source);
        let builds_pipeline = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| CONSTRUCTORS.iter().any(|c| l.contains(c)));
        if !builds_pipeline || HANDLED_ELSEWHERE.iter().any(|(f, _)| *f == relative) {
            continue;
        }
        if !HANDLERS.iter().any(|h| code.contains(h)) {
            missing.push(relative);
        }
    }

    assert!(
        missing.is_empty(),
        "these files build a pipeline that does not set the scheduling of the threads it picks \
         from GStreamer's pool: {:?}. Call thread_priority::reset_streaming_threads on a pipeline \
         outside a flow, or list the file in HANDLED_ELSEWHERE with the reason",
        missing
    );
}

/// The listed files still build a pipeline; an entry left behind after a
/// refactor would exempt whatever comes next in that file.
#[test]
fn every_exemption_still_builds_a_pipeline() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (file, reason) in HANDLED_ELSEWHERE {
        let source = std::fs::read_to_string(src.join(file))
            .unwrap_or_else(|e| panic!("{file} ({reason}): {e}"));
        assert!(
            CONSTRUCTORS
                .iter()
                .any(|c| non_test_part(&source).contains(c)),
            "{file} no longer builds a pipeline; remove it from HANDLED_ELSEWHERE"
        );
    }
}
