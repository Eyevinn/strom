//! Stinger measurements: how long cueing and analysing a clip take, and how
//! early its frames reach the mixer, across clip formats, sizes and preroll
//! settings. Prints a table; asserts nothing beyond the takes running.
//!
//! ```text
//! cargo test --release --test stinger_bench -- --ignored --nocapture
//! ```
//!
//! VP9-with-alpha clips (the common OBS pack format) are made with the
//! `ffmpeg` command when it is installed; GStreamer cannot encode them.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const FRAMES: usize = 45;

/// A clip of `w`x`h` BGRA frames: a band sweeping across, opaque, on
/// transparency. `sbs` adds a matte half.
fn synthetic(path: &Path, w: u32, h: u32, sbs: bool, encoder: &str, muxer: &str, format: &str) {
    let full_w = if sbs { 2 * w } else { w };
    let desc = format!(
        "appsrc name=src ! videoconvert ! video/x-raw,format={format} ! {encoder} ! {muxer} ! filesink location=\"{}\"",
        path.display()
    );
    let pipeline = gst::parse::launch(&desc)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let src = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    src.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", full_w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    src.set_format(gst::Format::Time);
    pipeline.set_state(gst::State::Playing).unwrap();
    let frame_ns = 1_000_000_000u64 / 30;
    for i in 0..FRAMES {
        let edge = (i as u32 * w) / (FRAMES as u32 - 1);
        let mut data = vec![0u8; (full_w * h * 4) as usize];
        for y in 0..h {
            for x in 0..full_w {
                let o = ((y * full_w + x) * 4) as usize;
                if x < w {
                    if x + w / 8 > edge && x < edge + w / 8 {
                        data[o..o + 4].copy_from_slice(&[255, 200, 40, 255]);
                    }
                } else {
                    let v = if x - w < edge { 255 } else { 0 };
                    data[o..o + 4].copy_from_slice(&[v, v, v, 255]);
                }
            }
        }
        let mut buf = gst::Buffer::from_mut_slice(data);
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(i as u64 * frame_ns));
            b.set_duration(gst::ClockTime::from_nseconds(frame_ns));
        }
        src.push_buffer(buf).unwrap();
    }
    src.end_of_stream().unwrap();
    let msg = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(300),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
        "encode {} failed",
        path.display()
    );
}

fn ffmpeg_vp9_alpha(from: &Path, to: &Path) -> bool {
    std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(from)
        .args([
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuva420p",
            "-auto-alt-ref",
            "0",
            "-b:v",
            "8M",
        ])
        .arg(to)
        .status()
        .is_ok_and(|s| s.success())
}

fn flow_for(clips: &[PathBuf], preroll_ms: u64) -> Flow {
    let mut flow = Flow::new("stinger_bench");
    let props = |pairs: Vec<(&str, PV)>| -> HashMap<String, PV> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    };
    flow.blocks.push(strom_types::BlockInstance {
        id: "bench-mixer".to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: props(vec![
            ("compositor_preference", PV::String("gpu".into())),
            ("num_inputs", PV::UInt(2)),
            ("pgm_resolution", PV::String("1920x1080".into())),
            ("multiview_resolution", PV::String("640x360".into())),
            ("pgm_framerate", PV::String("30/1".into())),
            ("enable_stinger", PV::Bool(true)),
            ("stinger_preroll_ms", PV::UInt(preroll_ms)),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let playlist: Vec<String> = clips
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    flow.blocks.push(strom_types::BlockInstance {
        id: "bench-sting".to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: props(vec![
            ("stinger_mode", PV::Bool(true)),
            (
                "playlist",
                PV::String(serde_json::to_string(&playlist).unwrap()),
            ),
            ("num_audio_tracks", PV::UInt(0)),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let elem = |id: &str, ty: &str, p: Vec<(&str, PV)>| strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: p.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    };
    for (i, pattern) in ["smpte", "ball"].iter().enumerate() {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![
                ("pattern", PV::String(pattern.to_string())),
                ("is-live", PV::Bool(true)),
            ],
        ));
        flow.elements.push(elem(
            &format!("caps{i}"),
            "capsfilter",
            vec![(
                "caps",
                PV::String("video/x-raw,width=1920,height=1080,framerate=30/1".into()),
            )],
        ));
        flow.links.push(Link {
            from: format!("src{i}:src"),
            to: format!("caps{i}:sink"),
        });
        flow.links.push(Link {
            from: format!("caps{i}:src"),
            to: format!("bench-mixer:video_in_{i}"),
        });
    }
    flow.elements
        .push(elem("pgmsink", "fakesink", vec![("sync", PV::Bool(false))]));
    flow.links.push(Link {
        from: "bench-sting:video_out".into(),
        to: "bench-mixer:stinger_in".into(),
    });
    flow.links.push(Link {
        from: "bench-mixer:pgm_out".into(),
        to: "pgmsink:sink".into(),
    });
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn stinger_measurements() {
    if !common::gl_available(&["glvideomixerelement", "glshader"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut clips: Vec<(String, PathBuf)> = Vec::new();
    let t = std::time::Instant::now();
    for (label, w, h, sbs, enc, mux, fmt, ext) in [
        (
            "FFV1 1080p classic",
            1920,
            1080,
            false,
            "avenc_ffv1",
            "matroskamux",
            "BGRA",
            "mkv",
        ),
        (
            "FFV1 1080p side-by-side",
            1920,
            1080,
            true,
            "avenc_ffv1",
            "matroskamux",
            "BGRA",
            "mkv",
        ),
        (
            "FFV1 4K classic",
            3840,
            2160,
            false,
            "avenc_ffv1",
            "matroskamux",
            "BGRA",
            "mkv",
        ),
        (
            "ProRes 4444 1080p classic",
            1920,
            1080,
            false,
            "avenc_prores_ks profile=4444",
            "qtmux",
            "A444_10LE",
            "mov",
        ),
        (
            "H.264 1080p side-by-side (no alpha)",
            1920,
            1080,
            true,
            "x264enc tune=zerolatency",
            "mp4mux",
            "I420",
            "mp4",
        ),
    ] {
        let path = dir.path().join(format!(
            "{}.{ext}",
            label.replace([' ', '(', ')', '.'], "_")
        ));
        synthetic(&path, w, h, sbs, enc, mux, fmt);
        clips.push((label.to_string(), path));
    }
    let webm = dir.path().join("vp9_alpha_1080p.webm");
    if ffmpeg_vp9_alpha(&clips[0].1, &webm) {
        clips.push(("VP9+alpha WebM 1080p classic".to_string(), webm));
    } else {
        eprintln!("ffmpeg with libvpx-vp9 not found: skipping the VP9-with-alpha clip");
    }
    eprintln!("clips encoded in {:.1} s", t.elapsed().as_secs_f64());

    let paths: Vec<PathBuf> = clips.iter().map(|(_, p)| p.clone()).collect();
    let mut rows = Vec::new();
    for preroll in [120u64, 80, 50] {
        let storage = NamedTempFile::new().unwrap();
        let blocks = NamedTempFile::new().unwrap();
        let state = AppState::new(
            JsonFileStorage::new(storage.path()),
            blocks.path(),
            dir.path(),
            vec![],
            "all".to_string(),
            vec![],
            false,
            false,
        );
        let flow = flow_for(&paths, preroll);
        let flow_id = flow.id;
        state.upsert_flow(flow).await.unwrap();
        state.start_flow(&flow_id).await.unwrap();
        // Let analysis finish so takes plan from it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let s = state.stinger_state(&flow_id, "bench-mixer").await.unwrap();
            if s.ready && s.clips.iter().all(|c| c.info.is_some()) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never ready");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let s = state.stinger_state(&flow_id, "bench-mixer").await.unwrap();
        for (i, (label, _)) in clips.iter().enumerate() {
            let cue = state
                .stinger_cue(&flow_id, "bench-mixer", i)
                .await
                .expect("cue");
            for _ in 0..3 {
                state
                    .stinger_take(&flow_id, "bench-mixer", Some(i))
                    .await
                    .expect("take");
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let st = state.stinger_state(&flow_id, "bench-mixer").await.unwrap();
                    if !st.running && st.ready {
                        let r = st.last_take.unwrap();
                        rows.push((
                            preroll,
                            label.clone(),
                            s.clips[i].info.as_ref().map_or(0, |i| i.analysis_ms),
                            cue,
                            r,
                        ));
                        break;
                    }
                }
            }
        }
        state.stop_flow(&flow_id).await.unwrap();
    }

    eprintln!(
        "\n| preroll | clip | analysis ms | cue ms | variant | take-to-air ms | frames | late | worst margin ms |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|---|");
    for (preroll, label, analysis, cue, r) in rows {
        eprintln!(
            "| {} | {} | {} | {} | {:?} | {:.0} | {}/{} | {} | {} |",
            preroll,
            label,
            analysis,
            cue,
            r.variant,
            r.take_to_air_ms,
            r.frames_arrived,
            r.frames_expected,
            r.frames_late,
            r.worst_margin_ms
                .map(|m| format!("{m:.0}"))
                .unwrap_or_else(|| "-".into())
        );
    }
}
