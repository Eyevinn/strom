//! Regression test: a CUDA-memory producer must reach the GPU Vision Mixer (#682).
//!
//! Every video input of the GPU Vision Mixer starts with `glupload`, which does
//! not take `memory:CUDAMemory`. The Media Player decodes in a pipeline of its
//! own and re-pushes whatever its decoder produced from an `appsrc`; behind
//! `nvh264dec` that is CUDA memory and nothing else, so the clip never reached
//! the mixer. The input front now splices `cudadownload` — the CUDA-to-GL interop
//! element — in front of `glupload` when the producer offers CUDA memory only.
//!
//! These tests drive the real Media Player block into the real Vision Mixer
//! block (see `vision_mixer_cuda`). The GL plugin must be installed (CI installs
//! `gstreamer1.0-gl`); `cudadownload` is not, so a stand-in registered under
//! that name takes its place. Real CUDA memory needs an NVIDIA host. The case
//! where `cudadownload` is missing is `vision_mixer_cuda_missing_test`.

pub mod common;
pub mod vision_mixer_cuda;

use gstreamer::prelude::*;
use strom::gst::gl_input_front::CUDA_ADAPTER_FACTORY;
use vision_mixer_cuda::*;

/// A stand-in for `cudadownload` with the same pad templates: CUDA or system
/// memory in, GL or system memory out. The tests only need the element to
/// exist under that name and to accept CUDA caps on its sink pad.
mod stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;

    mod imp {
        use super::*;
        use std::sync::LazyLock;

        #[derive(Default)]
        pub struct CudaDownloadStandIn;

        #[glib::object_subclass]
        impl ObjectSubclass for CudaDownloadStandIn {
            const NAME: &'static str = "StromTestCudaDownloadStandIn";
            type Type = super::CudaDownloadStandIn;
            type ParentType = gst::Element;
        }

        impl ObjectImpl for CudaDownloadStandIn {
            fn constructed(&self) {
                self.parent_constructed();
                let obj = self.obj();
                for name in ["sink", "src"] {
                    let templ = obj.element_class().pad_template(name).unwrap();
                    obj.add_pad(&gst::Pad::from_template(&templ)).unwrap();
                }
            }
        }

        impl GstObjectImpl for CudaDownloadStandIn {}

        impl ElementImpl for CudaDownloadStandIn {
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
                    let src: gst::Caps =
                        "video/x-raw(memory:GLMemory); video/x-raw".parse().unwrap();
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
    }

    glib::wrapper! {
        pub struct CudaDownloadStandIn(ObjectSubclass<imp::CudaDownloadStandIn>)
            @extends gst::Element, gst::Object;
    }

    /// Make [`super::CUDA_ADAPTER_FACTORY`] available for the rest of the
    /// process. On an NVIDIA host the real element is left in place and used.
    ///
    /// Registered once and never removed: `Registry::remove_feature` frees the
    /// factory while the element class keeps a plain pointer to it, so a later
    /// lookup or registration under the same name reads freed memory. That
    /// crashed this binary in about one run in four. The missing-adapter case
    /// lives in its own test binary instead, where nothing is registered.
    pub fn register() {
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
                CudaDownloadStandIn::static_type(),
            )
            .expect("register the cudadownload stand-in");
        });
    }
}

/// The defect: CUDA memory from the Media Player into a mixer input. With the
/// front, `cudadownload` sits between the input queue and `glupload`, and the
/// Media Player's output negotiated; without it, nothing was inserted and the
/// Media Player failed with not-negotiated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cuda_memory_from_the_media_player_gets_cudadownload_before_glupload() {
    common::require_elements(GL_ELEMENTS);
    stand_in::register();

    for input in ["video_in_0", "dsk_in_0"] {
        let suffix = if input == "dsk_in_0" { "dsk_0" } else { "0" };
        let queue = format!("{}:queue_{}", MIXER, suffix);
        let glupload = format!("{}:glupload_{}", MIXER, suffix);

        let manager = build_manager(&player_into_mixer("cuda_into_vm", input));
        let pipeline = manager.pipeline();
        assert_eq!(
            feeder_of(pipeline, &glupload).map(|e| e.name().to_string()),
            Some(queue.clone()),
            "{}: before any caps, {} should feed {} directly",
            input,
            queue,
            glupload
        );

        let outcome = push_from_player(pipeline, CUDA_CAPS);

        let adapter = feeder_of(pipeline, &glupload).unwrap_or_else(|| {
            panic!("{}: {} lost its feeder", input, glupload);
        });
        assert_eq!(
            factory_name(&adapter),
            CUDA_ADAPTER_FACTORY,
            "{}: CUDA memory from the Media Player reached {} without a {} in \
             front of it, so it cannot negotiate",
            input,
            glupload,
            CUDA_ADAPTER_FACTORY
        );
        assert_eq!(
            feeder_of(pipeline, &adapter.name()).map(|e| e.name().to_string()),
            Some(queue.clone()),
            "{}: the inserted {} is not fed by the input queue",
            input,
            CUDA_ADAPTER_FACTORY
        );

        assert!(
            outcome.errors.is_empty(),
            "{}: the flow failed: {:?}",
            input,
            outcome.errors
        );
        assert!(
            outcome
                .negotiated
                .as_ref()
                .and_then(|c| c.features(0))
                .is_some_and(|f| f.contains("memory:CUDAMemory")),
            "{}: the Media Player's output did not negotiate CUDA memory into the mixer \
             (got {:?})",
            input,
            outcome.negotiated
        );
    }
}

/// The front costs nothing where it is not needed: system memory goes straight
/// into `glupload`, which uploads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_memory_from_the_media_player_goes_straight_into_glupload() {
    common::require_elements(GL_ELEMENTS);
    stand_in::register();

    let manager = build_manager(&player_into_mixer("system_into_vm", "video_in_0"));
    let pipeline = manager.pipeline();
    let outcome = push_from_player(pipeline, SYSTEM_CAPS);

    assert!(
        outcome.negotiated.is_some() && outcome.errors.is_empty(),
        "system memory did not negotiate into the mixer: {:?}",
        outcome.errors
    );
    assert_eq!(
        feeder_of(pipeline, &format!("{}:glupload_0", MIXER)).map(|e| e.name().to_string()),
        Some(format!("{}:queue_0", MIXER)),
        "a system-memory input was given an adapter it does not need"
    );
}
