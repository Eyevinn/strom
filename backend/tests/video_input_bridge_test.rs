//! The WHEP Output video input bridge on a running pipeline, when what arrives
//! changes mid-stream or cannot be adapted.
//!
//! The bridge used to decide once, from the first CAPS event, and then remove
//! its probe. A producer that later switched memory type (a Media Player
//! moving on to a file the GPU decodes) reached the sink unadapted. And when it
//! could not adapt, it only logged a warning, so the flow failed with a bare
//! not-negotiated.
//!
//! These drive the real bridge: `appsrc ! queue ! capsfilter(video/x-raw) !
//! appsink`, with the bridge on the queue's source pad. The capsfilter plays a
//! sink that takes raw video in system memory only, so CUDA memory is refused
//! there and has to be downloaded. Real CUDA memory needs an NVIDIA host, so a
//! stand-in registered as `cudadownload` takes the real element's place; the
//! frames the `appsrc` pushes are plain system memory under CUDA caps, which
//! the stand-in passes through.

pub mod common;

#[path = "common/cuda_download_stand_in.rs"]
mod stand_in;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::gst::video_adapt::CUDA_DOWNLOAD_FACTORY;
use strom::gst::video_input_bridge::install_video_input_bridge;

const SYSTEM_NV12: &str = "video/x-raw,format=NV12,width=320,height=240,framerate=25/1";
const SYSTEM_I420: &str = "video/x-raw,format=I420,width=320,height=240,framerate=25/1";
const SYSTEM_RGBA: &str = "video/x-raw,format=RGBA,width=320,height=240,framerate=25/1";
const CUDA_NV12: &str =
    "video/x-raw(memory:CUDAMemory),format=NV12,width=320,height=240,framerate=25/1";
const NVMM_NV12: &str = "video/x-raw(memory:NVMM),format=NV12,width=320,height=240,framerate=25/1";

/// Elements the harness and the bridge's converter are built from. All ship
/// with GStreamer core and gst-plugins-base, which CI installs.
const ELEMENTS: &[&str] = &["appsrc", "queue", "capsfilter", "appsink", "videoconvert"];

const QUEUE: &str = "blk:video_queue";
const SYSTEM_ONLY: &str = "blk:system_only";

struct Harness {
    pipeline: gst::Pipeline,
    queue: gst::Element,
    frames: Arc<AtomicU64>,
    /// Caps the pusher switches to on its next frame.
    next_caps: Arc<Mutex<Option<gst::Caps>>>,
    stop: Arc<AtomicBool>,
    pusher: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start(caps: &str) -> Self {
        common::require_elements(ELEMENTS);
        stand_in::register();

        let pipeline = gst::Pipeline::new();
        let caps: gst::Caps = caps.parse().unwrap();
        let src = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let queue = gst::ElementFactory::make("queue")
            .name(QUEUE)
            .build()
            .unwrap();
        let system_only = gst::ElementFactory::make("capsfilter")
            .name(SYSTEM_ONLY)
            .property("caps", gst::Caps::new_empty_simple("video/x-raw"))
            .build()
            .unwrap();
        let frames = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&frames);
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        counter.fetch_add(1, Ordering::Relaxed);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            )
            .build();
        pipeline
            .add_many([src.upcast_ref(), &queue, &system_only, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([src.upcast_ref(), &queue, &system_only, sink.upcast_ref()])
            .unwrap();

        install_video_input_bridge(&queue.static_pad("src").unwrap(), QUEUE);
        pipeline.set_state(gst::State::Playing).unwrap();

        let next_caps: Arc<Mutex<Option<gst::Caps>>> = Arc::new(Mutex::new(None));
        let pusher_caps = Arc::clone(&next_caps);
        let stop = Arc::new(AtomicBool::new(false));
        let pusher_stop = Arc::clone(&stop);
        let pusher = std::thread::spawn(move || {
            let mut size = frame_size(&caps);
            let mut pts = gst::ClockTime::ZERO;
            let frame = gst::ClockTime::from_mseconds(40);
            while !pusher_stop.load(Ordering::Relaxed) {
                if let Some(caps) = pusher_caps.lock().unwrap().take() {
                    size = frame_size(&caps);
                    src.set_caps(Some(&caps));
                }
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
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        Harness {
            pipeline,
            queue,
            frames,
            next_caps,
            stop,
            pusher: Some(pusher),
        }
    }

    /// Wait until `n` more frames have reached the sink, or `timeout` passes.
    fn frames_arrive(&self, n: u64, timeout: Duration) -> bool {
        let target = self.frames.load(Ordering::Relaxed) + n;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.frames.load(Ordering::Relaxed) >= target {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn switch_to(&self, caps: &str) {
        *self.next_caps.lock().unwrap() = Some(caps.parse().unwrap());
    }

    /// The pixel format the stand-in for the sink has negotiated.
    fn sink_format(&self) -> Option<String> {
        self.pipeline
            .by_name(SYSTEM_ONLY)?
            .static_pad("sink")?
            .current_caps()?
            .structure(0)?
            .get::<String>("format")
            .ok()
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

    /// Factories between the queue and the capsfilter standing in for the
    /// sink, in link order.
    fn spliced(&self) -> Vec<String> {
        let mut spliced = Vec::new();
        let mut next = self.queue.static_pad("src").and_then(|pad| pad.peer());
        while let Some(pad) = next {
            let element = pad.parent_element().expect("linked pad has an element");
            if element.name() == SYSTEM_ONLY {
                break;
            }
            spliced.push(
                element
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default(),
            );
            next = element.static_pad("src").and_then(|pad| pad.peer());
        }
        spliced
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

fn frame_size(caps: &gst::Caps) -> usize {
    gst_video::VideoInfo::from_caps(caps)
        .expect("raw video caps")
        .size()
}

/// The defect: a producer that switches to CUDA memory mid-stream. With the
/// fix, the bridge decides again on the new caps and downloads; without it,
/// the probe was gone after the first caps and the sink refused CUDA memory
/// (not-negotiated).
#[test]
fn a_switch_to_cuda_memory_mid_stream_is_downloaded() {
    let harness = Harness::start(SYSTEM_NV12);
    assert!(
        harness.frames_arrive(5, Duration::from_secs(5)),
        "system-memory NV12 never reached the sink: {:?}",
        harness.errors()
    );
    assert!(
        harness.spliced().is_empty(),
        "nothing should be spliced in yet"
    );

    harness.switch_to(CUDA_NV12);
    let flowing = harness.frames_arrive(10, Duration::from_secs(5));
    let errors = harness.errors();
    assert!(errors.is_empty(), "the flow failed: {:?}", errors);
    assert!(flowing, "frames stopped after the switch to CUDA memory");
    assert_eq!(harness.spliced(), [CUDA_DOWNLOAD_FACTORY, "capsfilter"]);
}

/// The adapters follow the caps both ways: a converter put in for RGBA comes
/// out again when the producer moves to NV12, and a download goes in front of
/// it when the same format moves to CUDA memory.
#[test]
fn the_adapters_follow_the_caps_both_ways() {
    let harness = Harness::start(SYSTEM_RGBA);
    assert!(harness.frames_arrive(5, Duration::from_secs(5)));
    assert_eq!(harness.spliced(), ["videoconvert", "capsfilter"]);

    harness.switch_to(SYSTEM_NV12);
    assert!(harness.frames_arrive(5, Duration::from_secs(5)));
    assert!(
        harness.spliced().is_empty(),
        "the converter should have been taken out: {:?}",
        harness.spliced()
    );

    harness.switch_to(CUDA_NV12);
    assert!(harness.frames_arrive(5, Duration::from_secs(5)));
    assert_eq!(harness.spliced(), [CUDA_DOWNLOAD_FACTORY, "capsfilter"]);

    let errors = harness.errors();
    assert!(errors.is_empty(), "the flow failed: {:?}", errors);
    // Nothing taken out is left behind in the pipeline.
    let mut leftovers: Vec<String> = harness
        .pipeline
        .children()
        .into_iter()
        .map(|e| e.name().to_string())
        .filter(|n| n.starts_with(QUEUE) && n != QUEUE)
        .collect();
    leftovers.sort();
    assert!(
        leftovers.len() == 2
            && leftovers[0].ends_with("_cudadownload")
            && leftovers[1].ends_with("_system_memory"),
        "only the download and its pin should be left: {:?}",
        leftovers
    );
}

/// Memory the sink refuses and nothing here downloads fails the flow with an
/// error that names it. Before, the bridge only logged a warning and the flow
/// showed a bare not-negotiated.
#[test]
fn unsupported_memory_fails_the_flow_with_a_message() {
    let harness = Harness::start(NVMM_NV12);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut errors = Vec::new();
    while errors.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        errors = harness.errors();
    }
    let (src, message, debug) = errors
        .first()
        .cloned()
        .expect("the flow should have failed");
    assert_eq!(
        src, QUEUE,
        "the first error should come from the bridge: {:?}",
        errors
    );
    assert!(
        message.contains("memory:NVMM"),
        "the error should name what arrived: {:?}",
        errors
    );
    assert!(
        debug.contains("builtin.videoenc"),
        "the error should say what fixes it: {:?}",
        errors
    );
}

/// The sink keeps the format it started on. `webrtcsink` refuses a format
/// change on a running pad ("Renegotiation is not supported"), so taking the
/// converter out when RGBA turns into I420, or leaving I420 alone after the
/// sink started on NV12, would stop the video. Each producer format change
/// here is converted back to the sink's.
#[test]
fn the_sink_keeps_the_format_it_started_on() {
    let harness = Harness::start(SYSTEM_RGBA);
    assert!(harness.frames_arrive(5, Duration::from_secs(5)));
    assert_eq!(harness.sink_format().as_deref(), Some("NV12"));

    harness.switch_to(SYSTEM_I420);
    assert!(harness.frames_arrive(5, Duration::from_secs(5)));
    assert_eq!(
        harness.sink_format().as_deref(),
        Some("NV12"),
        "the sink's format changed"
    );
    assert_eq!(harness.spliced(), ["videoconvert", "capsfilter"]);

    let errors = harness.errors();
    assert!(errors.is_empty(), "the flow failed: {:?}", errors);
}

/// The same the other way: a sink that started on I420 is kept on I420 when
/// RGBA arrives, not moved to NV12.
#[test]
fn a_sink_that_started_on_i420_stays_on_i420() {
    let harness = Harness::start(SYSTEM_I420);
    assert!(harness.frames_arrive(5, Duration::from_secs(5)));
    assert!(harness.spliced().is_empty());

    harness.switch_to(SYSTEM_RGBA);
    assert!(harness.frames_arrive(5, Duration::from_secs(5)));
    assert_eq!(
        harness.sink_format().as_deref(),
        Some("I420"),
        "the sink's format changed"
    );

    let errors = harness.errors();
    assert!(errors.is_empty(), "the flow failed: {:?}", errors);
}
