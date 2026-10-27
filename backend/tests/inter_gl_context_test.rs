//! GL frames from another flow convert in the flow that subscribes to them.
//!
//! Two flows are two pipelines with a GL context each, and they share none. A
//! GPU Vision Mixer with `gl_download=false` sends GL memory into Inter Output,
//! and the first GL converter behind Inter Input in the other flow failed on
//! it: "input memory OpenGL context is different" (#1099). Inter Input now
//! downloads GL memory that arrives from another flow.
//!
//! Both tests build the real Inter Output and Inter Input blocks, each in a
//! pipeline of its own, as two flows are. `gltestsrc` makes GL memory in flow
//! A. In flow B the Inter Input feeds `glupload ! glcolorconvert`, as a GPU
//! Vision Mixer input does. `glupload` passes GL memory through whatever its
//! context, so with the download removed from the block the converter fails:
//! "Failed to convert video buffer".
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
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_gl as gst_gl;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::inter::{InterInputBuilder, InterOutputBuilder};
use strom::blocks::{BlockBuildContext, BlockBuildResult, BlockBuilder};
use strom_types::PropertyValue;

const GL_REQUIRED: &[&str] = &["gltestsrc", "glcolorconvert", "gldownload"];
const REQUIRED: &[&str] = &["identity", "capsfilter", "fakesink"];
const FRAMES: u64 = 15;

/// The inter elements are linked into Strom and registered at startup, so a
/// test binary registers them too.
fn register_inter() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gstrsinter::plugin_register_static().expect("register the inter plugin");
    });
}

fn ctx() -> BlockBuildContext {
    BlockBuildContext::new(Vec::new(), "all".to_string())
}

/// The block's elements in `pipeline`, with its internal links made.
fn add_block(pipeline: &gst::Pipeline, built: &BlockBuildResult) {
    for (_, element) in &built.elements {
        pipeline.add(element).unwrap();
    }
    let by_id = |id: &str| {
        built
            .elements
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| panic!("{id} in the block"))
    };
    for (from, to) in &built.internal_links {
        by_id(&from.element_id)
            .link_pads(
                from.pad_name.as_deref(),
                &by_id(&to.element_id),
                to.pad_name.as_deref(),
            )
            .expect("internal link");
    }
}

fn element(built: &BlockBuildResult, id: &str) -> gst::Element {
    built
        .elements
        .iter()
        .find(|(i, _)| i == id)
        .map(|(_, e)| e.clone())
        .unwrap_or_else(|| panic!("{id} in the block"))
}

/// Flow A: GL frames into the real Inter Output. Also returns its channel.
fn publisher() -> (gst::Pipeline, String) {
    let flow_id = uuid::Uuid::new_v4().to_string();
    let props: HashMap<String, PropertyValue> = [
        (
            "_flow_id".to_string(),
            PropertyValue::String(flow_id.clone()),
        ),
        (
            "_block_id".to_string(),
            PropertyValue::String("out".to_string()),
        ),
        ("sync".to_string(), PropertyValue::Bool(false)),
    ]
    .into();
    let built = InterOutputBuilder
        .build("out", &props, &ctx())
        .expect("Inter Output builds");
    let flow = gst::Pipeline::new();
    add_block(&flow, &built);
    let source = gst::parse::bin_from_description(
        "gltestsrc is-live=true ! \
         video/x-raw(memory:GLMemory),format=RGBA,width=320,height=240,framerate=30/1 ! identity",
        true,
    )
    .expect("GL source");
    flow.add(&source).unwrap();
    source
        .link(&element(&built, "out:intersink"))
        .expect("link the source");
    (flow, format!("strom_{}_out", flow_id))
}

/// Flow B: the real Inter Input into a GL converter, as a GPU mixer input.
/// Also returns the count of frames that reached its sink.
fn subscriber(channel: &str) -> (gst::Pipeline, Arc<AtomicU64>) {
    let props: HashMap<String, PropertyValue> = [
        (
            "channel".to_string(),
            PropertyValue::String(channel.to_string()),
        ),
        (
            "max_time".to_string(),
            PropertyValue::String("500".to_string()),
        ),
    ]
    .into();
    let built = InterInputBuilder
        .build("in", &props, &ctx())
        .expect("Inter Input builds");
    let flow = gst::Pipeline::new();
    add_block(&flow, &built);
    let consumer = gst::parse::bin_from_description(
        "glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12 ! gldownload ! \
         fakesink name=sink sync=false",
        true,
    )
    .expect("consumer");
    flow.add(&consumer).unwrap();
    element(&built, "in:front")
        .link(&consumer)
        .expect("link the block's output");
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
    (flow, received)
}

/// The first error on any of `pipelines`' buses.
fn error_on(pipelines: &[&gst::Pipeline]) -> Option<String> {
    for pipeline in pipelines {
        let bus = pipeline.bus().unwrap();
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
    }
    None
}

/// Wait until `done`, or an error on `pipelines`, or the deadline.
fn wait(
    pipelines: &[&gst::Pipeline],
    timeout: Duration,
    done: impl Fn() -> bool,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && !done() {
        if let Some(error) = error_on(pipelines) {
            return Some(error);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    error_on(pipelines)
}

#[test]
fn gl_frames_from_another_flow_convert_in_the_subscriber() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }
    register_inter();

    let (a, channel) = publisher();
    let (b, received) = subscriber(&channel);
    a.set_state(gst::State::Playing).expect("flow A plays");
    b.set_state(gst::State::Playing).expect("flow B plays");

    let error = wait(&[&a, &b], Duration::from_secs(15), || {
        received.load(Ordering::Relaxed) >= FRAMES
    });
    let _ = b.set_state(gst::State::Null);
    let _ = a.set_state(gst::State::Null);

    assert!(
        error.is_none(),
        "flow B's GL converter failed on flow A's frames: {:?}",
        error
    );
    assert!(
        received.load(Ordering::Relaxed) >= FRAMES,
        "only {} of {} frames reached flow B's sink",
        received.load(Ordering::Relaxed),
        FRAMES
    );
}

/// GL buffers that flow B holds keep flow A's GL context alive after A stops:
/// B downloads them without an error. Once B stops too, nothing holds the
/// context any more, and it is finalized.
#[test]
fn flow_a_gl_context_outlives_flow_a_until_flow_b_lets_go() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }
    register_inter();

    let (a, channel) = publisher();
    // Flow A's GL context, from the memory of a frame it publishes. Test
    // code: the probe removes itself after the first frame.
    let context: Arc<Mutex<Option<glib::WeakRef<gst_gl::GLContext>>>> = Arc::default();
    let seen = context.clone();
    a.iterate_sinks()
        .into_iter()
        .flatten()
        .next()
        .expect("flow A's intersink")
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(memory) = buffer
                .peek_memory(0)
                .downcast_memory_ref::<gst_gl::GLBaseMemory>()
            else {
                return gst::PadProbeReturn::Ok;
            };
            *seen.lock().unwrap() = Some(memory.context().downgrade());
            gst::PadProbeReturn::Remove
        });
    let (b, received) = subscriber(&channel);
    a.set_state(gst::State::Playing).expect("flow A plays");
    b.set_state(gst::State::Playing).expect("flow B plays");

    let error = wait(&[&a, &b], Duration::from_secs(15), || {
        received.load(Ordering::Relaxed) >= FRAMES
    });
    assert!(error.is_none(), "before flow A stops: {:?}", error);
    let context = context
        .lock()
        .unwrap()
        .take()
        .expect("flow A publishes GL memory");

    // Flow A stops and is dropped. B keeps running on what it holds.
    a.set_state(gst::State::Null).expect("flow A stops");
    drop(a);
    let error = wait(&[&b], Duration::from_secs(1), || false);
    assert!(error.is_none(), "after flow A stopped: {:?}", error);

    b.set_state(gst::State::Null).expect("flow B stops");
    drop(b);
    let deadline = Instant::now() + Duration::from_secs(5);
    while context.upgrade().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        context.upgrade().is_none(),
        "flow A's GL context is still alive after both flows stopped"
    );
}
