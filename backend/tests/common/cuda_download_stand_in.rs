//! A stand-in for the nvcodec `cudadownload` element, for tests that drive a
//! real adapter chain on a host without NVIDIA hardware.
//!
//! Registered as `cudadownload`: CUDA or system memory in, GL or system memory
//! out, buffers passed through unchanged. Include it with
//! `#[path = "common/cuda_download_stand_in.rs"] mod stand_in;`
//! in the test binaries that need it, so the others do not build it.

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
    pub struct CudaDownloadStandIn;

    #[glib::object_subclass]
    impl ObjectSubclass for CudaDownloadStandIn {
        const NAME: &'static str = "StromTestCudaDownloadStandIn";
        type Type = super::CudaDownloadStandIn;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for CudaDownloadStandIn {}
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
                let src: gst::Caps = "video/x-raw(memory:GLMemory); video/x-raw".parse().unwrap();
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

    impl BaseTransformImpl for CudaDownloadStandIn {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::AlwaysInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        /// The same video on the other side, in the memory types that side
        /// takes: CUDA becomes GL or system memory, and back.
        fn transform_caps(
            &self,
            direction: gst::PadDirection,
            caps: &gst::Caps,
            filter: Option<&gst::Caps>,
        ) -> Option<gst::Caps> {
            let features: &[&str] = if direction == gst::PadDirection::Sink {
                &["memory:GLMemory", gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY]
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
    pub struct CudaDownloadStandIn(ObjectSubclass<imp::CudaDownloadStandIn>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

/// Make `cudadownload` available for the rest of the process. On an
/// NVIDIA host the real element is left in place and used.
///
/// Registered once and never removed: `Registry::remove_feature` frees the
/// factory while the element class keeps a plain pointer to it, so a later
/// lookup or registration under the same name reads freed memory.
pub fn register() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gst::init().expect("GStreamer initialises");
        if gst::ElementFactory::find(strom::gst::video_adapt::CUDA_DOWNLOAD_FACTORY).is_some() {
            return;
        }
        gst::Element::register(
            None,
            strom::gst::video_adapt::CUDA_DOWNLOAD_FACTORY,
            gst::Rank::NONE,
            CudaDownloadStandIn::static_type(),
        )
        .expect("register the cudadownload stand-in");
    });
}
