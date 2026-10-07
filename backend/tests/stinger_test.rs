//! Stinger takes against a real vision mixer, frame by frame.
//!
//! Each clip is built so the program output says which clip frame the mixer
//! used: a classic clip covers only the left half, so the switch beneath
//! shows on the right; a matte clip's edge moves a known step per frame, so
//! the position of the old/new boundary names the matte frame on air. The
//! tests assert the clip lands on consecutive output frames from its first,
//! the cut lands on the cut point's frame, and every matte frame lines up
//! with the program frame it was meant for.

pub mod common;
#[path = "common/stinger.rs"]
pub mod rig;

use gstreamer::prelude::*;
use rig::*;
use strom_types::stinger::{StingerLayout, StingerVariant};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gpu_stingers_land_frame_accurately() {
    if !common::gl_available(GL_ELEMENTS) || !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("gpu", "gpu").await;
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert!(s.matte_supported);
    assert_eq!(
        s.clips.iter().map(|c| c.variant).collect::<Vec<_>>(),
        vec![
            Some(StingerVariant::Classic),
            Some(StingerVariant::TrackMatte),
            Some(StingerVariant::MaskOnly)
        ]
    );

    // Red to blue, classic.
    classic_take(&r).await;
    r.wait_until_parked().await;

    // Blue back to red through the side-by-side matte, cueing on the way.
    r.drain().await;
    let take = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), Some(1), None)
        .await
        .expect("track matte take");
    assert_eq!(take.variant, StingerVariant::TrackMatte);
    let n = r.clip_frames(1).await;
    // Pad state half way through the clip, for a failure message.
    let snapshot = {
        let (state, flow, mixer) = (r.state.clone(), r.flow_id, r.mixer());
        let wait = take.take_to_air_ms as u64 + 500;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            mixer_pads_of(&state, flow, &mixer).await
        })
    };
    let frames = r.collect(take.take_to_air_ms as u64 + 1600).await;
    let pads_mid_take = snapshot.await.unwrap();
    let start = frames
        .iter()
        .find(|(_, f)| colour(px(f, 3, 3)) == Colour::Yellow)
        .map(|(t, _)| *t)
        .expect("the graphic never went on air");
    // Every program frame's edge, so a failure shows the whole take.
    let seen: Vec<String> = frames
        .iter()
        .map(|(t, f)| format!("{}:{}", frame_index(*t, start), edge(f, Colour::Red)))
        .collect();
    // Middle-row pixels of the frame half way through, for a failure message.
    let mid_row = frames
        .iter()
        .find(|(t, _)| frame_index(*t, start) == n as i64 / 2)
        .map(|(_, f)| {
            (0..8)
                .map(|i| format!("{:?}", px(f, i * W / 8, H / 2)))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    for (t, f) in &frames {
        let k = frame_index(*t, start);
        if (0..n as i64).contains(&k) {
            let measured = edge(f, Colour::Red);
            let expected = matte_edge(k as usize);
            assert!(
                measured.abs_diff(expected) <= 3,
                "program frame {k} shows matte edge {measured}, clip frame {k} has {expected}; \
                 frame:edge {seen:?}; middle row half way: {mid_row}; pads mid take:\n{pads_mid_take}\nreport {:?}",
                r.state
                    .stinger_state(&r.flow_id, &r.mixer())
                    .await
                    .unwrap()
                    .last_take
            );
            assert_eq!(
                colour(px(f, 3, 3)),
                Colour::Yellow,
                "frame {k}: graphic on top"
            );
        } else if k < 0 {
            assert_eq!(
                edge(f, Colour::Red),
                0,
                "frame {k}: old source before the clip"
            );
        } else {
            assert_eq!(
                edge(f, Colour::Red),
                W,
                "frame {k}: new source after the clip"
            );
            assert_ne!(
                colour(px(f, 3, 3)),
                Colour::Yellow,
                "frame {k}: graphic gone"
            );
        }
    }
    let report = r.wait_for_report(1).await;
    assert!(report.cue_ms.is_some(), "the take had to cue clip 1");
    assert_eq!(report.frames_arrived, n, "{report:?}");
    r.wait_until_parked().await;

    // Red to blue through the mask only.
    r.drain().await;
    let take = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), Some(2), None)
        .await
        .expect("mask take");
    assert_eq!(take.variant, StingerVariant::MaskOnly);
    let n = r.clip_frames(2).await;
    let frames = r.collect(take.take_to_air_ms as u64 + 1600).await;
    let edges: Vec<(u64, u32)> = frames
        .iter()
        .map(|(t, f)| (*t, edge(f, Colour::Blue)))
        .collect();
    // Frame 1 is the first with any of the new source.
    let first = edges
        .iter()
        .find(|(_, e)| *e > 0)
        .map(|(t, _)| *t)
        .expect("the mask never moved");
    let start = first - FRAME_NS;
    for (t, e) in &edges {
        let k = frame_index(*t, start);
        let expected = if k < 0 {
            0
        } else if k >= n as i64 {
            W
        } else {
            matte_edge(k as usize)
        };
        assert!(
            e.abs_diff(expected) <= 3,
            "program frame {k} shows edge {e}, expected {expected}; frame:edge {:?}",
            edges
                .iter()
                .map(|(t, e)| format!("{}:{}", frame_index(*t, start), e))
                .collect::<Vec<_>>()
        );
    }
    let report = r.wait_for_report(2).await;
    assert_eq!(report.frames_arrived, n, "{report:?}");

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cpu_stingers_play_classic() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("cpu", "cpu").await;
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert!(!s.matte_supported);
    assert_eq!(s.clips[1].variant, Some(StingerVariant::Classic));
    assert_eq!(s.clips[1].downgraded_from, Some(StingerVariant::TrackMatte));
    assert_eq!(
        s.clips[1].info.as_ref().unwrap().detected_layout,
        StingerLayout::SideBySide
    );

    classic_take(&r).await;
    r.wait_until_parked().await;

    // The side-by-side clip as classic: its graphic (the yellow marker, cut
    // out of the left half) on air, the program cutting where the matte
    // crosses half way.
    r.drain().await;
    let take = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), Some(1), None)
        .await
        .expect("downgraded take");
    assert_eq!(take.variant, StingerVariant::Classic);
    assert_eq!(take.downgraded_from, Some(StingerVariant::TrackMatte));
    let frames = r.collect(take.take_to_air_ms as u64 + 1600).await;
    let start = frames
        .iter()
        .find(|(_, f)| colour(px(f, 3, 3)) == Colour::Yellow)
        .map(|(t, _)| *t)
        .expect("the graphic never went on air");
    let cut = frames
        .iter()
        .find(|(t, f)| *t >= start && colour(px(f, 3 * W / 4, H / 2)) == Colour::Red)
        .map(|(t, _)| frame_index(*t, start))
        .expect("the program never cut");
    let report = r.wait_for_report(1).await;
    assert!(
        (14..=16).contains(&cut),
        "cut at frame {cut}, where the matte crosses half way; {report:?}"
    );
    // The matte half of the clip never shows: the graphic is cropped out.
    for (_, f) in &frames {
        assert_ne!(colour(px(f, W - 3, 3)), Colour::Other, "matte half on air");
    }
    assert_eq!(report.frames_arrived, r.clip_frames(1).await, "{report:?}");

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A flow stopped in the middle of a take releases its pipeline, and the
/// take left waiting for its clip's end does not touch the next run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_mid_take_releases_the_pipeline() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let backend = if common::gl_available(GL_ELEMENTS) {
        "gpu"
    } else {
        "cpu"
    };
    let r = start("stop", backend).await;
    let pipeline = {
        let pipelines = r.state.pipelines_read().await;
        pipelines.get(&r.flow_id).unwrap().pipeline().downgrade()
    };
    r.state
        .stinger_take(&r.flow_id, &r.mixer(), Some(1), None)
        .await
        .expect("take");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        r.state
            .stinger_state(&r.flow_id, &r.mixer())
            .await
            .unwrap()
            .running
    );
    r.state.stop_flow(&r.flow_id).await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while pipeline.upgrade().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the pipeline outlived a stop during a stinger take"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // The next run takes stingers as if nothing had happened.
    r.state.start_flow(&r.flow_id).await.unwrap();
    r.wait_until_parked().await;
    assert!(
        !r.state
            .stinger_state(&r.flow_id, &r.mixer())
            .await
            .unwrap()
            .running
    );
    r.state
        .stinger_take(&r.flow_id, &r.mixer(), Some(0), None)
        .await
        .expect("take after restart");
    let report = r.wait_for_report(0).await;
    assert_eq!(report.frames_arrived, r.clip_frames(0).await, "{report:?}");
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A take whose flow restarts while it cues and analyses its clip gives up
/// without touching the new run: it neither programs the new mixer nor cuts
/// its program, and the next take on the new run lands as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_outlived_by_a_restart_leaves_the_new_run_alone() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("restart", "cpu").await;
    let mut events = r.state.events().subscribe();

    // Take A stops just before it programs the mixer; the flow restarts
    // under it there.
    strom::state::hold_takes_for_tests(r.flow_id, true);
    let take_a = {
        let (state, flow, mixer) = (r.state.clone(), r.flow_id, r.mixer());
        tokio::spawn(async move { state.stinger_take(&flow, &mixer, Some(1), None).await })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while strom::state::takes_held_for_tests(r.flow_id) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "take A never got there"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    r.state.stop_flow(&r.flow_id).await.unwrap();
    r.state.start_flow(&r.flow_id).await.unwrap();
    r.wait_until_parked().await;
    strom::state::hold_takes_for_tests(r.flow_id, false);

    let err = take_a.await.unwrap().expect_err("take A lost its flow");
    assert!(err.to_string().contains("restarted"), "{err}");
    let failed = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("no StingerFailed for take A")
        {
            Ok(strom_types::StromEvent::StingerFailed {
                program_changed, ..
            }) => break program_changed,
            _ => continue,
        }
    };
    assert!(!failed, "take A changed the new run's program");
    let overlay =
        strom::blocks::builtin::vision_mixer::overlay::get_overlay_state(&r.flow_id, &r.mixer())
            .unwrap();
    assert_eq!(overlay.pgm_input(), Some(0), "take A cut the new run");
    assert!(
        !r.state
            .stinger_state(&r.flow_id, &r.mixer())
            .await
            .unwrap()
            .running
    );

    // Take B on the new run lands frame by frame.
    classic_take(&r).await;
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A classic clip whose frame at the cut point reaches the mixer late leaves
/// the cut uncovered on air. The take still runs (its plan is fixed before
/// the clip plays), but its report says so, and counts only the frames that
/// arrived in time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_cut_frame_is_reported() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("late", "cpu").await;
    r.state
        .stinger_set_clip_settings(
            &r.flow_id,
            &r.mixer(),
            0,
            None,
            strom_types::stinger::StingerClipSettings {
                cut_point_ms: Some(500),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Hold the take's first clip frame for a second on its way into the
    // mixer's graphic pad: every frame up to well past the cut point
    // arrives late.
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (probe_pad, probe) = {
        let pipelines = r.state.pipelines_read().await;
        let mixer = pipelines
            .get(&r.flow_id)
            .and_then(|m| m.pipeline().by_name(&format!("{}:mixer", r.mixer())))
            .expect("mixer");
        let feeder = mixer
            .sink_pads()
            .into_iter()
            .filter_map(|p| p.peer())
            .find(|peer| {
                peer.parent_element().is_some_and(|e| {
                    let name = e.name();
                    name.ends_with(":queue_stinger_fill") || name.ends_with(":videocrop_stinger")
                })
            })
            .expect("graphic pad feeder");
        let hold = std::sync::Arc::clone(&armed);
        let probe = feeder
            .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                if hold.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
                gstreamer::PadProbeReturn::Ok
            })
            .unwrap();
        (feeder, probe)
    };
    armed.store(true, std::sync::atomic::Ordering::Release);
    r.state
        .stinger_take(&r.flow_id, &r.mixer(), Some(0), None)
        .await
        .expect("take");
    let report = r.wait_for_report(0).await;
    probe_pad.remove_probe(probe);
    let n = r.clip_frames(0).await;
    assert!(
        report.warning.is_some(),
        "a late cut frame must not report a clean take: {report:?}"
    );
    assert!(
        report.frames_arrived < n,
        "late frames counted as arrived: {report:?}"
    );
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A take whose programming fails part way through, after it ended a
/// fade-to-black and keyed the pads, puts the old source back alone on air,
/// tells clients the fade-to-black ended, and leaves the mixer ready for the
/// next take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_that_fails_to_program_leaves_the_old_source_on_air() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("halfprog", "cpu").await;
    r.state
        .stinger_set_clip_settings(
            &r.flow_id,
            &r.mixer(),
            0,
            None,
            strom_types::stinger::StingerClipSettings {
                cut_point_ms: Some(500),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(r
        .state
        .fade_to_black(&r.flow_id, &r.mixer(), 0)
        .await
        .unwrap());
    let mut events = r.state.events().subscribe();

    strom::state::fail_stinger_programming_for_tests(r.flow_id, true);
    r.drain().await;
    let err = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), Some(0), None)
        .await
        .expect_err("programming fails");
    strom::state::fail_stinger_programming_for_tests(r.flow_id, false);
    assert!(err.to_string().contains("on purpose"), "{err}");

    // Past where the clip and its cut would have aired: the old source alone,
    // out of black, with no graphic and no cut.
    let frames = r.collect(1800).await;
    for (t, f) in &frames {
        assert_eq!(
            colour(px(f, 3 * W / 4, H / 2)),
            Colour::Red,
            "frame at {t}: the old source must stay on air"
        );
        assert_eq!(
            colour(px(f, W / 4, H / 2)),
            Colour::Red,
            "frame at {t}: no graphic after a failed take"
        );
    }
    let mut ftb_ended = false;
    while let Ok(event) = events.try_recv() {
        if let strom_types::StromEvent::VisionMixerFtbChanged { active: false, .. } = event {
            ftb_ended = true;
        }
    }
    assert!(ftb_ended, "clients were not told the fade-to-black ended");

    // The next take runs as usual.
    r.wait_until_parked().await;
    classic_take(&r).await;
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A clip that will not load costs the graphic, not the change: the take
/// fails, says so, and the program cuts to PVW anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clip_that_will_not_play_still_changes_the_program() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("broken", "cpu").await;
    let player = strom::blocks::builtin::mediaplayer::MEDIA_PLAYER_REGISTRY
        .get(&strom::blocks::builtin::mediaplayer::MediaPlayerKey {
            flow_id: r.flow_id,
            block_id: "sting-broken".to_string(),
        })
        .unwrap();
    let mut playlist = player.playlist_files();
    playlist.push("/nonexistent/strom-stinger-missing.mkv".to_string());
    player.set_playlist(playlist);

    let err = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), Some(3), None)
        .await
        .expect_err("a missing clip cannot play");
    assert!(err.to_string().contains("cut instead"), "{err}");
    let overlay =
        strom::blocks::builtin::vision_mixer::overlay::get_overlay_state(&r.flow_id, &r.mixer())
            .unwrap();
    assert_eq!(overlay.pgm_input(), Some(1), "the program cut to PVW");
    assert!(
        !r.state
            .stinger_state(&r.flow_id, &r.mixer())
            .await
            .unwrap()
            .running,
        "the failed take released the mixer"
    );
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// Audio and video share the clip source's timing. A clip whose audio starts
/// before its video took the take's pinned start from whichever stream
/// reached the bridge first; when that was the audio, the graphic aired
/// 200 ms (six frames) after the frame the mixer was programmed for, cutting
/// its last frames, and the cut beneath it came early.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clip_with_early_audio_lands_its_first_video_frame_on_the_take() {
    if !common::plugins_available(CODEC_ELEMENTS)
        || !common::plugins_available(&["audiotestsrc", "fakesink"])
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("classic_av.mov");
    classic_clip_with_early_audio(&clip, 200_000_000);
    let tag = "earlyaudio";
    let r = start_edited(tag, "cpu", dir, vec![clip], |flow| {
        let player = flow
            .blocks
            .iter_mut()
            .find(|b| b.id == format!("sting-{tag}"))
            .unwrap();
        player.properties.insert(
            "num_audio_tracks".to_string(),
            strom_types::PropertyValue::UInt(1),
        );
        flow.elements.push(strom_types::Element {
            id: "asink".to_string(),
            element_type: "fakesink".to_string(),
            properties: [
                ("sync", strom_types::PropertyValue::Bool(false)),
                ("async", strom_types::PropertyValue::Bool(false)),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
            position: [0.0, 0.0].into(),
            pad_properties: Default::default(),
        });
        flow.links.push(strom_types::element::Link {
            from: format!("sting-{tag}:audio_out"),
            to: "asink:sink".to_string(),
        });
    })
    .await;

    // Hold the take's first video frame at the bridge, so the audio let go
    // with it always reaches the shared timing first. Without the hold that
    // is a race either stream can win. The clip is parked, so the next
    // video sample is the take's.
    strom::blocks::builtin::mediaplayer::hold_bridge_for_tests(
        &format!("sting-{tag}"),
        "video",
        30,
    );

    classic_take(&r).await;

    r.state.stop_flow(&r.flow_id).await.unwrap();
}
