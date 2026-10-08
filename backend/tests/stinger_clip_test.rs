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
#[serial_test::serial(stinger)]
fn examples_encode_and_analyse_as_their_own_variants() {
    require(&["avenc_ffv1", "avdec_ffv1", "matroskamux", "matroskademux"]);
    let dir = tempfile::tempdir().unwrap();
    let files = examples::write_examples(dir.path()).expect("write examples");
    assert_eq!(
        files,
        [
            "stingers/strom-sweep.mkv",
            "stingers/strom-blade.mkv",
            "stingers/strom-ribbons.mkv",
            "stingers/strom-shards.mkv",
        ]
    );

    let uri = |rel: &str| {
        gstreamer::glib::filename_to_uri(dir.path().join(rel), None)
            .unwrap()
            .to_string()
    };
    // GStreamer 1.24.2's FFV1 decoder drops a stream's last frame now and
    // then (fixed in 1.24.4); the examples are FFV1.
    let frames_near = |got: u32, want: u32| got == want || got + 1 == want;

    let sweep = analysis::analyze(&uri(&files[0])).expect("analyse sweep");
    assert_eq!(sweep.detected_layout, StingerLayout::Classic);
    assert!(sweep.has_alpha);
    assert_eq!((sweep.width, sweep.height), (1920, 1080));
    assert!(frames_near(sweep.frames, 45), "{} frames", sweep.frames);
    assert_eq!(sweep.duration_ms, sweep.frames as u64 * 1000 / 30);
    assert!(sweep.cover_peak > 0.99, "peak cover {}", sweep.cover_peak);
    let peak = sweep.cover_peak_ms.expect("cover peak");
    assert!(
        (650..=850).contains(&peak),
        "the sweep holds full cover in its middle, cut found at {peak} ms"
    );

    let blade = analysis::analyze(&uri(&files[1])).expect("analyse blade");
    assert_eq!(blade.detected_layout, StingerLayout::SideBySide);
    assert!(blade.has_alpha);
    assert_eq!((blade.width, blade.height), (3840, 1080));
    assert!(frames_near(blade.frames, 40), "{} frames", blade.frames);
    let mid = blade.matte_midpoint_ms.expect("matte midpoint");
    assert!(
        (500..=800).contains(&mid),
        "the blade is half way across mid-clip, matte switched at {mid} ms"
    );

    let ribbons = analysis::analyze(&uri(&files[2])).expect("analyse ribbons");
    assert_eq!(ribbons.detected_layout, StingerLayout::Stacked);
    assert!(ribbons.has_alpha);
    assert_eq!((ribbons.width, ribbons.height), (1920, 2160));
    assert!(frames_near(ribbons.frames, 42), "{} frames", ribbons.frames);
    let mid = ribbons.matte_midpoint_ms.expect("matte midpoint");
    assert!(
        (300..=900).contains(&mid),
        "the ribbons have revealed half the picture mid-clip, not at {mid} ms"
    );

    let shards = analysis::analyze(&uri(&files[3])).expect("analyse shards");
    assert_eq!(shards.detected_layout, StingerLayout::MaskOnly);
    assert!(!shards.has_alpha);
    assert_eq!(shards.cover_peak_ms, None, "a mask has no graphic");

    // Every matte example switches once, from black to white, inside the
    // clip: it starts moving, crosses half way, and comes to rest.
    for info in [&blade, &ribbons, &shards] {
        let (start, mid, end) = (
            info.matte_start_ms.expect("matte start"),
            info.matte_midpoint_ms.expect("matte midpoint"),
            info.matte_end_ms.expect("matte end"),
        );
        assert!(
            start < mid && mid <= end && end < info.duration_ms,
            "{:?}: matte marks {start} / {mid} / {end} in {} ms",
            info.detected_layout,
            info.duration_ms
        );
    }
}

#[test]
#[serial_test::serial(stinger)]
fn analysis_is_cached_until_the_file_changes() {
    require(&["avenc_ffv1", "avdec_ffv1", "matroskamux", "matroskademux"]);
    let dir = tempfile::tempdir().unwrap();
    // The mask: the cheapest example to render.
    let ex = examples::examples()
        .into_iter()
        .find(|e| e.file_name == "strom-shards.mkv")
        .unwrap();
    let path = dir.path().join(ex.file_name);
    examples::write_example(&ex, &path).unwrap();
    let uri = gstreamer::glib::filename_to_uri(&path, None)
        .unwrap()
        .to_string();

    assert!(analysis::cached(&uri).is_none());
    let first = analysis::analyze_cached(&uri).unwrap();
    assert_eq!(analysis::cached(&uri), Some(first.clone()));
}

#[test]
#[serial_test::serial(stinger)]
fn a_missing_clip_is_an_error_not_a_hang() {
    gstreamer::init().unwrap();
    let err = analysis::analyze("file:///nonexistent/strom-stinger.mkv").unwrap_err();
    assert!(!err.is_empty());
}

/// Writes the examples to `STROM_STINGER_EXAMPLES_DIR` for a look by eye.
#[test]
#[serial_test::serial(stinger)]
#[ignore]
fn write_examples_for_inspection() {
    require(&["avenc_ffv1", "matroskamux"]);
    let dir = std::env::var("STROM_STINGER_EXAMPLES_DIR").expect("set STROM_STINGER_EXAMPLES_DIR");
    let files = examples::write_examples(std::path::Path::new(&dir)).unwrap();
    println!("{files:?}");
}
