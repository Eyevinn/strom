//! `stromunpremultiply`: divides colour by alpha, in place.
//!
//! `compositor` blends straight alpha only and has no operator that handles a
//! premultiplied source, so a keyed input declared premultiplied is converted
//! to straight before it reaches the CPU mixer. The GL mixer does not need
//! this element; it corrects the blend on the pad instead.
//!
//! Registration is static and idempotent; see [`register`].

use std::sync::{LazyLock, OnceLock};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_video as gst_video;
use tracing::warn;

/// The GStreamer factory name.
pub const ELEMENT_NAME: &str = "stromunpremultiply";

glib::wrapper! {
    pub struct Unpremultiply(ObjectSubclass<imp::Unpremultiply>)
        @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

/// Register the element with the GStreamer registry, once per process.
///
/// Returns whether `stromunpremultiply` is available afterwards. Callers must
/// have initialised GStreamer first.
pub fn register() -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        // Rank NONE: built by name for premultiplied keyed inputs, never
        // autoplugged.
        match gst::Element::register(
            None,
            ELEMENT_NAME,
            gst::Rank::NONE,
            Unpremultiply::static_type(),
        ) {
            Ok(()) => true,
            Err(e) => {
                warn!("could not register {}: {}", ELEMENT_NAME, e);
                false
            }
        }
    })
}

/// `LUT[(a << 8) | c]` is `c` unpremultiplied by `a`, rounded and clamped.
/// Rows for alpha 0 and 255 are never read: those pixels are skipped.
static LUT: LazyLock<Box<[u8]>> = LazyLock::new(|| {
    let mut lut = vec![0u8; 256 * 256].into_boxed_slice();
    for a in 1..256usize {
        for c in 0..256usize {
            lut[(a << 8) | c] = ((c * 255 + a / 2) / a).min(255) as u8;
        }
    }
    lut
});

/// Unpremultiply one row of packed 4-byte pixels whose alpha is at byte
/// `alpha_at` and whose colour occupies the other three.
fn unpremultiply_row(row: &mut [u8], alpha_at: usize) {
    let lut = &**LUT;
    for px in row.chunks_exact_mut(4) {
        let a = px[alpha_at] as usize;
        // Fully transparent and fully opaque pixels are identical in both
        // encodings, and they are most of any graphic.
        if a == 0 || a == 255 {
            continue;
        }
        let base = a << 8;
        for (i, c) in px.iter_mut().enumerate() {
            if i != alpha_at {
                *c = lut[base | *c as usize];
            }
        }
    }
}

mod imp {
    use super::*;
    use gstreamer::subclass::prelude::*;
    use gstreamer_base::subclass::prelude::*;
    use gstreamer_video::prelude::*;
    use gstreamer_video::subclass::prelude::*;
    use gstreamer_video::{VideoFormat, VideoFrameRef};

    #[derive(Default)]
    pub struct Unpremultiply;

    #[glib::object_subclass]
    impl ObjectSubclass for Unpremultiply {
        const NAME: &'static str = "StromUnpremultiply";
        type Type = super::Unpremultiply;
        type ParentType = gst_video::VideoFilter;
    }

    impl ObjectImpl for Unpremultiply {}

    impl GstObjectImpl for Unpremultiply {}

    impl ElementImpl for Unpremultiply {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "Unpremultiply alpha",
                    "Filter/Effect/Video",
                    "Converts premultiplied-alpha video to straight alpha in place",
                    "Strom contributors",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                // 8-bit packed RGB with alpha only. Anything else, including
                // decoded YUV-with-alpha clips, needs a converter in front.
                let caps = gst_video::VideoCapsBuilder::new()
                    .format_list([
                        VideoFormat::Bgra,
                        VideoFormat::Rgba,
                        VideoFormat::Argb,
                        VideoFormat::Abgr,
                    ])
                    .build();
                vec![
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("static sink template"),
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("static src template"),
                ]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseTransformImpl for Unpremultiply {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::AlwaysInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
    }

    impl VideoFilterImpl for Unpremultiply {
        fn transform_frame_ip(
            &self,
            frame: &mut VideoFrameRef<&mut gst::BufferRef>,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let alpha_at = match frame.format() {
                VideoFormat::Bgra | VideoFormat::Rgba => 3,
                VideoFormat::Argb | VideoFormat::Abgr => 0,
                _ => return Err(gst::FlowError::NotNegotiated),
            };
            let width = frame.width() as usize;
            let height = frame.height() as usize;
            let stride = frame.plane_stride()[0] as usize;
            let data = frame.plane_data_mut(0).map_err(|_| gst::FlowError::Error)?;
            for row in data.chunks_mut(stride).take(height) {
                unpremultiply_row(&mut row[..width * 4], alpha_at);
            }
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer_app as gst_app;

    #[test]
    fn lut_matches_the_exact_division() {
        for a in 1..255u32 {
            // Premultiplied colour never exceeds its alpha.
            for c in 0..=a {
                let expected = ((c as f64) * 255.0 / a as f64).round() as u32;
                let got = LUT[((a as usize) << 8) | c as usize] as u32;
                assert!(
                    got.abs_diff(expected) <= 1,
                    "a={a} c={c}: got {got}, expected {expected}"
                );
            }
        }
    }

    #[test]
    fn row_leaves_alpha_and_the_endpoints_alone() {
        // BGRA: half-alpha white, opaque grey, transparent black.
        let mut row = [128, 128, 128, 128, 64, 64, 64, 255, 0, 0, 0, 0];
        unpremultiply_row(&mut row, 3);
        assert_eq!(row, [255, 255, 255, 128, 64, 64, 64, 255, 0, 0, 0, 0]);

        // ARGB: alpha leads.
        let mut row = [64, 32, 16, 64];
        unpremultiply_row(&mut row, 0);
        assert_eq!(row, [64, 128, 64, 255]);
    }

    /// Runs the registered element on one premultiplied frame per format.
    #[test]
    fn element_unpremultiplies_every_supported_format() {
        gst::init().unwrap();
        assert!(register(), "element registers");

        for (format, px, expected) in [
            ("BGRA", [128u8, 64, 0, 128], [255u8, 128, 0, 128]),
            ("RGBA", [0, 64, 128, 128], [0, 128, 255, 128]),
            ("ARGB", [128, 128, 64, 0], [128, 255, 128, 0]),
            ("ABGR", [128, 0, 64, 128], [128, 0, 128, 255]),
        ] {
            let pipeline = gst::parse::launch(&format!(
                "appsrc name=src format=time caps=video/x-raw,format={format},width=4,height=2,framerate=30/1 \
                 ! {ELEMENT_NAME} ! appsink name=sink sync=false"
            ))
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
            let src = pipeline
                .by_name("src")
                .unwrap()
                .downcast::<gst_app::AppSrc>()
                .unwrap();
            let sink = pipeline
                .by_name("sink")
                .unwrap()
                .downcast::<gst_app::AppSink>()
                .unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();

            let frame: Vec<u8> = px.iter().copied().cycle().take(4 * 4 * 2).collect();
            src.push_buffer(gst::Buffer::from_slice(frame)).unwrap();
            let sample = sink
                .try_pull_sample(gst::ClockTime::from_seconds(5))
                .unwrap_or_else(|| panic!("{format}: no output"));
            let map = sample.buffer().unwrap().map_readable().unwrap();
            for out in map.chunks_exact(4) {
                assert_eq!(out, expected, "{format}");
            }
            drop(map);
            pipeline.set_state(gst::State::Null).unwrap();
        }
    }
}
