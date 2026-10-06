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
    eprintln!("dist mixer pads half way through the matte take:\n{pads_mid_take}");
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
    for (t, f) in &frames {
        let k = frame_index(*t, start);
        if (0..n as i64).contains(&k) {
            let measured = edge(f, Colour::Red);
            let expected = matte_edge(k as usize);
            assert!(
                measured.abs_diff(expected) <= 3,
                "program frame {k} shows matte edge {measured}, clip frame {k} has {expected}; \
                 frame:edge {seen:?}; pads mid take:\n{pads_mid_take}\nreport {:?}",
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
    assert!(
        (14..=16).contains(&cut),
        "cut at frame {cut}, where the matte crosses half way"
    );
    // The matte half of the clip never shows: the graphic is cropped out.
    for (_, f) in &frames {
        assert_ne!(colour(px(f, W - 3, 3)), Colour::Other, "matte half on air");
    }
    let report = r.wait_for_report(1).await;
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
        strom::blocks::builtin::vision_mixer::overlay::get_overlay_state(&r.mixer()).unwrap();
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
