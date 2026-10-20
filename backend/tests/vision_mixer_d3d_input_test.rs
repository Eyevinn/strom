//! Regression test: D3D12 memory from the Media Player must reach the GPU
//! Vision Mixer.
//!
//! On Windows the Media Player's `uridecodebin3` picks `d3d12h264dec`, which
//! decodes to `memory:D3D12Memory` and nothing else. The Media Player re-pushes
//! those frames from an `appsrc`, and the mixer input's `glupload` does not
//! take D3D12 memory, so an HLS stream never reached the mixer. The input
//! front now splices `d3d12download` in front of `glupload` when the producer
//! offers D3D12 memory only.
//!
//! These tests drive the real Media Player block into the real Vision Mixer
//! block (see `vision_mixer_cuda`, the harness the CUDA tests share). The GL
//! plugin must be installed (CI installs `gstreamer1.0-gl`); `d3d12download`
//! exists on Windows only, so a stand-in registered under that name takes its
//! place elsewhere. The case where it is missing is
//! `vision_mixer_d3d_missing_test`.

pub mod common;
pub mod vision_mixer_cuda;

use gstreamer::prelude::*;
use strom::gst::video_adapt::D3D12_DOWNLOAD_FACTORY;
use vision_mixer_cuda::*;

/// One frame as `d3d12h264dec` delivers it.
const D3D12_CAPS: &str = "video/x-raw(memory:D3D12Memory),format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";

/// A stand-in for `d3d12download` with its memory types: D3D12 or system
/// memory in, system memory out. The test only needs the element to exist
/// under that name and to accept D3D12 caps on its sink pad.
mod stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;

    mod imp {
        use super::*;
        use std::sync::LazyLock;

        #[derive(Default)]
        pub struct D3d12DownloadStandIn;

        #[glib::object_subclass]
        impl ObjectSubclass for D3d12DownloadStandIn {
            const NAME: &'static str = "StromTestD3d12DownloadStandIn";
            type Type = super::D3d12DownloadStandIn;
            type ParentType = gst::Element;
        }

        impl ObjectImpl for D3d12DownloadStandIn {
            fn constructed(&self) {
                self.parent_constructed();
                let obj = self.obj();
                for name in ["sink", "src"] {
                    let templ = obj.element_class().pad_template(name).unwrap();
                    obj.add_pad(&gst::Pad::from_template(&templ)).unwrap();
                }
            }
        }

        impl GstObjectImpl for D3d12DownloadStandIn {}

        impl ElementImpl for D3d12DownloadStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "d3d12download stand-in",
                        "Filter/Video",
                        "Test stand-in for the d3d12 plugin's d3d12download element",
                        "Strom tests",
                    )
                });
                Some(&META)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let sink: gst::Caps = "video/x-raw(memory:D3D12Memory); video/x-raw"
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
    }

    glib::wrapper! {
        pub struct D3d12DownloadStandIn(ObjectSubclass<imp::D3d12DownloadStandIn>)
            @extends gst::Element, gst::Object;
    }

    /// Make [`super::D3D12_DOWNLOAD_FACTORY`] available for the rest of the
    /// process. On Windows the real element is left in place and used.
    ///
    /// Registered once and never removed: `Registry::remove_feature` frees the
    /// factory while the element class keeps a plain pointer to it. The
    /// missing-adapter case lives in its own test binary instead.
    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            gst::init().expect("GStreamer initialises");
            if gst::ElementFactory::find(super::D3D12_DOWNLOAD_FACTORY).is_some() {
                return;
            }
            gst::Element::register(
                None,
                super::D3D12_DOWNLOAD_FACTORY,
                gst::Rank::NONE,
                D3d12DownloadStandIn::static_type(),
            )
            .expect("register the d3d12download stand-in");
        });
    }
}

/// The defect: D3D12 memory from the Media Player into a mixer input. With the
/// front, `d3d12download` sits between the input queue and `glupload`, and the
/// Media Player's output negotiated; without it, nothing was inserted and the
/// Media Player failed with not-negotiated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d3d12_memory_from_the_media_player_gets_d3d12download_before_glupload() {
    common::require_elements(GL_ELEMENTS);
    stand_in::register();

    for input in ["video_in_0", "dsk_in_0"] {
        let suffix = if input == "dsk_in_0" { "dsk_0" } else { "0" };
        let queue = format!("{}:queue_{}", MIXER, suffix);
        let glupload = format!("{}:glupload_{}", MIXER, suffix);

        let manager = build_manager(&player_into_mixer("d3d12_into_vm", input));
        let pipeline = manager.pipeline();
        assert_eq!(
            feeder_of(pipeline, &glupload).map(|e| e.name().to_string()),
            Some(queue.clone()),
            "{}: before any caps, {} should feed {} directly",
            input,
            queue,
            glupload
        );

        let outcome = push_from_player(pipeline, D3D12_CAPS);

        let adapter = feeder_of(pipeline, &glupload).unwrap_or_else(|| {
            panic!("{}: {} lost its feeder", input, glupload);
        });
        assert_eq!(
            factory_name(&adapter),
            D3D12_DOWNLOAD_FACTORY,
            "{}: D3D12 memory from the Media Player reached {} without a {} in \
             front of it, so it cannot negotiate",
            input,
            glupload,
            D3D12_DOWNLOAD_FACTORY
        );
        assert_eq!(
            feeder_of(pipeline, &adapter.name()).map(|e| e.name().to_string()),
            Some(queue.clone()),
            "{}: the inserted {} is not fed by the input queue",
            input,
            D3D12_DOWNLOAD_FACTORY
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
                .is_some_and(|f| f.contains("memory:D3D12Memory")),
            "{}: the Media Player's output did not negotiate D3D12 memory into the mixer \
             (got {:?})",
            input,
            outcome.negotiated
        );
    }
}
