//! Regression tests for the Media Player's control calls, driven through a
//! real flow.
//!
//! A burst of playlist jumps and pauses used to hang the player for good
//! (issue #963): a call never returned, and stopping the flow then never
//! returned either.

pub mod common;

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;
use strom::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use strom_types::{Flow, Link, PropertyValue};

const BLOCK_ID: &str = "player";

/// Run the default GLib main context, where the player's internal bus watch
/// (EOS → next file) is dispatched, as Strom does in production.
fn run_glib_main_loop() {
    std::thread::spawn(|| {
        let main_loop = gstreamer::glib::MainLoop::new(None, false);
        main_loop.run();
    });
}

/// A short H.264 clip in MP4, the kind of file the hang was seen with.
/// `x264enc` is in gstreamer1.0-plugins-ugly, `mp4mux` in -good, both
/// installed in CI.
fn write_h264_clip(dir: &std::path::Path, name: &str, frames: u32) -> String {
    use gstreamer::prelude::*;
    let path = dir.join(name);
    let writer = gstreamer::parse::launch(&format!(
        "videotestsrc num-buffers={} ! video/x-raw,width=160,height=90,framerate=25/1 \
         ! x264enc tune=zerolatency key-int-max=10 ! h264parse ! mp4mux ! filesink name=out",
        frames
    ))
    .expect("x264enc and mp4mux are installed in CI");
    writer
        .downcast_ref::<gstreamer::Bin>()
        .unwrap()
        .by_name("out")
        .unwrap()
        .set_property("location", &path);
    writer.set_state(gstreamer::State::Playing).unwrap();
    let msg = writer
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(20),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        )
        .expect("writing the clip finishes");
    assert!(
        matches!(msg.view(), gstreamer::MessageView::Eos(_)),
        "{:?}",
        msg
    );
    writer.set_state(gstreamer::State::Null).unwrap();
    gstreamer::glib::filename_to_uri(&path, None)
        .unwrap()
        .to_string()
}

/// A Media Player with default properties (decoder = decodebin3) and
/// `playlist`, its `video_out` into a fakesink.
fn build_player_flow(name: &str, playlist: &[String]) -> Flow {
    let mut flow = Flow::new(name);
    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::new(),
        position: [300.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "playlist".to_string(),
                PropertyValue::String(serde_json::to_string(playlist).unwrap()),
            );
            p.insert("decode".to_string(), PropertyValue::Bool(true));
            p.insert("num_audio_tracks".to_string(), PropertyValue::UInt(0));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow.links.push(Link {
        from: format!("{}:video_out", BLOCK_ID),
        to: "sink:sink".to_string(),
    });
    flow
}

/// A small deterministic PRNG, so a failing sequence can be replayed.
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The repro from issue #963: playlist jumps and pauses from one thread, in
/// random order, each issued as soon as the previous returns. This used to
/// deadlock inside uridecodebin3, typically within a few hundred calls: a
/// state change walked into the parsebin urisourcebin's typefind thread was
/// still adding. The flow could then not be stopped either.
///
/// `STROM_STORM_CALLS` and `STROM_STORM_SEED` replay a longer or different
/// sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storm_of_jumps_and_pauses_never_hangs_the_player() {
    gstreamer::init().unwrap();
    run_glib_main_loop();

    // A deadlocked GStreamer thread can keep the process alive after the test
    // fails, and teardown can block on it. Whatever happens, end the process.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(300));
        eprintln!("media player storm test still running after 300 s, aborting");
        std::process::exit(101);
    });

    let dir = tempfile::tempdir().unwrap();
    let playlist = vec![
        write_h264_clip(dir.path(), "a.mp4", 25),
        write_h264_clip(dir.path(), "b.mp4", 25),
    ];

    let state = common::state::new();
    let flow = build_player_flow("media_player_control_storm", &playlist);
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");
    state.start_flow(&flow_id).await.expect("start_flow failed");

    let player = MEDIA_PLAYER_REGISTRY
        .get(&MediaPlayerKey {
            flow_id,
            block_id: BLOCK_ID.to_string(),
        })
        .expect("the Media Player registered");

    let calls: usize = std::env::var("STROM_STORM_CALLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let seed: u64 = std::env::var("STROM_STORM_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let (progress_tx, progress_rx) = mpsc::channel::<usize>();
    let storm_player = std::sync::Arc::clone(&player);
    std::thread::spawn(move || {
        let mut rng = XorShift(seed);
        for i in 0..calls {
            let result = match rng.next() % 3 {
                0 => storm_player.goto(0),
                1 => storm_player.goto(1),
                _ => storm_player.pause(),
            };
            if let Err(e) = result {
                eprintln!("call {} returned an error: {}", i, e);
            }
            if progress_tx.send(i + 1).is_err() {
                return;
            }
        }
    });

    // A call takes milliseconds. Even one whose source never set up would
    // give up waiting for it within 10 s; one that has not returned in 30
    // has deadlocked.
    let mut done = 0;
    while done < calls {
        match progress_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(n) => done = n,
            Err(_) => panic!(
                "a Media Player control call did not return after {} of {} calls \
                 (seed {:#x}): the internal pipeline deadlocked (issue #963)",
                done, calls, seed
            ),
        }
    }
    drop(player);

    // On its own task: a stop that blocks in a state change would otherwise
    // block the timer that is meant to catch it.
    let stopper = state.clone();
    let stop = tokio::spawn(async move { stopper.stop_flow(&flow_id).await });
    let stop = tokio::time::timeout(Duration::from_secs(20), stop).await;
    assert!(
        stop.is_ok(),
        "stop_flow did not return within 20 s after the storm (issue #963)"
    );
    stop.unwrap().unwrap().expect("stop_flow failed");
}
