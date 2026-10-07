//! Regression tests: CUDA memory into a GPU Vision Mixer input that has had a
//! `videoconvert` spliced in for a format `glupload` does not take (#674).
//!
//! The converter converts on the CPU. When the same input later gets CUDA
//! memory, the converter must come out and `cudadownload` (CUDA-GL interop)
//! must go in directly in front of `glupload`, so the frames never take a CPU
//! round trip. And while the converter is in, the input's answer to a CAPS
//! query must still offer CUDA memory ahead of the converter's formats, so a
//! decoder upstream picks CUDA memory rather than system memory.
//!
//! On a host without nvcodec, a stand-in registered as `cudadownload` takes
//! its place and frames labelled as CUDA memory come from an `appsrc` (see
//! `stand_in`). The NVDEC test needs real hardware and is ignored by default.

pub mod common;
pub mod vision_mixer_input_format;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::gst::gl_input_front::CUDA_ADAPTER_FACTORY;
use strom_types::{Flow, PropertyValue as PV};
use vision_mixer_input_format::*;

/// A stand-in for `cudadownload`: CUDA or system memory in, system memory
/// out, buffers passed through. The stand-in's frames are system memory
/// labelled as CUDA memory, so it hands them to `glupload` as what they are.
mod stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;
    use gstreamer_base as gst_base;
    use gstreamer_base::subclass::prelude::*;

    mod imp {
        use super::*;
        use std::sync::LazyLock;

        #[derive(Default)]
        pub struct CudaToSystemStandIn;

        #[glib::object_subclass]
        impl ObjectSubclass for CudaToSystemStandIn {
            const NAME: &'static str = "StromTestCudaToSystemStandIn";
            type Type = super::CudaToSystemStandIn;
            type ParentType = gst_base::BaseTransform;
        }

        impl ObjectImpl for CudaToSystemStandIn {}
        impl GstObjectImpl for CudaToSystemStandIn {}

        impl ElementImpl for CudaToSystemStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "cudadownload stand-in",
                        "Filter/Video",
                        "Test stand-in for the nvcodec cudadownload element",
                        "Strom tests",
                    )
                });
                Some(&META)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let sink: gst::Caps = "video/x-raw(memory:CUDAMemory); video/x-raw"
                        .parse()
                        .unwrap();
                    let src: gst::Caps = "video/x-raw".parse().unwrap();
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

        impl BaseTransformImpl for CudaToSystemStandIn {
            const MODE: gst_base::subclass::BaseTransformMode =
                gst_base::subclass::BaseTransformMode::AlwaysInPlace;
            const PASSTHROUGH_ON_SAME_CAPS: bool = false;
            const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

            fn transform_caps(
                &self,
                direction: gst::PadDirection,
                caps: &gst::Caps,
                filter: Option<&gst::Caps>,
            ) -> Option<gst::Caps> {
                let features: &[&str] = if direction == gst::PadDirection::Sink {
                    &[gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY]
                } else {
                    &["memory:CUDAMemory", gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY]
                };
                let mut out = gst::Caps::new_empty();
                {
                    let out = out.get_mut().unwrap();
                    for feature in features {
                        for structure in caps.iter() {
                            out.append_structure_full(
                                structure.to_owned(),
                                Some(gst::CapsFeatures::new([*feature])),
                            );
                        }
                    }
                }
                Some(match filter {
                    Some(f) => f.intersect_with_mode(&out, gst::CapsIntersectMode::First),
                    None => out,
                })
            }

            fn transform_ip(
                &self,
                _buf: &mut gst::BufferRef,
            ) -> Result<gst::FlowSuccess, gst::FlowError> {
                Ok(gst::FlowSuccess::Ok)
            }
        }
    }

    glib::wrapper! {
        pub struct CudaToSystemStandIn(ObjectSubclass<imp::CudaToSystemStandIn>)
            @extends gst_base::BaseTransform, gst::Element, gst::Object;
    }

    /// Register the stand-in as `cudadownload`, unless the real one is
    /// installed. Returns whether the stand-in is the one in use. Never
    /// removed: see `common/cuda_download_stand_in.rs` for why.
    pub fn register() -> bool {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            gst::init().expect("GStreamer initialises");
            if gst::ElementFactory::find(super::CUDA_ADAPTER_FACTORY).is_some() {
                return;
            }
            gst::Element::register(
                None,
                super::CUDA_ADAPTER_FACTORY,
                gst::Rank::NONE,
                CudaToSystemStandIn::static_type(),
            )
            .expect("register the cudadownload stand-in");
        });
        gst::ElementFactory::find(super::CUDA_ADAPTER_FACTORY)
            .is_some_and(|f| f.element_type() == CudaToSystemStandIn::static_type())
    }
}

/// What the `appsrc` pushes: white frames in AYUV64, or in NV12 labelled as
/// CUDA memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Feed {
    Ayuv64,
    Cuda,
}

fn feed_sample(feed: Feed) -> gst::Sample {
    let (caps, data): (String, Vec<u8>) = match feed {
        Feed::Ayuv64 => (
            format_caps("AYUV64"),
            // A, Y, U, V as 16-bit little endian: opaque white.
            [0xff, 0xff, 0x00, 0xeb, 0x00, 0x80, 0x00, 0x80].repeat((W * H) as usize),
        ),
        Feed::Cuda => (
            format!(
                "video/x-raw(memory:CUDAMemory),format=NV12,width={W},height={H},framerate=30/1"
            ),
            [
                vec![235u8; (W * H) as usize],
                vec![128u8; (W * H / 2) as usize],
            ]
            .concat(),
        ),
    };
    gst::Sample::builder()
        .buffer(&gst::Buffer::from_mut_slice(data))
        .caps(&caps.parse::<gst::Caps>().unwrap())
        .build()
}

/// Pushes `feed`'s frames at 30 fps from a thread until dropped.
struct Feeder {
    feed: Arc<Mutex<Feed>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Feeder {
    fn start(appsrc: gstreamer_app::AppSrc, feed: Feed) -> Feeder {
        let feed = Arc::new(Mutex::new(feed));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let feed = Arc::clone(&feed);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let sample = feed_sample(*feed.lock().unwrap());
                    if appsrc.push_sample(&sample).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(33));
                }
            })
        };
        Feeder {
            feed,
            stop,
            thread: Some(thread),
        }
    }

    fn switch(&self, feed: Feed) {
        *self.feed.lock().unwrap() = feed;
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn appsrc_flow(block_id: &str) -> Flow {
    let mut flow = Flow::new(format!("vm_input_format_{block_id}"));
    flow.blocks.push(mixer_block(block_id, Backend::Gpu));
    flow.elements.push(elem("src", "appsrc", vec![]));
    link(&mut flow, "src:src", &format!("{block_id}:video_in_0"));
    add_pgm_tap(&mut flow, block_id);
    flow
}

fn has_feature(caps: Option<gst::Caps>, feature: &str) -> bool {
    caps.as_ref()
        .and_then(|c| c.features(0))
        .is_some_and(|f| f.contains(feature))
}

fn wait_for_cuda_input(running: &Running) {
    let queue_src = running.mixer_element("queue_0").static_pad("src").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !has_feature(queue_src.current_caps(), "memory:CUDAMemory") {
        assert!(
            Instant::now() < deadline,
            "input 0 never delivered CUDA memory, it delivers {:?}",
            queue_src.current_caps()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// CUDA memory after a converted format, on one input, through the real
/// block: the converter comes out and `cudadownload` goes in right in front
/// of `glupload`, and back and forth, with PGM showing the source throughout.
/// While the converter is in, the input offers CUDA memory ahead of what only
/// the converter takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cuda_after_a_converted_format_bypasses_the_converter() {
    if !common::gl_available(GPU_ELEMENTS) {
        return;
    }
    if !stand_in::register() {
        // The real `cudadownload` cannot take the appsrc's frames, which are
        // system memory labelled as CUDA memory. The NVDEC test covers this
        // case on such a host.
        eprintln!("SKIP: the real {CUDA_ADAPTER_FACTORY} is installed; run the NVDEC test");
        return;
    }
    let block_id = "vmfmt_cuda_switch";
    let running = Running::start_with(block_id, appsrc_flow(block_id), |pipeline| {
        let appsrc = pipeline
            .by_name("src")
            .unwrap()
            .downcast::<gstreamer_app::AppSrc>()
            .unwrap();
        appsrc.set_format(gst::Format::Time);
        appsrc.set_is_live(true);
        appsrc.set_do_timestamp(true);
    });
    let appsrc = running
        .element("src")
        .downcast::<gstreamer_app::AppSrc>()
        .unwrap();
    let feeder = Feeder::start(appsrc, Feed::Ayuv64);

    running.wait_for_input_format("AYUV64");
    running.wait_for_white_pgm("the AYUV64 source", 3);
    assert_eq!(running.gpu_input_adapters(), ["videoconvert"]);

    // The answer while the converter is in: CUDA memory before anything only
    // the converter takes.
    let answer = running
        .mixer_element("queue_0")
        .static_pad("sink")
        .unwrap()
        .query_caps(None);
    let uploads = strom::gst::video_adapt::gl_upload_system_caps()
        .expect("glupload's formats")
        .clone();
    let first_cuda = answer
        .iter_with_features()
        .position(|(_, f)| f.contains("memory:CUDAMemory"))
        .unwrap_or_else(|| panic!("the input does not offer CUDA memory: {answer}"));
    let first_converted = answer
        .iter_with_features()
        .position(|(s, f)| {
            if f.contains("memory:GLMemory") || f.contains("memory:CUDAMemory") {
                return false;
            }
            let mut one = gst::Caps::new_empty();
            one.make_mut().append_structure(s.to_owned());
            !s.has_field("format") || !one.is_subset(&uploads)
        })
        .expect("the converter adds formats glupload does not take");
    assert!(
        first_cuda < first_converted,
        "the input offers what only the converter takes (entry {first_converted}) ahead of \
         CUDA memory (entry {first_cuda}): {answer}"
    );

    for (feed, adapters) in [
        (Feed::Cuda, vec!["cudadownload"]),
        (Feed::Ayuv64, vec!["videoconvert", "cudadownload"]),
        (Feed::Cuda, vec!["cudadownload"]),
    ] {
        feeder.switch(feed);
        match feed {
            Feed::Cuda => wait_for_cuda_input(&running),
            Feed::Ayuv64 => running.wait_for_input_format("AYUV64"),
        }
        running.wait_for_white_pgm(&format!("the {feed:?} source"), 15);
        assert_eq!(
            running.gpu_input_adapters(),
            adapters,
            "{feed:?}: the input's adapters are wrong"
        );
        if feed == Feed::Cuda {
            // The stand-in hands glupload the frames as they are.
            assert_eq!(running.upload_format().as_deref(), Some("NV12"));
            assert!(
                running.spliced_converters().is_empty(),
                "a removed converter is still in the pipeline: {:?}",
                running.spliced_converters()
            );
        }
    }
    drop(feeder);
    running.stop();
}

/// NVDEC into a GPU input that had a converter in: the decoder must pick GPU
/// memory, and its frames must reach `glupload` with no converter and no CPU
/// round trip.
///
/// Needs an NVIDIA GPU and the GStreamer nvcodec plugin (`nvh264enc`,
/// `nvh264dec`, `cudadownload`), so CI cannot run it. Run it on an NVIDIA
/// host with:
///
/// ```text
/// cargo test --test vision_mixer_input_format_cuda_test -- --ignored nvdec
/// ```
///
/// It fails, rather than skipping, when the nvcodec elements are missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an NVIDIA GPU with the GStreamer nvcodec plugin"]
async fn nvdec_cuda_after_a_converted_format_stays_on_the_gpu() {
    common::require_elements(&[
        "nvh264enc",
        "nvh264dec",
        "h264parse",
        "input-selector",
        CUDA_ADAPTER_FACTORY,
    ]);
    assert!(common::gl_available(GPU_ELEMENTS), "no GL context");

    let block_id = "vmfmt_nvdec";
    let mut flow = Flow::new(format!("vm_input_format_{block_id}"));
    flow.blocks.push(mixer_block(block_id, Backend::Gpu));
    let white = || {
        vec![
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ]
    };
    // Branch 0: a format glupload does not take, so the converter goes in.
    flow.elements
        .push(elem("conv_src", "videotestsrc", white()));
    flow.elements.push(elem(
        "conv_caps",
        "capsfilter",
        vec![("caps", PV::String(format_caps("GBR_10LE")))],
    ));
    // Branch 1: H.264 decoded by NVDEC, which picks its output memory from
    // what the input answers.
    flow.elements.push(elem("dec_src", "videotestsrc", white()));
    flow.elements.push(elem(
        "dec_caps",
        "capsfilter",
        vec![("caps", PV::String(format_caps("NV12")))],
    ));
    flow.elements.push(elem("enc", "nvh264enc", vec![]));
    flow.elements.push(elem("parse", "h264parse", vec![]));
    flow.elements.push(elem("dec", "nvh264dec", vec![]));
    flow.elements.push(elem("sel", "input-selector", vec![]));
    for (from, to) in [
        ("conv_src:src", "conv_caps:sink"),
        ("conv_caps:src", "sel:sink_0"),
        ("dec_src:src", "dec_caps:sink"),
        ("dec_caps:src", "enc:sink"),
        ("enc:src", "parse:sink"),
        ("parse:src", "dec:sink"),
        ("dec:src", "sel:sink_1"),
    ] {
        link(&mut flow, from, to);
    }
    link(&mut flow, "sel:src", &format!("{block_id}:video_in_0"));
    add_pgm_tap(&mut flow, block_id);

    let running = Running::start(block_id, flow);
    let sel = running.element("sel");
    sel.set_property("active-pad", sel.static_pad("sink_0").unwrap());
    running.wait_for_input_format("GBR_10LE");
    running.wait_for_white_pgm("the GBR_10LE source", 3);
    assert_eq!(running.gpu_input_adapters(), ["videoconvert"]);

    sel.set_property("active-pad", sel.static_pad("sink_1").unwrap());
    let queue_src = running.mixer_element("queue_0").static_pad("src").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while has_feature(queue_src.current_caps(), "memory:SystemMemory")
        || queue_src
            .current_caps()
            .is_some_and(|c| c.features(0).is_some_and(|f| f.is_empty()))
        || running.input_format().as_deref() == Some("GBR_10LE")
    {
        assert!(
            Instant::now() < deadline,
            "the decoder's frames never reached the input in GPU memory: {:?}",
            queue_src.current_caps()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    running.wait_for_white_pgm("the NVDEC source", 5);

    let decoded = running
        .element("dec")
        .static_pad("src")
        .unwrap()
        .current_caps();
    assert!(
        has_feature(decoded.clone(), "memory:CUDAMemory")
            || has_feature(decoded.clone(), "memory:GLMemory"),
        "nvh264dec decodes to system memory: {decoded:?}"
    );
    let uploaded = running
        .mixer_element("glupload_0")
        .static_pad("sink")
        .unwrap()
        .current_caps();
    assert!(
        has_feature(uploaded.clone(), "memory:GLMemory")
            || has_feature(uploaded.clone(), "memory:CUDAMemory"),
        "glupload receives system memory: {uploaded:?}"
    );
    assert!(
        running.spliced_converters().is_empty(),
        "a converter is still in the pipeline: {:?}",
        running.spliced_converters()
    );
    let adapters = running.gpu_input_adapters();
    assert!(
        adapters.is_empty() || adapters == [CUDA_ADAPTER_FACTORY],
        "the input runs {adapters:?}"
    );
    running.stop();
}
