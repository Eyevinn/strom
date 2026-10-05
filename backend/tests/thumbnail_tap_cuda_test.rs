//! Regression test: a thumbnail on a tee that carries CUDA memory must
//! actually produce a thumbnail.
//!
//! The thumbnail tap used to pick its branch from a caps-string check for GL
//! memory and send everything else to `videoconvertscale`. A tee carrying
//! `video/x-raw(memory:CUDAMemory)` (a Media Player decoding with nvh264dec,
//! an SRT input into a GPU Vision Mixer) then got a CPU branch whose link to
//! the tee was refused ("Pads do not have common format"). The rollback left
//! the tee's request pad behind, so every later attempt failed too: such a tee
//! never got a thumbnail. The main chain kept flowing. The tap now asks
//! `video_adapt::decide` and puts `cudadownload` in front of the CPU branch,
//! and refuses what it cannot read without attaching anything.
//!
//! These tests drive the real `ThumbnailTap` on a running pipeline. Real CUDA
//! memory needs an NVIDIA host, so a stand-in registered as `cudadownload`
//! takes the real element's place: CUDA or system memory in, GL or system
//! memory out, buffers passed through. It is not the one in
//! `vision_mixer_cuda_input_test`: that one has no chain function and forwards
//! the CAPS event unchanged, which is enough for a mixer left in NULL but not
//! for a branch that has to carry frames.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::gst::video_adapt::CUDA_DOWNLOAD_FACTORY;
use strom::gst::{ThumbnailTap, ThumbnailTapConfig};

const CUDA_CAPS: &str = "video/x-raw(memory:CUDAMemory),format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";
const NVMM_CAPS: &str = "video/x-raw(memory:NVMM),format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";

/// Elements the tap and the harness are built from. All ship with GStreamer
/// core and gst-plugins-base, which CI installs.
const ELEMENTS: &[&str] = &["appsrc", "tee", "queue", "appsink", "videoconvertscale"];

#[path = "common/cuda_download_stand_in.rs"]
mod stand_in;

/// `appsrc(caps) ! tee ! queue ! appsink(main)`, playing, with frames pushed
/// from a thread until the returned flag is set. The tee is named the way a
/// block names its thumbnail tee.
struct Harness {
    pipeline: gst::Pipeline,
    tee: gst::Element,
    main_frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    pusher: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start(caps: &str) -> Self {
        let pipeline = gst::Pipeline::new();
        let caps: gst::Caps = caps.parse().unwrap();
        let src = gst_app::AppSrc::builder()
            .name("blk:src")
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let tee = gst::ElementFactory::make("tee")
            .name("blk:tee")
            .property("allow-not-linked", true)
            .build()
            .unwrap();
        let queue = gst::ElementFactory::make("queue")
            .name("blk:main_queue")
            .build()
            .unwrap();
        let main_frames = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&main_frames);
        let sink = gst_app::AppSink::builder()
            .name("blk:main_sink")
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
            .add_many([src.upcast_ref(), &tee, &queue, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([src.upcast_ref(), &tee, &queue, sink.upcast_ref()]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let pusher_stop = Arc::clone(&stop);
        let pusher = std::thread::spawn(move || {
            let mut pts = gst::ClockTime::ZERO;
            let frame = gst::ClockTime::from_mseconds(40);
            while !pusher_stop.load(Ordering::Relaxed) {
                let mut buffer = gst::Buffer::with_size(320 * 240 * 3 / 2).unwrap();
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
            tee,
            main_frames,
            stop,
            pusher: Some(pusher),
        }
    }

    /// Wait until the tee has negotiated and the pipeline is playing.
    fn wait_until_flowing(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let caps = self.tee.static_pad("sink").unwrap().current_caps();
            if caps.is_some()
                && self.pipeline.current_state() == gst::State::Playing
                && self.main_frames.load(Ordering::Relaxed) > 0
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the main chain never started flowing"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn errors(&self) -> Vec<String> {
        let bus = self.pipeline.bus().unwrap();
        let mut errors = Vec::new();
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
        errors
    }

    /// Factory names of every element the tap added to the pipeline.
    fn branch_factories(&self) -> Vec<String> {
        self.pipeline
            .children()
            .into_iter()
            .filter(|e| e.name().contains("thumb"))
            .filter_map(|e| e.factory().map(|f| f.name().to_string()))
            .collect()
    }

    /// Whether the main chain still receives frames over the next `window`.
    fn main_keeps_flowing(&self, window: Duration) -> bool {
        let before = self.main_frames.load(Ordering::Relaxed);
        std::thread::sleep(window);
        self.main_frames.load(Ordering::Relaxed) > before
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

fn tap_config() -> ThumbnailTapConfig {
    ThumbnailTapConfig {
        update_interval: Duration::from_millis(100),
        ..ThumbnailTapConfig::default()
    }
}

/// The defect: CUDA memory on the tee. With the fix, the branch starts with
/// `cudadownload`, a thumbnail arrives, and the main chain keeps flowing;
/// without it, the tee refused the link to a `videoconvertscale` branch and
/// no thumbnail ever arrived.
#[test]
fn thumbnail_on_a_cuda_memory_tee_downloads_first_and_keeps_the_stream_flowing() {
    gst::init().unwrap();
    common::require_elements(ELEMENTS);
    stand_in::register();

    let harness = Harness::start(CUDA_CAPS);
    harness.wait_until_flowing();

    let tap = ThumbnailTap::new_with_tee(
        &harness.pipeline,
        "blk:thumb_0",
        harness.tee.clone(),
        tap_config(),
    );

    // The first request attaches the branch; a frame follows.
    let first = tap.get_thumbnail();
    assert!(
        !matches!(
            first,
            Err(strom::gst::thumbnail::ThumbnailError::UnsupportedFormat(_))
        ),
        "the tap refused CUDA memory although cudadownload is available: {:?}",
        first
    );

    let factories = harness.branch_factories();
    assert!(
        factories.iter().any(|f| f == CUDA_DOWNLOAD_FACTORY),
        "the thumbnail branch for CUDA memory has no {} (branch: {:?}, first request: {:?})",
        CUDA_DOWNLOAD_FACTORY,
        factories,
        first
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let jpeg = loop {
        if let Ok(jpeg) = tap.get_thumbnail() {
            break Some(jpeg);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let errors = harness.errors();
    assert!(errors.is_empty(), "the flow failed: {:?}", errors);
    assert!(
        harness.main_keeps_flowing(Duration::from_millis(500)),
        "the main chain stopped after the thumbnail branch was attached"
    );
    let jpeg = jpeg.expect("no thumbnail arrived from the CUDA-memory tee");
    assert!(
        jpeg.starts_with(&[0xFF, 0xD8]),
        "the thumbnail is not a JPEG"
    );
}

/// Memory the tap cannot read is refused before anything is attached, so the
/// main chain is never touched.
#[test]
fn thumbnail_on_an_unreadable_memory_tee_is_refused_without_attaching() {
    gst::init().unwrap();
    common::require_elements(ELEMENTS);

    let harness = Harness::start(NVMM_CAPS);
    harness.wait_until_flowing();

    let tap = ThumbnailTap::new_with_tee(
        &harness.pipeline,
        "blk:thumb_0",
        harness.tee.clone(),
        tap_config(),
    );

    for _ in 0..3 {
        let result = tap.get_thumbnail();
        assert!(
            matches!(
                result,
                Err(strom::gst::thumbnail::ThumbnailError::UnsupportedFormat(ref m))
                    if m.contains("memory:NVMM")
            ),
            "the tap did not refuse NVMM memory: {:?}",
            result
        );
    }
    assert!(
        harness.branch_factories().is_empty(),
        "the tap attached a branch it cannot feed: {:?}",
        harness.branch_factories()
    );

    let errors = harness.errors();
    assert!(errors.is_empty(), "the flow failed: {:?}", errors);
    assert!(
        harness.main_keeps_flowing(Duration::from_millis(300)),
        "the main chain stopped"
    );
}

/// A branch still attached when its pipeline goes away is freed with it. The
/// appsink callback used to hold the tap state strongly, and the state holds
/// the appsink: a cycle that kept the whole branch (and, on CUDA memory, the
/// `cudadownload` with its CUDA context) alive after every flow restart.
#[test]
fn an_attached_branch_is_freed_with_its_pipeline() {
    gst::init().unwrap();
    common::require_elements(ELEMENTS);

    let harness = Harness::start(
        "video/x-raw,format=NV12,width=320,height=240,framerate=25/1,\
         pixel-aspect-ratio=1/1,interlace-mode=progressive",
    );
    harness.wait_until_flowing();

    let tap = ThumbnailTap::new_with_tee(
        &harness.pipeline,
        "blk:thumb_0",
        harness.tee.clone(),
        tap_config(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while tap.get_thumbnail().is_err() {
        assert!(
            Instant::now() < deadline,
            "no thumbnail arrived from the system-memory tee"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let appsink = harness
        .pipeline
        .by_name("blk:thumb_0_thumb_sink")
        .expect("the thumbnail branch has an appsink")
        .downgrade();

    drop(harness);
    drop(tap);

    assert!(
        appsink.upgrade().is_none(),
        "the thumbnail branch outlived its pipeline"
    );
}
