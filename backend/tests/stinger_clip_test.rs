//! The example stinger clips, encoded for real, and what analysing them
//! finds: each is detected as its own variant, and the marks a classic take
//! cuts on land where the artwork puts them.

use strom::stinger::{analysis, examples};
use strom_types::stinger::StingerLayout;

fn require(elements: &[&str]) {
    gstreamer::init().expect("gstreamer init");
    for name in elements {
        assert!(
            gstreamer::ElementFactory::find(name).is_some(),
            "{name} missing: CI installs gst-libav and plugins-good"
        );
    }
}

#[test]
fn examples_encode_and_analyse_as_their_own_variants() {
    require(&["avenc_ffv1", "avdec_ffv1", "matroskamux", "matroskademux"]);
    let dir = tempfile::tempdir().unwrap();
    let files = examples::write_examples(dir.path()).expect("write examples");
    assert_eq!(files.len(), 3);

    let uri = |rel: &str| {
        gstreamer::glib::filename_to_uri(dir.path().join(rel), None)
            .unwrap()
            .to_string()
    };

    let sweep = analysis::analyze(&uri(&files[0])).expect("analyse sweep");
    assert_eq!(sweep.detected_layout, StingerLayout::Classic);
    assert!(sweep.has_alpha);
    assert_eq!((sweep.width, sweep.height), (1920, 1080));
    assert_eq!(sweep.frames, 45);
    assert_eq!(sweep.duration_ms, 1500);
    assert!(sweep.cover_peak > 0.99, "peak cover {}", sweep.cover_peak);
    let peak = sweep.cover_peak_ms.expect("cover peak");
    assert!(
        (650..=850).contains(&peak),
        "the sweep holds full cover in its middle, cut found at {peak} ms"
    );

    let blade = analysis::analyze(&uri(&files[1])).expect("analyse blade");
    assert_eq!(blade.detected_layout, StingerLayout::SideBySide);
    assert_eq!((blade.width, blade.height), (3840, 1080));
    let mid = blade.matte_midpoint_ms.expect("matte midpoint");
    assert!(
        (450..=750).contains(&mid),
        "the blade is half way across mid-clip, matte switched at {mid} ms"
    );
    assert!(blade.matte_start_ms < blade.matte_end_ms);

    let shards = analysis::analyze(&uri(&files[2])).expect("analyse shards");
    assert_eq!(shards.detected_layout, StingerLayout::MaskOnly);
    assert!(!shards.has_alpha);
    assert!(shards.matte_midpoint_ms.is_some());
    assert_eq!(shards.cover_peak_ms, None, "a mask has no graphic");
}

#[test]
fn analysis_is_cached_until_the_file_changes() {
    require(&["avenc_ffv1", "avdec_ffv1", "matroskamux", "matroskademux"]);
    let dir = tempfile::tempdir().unwrap();
    let ex = &examples::examples()[2];
    let path = dir.path().join(ex.file_name);
    examples::write_example(ex, &path).unwrap();
    let uri = gstreamer::glib::filename_to_uri(&path, None)
        .unwrap()
        .to_string();

    assert!(analysis::cached(&uri).is_none());
    let first = analysis::analyze_cached(&uri).unwrap();
    assert_eq!(analysis::cached(&uri), Some(first.clone()));
}

#[test]
fn a_missing_clip_is_an_error_not_a_hang() {
    gstreamer::init().unwrap();
    let err = analysis::analyze("file:///nonexistent/strom-stinger.mkv").unwrap_err();
    assert!(!err.is_empty());
}

/// Writes the examples to `STROM_STINGER_EXAMPLES_DIR` for a look by eye.
#[test]
#[ignore]
fn write_examples_for_inspection() {
    require(&["avenc_ffv1", "matroskamux"]);
    let dir = std::env::var("STROM_STINGER_EXAMPLES_DIR").expect("set STROM_STINGER_EXAMPLES_DIR");
    let files = examples::write_examples(std::path::Path::new(&dir)).unwrap();
    println!("{files:?}");
}
