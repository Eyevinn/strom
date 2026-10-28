//! `builtin.videoformat` fed raw video in GPU memory.
//!
//! Issue #969: the block built `autovideoconvert` or `videoconvert` from the
//! global video convert mode, in front of a capsfilter that pinned system
//! memory. CUDA memory passed through `videoscale` and `videoconvert` untouched
//! and was refused at the capsfilter (`not-negotiated`); in the GPU mode
//! `autovideoconvert` chose its own GPU path by rank. The block now picks its
//! scaling chain from the negotiated caps (`gst::scale_front`): GPU memory is
//! scaled on the GPU, and downloaded after scaling only when what follows
//! does not take it.
//!
//! These build the real block through `VideoFormatBuilder`. The GL cases run
//! `gltestsrc` on the GL runner (surfaceless EGL on Linux), with the producer
//! linked before the block's output, as a flow may order its links. The CUDA
//! case feeds an `appsrc` whose caps say CUDA memory on a host without
//! `cudaconvertscale`, so the block downloads first; a stand-in registered as
//! `cudadownload` takes the real element's place, passing system-memory
//! frames under CUDA caps through.
//!
//! Every test checks through [`add_block`] that the block builds plain
//! `videoconvert`, never `autovideoconvert`.
//!
//! On a host with the real `cudadownload`, the CUDA test skips: it pushes
//! system-memory buffers under CUDA caps, which the real element would read as
//! CUDA memory.

pub mod common;

#[path = "common/cuda_download_stand_in.rs"]
mod stand_in;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::videoformat::VideoFormatBuilder;
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const INSTANCE: &str = "vf";

const REQUIRED: &[&str] = &[
    "appsrc",
    "identity",
    "capsfilter",
    "videoscale",
    "videoconvert",
    "fakesink",
];

const CUDA_NV12: &str =
    "video/x-raw(memory:CUDAMemory),format=NV12,width=320,height=240,framerate=25/1";
const SYSTEM_NV12: &str = "video/x-raw,format=NV12,width=320,height=240,framerate=25/1";
const NVMM_NV12: &str = "video/x-raw(memory:NVMM),format=NV12,width=320,height=240,framerate=25/1";

/// What the block is set to produce: a smaller size in another format, so
/// both the scaler and the converter have work to do.
const RESOLUTION: &str = "160x120";
const FORMAT: &str = "I420";

/// Build the block into `pipeline` and return its declared input and output
/// pads.
///
/// The block's converter must be plain `videoconvert`.
fn add_block(pipeline: &gst::Pipeline) -> (gst::Pad, gst::Pad) {
    let props = HashMap::from([
        (
            "resolution".to_string(),
            PropertyValue::String(RESOLUTION.into()),
        ),
        ("format".to_string(), PropertyValue::String(FORMAT.into())),
    ]);
    let built = VideoFormatBuilder
        .build(INSTANCE, &props, &common::block::context())
        .expect("videoformat block builds");
    let by_id = common::block::install(pipeline, &built);

    let converter = by_id[&format!("{}:videoconvert", INSTANCE)]
        .factory()
        .map(|f| f.name().to_string());
    assert_eq!(
        converter.as_deref(),
        Some("videoconvert"),
        "the block built {:?}; it must be plain videoconvert, with GPU memory \
         scaled by its own chain (#969)",
        converter
    );

    // The block's declared external pads, not the element names they point
    // at today.
    let definition = strom::blocks::builtin::videoformat::get_blocks()
        .into_iter()
        .next()
        .expect("videoformat definition");
    let pad = |p: &strom_types::block::ExternalPad| {
        by_id[&format!("{}:{}", INSTANCE, p.internal_element_id)]
            .static_pad(&p.internal_pad_name)
            .expect("block pad")
    };
    (
        pad(&definition.external_pads.inputs[0]),
        pad(&definition.external_pads.outputs[0]),
    )
}

/// How many elements of `factory` are in `pipeline`.
fn count(pipeline: &gst::Pipeline, factory: &str) -> usize {
    pipeline
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.factory().is_some_and(|f| f.name() == factory))
        .count()
}

/// Check what left the block: `memory`, at the size and in the format the
/// block was set to.
fn assert_output(block_out: &gst::Pad, memory: &str) {
    let caps = block_out.current_caps().expect("the block's output caps");
    assert!(
        caps.features(0).is_some_and(|f| f.contains(memory)),
        "the block emitted {caps}, not {memory}"
    );
    let info = gst_video::VideoInfo::from_caps(&caps).expect("raw video caps");
    assert_eq!((info.width(), info.height()), (160, 120), "{caps}");
    assert_eq!(info.format(), gst_video::VideoFormat::I420, "{caps}");
}

/// `appsrc ! [videoformat block] ! fakesink`, the appsrc pushing frames from a
/// thread.
struct Harness {
    pipeline: gst::Pipeline,
    block_out: gst::Pad,
    frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    pusher: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// `None`, after saying why, when the test needs the `cudadownload`
    /// stand-in and the real element is installed.
    fn start(caps: &str) -> Option<Self> {
        common::require_elements(REQUIRED);
        if !stand_in::register() && caps.contains("memory:CUDAMemory") {
            eprintln!(
                "SKIP: the real cudadownload is installed; these buffers are not CUDA memory"
            );
            return None;
        }

        let pipeline = gst::Pipeline::new();
        let (block_in, block_out) = add_block(&pipeline);
        let caps: gst::Caps = caps.parse().unwrap();
        let src = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([src.upcast_ref(), &sink]).unwrap();
        src.static_pad("src")
            .unwrap()
            .link(&block_in)
            .expect("the producer links into the block");
        block_out
            .link(&sink.static_pad("sink").unwrap())
            .expect("the block links into the sink");

        let frames = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&frames);
        block_out.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        pipeline.set_state(gst::State::Playing).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let pusher_stop = Arc::clone(&stop);
        let size = gst_video::VideoInfo::from_caps(&caps)
            .expect("raw video caps")
            .size();
        let pusher = std::thread::spawn(move || {
            let mut pts = gst::ClockTime::ZERO;
            let frame = gst::ClockTime::from_mseconds(40);
            while !pusher_stop.load(Ordering::Relaxed) {
                let mut buffer = gst::Buffer::with_size(size).unwrap();
                {
                    let b = buffer.get_mut().unwrap();
                    b.set_pts(pts);
                    b.set_duration(frame);
                }
                pts += frame;
                if src.push_buffer(buffer).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        Some(Harness {
            pipeline,
            block_out,
            frames,
            stop,
            pusher: Some(pusher),
        })
    }

    /// Wait until `n` frames have left the block, or `timeout` passes.
    fn produces(&self, n: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.frames.load(Ordering::Relaxed) >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn errors(&self) -> Vec<(String, String, String)> {
        let bus = self.pipeline.bus().unwrap();
        let mut errors = Vec::new();
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(err) = msg.view() {
                errors.push((
                    msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    err.error().to_string(),
                    err.debug().map(|d| d.to_string()).unwrap_or_default(),
                ));
            }
        }
        errors
    }

    fn downloads(&self) -> usize {
        count(
            &self.pipeline,
            strom::gst::video_adapt::CUDA_DOWNLOAD_FACTORY,
        )
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(pusher) = self.pusher.take() {
            let _ = pusher.join();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// CUDA memory from a hardware decoder, on a host without `cudaconvertscale`,
/// is downloaded, then scaled and converted on the CPU.
#[test]
fn cuda_memory_without_a_cuda_scaler_is_downloaded_then_scaled() {
    gst::init().expect("GStreamer initialises");
    if strom::gst::video_adapt::factory_available("cudaconvertscale") {
        eprintln!("SKIP: cudaconvertscale is installed here; the block scales CUDA on the GPU");
        return;
    }
    let Some(harness) = Harness::start(CUDA_NV12) else {
        return;
    };
    let produced = harness.produces(10, Duration::from_secs(15));
    let errors = harness.errors();
    assert!(errors.is_empty(), "pipeline errors: {:?}", errors);
    assert!(produced, "no frames left the block for CUDA input");
    assert_output(&harness.block_out, gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);
    assert_eq!(harness.downloads(), 1, "expected one cudadownload");
}

/// System memory goes in with no download.
#[test]
fn system_memory_is_scaled_and_converted_without_a_download() {
    let Some(harness) = Harness::start(SYSTEM_NV12) else {
        return;
    };
    assert!(
        harness.produces(10, Duration::from_secs(15)),
        "no frames: {:?}",
        harness.errors()
    );
    assert_output(&harness.block_out, gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);
    assert_eq!(harness.downloads(), 0);
}

/// Memory nothing here downloads fails the flow, naming the memory and the
/// block, instead of a bare not-negotiated.
#[test]
fn memory_nothing_downloads_fails_the_flow_with_a_message() {
    let Some(harness) = Harness::start(NVMM_NV12) else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut errors = Vec::new();
    while errors.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        errors = harness.errors();
    }
    let (source, message, _) = errors.first().expect("an element error");
    assert_eq!(source, &format!("{}:input", INSTANCE));
    assert!(
        message.contains("memory:NVMM") && message.contains("Video Format"),
        "{message}"
    );
}

/// GL memory into the block, as `vtdec` on macOS or a GPU Vision Mixer
/// delivers it.
///
/// The producer is linked into the block before the block's output is
/// linked, as a flow may order its links. The output link's caps check then
/// asks through the block for what upstream offers: GL memory at 320x240,
/// which the block's 160x120 capsfilter would refuse (`NoFormat`) unless the
/// block answers with what it can scale that to.
///
/// The tests run one at a time (`serial(gl)`): GL contexts started in
/// parallel race in EGL (#1095).
#[cfg(not(target_os = "windows"))]
mod gl {
    use super::*;

    const GL_REQUIRED: &[&str] = &["gltestsrc", "glcolorconvert", "glcolorscale", "gldownload"];

    /// `gltestsrc ! capsfilter(GL RGBA 320x240) ! [videoformat] ! [system
    /// memory pin] ! fakesink`, run to EOS. Returns the frame count out of the
    /// block, its output caps and the factories in the pipeline.
    fn run(system_memory_downstream: bool) -> (u64, gst::Caps, Vec<String>) {
        let pipeline = gst::Pipeline::new();
        let (block_in, block_out) = add_block(&pipeline);
        let src = gst::ElementFactory::make("gltestsrc")
            .property("num-buffers", 30i32)
            .build()
            .unwrap();
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                "video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240, framerate=30/1"
                    .parse::<gst::Caps>()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &filter, &sink]).unwrap();
        src.link(&filter).expect("link the GL source");
        filter
            .static_pad("src")
            .unwrap()
            .link(&block_in)
            .expect("link into the block");
        let downstream = if system_memory_downstream {
            // A consumer that takes system memory only.
            let pin = gst::ElementFactory::make("capsfilter")
                .property("caps", gst::Caps::new_empty_simple("video/x-raw"))
                .build()
                .unwrap();
            pipeline.add(&pin).unwrap();
            pin.link(&sink).unwrap();
            pin
        } else {
            sink.clone()
        };
        block_out
            .link(&downstream.static_pad("sink").unwrap())
            .expect("link out of the block");

        let frames = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&frames);
        block_out.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        pipeline.set_state(gst::State::Playing).unwrap();
        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(20),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS or an error");
        let factories: Vec<String> = pipeline
            .iterate_recurse()
            .into_iter()
            .filter_map(Result::ok)
            .filter_map(|e| e.factory().map(|f| f.name().to_string()))
            .collect();
        let caps = block_out.current_caps();
        pipeline.set_state(gst::State::Null).unwrap();
        if let gst::MessageView::Error(e) = msg.view() {
            panic!("pipeline error: {} ({:?})", e.error(), e.debug());
        }
        (
            frames.load(Ordering::Relaxed),
            caps.expect("the block's output caps"),
            factories,
        )
    }

    fn has(factories: &[String], factory: &str) -> bool {
        factories.iter().any(|f| f == factory)
    }

    /// Downstream takes GL memory: the block scales and converts on the GPU
    /// and emits GL memory, with no download and no CPU scaler.
    #[test]
    #[serial_test::serial(gl)]
    fn gl_memory_is_scaled_on_the_gpu_and_stays_there() {
        if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
            return;
        }
        let (frames, caps, factories) = run(false);
        assert!(frames > 0, "no frames out of the block");
        assert!(
            caps.features(0)
                .is_some_and(|f| f.contains(strom::gst::video_adapt::GL_MEMORY_FEATURE)),
            "the block emitted {caps}, not GL memory"
        );
        let info = gst_video::VideoInfo::from_caps(&caps).expect("raw video caps");
        assert_eq!((info.width(), info.height()), (160, 120), "{caps}");
        assert_eq!(info.format(), gst_video::VideoFormat::I420, "{caps}");
        assert!(
            has(&factories, "glcolorscale"),
            "no GPU scaler: {factories:?}"
        );
        assert!(
            !has(&factories, "gldownload"),
            "GL memory was downloaded: {factories:?}"
        );
        assert!(
            !has(&factories, "videoscale"),
            "scaled on the CPU: {factories:?}"
        );
    }

    /// Downstream takes system memory only: the block scales on the GPU, then
    /// downloads the smaller frame.
    #[test]
    #[serial_test::serial(gl)]
    fn gl_memory_is_scaled_on_the_gpu_then_downloaded() {
        if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
            return;
        }
        let (frames, caps, factories) = run(true);
        assert!(frames > 0, "no frames out of the block");
        let info = gst_video::VideoInfo::from_caps(&caps).expect("raw video caps");
        assert_eq!((info.width(), info.height()), (160, 120), "{caps}");
        assert_eq!(info.format(), gst_video::VideoFormat::I420, "{caps}");
        assert!(
            caps.features(0)
                .is_some_and(|f| f.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)),
            "the block emitted {caps}, not system memory"
        );
        assert!(
            has(&factories, "glcolorscale"),
            "no GPU scaler: {factories:?}"
        );
        assert!(has(&factories, "gldownload"), "no download: {factories:?}");
        assert!(
            !has(&factories, "videoscale"),
            "scaled on the CPU: {factories:?}"
        );
    }
}
