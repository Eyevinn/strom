//! Regression test: an error in the Media Player's internal pipeline reaches
//! the operator.
//!
//! The Media Player plays in a pipeline of its own, whose bus is not the
//! flow's. Its errors were only logged, so a file or stream that could not
//! play left the block showing as playing, with nothing on its outputs and no
//! reason given (issue #1079: an https HLS stream on a Windows install whose
//! GIO had no TLS backend).

pub mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom_types::{Flow, Link, PropertyValue, StromEvent};

use gstreamer as gst;

const BLOCK_ID: &str = "player";

/// Run the default GLib main context, where the internal pipeline's bus watch
/// is dispatched, as Strom does in production.
fn run_glib_main_loop() {
    std::thread::spawn(|| {
        let main_loop = gst::glib::MainLoop::new(None, false);
        main_loop.run();
    });
}

fn fakesink(id: &str) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::from([("async".to_string(), PropertyValue::Bool(false))]),
        position: [300.0, 200.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// A Media Player whose playlist holds `file`, outputs into fakesinks.
fn player_flow(file: &str) -> Flow {
    let mut flow = Flow::new("media_player_error_event");
    flow.elements.push(fakesink("vsink"));
    flow.elements.push(fakesink("asink"));
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: HashMap::from([(
            "playlist".to_string(),
            PropertyValue::String(serde_json::to_string(&[file]).unwrap()),
        )]),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_cannot_play_reports_an_error_naming_the_block() {
    if !common::plugins_available(&["uridecodebin3", "filesrc", "fakesink", "clocksync"]) {
        return;
    }
    run_glib_main_loop();

    let media = tempfile::tempdir().unwrap();
    let missing = "no-such-clip-1079.mp4";
    let state = common::state::with_media(media.path());
    let mut events = state.events().subscribe();

    let flow = player_flow(missing);
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");
    state.start_flow(&flow_id).await.expect("start_flow failed");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut reported = None;
    while reported.is_none() && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(StromEvent::PipelineError {
                flow_id: id,
                error,
                source,
            })) if id == flow_id && source.as_deref() == Some(BLOCK_ID) => {
                reported = Some(error);
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            _ => {}
        }
    }
    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    let error = reported.expect(
        "a Media Player whose file cannot be opened sent no PipelineError naming the \
         block: its internal pipeline's error stayed in the log, and the block shows \
         as playing with nothing on its outputs (issue #1079)",
    );
    assert!(
        error.contains(missing),
        "the error does not say which file failed: {}",
        error
    );
}
