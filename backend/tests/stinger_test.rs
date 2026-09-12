//! Stinger transitions — behaviour against real blocks in a real pipeline.
//!
//! The unit tests in `gst::stinger` cover binding resolution, which is pure.
//! These cover what only a running pipeline can show: that a declared stinger
//! source is actually parked on its first frame and stopped from looping, and
//! that a media player on a keyed input which does *not* declare itself is
//! left completely alone. Arming pauses a player and disables
//! looping, so applying it by wiring alone would silently stop a looping graphic.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video::{self, prelude::VideoFrameExt};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use strom::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::{Flow, PropertyValue};
use tempfile::NamedTempFile;

/// Block ids are derived per test. The vision mixer's overlay registry is
/// keyed by block id alone, so two tests sharing one id overwrite each other's
/// state when cargo runs them in the same process.
fn mixer_id(tag: &str) -> String {
    format!("mixer-{tag}")
}
fn source_id(tag: &str) -> String {
    format!("mp-{tag}")
}
const W: u32 = 320;
const H: u32 = 180;
const FRAMES: usize = 30;
const FRAME_DUR_NS: u64 = 33_333_333;

fn clip_frame(index: usize) -> gst::Buffer {
    let mut data = vec![0u8; (W * H * 4) as usize];
    for (i, px) in data.chunks_exact_mut(4).enumerate() {
        // Left half opaque green, right half fully transparent.
        if (i as u32 % W) < W / 2 {
            px[1] = 255;
            px[3] = 255;
        }
    }
    let mut buf = gst::Buffer::from_mut_slice(data);
    {
        let b = buf.get_mut().expect("fresh buffer is writable");
        b.set_pts(gst::ClockTime::from_nseconds(index as u64 * FRAME_DUR_NS));
        b.set_duration(gst::ClockTime::from_nseconds(FRAME_DUR_NS));
    }
    buf
}

/// A frame whose opaque green column grows with the frame index, so which
/// frame is on screen is readable from the picture. The flat clip above cannot
/// distinguish frame 0 from the last frame, which is exactly what arming has
/// to get right.
fn sweep_frame(index: usize) -> gst::Buffer {
    let mut data = vec![0u8; (W * H * 4) as usize];
    let covered = (W as usize * (index + 1) / FRAMES) as u32;
    for (i, px) in data.chunks_exact_mut(4).enumerate() {
        if (i as u32 % W) < covered {
            px[1] = 255;
            px[3] = 255;
        }
    }
    let mut buf = gst::Buffer::from_mut_slice(data);
    {
        let b = buf.get_mut().expect("fresh buffer is writable");
        b.set_pts(gst::ClockTime::from_nseconds(index as u64 * FRAME_DUR_NS));
        b.set_duration(gst::ClockTime::from_nseconds(FRAME_DUR_NS));
    }
    buf
}

/// Lossless BGRA clip as FFV1 in Matroska — the alpha-carrying pair CI has.
fn write_clip(path: &std::path::Path) -> Result<(), String> {
    write_clip_with(path, clip_frame)
}

fn write_clip_with(path: &std::path::Path, frame: fn(usize) -> gst::Buffer) -> Result<(), String> {
    write_clip_frames(path, frame, FRAMES)
}

fn write_clip_frames(
    path: &std::path::Path,
    frame: fn(usize) -> gst::Buffer,
    frames: usize,
) -> Result<(), String> {
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=fg ! avenc_ffv1 ! matroskamux ! filesink location={}",
        path.display()
    ))
    .map_err(|e| format!("parse: {e}"))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "not a pipeline".to_string())?;
    let appsrc = pipeline
        .by_name("fg")
        .ok_or("no appsrc")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "not an appsrc".to_string())?;
    appsrc.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", W as i32)
            .field("height", H as i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    appsrc.set_format(gst::Format::Time);
    appsrc.set_is_live(false);
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("PLAYING: {e}"))?;
    for i in 0..frames {
        appsrc
            .push_buffer(frame(i))
            .map_err(|e| format!("push: {e:?}"))?;
    }
    let _ = appsrc.end_of_stream();
    let bus = pipeline.bus().expect("bus");
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    match msg {
        Some(m) if matches!(m.view(), gst::MessageView::Eos(_)) => Ok(()),
        Some(m) => Err(format!("encode failed: {:?}", m.view())),
        None => Err("encode timed out".to_string()),
    }
}

/// A media player wired into the mixer's first keyed input, declaring itself a
/// stinger source or not.
fn build_flow(tag: &str, clip: &std::path::Path, declare_stinger: bool) -> Flow {
    let mut flow = Flow::new("stinger_test");
    flow.blocks.push(strom_types::BlockInstance {
        id: mixer_id(tag),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: HashMap::from([
            (
                "compositor_preference".to_string(),
                PropertyValue::String("cpu".to_string()),
            ),
            ("num_inputs".to_string(), PropertyValue::UInt(2)),
            ("num_dsk_inputs".to_string(), PropertyValue::UInt(1)),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let mut source_props = HashMap::from([
        ("decode".to_string(), PropertyValue::Bool(true)),
        ("sync".to_string(), PropertyValue::Bool(true)),
        (
            "playlist".to_string(),
            PropertyValue::String(
                serde_json::to_string(&vec![clip.display().to_string()]).unwrap(),
            ),
        ),
    ]);
    if declare_stinger {
        source_props.insert("stinger_source".to_string(), PropertyValue::Bool(true));
    }
    flow.blocks.push(strom_types::BlockInstance {
        id: source_id(tag),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: source_props,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow.links.push(Link {
        from: format!("{}:video_out", source_id(tag)),
        to: format!("{}:dsk_in_0", mixer_id(tag)),
    });
    flow
}

/// Program-observable flow: blue on input 0, red on input 1, and an appsink on
/// PGM so composited frames can be inspected.
fn build_watchable_flow(tag: &str, clip: &std::path::Path) -> Flow {
    use strom_types::PropertyValue as PV;
    let mut flow = build_flow(tag, clip, true);

    if let Some(mixer) = flow.blocks.iter_mut().find(|b| b.id == mixer_id(tag)) {
        mixer
            .properties
            .insert("pgm_resolution".to_string(), PV::String(format!("{W}x{H}")));
    }

    let elem = |id: &str, ty: &str, props: Vec<(&str, PV)>| strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    };
    let src_caps = format!("video/x-raw,width={W},height={H},framerate=30/1");
    for (id, colour) in [("src0", "0xff0000ff"), ("src1", "0xffff0000")] {
        flow.elements.push(elem(
            id,
            "videotestsrc",
            vec![
                ("pattern", PV::String("solid-color".into())),
                ("foreground-color", PV::String(colour.into())),
                ("is-live", PV::Bool(true)),
            ],
        ));
    }
    for id in ["caps0", "caps1"] {
        flow.elements.push(elem(
            id,
            "capsfilter",
            vec![("caps", PV::String(src_caps.clone()))],
        ));
    }
    flow.elements.push(elem("pgmconv", "videoconvert", vec![]));
    flow.elements.push(elem(
        "pgmcaps",
        "capsfilter",
        vec![("caps", PV::String("video/x-raw,format=RGBA".into()))],
    ));
    flow.elements.push(elem(
        "pgmsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    for (from, to) in [
        ("src0:src", "caps0:sink"),
        ("caps0:src", &format!("{}:video_in_0", mixer_id(tag))),
        ("src1:src", "caps1:sink"),
        ("caps1:src", &format!("{}:video_in_1", mixer_id(tag))),
        (&format!("{}:pgm_out", mixer_id(tag)), "pgmconv:sink"),
        ("pgmconv:src", "pgmcaps:sink"),
        ("pgmcaps:src", "pgmsink:sink"),
    ] {
        flow.links.push(Link {
            from: from.to_string(),
            to: to.to_string(),
        });
    }
    flow
}

struct Running {
    mixer: String,
    source: String,
    state: AppState,
    flow_id: strom_types::FlowId,
    _storage: NamedTempFile,
    _blocks: NamedTempFile,
}

impl Running {
    fn player(&self) -> std::sync::Arc<strom::blocks::builtin::mediaplayer::MediaPlayerState> {
        MEDIA_PLAYER_REGISTRY
            .get(&MediaPlayerKey {
                flow_id: self.flow_id,
                block_id: self.source.clone(),
            })
            .expect("media player registered")
    }
}

/// Stamp a clip's timing onto its source block. Cut point and the transition
/// beneath are declared by the clip, not by the take, so a test that wants
/// particular timing sets it here before the flow starts.
fn with_timing(tag: &str, mut flow: Flow, cut_ms: u64, under: &str, under_ms: u64) -> Flow {
    let props = &mut flow
        .blocks
        .iter_mut()
        .find(|b| b.id == source_id(tag))
        .expect("clip source block")
        .properties;
    props.insert(
        "stinger_cut_point_ms".to_string(),
        PropertyValue::UInt(cut_ms),
    );
    props.insert(
        "stinger_under_transition".to_string(),
        PropertyValue::String(under.to_string()),
    );
    props.insert(
        "stinger_under_duration_ms".to_string(),
        PropertyValue::UInt(under_ms),
    );
    flow
}

/// Wait until the mixer is actually producing. A transition needs the mixer's
/// position for its timebase, so triggering before the first frame fails — and
/// under parallel test load that first frame can be seconds away. Same
/// readiness gate `vision_mixer_fx_test` uses.
async fn wait_until_mixer_produces(state: &AppState, flow_id: &strom_types::FlowId, tag: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let ready = {
            let pipelines = state.pipelines_read().await;
            pipelines
                .get(flow_id)
                .and_then(|m| m.pipeline().by_name(&format!("{}:mixer", mixer_id(tag))))
                .and_then(|mixer| mixer.query_position::<gst::ClockTime>())
                .is_some()
        };
        if ready {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mixer never produced output"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Build the flow through `AppState` and start it, which is the path that
/// computes the mixer's dynamic DSK pads and arms declared stinger sources.
async fn start(tag: &str, clip: &std::path::Path, declare_stinger: bool) -> Running {
    start_with(
        tag,
        with_timing(
            tag,
            build_flow(tag, clip, declare_stinger),
            300,
            "wipe_left",
            200,
        ),
    )
    .await
}

async fn start_with(tag: &str, flow: Flow) -> Running {
    // The CPU mixer builder picks its videoconvert from detected GPU
    // capabilities, so they must be probed before building.
    let _ = strom::gpu::detect_gpu_capabilities();

    let storage = NamedTempFile::new().unwrap();
    let blocks = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage.path()),
        blocks.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
    );

    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");
    // Software compositor: there is nothing environmental to skip for.
    state.start_flow(&flow_id).await.expect("start_flow");

    // Let the media player's internal pipeline settle.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    wait_until_mixer_produces(&state, &flow_id, tag).await;

    Running {
        mixer: mixer_id(tag),
        source: source_id(tag),
        state,
        flow_id,
        _storage: storage,
        _blocks: blocks,
    }
}

fn clip_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "strom_stinger_test_{}_{tag}.mkv",
        std::process::id()
    ))
}

/// Init GStreamer, check the codecs CI provides, and write a fresh clip.
fn prepare(tag: &str) -> std::path::PathBuf {
    gst::init().expect("gstreamer init");
    for name in ["avenc_ffv1", "matroskamux"] {
        if gst::ElementFactory::find(name).is_none() {
            panic!("{name} missing — CI installs gst-libav and plugins-good");
        }
    }
    let clip = clip_path(tag);
    write_clip(&clip).expect("write clip");
    clip
}

/// A declared source is parked on frame 0 and stopped from looping by
/// `start_flow` itself — the state the ~0.5 ms armed start depends on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_flow_arms_a_declared_stinger_source() {
    let clip = prepare("declared");
    let running = start("declared", &clip, true).await;
    let player = running.player();

    assert!(
        player.is_stinger_armed(),
        "start_flow must park a declared stinger source on its first frame"
    );
    assert!(
        !player.loop_playlist.load(Ordering::SeqCst),
        "a stinger plays once per trigger, so looping must be off"
    );
    let _ = std::fs::remove_file(&clip);
}

/// The regression the declaration exists to prevent: a media player on a keyed
/// input that has NOT opted in must keep playing and keep looping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeclared_source_on_keyed_input_is_left_alone() {
    let clip = prepare("undeclared");
    let running = start("undeclared", &clip, false).await;
    let player = running.player();

    assert!(
        !player.is_stinger_armed(),
        "a source that never declared itself must not be parked"
    );
    assert!(
        player.loop_playlist.load(Ordering::SeqCst),
        "arming must not disable looping on a source that did not opt in — \
         that would silently stop a looping graphic on a keyed input"
    );
    assert_eq!(
        player.state(),
        strom_types::mediaplayer::PlayerState::Playing,
        "an undeclared source must keep playing"
    );
    let _ = std::fs::remove_file(&clip);
}

/// The whole cycle: the clip rolls, the transition beneath runs, and the keyed
/// input is hidden and the clip re-armed once it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stinger_runs_then_tears_down_and_re_arms() {
    let clip = prepare("cycle");
    let running = start("cycle", &clip, true).await;

    let mut rx = running.state.events().subscribe();
    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect("stinger must start");

    // Playing clears the armed flag; if it were still set the clip never rolled.
    assert!(
        !running.player().is_stinger_armed(),
        "the clip should be rolling, so it is no longer parked on frame 0"
    );

    // Clip is 1 s; wait it out plus slack for teardown.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !running.player().is_stinger_armed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the clip must be re-armed after the stinger completes, or the next \
             fire pays the unarmed cost"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Prove the transition *beneath* actually ran rather than erroring inside
    // the spawned task, where a failure would otherwise only be logged.
    //
    // Asserted on the transition event, not on the alpha values synced back
    // into the flow definition: that sync depends on the mixer reporting a PGM
    // change and is persistence bookkeeping, not the behaviour under test.
    // That the program genuinely changes is proved on composited pixels by
    // `stinger_covers_program_then_leaves_it_on_the_new_source`.
    let beneath = wait_for_event(&mut rx, 8000, |e| match e {
        strom_types::StromEvent::TransitionTriggered {
            transition_type,
            from_input,
            to_input,
            ..
        } => Some((transition_type.clone(), *from_input, *to_input)),
        _ => None,
    })
    .await
    .expect("the transition beneath must have run");
    assert_eq!(
        beneath,
        ("wipe_left".to_string(), 0, 1),
        "the transition beneath should have been the one requested, 0 -> 1"
    );

    // The mixer must be free again — proved by a second stinger being accepted.
    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            1,
            0,
            Some(running.source.as_str()),
        )
        .await
        .expect("mixer must be free once the first stinger finished");
    let _ = std::fs::remove_file(&clip);
}

/// A stinger owns the program bus for the length of its clip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_stinger_while_one_is_in_flight_is_rejected() {
    let clip = prepare("concurrent");
    let running = start("concurrent", &clip, true).await;

    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect("first stinger must start");

    let err = running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            1,
            0,
            Some(running.source.as_str()),
        )
        .await
        .expect_err("a second stinger must be refused while one is in flight");
    assert!(
        err.to_string().contains("already running"),
        "expected an already-running error, got: {err}"
    );
    let _ = std::fs::remove_file(&clip);
}

/// Requests that cannot be honoured are refused before anything moves on air,
/// and leave the mixer free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_stinger_requests_are_refused_and_leave_the_mixer_free() {
    let clip = prepare("invalid");
    let running = start("invalid", &clip, true).await;

    let cases: Vec<(&str, Option<&str>, &str)> = vec![
        ("unknown source", Some("no_such_block"), "no block"),
        ("missing source", None, "requires a clip source"),
    ];

    for (label, source, expected) in cases {
        let result = running
            .state
            .trigger_stinger(&running.flow_id, &running.mixer, 0, 1, source)
            .await;
        let err = match result {
            Ok(_) => panic!("{label} should have been refused, but the stinger started"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains(expected),
            "{label}: expected a message containing '{expected}', got: {err}"
        );
    }

    // Every rejection happened before the mixer was claimed, so a valid
    // request must still be accepted.
    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect("a valid stinger must still be accepted after refusals");
    let _ = std::fs::remove_file(&clip);
}

/// A cut point past the end of the clip is declared on the clip source, so it
/// is caught when the take resolves rather than leaving the program stranded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_point_beyond_the_clip_is_refused() {
    let clip = prepare("beyond");
    let running = start_with(
        "beyond",
        with_timing(
            "beyond",
            build_flow("beyond", &clip, true),
            999_999,
            "cut",
            0,
        ),
    )
    .await;

    let err = running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect_err("a cut point past the clip must be refused")
        .to_string();
    assert!(
        err.contains("beyond the clip length"),
        "expected a cut-point message, got: {err}"
    );
    let _ = std::fs::remove_file(&clip);
}

/// A repeat stinger plays forward from the start of its clip.
///
/// This does NOT guard the stale-frame flash it was written for. That flash
/// lasts about two frames, and at the boundary the appsink hands back the
/// frame it was already holding, which is indistinguishable from it — the test
/// passes either way with the fix reverted. It is kept as a smoke test that a
/// second stinger runs forward rather than jumping about, and the flash itself
/// was verified by capturing program output frame by frame off a running
/// server. The clip sweeps, so coverage says which frame is on air.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeat_stinger_plays_forward_from_the_start() {
    let clip = clip_path("rearm-frame");
    gst::init().expect("gstreamer init");
    write_clip_with(&clip, sweep_frame).expect("write sweep clip");
    let running = start_with(
        "rearm-frame",
        with_timing(
            "rearm-frame",
            build_watchable_flow("rearm-frame", &clip),
            500,
            "cut",
            0,
        ),
    )
    .await;
    let mut rx = running.state.events().subscribe();

    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect("first stinger must start");
    wait_for_event(&mut rx, 5000, |e| match e {
        strom_types::StromEvent::StingerCompleted { .. } => Some(()),
        _ => None,
    })
    .await
    .expect("the first stinger must finish");

    // Wait until the keyed input is actually off air before firing again, so
    // the first sample below cannot be the tail of the playthrough just ended.
    let clear_by = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let frame = pgm_frame(&running).await.expect("a PGM frame");
        if green_fraction(&frame) < 0.02 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < clear_by,
            "the keyed input must go off air once its clip ends"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            1,
            0,
            Some(running.source.as_str()),
        )
        .await
        .expect("second stinger must start");

    // The sweep only ever grows, so coverage that falls means something other
    // than this playthrough was on air: the frame the last one ended on.
    // Pulling blocks until the next composited frame, so this is a dozen
    // frames from the start of the clip, well before it ends.
    let mut seq = Vec::new();
    for _ in 0..12 {
        if let Some(frame) = pgm_frame(&running).await {
            seq.push(green_fraction(&frame));
        }
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
    }
    let _ = std::fs::remove_file(&clip);

    // The appsink can hand back the frame it was already holding, from before
    // the take, so judge the run rather than that first sample.
    let seq = &seq[1..];
    let drop_at = seq.windows(2).position(|w| w[1] + 0.05 < w[0]);
    assert!(
        drop_at.is_none(),
        "a repeat stinger must play forward; coverage fell at sample {:?} of {:?}",
        drop_at,
        seq.iter().map(|g| (g * 100.0) as u32).collect::<Vec<_>>()
    );
}

/// A take left running when its flow stops must not touch the next pipeline.
///
/// Its task is still sleeping out the clip. Without a claim it can check, it
/// wakes against the restarted flow, runs the transition beneath on a program
/// nobody asked it to change, and drops a claim that may by then belong to a
/// different take — after which two stingers can run on one mixer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stinger_outliving_its_flow_leaves_the_next_one_alone() {
    // A long clip so the take is still asleep after a restart, which on a
    // loaded machine takes seconds. The window this guards is only open while
    // the take has not yet finished and released its claim by itself.
    let clip = clip_path("outlive");
    gst::init().expect("gstreamer init");
    write_clip_frames(&clip, clip_frame, 300).expect("write long clip");
    let running = start_with(
        "outlive",
        with_timing(
            "outlive",
            build_flow("outlive", &clip, true),
            9_000,
            "cut",
            0,
        ),
    )
    .await;

    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(&running.source),
        )
        .await
        .expect("stinger must start");

    // Stop well inside the clip, so the take's task is still asleep.
    running
        .state
        .stop_flow(&running.flow_id)
        .await
        .expect("stop_flow");
    running
        .state
        .start_flow(&running.flow_id)
        .await
        .expect("start_flow");

    wait_until_mixer_produces(&running.state, &running.flow_id, "outlive").await;

    // Fire while the stale task is still asleep, which is when a claim left
    // behind by the stopped flow would still be holding the mixer.
    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(&running.source),
        )
        .await
        .expect("the restarted mixer must accept a stinger");

    let _ = std::fs::remove_file(&clip);
}

/// Pull the newest composited PGM frame as tightly packed RGBA./// Pull the newest composited PGM frame as tightly packed RGBA.
async fn pgm_frame(running: &Running) -> Option<Vec<u8>> {
    let pipelines = running.state.pipelines_read().await;
    let manager = pipelines.get(&running.flow_id)?;
    let appsink = manager
        .pipeline()
        .by_name("pgmsink")?
        .downcast::<gst_app::AppSink>()
        .ok()?;
    let sample = appsink.try_pull_sample(gst::ClockTime::from_mseconds(500))?;
    let caps = sample.caps()?;
    let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
    let buffer = sample.buffer()?;
    let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let stride = frame.plane_stride()[0] as usize;
    let src = frame.plane_data(0).ok()?;
    let w = info.width() as usize;
    let h = info.height() as usize;
    let mut packed = vec![0u8; w * h * 4];
    for y in 0..h {
        packed[y * w * 4..(y + 1) * w * 4].copy_from_slice(&src[y * stride..y * stride + w * 4]);
    }
    Some(packed)
}

/// Fraction of pixels that read predominantly green — the stinger clip's
/// opaque half, and nothing else in this flow.
fn green_fraction(frame: &[u8]) -> f64 {
    let total = frame.len() / 4;
    let green = frame
        .chunks_exact(4)
        .filter(|px| px[1] > 150 && px[0] < 100 && px[2] < 100)
        .count();
    green as f64 / total as f64
}

/// Fraction reading predominantly red — input 1.
fn red_fraction(frame: &[u8]) -> f64 {
    let total = frame.len() / 4;
    let red = frame
        .chunks_exact(4)
        .filter(|px| px[0] > 150 && px[1] < 100 && px[2] < 100)
        .count();
    red as f64 / total as f64
}

/// End to end, on composited pixels: the keyed input contributes nothing while
/// idle, the clip covers during the stinger, and the program ends up on the new
/// source with the keyed input gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stinger_covers_program_then_leaves_it_on_the_new_source() {
    let clip = prepare("frames");
    let running = start_with(
        "frames",
        with_timing(
            "frames",
            build_watchable_flow("frames", &clip),
            500,
            "cut",
            0,
        ),
    )
    .await;

    // Idle: the clip is parked and its keyed input is disabled, so no green.
    let idle = pgm_frame(&running).await.expect("a PGM frame while idle");
    assert!(
        green_fraction(&idle) < 0.01,
        "the keyed input must contribute nothing while idle, saw {:.1}% green",
        green_fraction(&idle) * 100.0
    );

    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect("stinger must start");

    // While the clip plays, its opaque half must reach the program bus.
    let mut peak_green: f64 = 0.0;
    for _ in 0..25 {
        if let Some(f) = pgm_frame(&running).await {
            peak_green = peak_green.max(green_fraction(&f));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        peak_green > 0.20,
        "the stinger clip should have covered a large part of the frame; \
         peak green was only {:.1}%",
        peak_green * 100.0
    );

    // After the clip ends: keyed input hidden, program on input 1 (red).
    // Poll for the settled frame: clip length plus teardown varies with load,
    // so a fixed wait is flaky without being any stricter.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let after = pgm_frame(&running).await.expect("a PGM frame after");
        if green_fraction(&after) < 0.01 && red_fraction(&after) > 0.80 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the keyed input must be hidden and the program left on input 1 \
             alone once the clip ends; saw {:.1}% green, {:.1}% red",
            green_fraction(&after) * 100.0,
            red_fraction(&after) * 100.0
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let _ = std::fs::remove_file(&clip);
}

/// A clip that cannot play costs the branding, not the cut. The program must
/// still change rather than being left mid-transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stinger_with_an_unplayable_clip_still_changes_the_program() {
    let clip = prepare("unplayable");
    // Point the player at a file that does not exist, so its duration is never
    // readable — the same observable as an unreadable or undecodable clip.
    let missing = clip_path("does_not_exist");
    let _ = std::fs::remove_file(&missing);
    let mut flow = build_watchable_flow("unplayable", &clip);
    if let Some(source) = flow
        .blocks
        .iter_mut()
        .find(|b| b.id == source_id("unplayable"))
    {
        source.properties.insert(
            "playlist".to_string(),
            PropertyValue::String(
                serde_json::to_string(&vec![missing.display().to_string()]).unwrap(),
            ),
        );
    }
    let running = start_with("unplayable", with_timing("unplayable", flow, 300, "cut", 0)).await;
    eprintln!(
        "DIAG duration={:?} armed={}",
        running.player().duration(),
        running.player().is_stinger_armed()
    );

    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect("an unplayable clip must degrade, not fail the take");

    // Poll rather than wait a fixed interval: how long the take takes to reach
    // the sink varies with load.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let after = loop {
        let frame = pgm_frame(&running).await.expect("a PGM frame after");
        if red_fraction(&frame) > 0.80 {
            break frame;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the program must still have changed to input 1 despite the clip \
             failing, saw {:.1}% red",
            red_fraction(&frame) * 100.0
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(
        green_fraction(&after) < 0.01,
        "no keyed content should be on air when the clip could not play"
    );

    let _ = std::fs::remove_file(&clip);
}

/// Read events until one matches or the deadline passes.
async fn wait_for_event<T>(
    rx: &mut tokio::sync::broadcast::Receiver<strom_types::StromEvent>,
    ms: u64,
    mut f: impl FnMut(&strom_types::StromEvent) -> Option<T>,
) -> Option<T> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                if let Some(v) = f(&event) {
                    return Some(v);
                }
            }
            // Lagged: keep reading. Closed or timed out: give up.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return None,
        }
    }
}

/// A transition beneath that would outlast the clip is shortened, and the
/// started event reports both the requested and the applied duration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn under_transition_outlasting_the_clip_is_clamped_and_reported() {
    let clip = prepare("clamp");
    let running = start_with(
        "clamp",
        with_timing("clamp", build_flow("clamp", &clip, true), 900, "fade", 500),
    )
    .await;
    let mut rx = running.state.events().subscribe();

    // The clip is ~1 s. Cutting at 900 ms leaves ~100 ms, so a 500 ms
    // transition beneath cannot fit and must be shortened.
    running
        .state
        .trigger_stinger(
            &running.flow_id,
            &running.mixer,
            0,
            1,
            Some(running.source.as_str()),
        )
        .await
        .expect("stinger must start");

    let (applied, requested, clip_ms) = wait_for_event(&mut rx, 3000, |e| match e {
        strom_types::StromEvent::StingerStarted {
            under_duration_ms,
            under_duration_clamped_from,
            clip_ms,
            ..
        } => Some((*under_duration_ms, *under_duration_clamped_from, *clip_ms)),
        _ => None,
    })
    .await
    .expect("a StingerStarted event");

    assert_eq!(
        requested,
        Some(500),
        "the event must name the duration originally requested"
    );
    assert!(
        applied < 500,
        "the transition beneath must be shortened, got {applied} ms"
    );
    assert!(
        900 + applied <= clip_ms,
        "the shortened transition must finish before the {clip_ms} ms clip ends, \
         but 900 + {applied} does not"
    );

    // And the stinger reports completion once the clip ends.
    let completed = wait_for_event(&mut rx, 4000, |e| match e {
        strom_types::StromEvent::StingerCompleted {
            source_block_id, ..
        } => Some(source_block_id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        completed.as_deref(),
        Some(running.source.as_str()),
        "a stinger must report completion naming its clip source"
    );

    let _ = std::fs::remove_file(&clip);
}
