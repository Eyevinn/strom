//! A stinger clip whose hardware decoder cannot decode it plays through a
//! software decoder.
//!
//! On a Mac whose VideoToolbox has no ProRes decoder (the virtual M1 of the
//! macOS CI runner) an autoplugger still picks `vtdec_hw` for ProRes, which
//! refuses every frame, and nothing falls back: the clip never parked. Here a
//! stand-in "hardware" PNG decoder, ranked above `pngdec`, refuses every
//! frame the same way, so the case runs on every platform. It is registered
//! for the whole process, which is why this is a test binary of its own.

pub mod common;
#[path = "common/stinger.rs"]
pub mod rig;

use gstreamer::prelude::*;
use rig::*;

mod stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;
    use gstreamer_video as gst_video;
    use gstreamer_video::subclass::prelude::*;

    mod imp {
        use super::*;
        use std::sync::LazyLock;

        #[derive(Default)]
        pub struct RefusingDecoder;

        #[glib::object_subclass]
        impl ObjectSubclass for RefusingDecoder {
            const NAME: &'static str = "StromTestRefusingHardwareDecoder";
            type Type = super::RefusingDecoder;
            type ParentType = gst_video::VideoDecoder;
        }

        impl ObjectImpl for RefusingDecoder {}
        impl GstObjectImpl for RefusingDecoder {}

        impl ElementImpl for RefusingDecoder {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "Refusing hardware PNG decoder",
                        "Codec/Decoder/Video/Hardware",
                        "Test stand-in for a hardware decoder that cannot decode its input",
                        "Strom tests",
                    )
                });
                Some(&META)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    vec![
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &gst::Caps::builder("image/png").build(),
                        )
                        .unwrap(),
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &gst::Caps::builder("video/x-raw").build(),
                        )
                        .unwrap(),
                    ]
                });
                TEMPLATES.as_ref()
            }
        }

        impl VideoDecoderImpl for RefusingDecoder {
            /// What VideoToolbox does with ProRes on a Mac without its
            /// decoder: every frame fails.
            fn handle_frame(
                &self,
                _frame: gst_video::VideoCodecFrame,
            ) -> Result<gst::FlowSuccess, gst::FlowError> {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Decode,
                    ["the stand-in hardware decoder refuses every frame"]
                );
                Err(gst::FlowError::Error)
            }
        }
    }

    glib::wrapper! {
        pub struct RefusingDecoder(ObjectSubclass<imp::RefusingDecoder>)
            @extends gst_video::VideoDecoder, gst::Element, gst::Object;
    }

    pub const FACTORY: &str = "stromtestrefusinghwpngdec";

    /// Register the stand-in above `pngdec`, so an autoplugger picks it for
    /// PNG.
    pub fn register() {
        gst::init().unwrap();
        let rank = gst::ElementFactory::find("pngdec")
            .map(|f| f.rank())
            .unwrap_or(gst::Rank::PRIMARY);
        gst::Element::register(None, FACTORY, rank + 100, RefusingDecoder::static_type()).unwrap();
    }
}

/// The analysis finds that the hardware decoder refuses the clip and decodes
/// it in software; the source parks the clip that way, and a take airs every
/// frame of it, cut where it should be.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn a_clip_its_hardware_decoder_refuses_plays_in_software() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    stand_in::register();
    // The premise: left to itself, an autoplugger picks the stand-in.
    let picked = gstreamer::ElementFactory::factories_with_type(
        gstreamer::ElementFactoryType::DECODER | gstreamer::ElementFactoryType::MEDIA_IMAGE,
        gstreamer::Rank::MARGINAL,
    )
    .into_iter()
    .chain(gstreamer::ElementFactory::factories_with_type(
        gstreamer::ElementFactoryType::DECODER | gstreamer::ElementFactoryType::MEDIA_VIDEO,
        gstreamer::Rank::MARGINAL,
    ))
    .filter(|f| f.can_sink_any_caps(&gstreamer::Caps::builder("image/png").build()))
    .max_by_key(|f| f.rank())
    .map(|f| f.name().to_string());
    assert_eq!(picked.as_deref(), Some(stand_in::FACTORY));

    // `start` waits for the first clip to park.
    let r = start("swdecode", "cpu").await;
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    let uri = strom::blocks::builtin::mediaplayer::normalize_uri(
        &s.clips[0].file,
        std::path::Path::new("/"),
    );
    assert_eq!(
        strom::stinger::analysis::software_only(&uri).as_deref(),
        Some("image/png"),
        "the analysis did not find the clip has to decode in software"
    );
    classic_take(&r).await;

    r.state.stop_flow(&r.flow_id).await.unwrap();
}
