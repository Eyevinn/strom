//! GL frames from a Media Player's internal pipeline convert in the flow.
//!
//! The Media Player decodes in a pipeline of its own and hands the frames to
//! the flow over an appsink/appsrc pair. A decoder that outputs GL memory,
//! VideoToolbox on macOS, made a GL context of its own, and the flow's first GL
//! converter failed on the first frame: "input memory OpenGL context is
//! different" (#1091, #1099).
//!
//! This builds the real block and uses the internal pipeline the block made,
//! with the context forwarding the block installed on it. CI has no decoder
//! that outputs GL memory, so `gltestsrc` stands in for the decoder inside that
//! pipeline and feeds the block's own appsrc. In the flow the block's output
//! goes into `glcolorconvert`, as a GPU Vision Mixer input does. With the
//! forwarding removed from the block, the converter fails with that error.
//!
//! It needs a GL context. On a Linux host with no display, which is CI,
//! `common::init_gl` asks for a surfaceless EGL context, which Mesa's software
//! rasteriser provides.
//!
//! Not built on Windows. The Windows CI runner has no OpenGL driver that
//! GStreamer can use, and there is no software GL context to fall back to.

#![cfg(not(target_os = "windows"))]

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::mediaplayer::{
    MediaPlayerBuilder, MediaPlayerKey, MEDIA_PLAYER_REGISTRY,
};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

const GL_REQUIRED: &[&str] = &["gltestsrc", "glcolorconvert", "gldownload"];
const REQUIRED: &[&str] = &["appsink", "fakesink", "uridecodebin3"];
const FRAMES: u64 = 15;

#[test]
fn gl_frames_from_the_internal_pipeline_convert_in_the_flow() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }

    // The real block, in decode mode, with one video output and no playlist.
    let flow_id = uuid::Uuid::new_v4();
    let props: HashMap<String, PropertyValue> = [
        ("decode".to_string(), PropertyValue::Bool(true)),
        ("num_audio_tracks".to_string(), PropertyValue::UInt(0)),
        (
            "_flow_id".to_string(),
            PropertyValue::String(flow_id.to_string()),
        ),
    ]
    .into();
    let built = MediaPlayerBuilder
        .build(
            "mp",
            &props,
            &BlockBuildContext::new(Vec::new(), "all".to_string()),
        )
        .expect("Media Player builds");
    let key = MediaPlayerKey {
        flow_id,
        block_id: "mp".to_string(),
    };
    let player = MEDIA_PLAYER_REGISTRY.get(&key).expect("registered");
    let internal = player
        .internal_pipeline
        .read()
        .unwrap()
        .clone()
        .expect("internal pipeline");

    // The flow: the block's elements, then a GL converter.
    let flow = gst::Pipeline::new();
    for (_, element) in &built.elements {
        flow.add(element).unwrap();
    }
    let by_id = |id: &str| {
        built
            .elements
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| panic!("{id} in the block"))
    };
    let consumer = gst::parse::bin_from_description(
        "glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12 ! gldownload ! \
         fakesink name=sink sync=false",
        true,
    )
    .expect("consumer");
    flow.add(&consumer).unwrap();
    gst::Element::link_many([
        &by_id("mp:appsrc_video"),
        &by_id("mp:queue_video"),
        &by_id("mp:video_out"),
        consumer.upcast_ref(),
    ])
    .expect("link the flow");
    let received = Arc::new(AtomicU64::new(0));
    let counter = received.clone();
    consumer
        .by_name("sink")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

    // Inside: GL frames where the decoder would be, into the block's appsrc.
    let source = internal
        .by_name("mp_uridecodebin3")
        .expect("the block's decoder");
    internal.remove(&source).unwrap();
    let decoder = gst::parse::bin_from_description(
        &format!(
            "gltestsrc num-buffers={FRAMES} ! \
             video/x-raw(memory:GLMemory),format=RGBA,width=320,height=240,framerate=30/1 ! \
             appsink name=out sync=false"
        ),
        false,
    )
    .expect("GL stand-in for the decoder");
    internal.add(&decoder).unwrap();
    let appsrc = player.flow_anchor().expect("video output").downgrade();
    let eos_to = appsrc.clone();
    decoder
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap()
        .set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    appsrc
                        .upgrade()
                        .ok_or(gst::FlowError::Flushing)?
                        .push_sample(&sample)
                })
                .eos(move |_| {
                    if let Some(src) = eos_to.upgrade() {
                        let _ = src.end_of_stream();
                    }
                })
                .build(),
        );

    // As in a flow: the flow first, then the player.
    flow.set_state(gst::State::Playing).expect("flow plays");
    internal
        .set_state(gst::State::Playing)
        .expect("internal plays");

    let mut error = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    'wait: while Instant::now() < deadline && received.load(Ordering::Relaxed) < FRAMES {
        for bus in [flow.bus().unwrap(), internal.bus().unwrap()] {
            while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
                if let gst::MessageView::Error(e) = msg.view() {
                    error = Some(format!("{} ({:?})", e.error(), e.debug()));
                    break 'wait;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = internal.set_state(gst::State::Null);
    let _ = flow.set_state(gst::State::Null);
    MEDIA_PLAYER_REGISTRY.unregister(&key);

    assert!(
        error.is_none(),
        "the flow's GL converter failed on the player's frames: {:?}",
        error
    );
    assert_eq!(
        received.load(Ordering::Relaxed),
        FRAMES,
        "every frame reached the flow's sink"
    );
}
