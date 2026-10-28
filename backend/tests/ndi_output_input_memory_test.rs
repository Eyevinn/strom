//! `builtin.ndi_output` adapts the video that arrives to what `ndisink` takes
//! (#969).
//!
//! The block used to put a converter chosen from the global video convert
//! mode (`autovideoconvert` or `videoconvert`) in front of `ndisink`. With
//! plain `videoconvert`, GL or CUDA memory passed through it (`video/x-raw(ANY)`)
//! and `ndisink`, which takes system memory only, refused it: a silent
//! `not-negotiated`. The block now has an input front that downloads GPU
//! memory from the negotiated caps (`gst::download_front`), then a plain
//! `videoconvert`.
//!
//! **CI does not run the real `ndisink`.** It needs the NDI SDK runtime, which
//! the runners do not have. These tests build the real block and leave its
//! sink out: the block's `videoconvert` feeds an `appsink` whose caps are
//! `ndisink`'s video sink template (system memory, the nine formats NDI
//! takes). Where `ndisink` is not installed, a stand-in registered under that
//! name lets the block build; it is never linked. CUDA memory comes from an
//! `appsrc` under CUDA caps with the `cudadownload` stand-in; GL memory from
//! `gltestsrc` on the GL runner.

pub mod common;

#[path = "common/cuda_download_stand_in.rs"]
mod stand_in;

/// A stand-in for `ndisink`, registered only where the real one is missing,
/// so the block can be built. It has the property the block sets in video
/// mode and the real sink template; the tests never link it.
mod ndisink_stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;
    use gstreamer_base as gst_base;
    use gstreamer_base::subclass::prelude::*;
    use std::sync::{LazyLock, Mutex};

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct NdiSinkStandIn {
            ndi_name: Mutex<Option<String>>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for NdiSinkStandIn {
            const NAME: &'static str = "StromTestNdiSinkStandIn";
            type Type = super::NdiSinkStandIn;
            type ParentType = gst_base::BaseSink;
        }

        impl ObjectImpl for NdiSinkStandIn {
            fn properties() -> &'static [glib::ParamSpec] {
                static PROPS: LazyLock<Vec<glib::ParamSpec>> =
                    LazyLock::new(|| vec![glib::ParamSpecString::builder("ndi-name").build()]);
                PROPS.as_ref()
            }

            fn set_property(&self, _id: usize, value: &glib::Value, _pspec: &glib::ParamSpec) {
                *self.ndi_name.lock().unwrap() = value.get().unwrap();
            }

            fn property(&self, _id: usize, _pspec: &glib::ParamSpec) -> glib::Value {
                self.ndi_name.lock().unwrap().to_value()
            }
        }

        impl GstObjectImpl for NdiSinkStandIn {}

        impl ElementImpl for NdiSinkStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "ndisink stand-in",
                        "Sink/Video",
                        "Test stand-in for the NDI sink",
                        "Strom tests",
                    )
                });
                Some(&META)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let caps: gst::Caps = super::super::NDI_VIDEO.parse().unwrap();
                    vec![gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap()]
                });
                TEMPLATES.as_ref()
            }
        }

        impl BaseSinkImpl for NdiSinkStandIn {}
    }

    glib::wrapper! {
        pub struct NdiSinkStandIn(ObjectSubclass<imp::NdiSinkStandIn>)
            @extends gst_base::BaseSink, gst::Element, gst::Object;
    }

    /// Register the stand-in as `ndisink` where the real one is missing.
    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            gst::init().expect("GStreamer initialises");
            if gst::ElementFactory::find("ndisink").is_some() {
                return;
            }
            gst::Element::register(
                None,
                "ndisink",
                gst::Rank::NONE,
                NdiSinkStandIn::static_type(),
            )
            .expect("register the ndisink stand-in");
        });
    }
}

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::ndi::NDIOutputBuilder;
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const INSTANCE: &str = "ndi";

/// `ndisink`'s video sink template: system memory, the formats NDI takes.
const NDI_VIDEO: &str =
    "video/x-raw, format={ UYVY, I420, NV12, NV21, YV12, BGRA, BGRx, RGBA, RGBx }";

const REQUIRED: &[&str] = &["appsrc", "appsink", "identity", "videoconvert"];

const CUDA_NV12: &str =
    "video/x-raw(memory:CUDAMemory),format=NV12,width=320,height=240,framerate=25/1";
const NVMM_NV12: &str = "video/x-raw(memory:NVMM),format=NV12,width=320,height=240,framerate=25/1";
/// A system-memory format NDI does not take.
const SYSTEM_Y42B: &str = "video/x-raw,format=Y42B,width=320,height=240,framerate=25/1";

/// Build the block (video mode) into `pipeline`, its sink left out, and link
/// its `videoconvert` to an `appsink` with `ndisink`'s caps. Returns the
/// block's declared video input and the appsink.
///
/// The block's converter must be plain `videoconvert`, never
/// `autovideoconvert`.
fn add_block(pipeline: &gst::Pipeline) -> (gst::Pad, gst_app::AppSink) {
    ndisink_stand_in::register();
    let props = HashMap::from([
        ("mode".to_string(), PropertyValue::String("video".into())),
        (
            "ndi_name".to_string(),
            PropertyValue::String("strom-test".into()),
        ),
    ]);
    let built = NDIOutputBuilder
        .build(INSTANCE, &props, &common::block::context())
        .expect("ndi output block builds");
    let sink_id = format!("{}:ndisink", INSTANCE);
    let by_id = common::block::install_except(pipeline, &built, &[sink_id.as_str()]);

    let converter = by_id[&format!("{}:videoconvert", INSTANCE)].clone();
    assert_eq!(
        converter.factory().map(|f| f.name().to_string()).as_deref(),
        Some("videoconvert"),
        "the block built another converter in front of ndisink; it must be plain \
         videoconvert, with GPU memory left to the input front (#969)"
    );

    let appsink = gst_app::AppSink::builder()
        .caps(&NDI_VIDEO.parse::<gst::Caps>().unwrap())
        .sync(false)
        .build();
    pipeline.add(&appsink).unwrap();
    converter
        .link(&appsink)
        .expect("the block's converter links to the NDI caps");

    let pads = NDIOutputBuilder
        .get_external_pads(&props)
        .expect("ndi output pads");
    let video_in = pads
        .inputs
        .iter()
        .find(|p| p.name == "video_in")
        .expect("a video input");
    let block_in = by_id[&format!("{}:{}", INSTANCE, video_in.internal_element_id)]
        .static_pad(&video_in.internal_pad_name)
        .expect("block input pad");
    (block_in, appsink)
}

/// The caps of the first sample that reached the stand-in for `ndisink`, or
/// `None` when no sample arrived (or it carried no caps) within `timeout`.
fn received(appsink: &gst_app::AppSink, timeout: Duration) -> Option<gst::Caps> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            return sample.caps_owned();
        }
    }
    None
}

fn errors(pipeline: &gst::Pipeline) -> Vec<(String, String)> {
    let bus = pipeline.bus().unwrap();
    std::iter::from_fn(|| bus.pop_filtered(&[gst::MessageType::Error]))
        .filter_map(|msg| match msg.view() {
            gst::MessageView::Error(err) => Some((
                msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                err.error().to_string(),
            )),
            _ => None,
        })
        .collect()
}

/// `appsrc(caps) ! [ndi output, sink left out] ! appsink(NDI caps)`, the
/// appsrc pushing frames from a thread.
struct Harness {
    pipeline: gst::Pipeline,
    appsink: gst_app::AppSink,
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
        let (block_in, appsink) = add_block(&pipeline);
        let caps: gst::Caps = caps.parse().unwrap();
        let src = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        pipeline.add(&src).unwrap();
        src.static_pad("src")
            .unwrap()
            .link(&block_in)
            .expect("the producer links into the block");
        pipeline.set_state(gst::State::Playing).unwrap();

        let size = gst_video::VideoInfo::from_caps(&caps)
            .expect("raw video caps")
            .size();
        let stop = Arc::new(AtomicBool::new(false));
        let pusher_stop = Arc::clone(&stop);
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
            appsink,
            stop,
            pusher: Some(pusher),
        })
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

fn count(pipeline: &gst::Pipeline, factory: &str) -> usize {
    pipeline
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.factory().is_some_and(|f| f.name() == factory))
        .count()
}

fn assert_ndi_takes(caps: &gst::Caps) {
    assert!(
        caps.can_intersect(&NDI_VIDEO.parse::<gst::Caps>().unwrap()),
        "ndisink would get {caps}"
    );
    assert!(
        caps.features(0)
            .is_some_and(|f| f.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)),
        "ndisink would get {caps}, not system memory"
    );
}

/// The block in video mode, sink included, reaches PLAYING before any frame
/// arrives (#1150).
///
/// Its `ndisink` was left async in video mode, unlike the combined and audio
/// modes, so it held the flow in PAUSED until it prerolled. Behind an SRT input
/// with four video tracks, two NDI Outputs prerolled, blocked, and starved the
/// other two of frames: the receiving flow stayed in PAUSED. With nothing
/// linked to its input this sink never prerolls, so the pipeline reaches
/// PLAYING only when the sink does not wait. Here the sink is the real
/// `ndisink` where installed, the stand-in (a `BaseSink`, async by default)
/// on CI.
#[test]
fn video_mode_reaches_playing_with_no_frame() {
    ndisink_stand_in::register();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let props = HashMap::from([
        ("mode".to_string(), PropertyValue::String("video".into())),
        (
            "ndi_name".to_string(),
            PropertyValue::String("strom-test-idle".into()),
        ),
    ]);
    let built = NDIOutputBuilder
        .build(INSTANCE, &props, &common::block::context())
        .expect("ndi output block builds");
    let pipeline = gst::Pipeline::new();
    common::block::install(&pipeline, &built);

    let _ = pipeline.set_state(gst::State::Playing);
    let (_, state, pending) = pipeline.state(gst::ClockTime::from_seconds(3));
    let _ = pipeline.set_state(gst::State::Null);
    assert_eq!(
        (state, pending),
        (gst::State::Playing, gst::State::VoidPending),
        "an NDI Output with no frame yet must not hold the flow out of PLAYING"
    );
}

/// CUDA memory from a hardware decoder is downloaded before NDI.
#[test]
fn cuda_memory_is_downloaded_for_ndi() {
    let Some(harness) = Harness::start(CUDA_NV12) else {
        return;
    };
    let caps = received(&harness.appsink, Duration::from_secs(15));
    let errors = errors(&harness.pipeline);
    assert!(errors.is_empty(), "pipeline errors: {:?}", errors);
    assert_ndi_takes(&caps.expect("no frame reached ndisink for CUDA input"));
    assert_eq!(
        count(
            &harness.pipeline,
            strom::gst::video_adapt::CUDA_DOWNLOAD_FACTORY
        ),
        1,
        "expected one cudadownload"
    );
}

/// A system-memory format NDI does not take is converted to one it does.
#[test]
fn a_format_ndi_does_not_take_is_converted() {
    let Some(harness) = Harness::start(SYSTEM_Y42B) else {
        return;
    };
    let caps = received(&harness.appsink, Duration::from_secs(15));
    assert_ndi_takes(&caps.expect("no frame reached ndisink for Y42B input"));
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
        errors = self::errors(&harness.pipeline);
    }
    let (source, message) = errors.first().expect("an element error");
    assert_eq!(source, &format!("{}:input", INSTANCE));
    assert!(
        message.contains("memory:NVMM") && message.contains("NDI Output"),
        "{message}"
    );
}

/// GL memory, as `vtdec` or a GPU Vision Mixer delivers it, is downloaded
/// before NDI.
#[cfg(not(target_os = "windows"))]
#[test]
#[serial_test::serial(gl)]
fn gl_memory_is_downloaded_for_ndi() {
    if !common::gl_available(&["gltestsrc", "gldownload"]) || !common::plugins_available(REQUIRED) {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let (block_in, appsink) = add_block(&pipeline);
    let src = gst::ElementFactory::make("gltestsrc")
        .property("is-live", true)
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
    pipeline.add_many([&src, &filter]).unwrap();
    src.link(&filter).unwrap();
    filter
        .static_pad("src")
        .unwrap()
        .link(&block_in)
        .expect("link the GL source into the block");
    pipeline.set_state(gst::State::Playing).unwrap();
    let caps = received(&appsink, Duration::from_secs(15));
    let errors = errors(&pipeline);
    let downloads = count(&pipeline, "gldownload");
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(errors.is_empty(), "pipeline errors: {:?}", errors);
    assert_ndi_takes(&caps.expect("no frame reached ndisink for GL input"));
    assert_eq!(downloads, 1, "expected one gldownload");
}
