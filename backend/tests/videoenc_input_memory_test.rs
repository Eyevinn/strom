//! `builtin.videoenc` fed raw video in a memory its encoder does not take.
//!
//! Issue #1104: an MPEG-TS/SRT Input decodes with `nvh264dec` to CUDA memory.
//! The block's `autovideoconvert` turned that into GL RGBA, which `nvh264enc`
//! never negotiated from, and Live Recorder dropped the video track. The block
//! now has a plain `videoconvert` and an input front that downloads memory the
//! encoder does not list, decided from the negotiated caps.
//!
//! These build the real block through `VideoEncBuilder` with the software
//! encoder (`x264enc`, system memory only) and feed it from an `appsrc` whose
//! caps say CUDA memory. Real CUDA memory needs an NVIDIA host, so a stand-in
//! registered as `cudadownload` takes the real element's place; the frames are
//! plain system memory under CUDA caps, which the stand-in passes through. The
//! GL case runs `gltestsrc` on the GL runner (surfaceless EGL on Linux).
//!
//! The zero-copy case (CUDA into an encoder that lists CUDA memory) needs
//! `nvh264enc`, which CI does not have. A stand-in registered as `nvh264enc`
//! (CUDA or system NV12 in, one pre-encoded H.264 frame out per input frame)
//! takes its place, so the real block selects it with the hardware preference.
//!
//! The binary forces the global video convert mode to the GPU one before any
//! block is built: the block used to build `autovideoconvert` in that mode,
//! and must build plain `videoconvert` whatever the mode says. Every test here
//! checks that through [`add_block`].
//!
//! On a host with the real `cudadownload` or `nvh264enc`, the tests that need
//! a stand-in skip: they push system-memory buffers under CUDA caps, which the
//! real elements would read as CUDA memory.

pub mod common;

#[path = "common/cuda_download_stand_in.rs"]
mod stand_in;

/// A stand-in for `nvh264enc`: raw NV12 in CUDA or system memory in, the same
/// pre-encoded H.264 access unit out for each frame, with the frame's
/// timestamps. Has the `bitrate` property the block sets on NVENC encoders.
mod nvenc_stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;
    use gstreamer_base as gst_base;
    use gstreamer_base::subclass::base_transform::GenerateOutputSuccess;
    use gstreamer_base::subclass::prelude::*;
    use std::sync::{LazyLock, Mutex, OnceLock};

    /// The access unit every frame is encoded to.
    static ACCESS_UNIT: OnceLock<gst::Buffer> = OnceLock::new();

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct NvencStandIn {
            bitrate: Mutex<u32>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for NvencStandIn {
            const NAME: &'static str = "StromTestNvencStandIn";
            type Type = super::NvencStandIn;
            type ParentType = gst_base::BaseTransform;
        }

        impl ObjectImpl for NvencStandIn {
            fn properties() -> &'static [glib::ParamSpec] {
                static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                    vec![glib::ParamSpecUInt::builder("bitrate")
                        .maximum(2_000_000)
                        .build()]
                });
                PROPS.as_ref()
            }

            fn set_property(&self, _id: usize, value: &glib::Value, _pspec: &glib::ParamSpec) {
                *self.bitrate.lock().unwrap() = value.get().unwrap();
            }

            fn property(&self, _id: usize, _pspec: &glib::ParamSpec) -> glib::Value {
                self.bitrate.lock().unwrap().to_value()
            }
        }

        impl GstObjectImpl for NvencStandIn {}

        impl ElementImpl for NvencStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "nvh264enc stand-in",
                        "Codec/Encoder/Video",
                        "Test stand-in for an encoder that takes CUDA memory",
                        "Strom tests",
                    )
                });
                Some(&META)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let sink: gst::Caps =
                        "video/x-raw(memory:CUDAMemory), format=NV12; video/x-raw, format=NV12"
                            .parse()
                            .unwrap();
                    let src: gst::Caps = "video/x-h264, stream-format=byte-stream, alignment=au"
                        .parse()
                        .unwrap();
                    vec![
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &sink,
                        )
                        .unwrap(),
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &src,
                        )
                        .unwrap(),
                    ]
                });
                TEMPLATES.as_ref()
            }
        }

        impl BaseTransformImpl for NvencStandIn {
            const MODE: gst_base::subclass::BaseTransformMode =
                gst_base::subclass::BaseTransformMode::NeverInPlace;
            const PASSTHROUGH_ON_SAME_CAPS: bool = false;
            const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

            fn transform_caps(
                &self,
                direction: gst::PadDirection,
                _caps: &gst::Caps,
                filter: Option<&gst::Caps>,
            ) -> Option<gst::Caps> {
                let other = if direction == gst::PadDirection::Sink {
                    "src"
                } else {
                    "sink"
                };
                let caps = self.obj().pad_template(other)?.caps().clone();
                Some(match filter {
                    Some(f) => f.intersect_with_mode(&caps, gst::CapsIntersectMode::First),
                    None => caps,
                })
            }

            fn generate_output(&self) -> Result<GenerateOutputSuccess, gst::FlowError> {
                let Some(input) = self.take_queued_buffer() else {
                    return Ok(GenerateOutputSuccess::NoOutput);
                };
                let mut output = ACCESS_UNIT.get().ok_or(gst::FlowError::Error)?.copy();
                {
                    let output = output.get_mut().unwrap();
                    output.set_pts(input.pts());
                    output.set_dts(input.pts());
                    output.set_duration(input.duration());
                }
                Ok(GenerateOutputSuccess::Buffer(output))
            }
        }
    }

    glib::wrapper! {
        pub struct NvencStandIn(ObjectSubclass<imp::NvencStandIn>)
            @extends gst_base::BaseTransform, gst::Element, gst::Object;
    }

    /// One H.264 High profile IDR access unit with SPS and PPS, from x264enc.
    fn encode_one_frame() -> gst::Buffer {
        let pipeline = gst::parse::launch(
            "videotestsrc num-buffers=1 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 \
             ! x264enc key-int-max=1 \
             ! video/x-h264,profile=high,stream-format=byte-stream,alignment=au \
             ! appsink name=out sync=false",
        )
        .expect("encoder pipeline")
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let sample = sink.pull_sample().expect("an encoded frame");
        pipeline.set_state(gst::State::Null).unwrap();
        sample.buffer_owned().expect("buffer")
    }

    /// Make `nvh264enc` the stand-in, at a rank the block selects. Returns
    /// true when the stand-in is the `nvh264enc` in the registry; false on a
    /// host with the real one.
    pub fn register() -> bool {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            gst::init().expect("GStreamer initialises");
            if gst::ElementFactory::find("nvh264enc").is_some() {
                return;
            }
            let _ = ACCESS_UNIT.set(encode_one_frame());
            gst::Element::register(
                None,
                "nvh264enc",
                gst::Rank::PRIMARY,
                NvencStandIn::static_type(),
            )
            .expect("register the nvh264enc stand-in");
        });
        gst::ElementFactory::find("nvh264enc")
            .is_some_and(|f| f.element_type() == NvencStandIn::static_type())
    }
}

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::videoenc::VideoEncBuilder;
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const INSTANCE: &str = "venc";

const REQUIRED: &[&str] = &[
    "appsrc",
    "identity",
    "capsfilter",
    "videoconvert",
    "x264enc",
    "h264parse",
    "fakesink",
];

const CUDA_NV12: &str =
    "video/x-raw(memory:CUDAMemory),format=NV12,width=320,height=240,framerate=25/1";
const SYSTEM_NV12: &str = "video/x-raw,format=NV12,width=320,height=240,framerate=25/1";
const NVMM_NV12: &str = "video/x-raw(memory:NVMM),format=NV12,width=320,height=240,framerate=25/1";
const D3D12_NV12: &str =
    "video/x-raw(memory:D3D12Memory),format=NV12,width=320,height=240,framerate=25/1";

/// Build the block into `pipeline` and return its declared input and output
/// pads. `preference` is the block's `encoder_preference`: `software` gives
/// `x264enc`, which takes system memory only, on every host.
///
/// The global video convert mode is forced to the GPU one first, and the
/// block's converter must still be plain `videoconvert`.
fn add_block(pipeline: &gst::Pipeline, preference: &str) -> (gst::Pad, gst::Pad) {
    assert_eq!(
        strom::gpu::force_video_convert_mode(strom::gpu::VideoConvertMode::GpuAccelerated),
        strom::gpu::VideoConvertMode::GpuAccelerated,
        "the convert mode was set before this test could force it"
    );
    let props = HashMap::from([
        ("codec".to_string(), PropertyValue::String("h264".into())),
        (
            "encoder_preference".to_string(),
            PropertyValue::String(preference.into()),
        ),
    ]);
    let built = VideoEncBuilder
        .build(INSTANCE, &props, &common::block::context())
        .expect("videoenc block builds");
    let by_id = common::block::install(pipeline, &built);

    let converter = by_id[&format!("{}:videoconvert", INSTANCE)]
        .factory()
        .map(|f| f.name().to_string());
    assert_eq!(
        converter.as_deref(),
        Some("videoconvert"),
        "in the GPU convert mode the block built {:?} in front of its encoder; \
         autovideoconvert turns CUDA memory into GL RGBA nvh264enc cannot take (#1104)",
        converter
    );

    // The block's declared external pads, not the element names they point
    // at today.
    let definition = strom::blocks::builtin::videoenc::get_blocks()
        .into_iter()
        .next()
        .expect("videoenc definition");
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

/// Factories of the elements in `pipeline`.
fn factories(pipeline: &gst::Pipeline) -> Vec<String> {
    pipeline
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| e.factory().map(|f| f.name().to_string()))
        .collect()
}

fn frame_size(caps: &gst::Caps) -> usize {
    gst_video::VideoInfo::from_caps(caps)
        .expect("raw video caps")
        .size()
}

/// `appsrc ! [videoenc block] ! fakesink`, the appsrc pushing frames from a
/// thread in caps that can change mid-stream.
struct Harness {
    pipeline: gst::Pipeline,
    block_out: gst::Pad,
    encoded: Arc<AtomicU64>,
    next_caps: Arc<Mutex<Option<gst::Caps>>>,
    stop: Arc<AtomicBool>,
    pusher: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// `None`, after saying why, when the test needs the `cudadownload`
    /// stand-in and the real element is installed.
    fn start(caps: &str) -> Option<Self> {
        Self::start_with(caps, "software")
    }

    fn start_with(caps: &str, preference: &str) -> Option<Self> {
        common::require_elements(REQUIRED);
        if !stand_in::register() && caps.contains("memory:CUDAMemory") {
            eprintln!(
                "SKIP: the real cudadownload is installed; these buffers are not CUDA memory"
            );
            return None;
        }

        let pipeline = gst::Pipeline::new();
        let (block_in, block_out) = add_block(&pipeline, preference);
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

        let encoded = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&encoded);
        block_out.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
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
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        Some(Harness {
            pipeline,
            block_out,
            encoded,
            next_caps,
            stop,
            pusher: Some(pusher),
        })
    }

    /// Wait until `n` more encoded frames have left the block, or `timeout`
    /// passes.
    fn encodes(&self, n: u64, timeout: Duration) -> bool {
        let target = self.encoded.load(Ordering::Relaxed) + n;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.encoded.load(Ordering::Relaxed) >= target {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn switch_to(&self, caps: &str) {
        *self.next_caps.lock().unwrap() = Some(caps.parse().unwrap());
    }

    fn output_media(&self) -> Option<String> {
        self.block_out
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
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
        factories(&self.pipeline)
            .iter()
            .filter(|f| f.as_str() == strom::gst::video_adapt::CUDA_DOWNLOAD_FACTORY)
            .count()
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

/// The #1104 case on a host whose encoder takes system memory only: CUDA
/// memory from a hardware decoder is downloaded and encoded.
#[test]
fn cuda_memory_the_encoder_does_not_take_is_downloaded_and_encoded() {
    let Some(harness) = Harness::start(CUDA_NV12) else {
        return;
    };
    let encoded = harness.encodes(10, Duration::from_secs(15));
    let errors = harness.errors();
    assert!(errors.is_empty(), "pipeline errors: {:?}", errors);
    assert!(encoded, "no encoded frames left the block for CUDA input");
    assert_eq!(harness.output_media().as_deref(), Some("video/x-h264"));
    assert_eq!(harness.downloads(), 1, "expected one cudadownload");
}

/// A producer that switches from CUDA to system memory gets its download
/// taken out again, and keeps encoding.
#[test]
fn a_switch_to_system_memory_removes_the_download() {
    let Some(harness) = Harness::start(CUDA_NV12) else {
        return;
    };
    assert!(harness.encodes(5, Duration::from_secs(15)), "CUDA input");
    assert_eq!(harness.downloads(), 1);
    harness.switch_to(SYSTEM_NV12);
    assert!(
        harness.encodes(20, Duration::from_secs(15)),
        "frames stopped after the switch to system memory: {:?}",
        harness.errors()
    );
    assert_eq!(harness.downloads(), 0, "the download was left in");
    assert!(harness.errors().is_empty());
}

/// Memory nothing here downloads fails the flow, naming the memory and the
/// encoder, instead of a bare not-negotiated.
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
        message.contains("memory:NVMM") && message.contains("x264enc"),
        "{message}"
    );
}

/// A download that is not installed fails the flow, naming it and its plugin.
/// `d3d12download` exists on Windows only.
#[test]
fn a_missing_download_fails_the_flow_naming_it() {
    common::require_elements(REQUIRED);
    if strom::gst::video_adapt::factory_available(strom::gst::video_adapt::D3D12_DOWNLOAD_FACTORY) {
        eprintln!("SKIP: d3d12download is installed here");
        return;
    }
    let Some(harness) = Harness::start(D3D12_NV12) else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut errors = Vec::new();
    while errors.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        errors = harness.errors();
    }
    let (_, message, debug) = errors.first().expect("an element error");
    assert!(
        message.contains("D3D12 memory") && message.contains("d3d12download"),
        "{message}"
    );
    assert!(debug.contains("d3d12 plugin"), "{debug}");
}

/// The zero-copy path (#1104 on an NVIDIA host): CUDA memory into an encoder
/// that lists CUDA memory reaches it through the block untouched, with no
/// download and no conversion. The `nvh264enc` stand-in plays the encoder.
#[test]
fn cuda_memory_the_encoder_lists_passes_straight_through() {
    common::require_elements(&["x264enc", "appsink"]);
    if !nvenc_stand_in::register() {
        eprintln!("SKIP: the real nvh264enc is installed; these buffers are not CUDA memory");
        return;
    }
    let Some(harness) = Harness::start_with(CUDA_NV12, "hardware") else {
        return;
    };
    let encoded = harness.encodes(10, Duration::from_secs(15));
    let errors = harness.errors();
    assert!(errors.is_empty(), "pipeline errors: {:?}", errors);
    assert!(encoded, "no encoded frames left the block for CUDA input");
    let encoder = harness
        .pipeline
        .by_name(&format!("{}:encoder", INSTANCE))
        .expect("the block's encoder");
    assert_eq!(
        encoder.factory().map(|f| f.name().to_string()).as_deref(),
        Some("nvh264enc")
    );
    let negotiated = encoder
        .static_pad("sink")
        .and_then(|p| p.current_caps())
        .expect("the encoder negotiated");
    assert!(
        negotiated
            .features(0)
            .is_some_and(|f| f.contains("memory:CUDAMemory")),
        "the encoder got {negotiated}, not the CUDA memory it lists"
    );
    assert_eq!(
        harness.downloads(),
        0,
        "CUDA memory the encoder takes was downloaded"
    );
}

/// System memory into the block in the GPU convert mode: no download, and the
/// converter is plain `videoconvert` (checked in [`add_block`]).
#[test]
fn system_memory_is_encoded_through_plain_videoconvert() {
    let Some(harness) = Harness::start(SYSTEM_NV12) else {
        return;
    };
    assert!(
        harness.encodes(10, Duration::from_secs(15)),
        "no encoded frames: {:?}",
        harness.errors()
    );
    assert_eq!(harness.output_media().as_deref(), Some("video/x-h264"));
    assert_eq!(harness.downloads(), 0);
}

/// GL memory into the block, as `vtdec` on macOS or a GPU Vision Mixer
/// delivers it. `Asks`: linked as a flow links it, the producer negotiating
/// with the block. `DecidesAlone`: the queue in front of the block answers
/// every caps query itself, as `decodebin` does for a decoder whose pad is not
/// linked yet, so the CAPS event reaches the block without it being asked.
#[cfg(not(target_os = "windows"))]
mod gl {
    use super::*;

    const GL_REQUIRED: &[&str] = &["gltestsrc", "gldownload"];
    const GL_RGBA: &str =
        "video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240, framerate=30/1";

    fn answer_any_caps(queue: &gst::Element) {
        queue.static_pad("sink").unwrap().add_probe(
            gst::PadProbeType::QUERY_DOWNSTREAM,
            |_, info| {
                let Some(gst::PadProbeData::Query(ref mut query)) = info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                match query.view_mut() {
                    gst::QueryViewMut::Caps(q) => {
                        q.set_result(&q.filter_owned().unwrap_or_else(gst::Caps::new_any));
                        gst::PadProbeReturn::Handled
                    }
                    gst::QueryViewMut::AcceptCaps(q) => {
                        q.set_result(true);
                        gst::PadProbeReturn::Handled
                    }
                    _ => gst::PadProbeReturn::Ok,
                }
            },
        );
    }

    /// `gltestsrc ! capsfilter(GL RGBA) ! queue ! [videoenc] ! fakesink`, run
    /// to EOS. Returns the encoded buffer count, the output media type and the
    /// number of `gldownload` elements.
    fn run(decides_alone: bool) -> (u64, Option<String>, usize) {
        let pipeline = gst::Pipeline::new();
        let (block_in, block_out) = add_block(&pipeline, "software");
        let src = gst::ElementFactory::make("gltestsrc")
            .property("num-buffers", 30i32)
            .build()
            .unwrap();
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", GL_RGBA.parse::<gst::Caps>().unwrap())
            .build()
            .unwrap();
        let queue = gst::ElementFactory::make("queue").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &filter, &queue, &sink]).unwrap();
        if decides_alone {
            answer_any_caps(&queue);
        }
        queue
            .static_pad("src")
            .unwrap()
            .link(&block_in)
            .expect("link into the block");
        block_out
            .link(&sink.static_pad("sink").unwrap())
            .expect("link out of the block");
        gst::Element::link_many([&src, &filter, &queue]).expect("link producer");

        let encoded = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&encoded);
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
        let media = block_out
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()));
        let downloads = factories(&pipeline)
            .iter()
            .filter(|f| f.as_str() == "gldownload")
            .count();
        pipeline.set_state(gst::State::Null).unwrap();
        if let gst::MessageView::Error(e) = msg.view() {
            panic!("pipeline error: {} ({:?})", e.error(), e.debug());
        }
        (encoded.load(Ordering::Relaxed), media, downloads)
    }

    #[test]
    fn gl_memory_from_a_producer_that_asks_is_downloaded_and_encoded() {
        if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
            return;
        }
        let (encoded, media, downloads) = run(false);
        assert_eq!(media.as_deref(), Some("video/x-h264"));
        assert!(encoded > 0, "no encoded buffers out of the block");
        assert_eq!(downloads, 1, "expected one gldownload");
    }

    #[test]
    fn gl_memory_decided_without_asking_is_downloaded_and_encoded() {
        if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
            return;
        }
        let (encoded, media, downloads) = run(true);
        assert_eq!(media.as_deref(), Some("video/x-h264"));
        assert!(encoded > 0, "no encoded buffers out of the block");
        assert_eq!(downloads, 1, "expected one gldownload");
    }
}
