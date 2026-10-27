//! `builtin.ndi_input` emits NDI's video as it arrives (#969).
//!
//! The block used to put a converter chosen from the global video convert
//! mode between `ndisrcdemux` and its `video/x-raw` capsfilter. NDI delivers
//! raw video in system memory and the capsfilter names no format, so the
//! converter only passed through or converted on a consumer's behalf. Each
//! consumer adapts its own input (BLOCK_GUIDELINES), so the demuxer's video
//! pad now links straight to the capsfilter.
//!
//! **CI does not run the real `ndisrc` or `ndisrcdemux`.** They need the NDI
//! SDK runtime. Where they are missing, stand-ins registered under their
//! names (with the properties the block sets) let the block build. The check
//! is structural: it reads the block's elements and internal links, and
//! fails when a converter comes back between the demuxer and the capsfilter.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::builtin::ndi::NDIInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::element::ElementPadRef;
use strom_types::PropertyValue;

/// Stand-ins for `ndisrc` and `ndisrcdemux`, registered only where the real
/// ones are missing, so the block can be built. Never linked or run.
mod stand_ins {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;
    use std::sync::{LazyLock, Mutex};

    mod imp {
        use super::*;

        /// `ndisrc`: the properties the block sets, stored and nothing more.
        #[derive(Default)]
        pub struct NdiSrcStandIn {
            bandwidth: Mutex<i32>,
            timeout: Mutex<u32>,
            connect_timeout: Mutex<u32>,
            ndi_name: Mutex<Option<String>>,
            url_address: Mutex<Option<String>>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for NdiSrcStandIn {
            const NAME: &'static str = "StromTestNdiSrcStandIn";
            type Type = super::NdiSrcStandIn;
            type ParentType = gst::Element;
        }

        impl ObjectImpl for NdiSrcStandIn {
            fn properties() -> &'static [glib::ParamSpec] {
                static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                    vec![
                        glib::ParamSpecInt::builder("bandwidth")
                            .minimum(-10)
                            .maximum(100)
                            .default_value(100)
                            .build(),
                        glib::ParamSpecUInt::builder("timeout").build(),
                        glib::ParamSpecUInt::builder("connect-timeout").build(),
                        glib::ParamSpecString::builder("ndi-name").build(),
                        glib::ParamSpecString::builder("url-address").build(),
                    ]
                });
                PROPS.as_ref()
            }

            fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
                match pspec.name() {
                    "bandwidth" => *self.bandwidth.lock().unwrap() = value.get().unwrap(),
                    "timeout" => *self.timeout.lock().unwrap() = value.get().unwrap(),
                    "connect-timeout" => {
                        *self.connect_timeout.lock().unwrap() = value.get().unwrap()
                    }
                    "ndi-name" => *self.ndi_name.lock().unwrap() = value.get().unwrap(),
                    _ => *self.url_address.lock().unwrap() = value.get().unwrap(),
                }
            }

            fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
                match pspec.name() {
                    "bandwidth" => self.bandwidth.lock().unwrap().to_value(),
                    "timeout" => self.timeout.lock().unwrap().to_value(),
                    "connect-timeout" => self.connect_timeout.lock().unwrap().to_value(),
                    "ndi-name" => self.ndi_name.lock().unwrap().to_value(),
                    _ => self.url_address.lock().unwrap().to_value(),
                }
            }
        }

        impl GstObjectImpl for NdiSrcStandIn {}

        impl ElementImpl for NdiSrcStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "ndisrc stand-in",
                        "Source/Network",
                        "Test stand-in for the NDI source",
                        "Strom tests",
                    )
                });
                Some(&META)
            }
        }

        /// `ndisrcdemux`: no properties, no pads.
        #[derive(Default)]
        pub struct NdiSrcDemuxStandIn;

        #[glib::object_subclass]
        impl ObjectSubclass for NdiSrcDemuxStandIn {
            const NAME: &'static str = "StromTestNdiSrcDemuxStandIn";
            type Type = super::NdiSrcDemuxStandIn;
            type ParentType = gst::Element;
        }

        impl ObjectImpl for NdiSrcDemuxStandIn {}
        impl GstObjectImpl for NdiSrcDemuxStandIn {}

        impl ElementImpl for NdiSrcDemuxStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "ndisrcdemux stand-in",
                        "Demuxer",
                        "Test stand-in for the NDI demuxer",
                        "Strom tests",
                    )
                });
                Some(&META)
            }
        }
    }

    glib::wrapper! {
        pub struct NdiSrcStandIn(ObjectSubclass<imp::NdiSrcStandIn>)
            @extends gst::Element, gst::Object;
    }

    glib::wrapper! {
        pub struct NdiSrcDemuxStandIn(ObjectSubclass<imp::NdiSrcDemuxStandIn>)
            @extends gst::Element, gst::Object;
    }

    /// Register the stand-ins where the real elements are missing.
    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            gst::init().expect("GStreamer initialises");
            if gst::ElementFactory::find("ndisrc").is_none() {
                gst::Element::register(
                    None,
                    "ndisrc",
                    gst::Rank::NONE,
                    NdiSrcStandIn::static_type(),
                )
                .expect("register the ndisrc stand-in");
            }
            if gst::ElementFactory::find("ndisrcdemux").is_none() {
                gst::Element::register(
                    None,
                    "ndisrcdemux",
                    gst::Rank::NONE,
                    NdiSrcDemuxStandIn::static_type(),
                )
                .expect("register the ndisrcdemux stand-in");
            }
        });
    }
}

const INSTANCE: &str = "ndiin";

/// Build the block in `mode` and check its video branch: the demuxer's video
/// pad feeds `capsfilter` (the block's video output) directly, and no
/// converter is built.
///
/// The global video convert mode is forced to the GPU one first, so a block
/// that reads it builds `autovideoconvert` instead of panicking.
fn check(mode: &str, capsfilter: &str) {
    stand_ins::register();
    assert_eq!(
        strom::gpu::force_video_convert_mode(strom::gpu::VideoConvertMode::GpuAccelerated),
        strom::gpu::VideoConvertMode::GpuAccelerated,
        "the convert mode was set before this test could force it"
    );
    let props = HashMap::from([
        ("mode".to_string(), PropertyValue::String(mode.into())),
        (
            "ndi_name".to_string(),
            PropertyValue::String("strom-test".into()),
        ),
    ]);
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let built = NDIInputBuilder
        .build(INSTANCE, &props, &ctx)
        .expect("ndi input block builds");

    let converters: Vec<String> = built
        .elements
        .iter()
        .filter_map(|(id, e)| {
            let factory = e.factory()?.name().to_string();
            (factory == "videoconvert" || factory == "autovideoconvert")
                .then(|| format!("{id} ({factory})"))
        })
        .collect();
    assert!(
        converters.is_empty(),
        "NDI Input ({mode}) builds a converter on its video: {converters:?}; the block \
         must emit NDI's video as it arrives and leave conversion to the consumer"
    );

    let demux_video = ElementPadRef::pad(format!("{INSTANCE}:ndisrcdemux"), "video");
    let output = ElementPadRef::pad(format!("{INSTANCE}:{capsfilter}"), "sink");
    let feeds: Vec<String> = built
        .internal_links
        .iter()
        .filter(|(from, _)| *from == demux_video)
        .map(|(_, to)| to.to_string_format())
        .collect();
    assert_eq!(
        feeds,
        vec![output.to_string_format()],
        "NDI Input ({mode}): the demuxer's video pad must feed the block's video output \
         directly"
    );
}

#[test]
fn combined_mode_emits_ndi_video_as_it_arrives() {
    check("combined", "videocapsfilter");
}

#[test]
fn video_mode_emits_ndi_video_as_it_arrives() {
    check("video", "capsfilter");
}
