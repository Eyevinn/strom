//! Regression test for the Media Player playing HLS in passthrough, driven
//! through a real flow.
//!
//! An HLS variant that carries audio and video in one MPEG-TS stream comes out
//! of `hlsdemux2` on a single streaming thread. In passthrough the bridge
//! linked each pad straight to its clocksync, so the first video buffer
//! blocked that thread in the video appsink's preroll, the audio appsink never
//! got a buffer, and the internal pipeline never reached PLAYING (issue
//! #1062).

pub mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use strom::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

use gstreamer as gst;
use gstreamer::prelude::*;

const BLOCK_ID: &str = "player";

/// Writing the stream needs the encoders and `hlssink2`; playing it needs
/// `hlsdemux2` and `souphttpsrc`. All are in the CI image.
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "audiotestsrc",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "hlssink2",
    "hlsdemux2",
    "souphttpsrc",
    "urisourcebin",
    "clocksync",
    "appsink",
    "fakesink",
];

fn new_app_state() -> (AppState, [NamedTempFile; 2]) {
    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage_file.path()),
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    (state, [storage_file, blocks_file])
}

/// Run the default GLib main context, where the flow's bus watch (and with it
/// the start of the player's internal pipeline) is dispatched, as Strom does
/// in production.
fn run_glib_main_loop() {
    std::thread::spawn(|| {
        let main_loop = gst::glib::MainLoop::new(None, false);
        main_loop.run();
    });
}

/// A 10 s HLS stream in `dir`, audio and video muxed into each MPEG-TS
/// segment - the kind of variant the stall was seen with.
fn write_muxed_hls(dir: &Path) {
    let writer = gst::parse::launch(&format!(
        "videotestsrc num-buffers=250 ! video/x-raw,width=160,height=90,framerate=25/1 \
         ! x264enc tune=zerolatency key-int-max=25 ! h264parse ! hls.video \
         audiotestsrc num-buffers=470 ! audio/x-raw,rate=48000 ! avenc_aac ! aacparse \
         ! hls.audio \
         hlssink2 name=hls target-duration=2 playlist-length=0 \
         location={} playlist-location={}",
        dir.join("seg%05d.ts").display(),
        dir.join("index.m3u8").display(),
    ))
    .expect("the HLS writer pipeline builds");
    writer.set_state(gst::State::Playing).unwrap();
    let msg = writer
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("writing the HLS stream finishes");
    assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{:?}", msg);
    writer.set_state(gst::State::Null).unwrap();
}

/// Serve the files in `dir` over HTTP on a local port, one connection at a
/// time; `hlsdemux2` only fetches over HTTP. Returns the base URL.
fn serve_dir(dir: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Read the headers up to the blank line; none of them matter.
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                line.clear();
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("/");
            let name = path.trim_start_matches('/').split('?').next().unwrap_or("");
            let response = match std::fs::read(dir.join(name)) {
                Ok(body) => {
                    let content_type = if name.ends_with(".m3u8") {
                        "application/vnd.apple.mpegurl"
                    } else {
                        "video/mp2t"
                    };
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        content_type,
                        body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(&body);
                    response
                }
                Err(_) => {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                }
            };
            let _ = stream.write_all(&response);
        }
    });
    format!("http://{}", addr)
}

fn fakesink(id: &str) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: "fakesink".to_string(),
        // Not async: the flow plays without waiting for a first buffer, so
        // what is measured is the player's internal pipeline alone.
        properties: HashMap::from([("async".to_string(), PropertyValue::Bool(false))]),
        position: [300.0, 200.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// A Media Player in passthrough with both outputs into fakesinks.
fn build_passthrough_flow(name: &str, uri: &str) -> Flow {
    let mut flow = Flow::new(name);
    flow.elements.push(fakesink("vsink"));
    flow.elements.push(fakesink("asink"));
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: HashMap::from([
            (
                "playlist".to_string(),
                PropertyValue::String(serde_json::to_string(&[uri]).unwrap()),
            ),
            ("decode".to_string(), PropertyValue::Bool(false)),
        ]),
        position: strom_types::block::Position { x: 100.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow.links.push(Link {
        from: format!("{}:video_out", BLOCK_ID),
        to: "vsink:sink".to_string(),
    });
    flow.links.push(Link {
        from: format!("{}:audio_out", BLOCK_ID),
        to: "asink:sink".to_string(),
    });
    flow
}

/// Whether the appsink named `name` in `internal` has taken a buffer.
fn has_sample(internal: &gst::Pipeline, name: &str) -> bool {
    internal
        .by_name(name)
        .and_then(|sink| sink.property::<Option<gst::Sample>>("last-sample"))
        .is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn muxed_hls_plays_audio_and_video_in_passthrough() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    run_glib_main_loop();

    let dir = tempfile::tempdir().unwrap();
    write_muxed_hls(dir.path());
    let uri = format!("{}/index.m3u8", serve_dir(dir.path().to_path_buf()));

    let (state, _files) = new_app_state();
    let flow = build_passthrough_flow("media_player_hls_passthrough", &uri);
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");
    state.start_flow(&flow_id).await.expect("start_flow failed");

    let player = MEDIA_PLAYER_REGISTRY
        .get(&MediaPlayerKey {
            flow_id,
            block_id: BLOCK_ID.to_string(),
        })
        .expect("the Media Player registered");

    // A local stream sets up in well under a second. The stall never clears.
    let deadline = Instant::now() + Duration::from_secs(15);
    let (mut playing, mut video, mut audio) = (false, false, false);
    while Instant::now() < deadline {
        if let Some(internal) = player.internal_pipeline.read().unwrap().as_ref() {
            playing = internal.current_state() == gst::State::Playing;
            video = has_sample(internal, &format!("{}_appsink_video", BLOCK_ID));
            audio = has_sample(internal, &format!("{}_appsink_audio", BLOCK_ID));
        }
        if playing && video && audio {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let internal = player
        .internal_pipeline
        .read()
        .unwrap()
        .as_ref()
        .expect("the Media Player built an internal pipeline")
        .downgrade();
    drop(player);
    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    assert!(
        playing && video && audio,
        "the player's internal pipeline must play muxed HLS in passthrough \
         (playing: {}, video buffer: {}, audio buffer: {}); one stuck in PAUSED \
         with no audio is the single-thread preroll stall of issue #1062",
        playing,
        video,
        audio
    );

    // The passthrough chain (queue, clocksync, appsink, bridged per pad) must
    // not keep the internal pipeline alive. A queued bus message can hold the
    // last reference for a moment; a reference kept by a signal handler never
    // goes.
    let deadline = Instant::now() + Duration::from_secs(5);
    while internal.upgrade().is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        internal.upgrade().is_none(),
        "the Media Player's internal pipeline survived teardown after playing \
         HLS in passthrough: something in its bridge chain still holds it"
    );
}
