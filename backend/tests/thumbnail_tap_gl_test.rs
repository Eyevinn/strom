//! A Thumbnail block on GL memory produces a thumbnail, whatever the format
//! or texture target (#1130).
//!
//! The thumbnail tap used to build `glcolorscale ! gldownload` for any GL
//! memory on the tee, assuming RGBA 2D textures, which is what `glcolorscale`
//! takes. GL memory in another format or texture target, such as NV12 or a
//! rectangle texture from VideoToolbox, did not negotiate, and the block gave
//! no thumbnail. The tap now asks `video_adapt::decide()` with
//! `Consumer::ScaleConvert`, which converts with `glcolorconvert` before
//! `glcolorscale`, or downloads first when the GPU scaler cannot take it.
//!
//! These tests build the real Thumbnail block and attach the real
//! `ThumbnailTap` to its tee, as the server does on a thumbnail request.
//! `gltestsrc ! glcolorconvert` makes GL memory in the format and texture
//! target under test.
//!
//! They need a GL context. On a Linux host with no display, which is CI,
//! `common::init_gl` asks for a surfaceless EGL context. Not built on
//! Windows: the Windows CI runner has no OpenGL driver GStreamer can use.
//! They run one at a time (`serial(gl)`, #1095).

#![cfg(not(target_os = "windows"))]

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_gl as gst_gl;
use gstreamer_gl::prelude::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use strom::blocks::builtin::thumbnail::ThumbnailBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::gst::{ThumbnailTap, ThumbnailTapConfig};

const GL_REQUIRED: &[&str] = &["gltestsrc", "glcolorconvert", "glcolorscale", "gldownload"];
const REQUIRED: &[&str] = &["tee", "queue", "fakesink", "videoscale", "videoconvert"];

/// A running flow: GL frames in `caps` into the real Thumbnail block's tee,
/// whose main output goes to a fakesink.
struct Flow {
    pipeline: gst::Pipeline,
    tee: gst::Element,
    /// Errors taken off the bus so far.
    errors: Mutex<Vec<String>>,
}

impl Flow {
    fn start(caps: &str) -> Self {
        let built = ThumbnailBuilder
            .build(
                "blk",
                &HashMap::new(),
                &BlockBuildContext::new(Vec::new(), "all".to_string()),
            )
            .expect("Thumbnail block builds");
        let pipeline = gst::Pipeline::new();
        let (_, tee) = built
            .elements
            .iter()
            .find(|(id, _)| id == "blk:tee")
            .cloned()
            .expect("the block's tee");
        pipeline.add(&tee).unwrap();
        let source = gst::parse::bin_from_description(
            &format!("gltestsrc is-live=true ! glcolorconvert ! {caps} ! identity"),
            true,
        )
        .expect("GL source");
        let main = gst::parse::bin_from_description("queue ! fakesink sync=false", true)
            .expect("main chain");
        pipeline.add_many([&source, &main]).unwrap();
        gst::Element::link_many([source.upcast_ref(), &tee, main.upcast_ref()])
            .expect("link the flow");
        pipeline.set_state(gst::State::Playing).unwrap();
        Flow {
            pipeline,
            tee,
            errors: Mutex::default(),
        }
    }

    /// Every error posted so far.
    fn errors(&self) -> Vec<String> {
        let bus = self.pipeline.bus().unwrap();
        let mut errors = self.errors.lock().unwrap();
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(err) = msg.view() {
                errors.push(format!(
                    "{}: {} ({:?})",
                    msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    err.error(),
                    err.debug()
                ));
            }
        }
        errors.clone()
    }

    /// Wait until the tee has negotiated what the source sends.
    fn wait_for_caps(&self) -> gst::Caps {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(caps) = self.tee.static_pad("sink").unwrap().current_caps() {
                return caps;
            }
            assert!(
                Instant::now() < deadline,
                "the tee never negotiated: {:?}",
                self.errors()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The size of the frames entering the tap's `gldownload`, if it has one.
    fn downloaded_size(&self) -> Option<(i32, i32)> {
        let download = self.pipeline.children().into_iter().find(|e| {
            e.name().contains("thumb") && e.factory().is_some_and(|f| f.name() == "gldownload")
        })?;
        let caps = download.static_pad("sink")?.current_caps()?;
        let s = caps.structure(0)?;
        Some((s.get("width").ok()?, s.get("height").ok()?))
    }

    /// Factory names of every element the tap added.
    fn branch_factories(&self) -> Vec<String> {
        self.pipeline
            .children()
            .into_iter()
            .filter(|e| e.name().contains("thumb"))
            .filter_map(|e| e.factory().map(|f| f.name().to_string()))
            .collect()
    }

    /// A thumbnail from the block's tee, as the server asks for one: the
    /// first request attaches the branch, later ones return its frame.
    fn thumbnail(&self) -> (Option<Vec<u8>>, Vec<String>) {
        let tap = ThumbnailTap::new_with_tee(
            &self.pipeline,
            "blk:thumb_0",
            self.tee.clone(),
            ThumbnailTapConfig {
                update_interval: Duration::from_millis(100),
                ..ThumbnailTapConfig::default()
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let jpeg = loop {
            if let Ok(jpeg) = tap.get_thumbnail() {
                break Some(jpeg);
            }
            if Instant::now() >= deadline || !self.errors().is_empty() {
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        (jpeg, self.branch_factories())
    }
}

impl Drop for Flow {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// The thumbnail's size, from the tap's default config.
const THUMBNAIL: (i32, i32) = (320, 180);

/// Run the block on GL memory in `caps` and expect a JPEG thumbnail, scaled
/// on the GPU: what enters the tap's `gldownload` is already thumbnail-sized.
/// Returns the factories of the tap's branch.
fn expect_thumbnail(caps: &str) -> Vec<String> {
    let flow = Flow::start(caps);
    let negotiated = flow.wait_for_caps();
    let (jpeg, factories) = flow.thumbnail();
    assert_eq!(
        flow.downloaded_size(),
        Some(THUMBNAIL),
        "{negotiated}: not downloaded at the thumbnail's size (branch: {factories:?})"
    );
    let errors = flow.errors();
    assert!(
        errors.is_empty(),
        "{negotiated}: the flow failed: {errors:?}"
    );
    let jpeg = jpeg
        .unwrap_or_else(|| panic!("{negotiated}: no thumbnail arrived (branch: {factories:?})"));
    assert!(
        jpeg.starts_with(&[0xFF, 0xD8]),
        "the thumbnail is not a JPEG"
    );
    factories
}

/// RGBA 2D textures, what the tap always handled: still scaled on the GPU,
/// then downloaded small.
#[test]
#[serial_test::serial(gl)]
fn gl_rgba_is_scaled_on_the_gpu() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }
    let factories = expect_thumbnail(
        "video/x-raw(memory:GLMemory),format=RGBA,width=640,height=480,framerate=30/1",
    );
    assert!(
        factories.iter().any(|f| f == "glcolorscale"),
        "not scaled on the GPU: {factories:?}"
    );
    assert!(
        !factories.iter().any(|f| f == "videoscale"),
        "scaled on the CPU: {factories:?}"
    );
}

/// The defect: GL NV12 on the tee gave no thumbnail.
#[test]
#[serial_test::serial(gl)]
fn gl_nv12_gives_a_thumbnail() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }
    expect_thumbnail(
        "video/x-raw(memory:GLMemory),format=NV12,width=640,height=480,framerate=30/1",
    );
}

/// The API of the GL context `gltestsrc` renders with here. Rectangle
/// textures are desktop GL (`OPENGL`, `OPENGL3`); GLES has none. Read from a
/// frame's memory, without the tap.
fn gl_api() -> gst_gl::GLAPI {
    let probe = gst::parse::launch(
        "gltestsrc num-buffers=1 ! video/x-raw(memory:GLMemory),format=RGBA,width=64,height=64 ! \
         appsink name=out sync=false",
    )
    .expect("GL probe")
    .downcast::<gst::Pipeline>()
    .unwrap();
    let sink = probe
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    probe.set_state(gst::State::Playing).unwrap();
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
    let _ = probe.set_state(gst::State::Null);
    let sample = sample.expect("the GL probe made no frame");
    let memory = sample
        .buffer()
        .expect("a frame")
        .peek_memory(0)
        .downcast_memory_ref::<gst_gl::GLBaseMemory>()
        .expect("GL memory")
        .to_owned();
    memory.context().gl_api()
}

/// A rectangle texture, what VideoToolbox outputs on macOS, gave no
/// thumbnail either. Skips only on a GLES context, which has no rectangle
/// textures, so nothing there produces them for the tap to meet. On desktop
/// GL it runs, and a failing rectangle conversion fails it.
#[test]
#[serial_test::serial(gl)]
fn gl_rectangle_texture_gives_a_thumbnail() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }
    let api = gl_api();
    if !api.intersects(gst_gl::GLAPI::OPENGL | gst_gl::GLAPI::OPENGL3) {
        eprintln!("SKIP: the GL context is {api:?}, which has no rectangle textures");
        return;
    }
    expect_thumbnail(
        "video/x-raw(memory:GLMemory),format=RGBA,width=640,height=480,framerate=30/1,\
         texture-target=rectangle",
    );
}
