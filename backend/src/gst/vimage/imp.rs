//! The `stromvimageconvert` element.
//!
//! A drop-in replacement for `videoconvert`/`videoconvertscale` that runs the
//! conversion through Accelerate's vImage where vImage has a direct path, and
//! through `GstVideoConverter` — the very code `videoconvert` runs — where it
//! does not. Both are chosen once per caps negotiation, so the streaming path
//! never has to ask which it is.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::subclass::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::subclass::prelude::*;
use gstreamer_video::{VideoFormat, VideoFormatInfo, VideoFrameRef, VideoInfo};

use super::convert;
use super::plan::Plan;

pub(super) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        super::ELEMENT_NAME,
        gst::DebugColorFlags::empty(),
        Some("vImage-backed video converter and scaler"),
    )
});

/// Matches `videoconvert`'s own default, so `gpu::configure_video_convert`
/// raising it off 1 means the same thing here as it does there.
const DEFAULT_N_THREADS: u32 = 1;

/// Fields a conversion is allowed to change. Everything else — framerate above
/// all — has to survive untouched, because this element cannot alter it.
const CONVERTIBLE_FIELDS: [&str; 6] = [
    "format",
    "colorimetry",
    "chroma-site",
    "width",
    "height",
    "pixel-aspect-ratio",
];

/// Value of the read-only `conversion-path` property before any caps have
/// been negotiated.
pub(super) const PATH_UNNEGOTIATED: &str = "unnegotiated";
/// Value of `conversion-path` while the vImage kernels are in use.
pub(super) const PATH_VIMAGE: &str = "vimage";
/// Value of `conversion-path` while `GstVideoConverter` is in use.
pub(super) const PATH_FALLBACK: &str = "fallback";

/// Which of the two conversion engines the negotiated caps resolved to.
enum Path {
    /// vImage has a direct call for this format pair.
    VImage(Plan),
    /// It does not, so run `GstVideoConverter` exactly as `videoconvert` would.
    Fallback(gst_video::VideoConverter),
}

impl Path {
    fn name(&self) -> &'static str {
        match self {
            Path::VImage(_) => PATH_VIMAGE,
            Path::Fallback(_) => PATH_FALLBACK,
        }
    }
}

#[derive(Default)]
pub struct VImageConvert {
    /// Thread count handed to the `GstVideoConverter` fallback. vImage runs
    /// its own internal thread pool and takes no such hint.
    n_threads: AtomicU32,
    state: Mutex<Option<Path>>,
}

#[glib::object_subclass]
impl ObjectSubclass for VImageConvert {
    const NAME: &'static str = "StromVImageConvert";
    type Type = super::VImageConvert;
    type ParentType = gst_video::VideoFilter;

    fn new() -> Self {
        Self {
            n_threads: AtomicU32::new(DEFAULT_N_THREADS),
            state: Mutex::new(None),
        }
    }
}

impl ObjectImpl for VImageConvert {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("n-threads")
                    .nick("Threads")
                    .blurb(
                        "Maximum number of threads the software fallback may use \
                         (0 = one per core). The vImage path manages its own pool.",
                    )
                    .default_value(DEFAULT_N_THREADS)
                    .mutable_ready()
                    .build(),
                // Read-only, and the only way to tell from outside which
                // engine the negotiated caps actually landed on. Without it a
                // silent fall back to GstVideoConverter looks exactly like a
                // working vImage path.
                glib::ParamSpecString::builder("conversion-path")
                    .nick("Conversion path")
                    .blurb(
                        "Which engine the negotiated caps resolved to: \
                         \"vimage\", \"fallback\", or \"unnegotiated\"",
                    )
                    .default_value(Some(PATH_UNNEGOTIATED))
                    .read_only()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "n-threads" => self
                .n_threads
                .store(value.get().expect("n-threads is a uint"), Ordering::Relaxed),
            other => unimplemented!("no such property {}", other),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "n-threads" => self.n_threads.load(Ordering::Relaxed).to_value(),
            "conversion-path" => self
                .state
                .lock()
                .unwrap()
                .as_ref()
                .map_or(PATH_UNNEGOTIATED, Path::name)
                .to_value(),
            other => unimplemented!("no such property {}", other),
        }
    }
}

impl GstObjectImpl for VImageConvert {}

impl ElementImpl for VImageConvert {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "vImage video converter and scaler",
                "Filter/Converter/Video/Scaler",
                "Converts video formats through Accelerate/vImage on macOS, \
                 falling back to GstVideoConverter for pairs vImage cannot do",
                "Strom contributors",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // The full raw format set, matching `videoconvertscale`: whatever
            // vImage declines still has the fallback behind it, so narrowing
            // the templates would only turn a slower conversion into a failed
            // link.
            let caps = gst_video::VideoCapsBuilder::new().build();
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

impl BaseTransformImpl for VImageConvert {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    /// Advertise the incoming caps unchanged first, then a copy with every
    /// convertible field dropped.
    ///
    /// Order matters: with the unchanged caps first, a peer that would accept
    /// them wins the intersection and `BaseTransform` goes passthrough instead
    /// of converting a frame into its own format.
    fn transform_caps(
        &self,
        _direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut relaxed = caps.copy();
        {
            let relaxed = relaxed.make_mut();
            for idx in 0..relaxed.size() {
                if let Some(s) = relaxed.structure_mut(idx) {
                    s.remove_fields(CONVERTIBLE_FIELDS);
                }
            }
        }

        let mut result = caps.copy();
        result.make_mut().append(relaxed);

        if let Some(filter) = filter {
            result = filter.intersect_with_mode(&result, gst::CapsIntersectMode::First);
        }
        Some(result)
    }

    /// Pin the other pad's caps, preferring no conversion at all.
    ///
    /// Format is chosen by the loss score in [`conversion_loss`]; size and
    /// pixel aspect ratio are pulled towards the input so that a stage asked
    /// only to convert does not silently rescale.
    fn fixate_caps(
        &self,
        _direction: gst::PadDirection,
        caps: &gst::Caps,
        othercaps: gst::Caps,
    ) -> gst::Caps {
        let Some(in_s) = caps.structure(0) else {
            let mut othercaps = othercaps;
            othercaps.fixate();
            return othercaps;
        };

        let in_format = in_s
            .get::<&str>("format")
            .ok()
            .and_then(|f| f.parse::<VideoFormat>().ok());

        let candidates = othercaps.to_string();
        let mut result = choose_format(&othercaps, in_format);

        {
            let result = result.make_mut();
            if let Some(s) = result.structure_mut(0) {
                fixate_int(s, "width", in_s.get::<i32>("width").ok());
                fixate_int(s, "height", in_s.get::<i32>("height").ok());
                let par = in_s
                    .get::<gst::Fraction>("pixel-aspect-ratio")
                    .unwrap_or_else(|_| gst::Fraction::new(1, 1));
                if s.has_field("pixel-aspect-ratio") {
                    s.fixate_field_nearest_fraction(
                        "pixel-aspect-ratio",
                        (par.numer(), par.denom()),
                    );
                } else {
                    s.set("pixel-aspect-ratio", par);
                }
            }
        }
        result.fixate();

        gst::debug!(
            CAT,
            imp = self,
            "fixated {} against candidates {} to {}",
            caps,
            candidates,
            result
        );
        result
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;
        Ok(())
    }
}

impl VideoFilterImpl for VImageConvert {
    fn set_info(
        &self,
        incaps: &gst::Caps,
        in_info: &VideoInfo,
        outcaps: &gst::Caps,
        out_info: &VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        let path = match Plan::build(in_info, out_info) {
            Some(plan) => {
                gst::info!(
                    CAT,
                    imp = self,
                    "vImage path ({}) for {:?} -> {:?} at {}x{}",
                    plan.describe(),
                    in_info.format(),
                    out_info.format(),
                    in_info.width(),
                    in_info.height()
                );
                Path::VImage(plan)
            }
            None => {
                let mut config = gst_video::VideoConverterConfig::new();
                config.set_threads(self.n_threads.load(Ordering::Relaxed));
                let converter = gst_video::VideoConverter::new(in_info, out_info, Some(config))
                    .map_err(|e| {
                        gst::loggable_error!(CAT, "no conversion for these caps: {}", e)
                    })?;
                gst::info!(
                    CAT,
                    imp = self,
                    "no vImage path for {:?} {}x{} -> {:?} {}x{}, using GstVideoConverter",
                    in_info.format(),
                    in_info.width(),
                    in_info.height(),
                    out_info.format(),
                    out_info.width(),
                    out_info.height()
                );
                Path::Fallback(converter)
            }
        };

        *self.state.lock().unwrap() = Some(path);
        self.parent_set_info(incaps, in_info, outcaps, out_info)
    }

    fn transform_frame(
        &self,
        inframe: &VideoFrameRef<&gst::BufferRef>,
        outframe: &mut VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let Some(path) = state.as_ref() else {
            return Err(gst::FlowError::NotNegotiated);
        };

        match path {
            Path::VImage(plan) => convert::run(plan, inframe, outframe).map_err(|err| {
                gst::error!(CAT, imp = self, "vImage conversion failed: error {}", err);
                gst::FlowError::Error
            })?,
            Path::Fallback(converter) => converter.frame_ref(inframe, outframe),
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

/// Fixate an integer field towards the input's value, adding it outright when
/// [`BaseTransformImpl::transform_caps`] dropped it.
fn fixate_int(s: &mut gst::StructureRef, name: &str, target: Option<i32>) {
    let Some(target) = target else { return };
    if s.has_field(name) {
        s.fixate_field_nearest_int(name, target);
    } else {
        s.set(name, target);
    }
}

/// Reduce candidate caps to a single structure with a fixed format.
///
/// Ties keep the earliest candidate, so where two formats cost the same the
/// peer's own preference order decides.
fn choose_format(othercaps: &gst::Caps, in_format: Option<VideoFormat>) -> gst::Caps {
    let mut best: Option<(u32, usize, VideoFormat)> = None;

    for idx in 0..othercaps.size() {
        let Some(s) = othercaps.structure(idx) else {
            continue;
        };
        for format in formats_of(s) {
            let loss = conversion_loss(in_format, format);
            if best.is_none_or(|(best_loss, _, _)| loss < best_loss) {
                best = Some((loss, idx, format));
            }
        }
    }

    let Some((_, idx, format)) = best else {
        let mut caps = othercaps.copy();
        caps.truncate();
        return caps;
    };

    let mut structure = othercaps
        .structure(idx)
        .expect("index came from this caps")
        .to_owned();
    structure.set("format", format.to_str());

    let features = othercaps.features(idx).map(|f| f.to_owned());
    let mut caps = gst::Caps::new_empty();
    caps.make_mut().append_structure_full(structure, features);
    caps
}

/// Every video format a caps structure's `format` field allows.
fn formats_of(s: &gst::StructureRef) -> Vec<VideoFormat> {
    if let Ok(name) = s.get::<&str>("format") {
        return name.parse::<VideoFormat>().into_iter().collect();
    }
    if let Ok(list) = s.get::<gst::List>("format") {
        return list
            .iter()
            .filter_map(|v| v.get::<&str>().ok())
            .filter_map(|name| name.parse::<VideoFormat>().ok())
            .collect();
    }
    Vec::new()
}

/// Penalty for a candidate deeper than the input, set above every other
/// penalty combined so that a same-or-shallower format always wins when one
/// exists — while still allowing a deeper one when it is the only option.
///
/// This is the one place the scoring deliberately parts company with
/// `videoconvert`. Feeding `vtenc_h264_hw` from RGBA, the encoder offers
/// `{ AYUV64, UYVY, NV12, I420, P010_10LE, ARGB64_BE, RGBA64_LE }` and
/// `videoconvert` takes AYUV64 — sixteen bits per component for an eight-bit
/// source, which the encoder then converts again. Measured over 40 pairs at
/// 1080p, picking the deeper format cost 15.7% end to end against picking
/// UYVY. Widening cannot add information, and it doubles the bytes touched on
/// every frame.
const SCORE_DEPTH_INCREASE: u32 = 128;

/// How much a conversion to `out` costs, in the spirit of `videoconvert`'s own
/// scoring: identity is free, and each way the output can represent less than
/// the input costs far more than the reverse.
///
/// This is deliberately a smaller model than `gst_video_convert`'s
/// `score_value` — it ranks colour space, alpha, bit depth and chroma
/// subsampling and stops there. The case that matters most, "the input format
/// is on the menu, take it", is exact; beyond that the ordering only has to be
/// sensible, and ties defer to the peer.
fn conversion_loss(in_format: Option<VideoFormat>, out: VideoFormat) -> u32 {
    let Some(in_format) = in_format else {
        // Nothing to compare against, so let caps order decide alone.
        return 0;
    };
    if in_format == out {
        return 0;
    }

    let i = VideoFormatInfo::from_format(in_format);
    let o = VideoFormatInfo::from_format(out);

    // Changing format at all costs something, so an identical format always
    // wins even against an otherwise lossless alternative.
    let mut loss = 1;

    if i.is_rgb() != o.is_rgb() || i.is_yuv() != o.is_yuv() || i.is_gray() != o.is_gray() {
        loss += 2;
    }
    if i.has_alpha() && !o.has_alpha() {
        loss += 8;
    } else if o.has_alpha() && !i.has_alpha() {
        loss += 1;
    }
    if o.has_palette() && !i.has_palette() {
        loss += 64;
    }

    let depth = |info: &VideoFormatInfo| info.depth().iter().copied().max().unwrap_or(8);
    let (in_depth, out_depth) = (depth(&i), depth(&o));
    if out_depth < in_depth {
        loss += 4 * (in_depth - out_depth);
    } else if out_depth > in_depth {
        loss += SCORE_DEPTH_INCREASE;
    }

    let sub = |values: &[u32]| values.iter().copied().max().unwrap_or(0);
    if sub(o.w_sub()) > sub(i.w_sub()) {
        loss += 16;
    } else if sub(o.w_sub()) < sub(i.w_sub()) {
        loss += 1;
    }
    if sub(o.h_sub()) > sub(i.h_sub()) {
        loss += 32;
    } else if sub(o.h_sub()) < sub(i.h_sub()) {
        loss += 1;
    }

    loss
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        gst::init().expect("gst init");
    }

    /// The one case that must be exact: if the peer will take the input format
    /// unchanged, nothing else may outscore it.
    #[test]
    fn identity_beats_every_alternative() {
        init();
        for candidate in [
            VideoFormat::I420,
            VideoFormat::Nv12,
            VideoFormat::Bgra,
            VideoFormat::Uyvy,
        ] {
            assert!(
                conversion_loss(Some(VideoFormat::Rgba), candidate)
                    > conversion_loss(Some(VideoFormat::Rgba), VideoFormat::Rgba),
                "{:?} should cost more than staying in RGBA",
                candidate
            );
        }
    }

    /// Dropping alpha is a real loss; gaining an alpha channel is not.
    #[test]
    fn losing_alpha_costs_more_than_gaining_it() {
        init();
        let losing = conversion_loss(Some(VideoFormat::Rgba), VideoFormat::Rgbx);
        let gaining = conversion_loss(Some(VideoFormat::Rgbx), VideoFormat::Rgba);
        assert!(
            losing > gaining,
            "losing alpha scored {losing}, gaining scored {gaining}"
        );
    }

    /// Subsampling chroma away costs more than keeping it.
    #[test]
    fn subsampling_costs_more_than_staying_full_chroma() {
        init();
        let to_420 = conversion_loss(Some(VideoFormat::Rgba), VideoFormat::I420);
        let to_uyvy = conversion_loss(Some(VideoFormat::Rgba), VideoFormat::Uyvy);
        assert!(
            to_420 > to_uyvy,
            "4:2:0 scored {to_420}, 4:2:2 scored {to_uyvy}"
        );
    }

    /// The measured case: from 8-bit RGBA, an 8-bit target must beat every
    /// 16-bit one on the menu `vtenc_h264_hw` offers. Scoring the deeper
    /// format first cost 15.7% end to end.
    #[test]
    fn an_eight_bit_target_beats_every_deeper_one() {
        init();
        let menu = [
            VideoFormat::Ayuv64,
            VideoFormat::Uyvy,
            VideoFormat::Nv12,
            VideoFormat::I420,
            VideoFormat::P01010le,
            VideoFormat::Argb64Be,
            VideoFormat::Rgba64Le,
        ];
        let best = menu
            .iter()
            .min_by_key(|&&f| conversion_loss(Some(VideoFormat::Rgba), f))
            .copied()
            .expect("menu is not empty");
        assert_eq!(
            best,
            VideoFormat::Uyvy,
            "an 8-bit source must not be widened when an 8-bit target exists"
        );
    }

    /// A deeper format is still chosen when nothing shallower is offered —
    /// the penalty orders candidates, it does not veto them.
    #[test]
    fn a_deeper_target_is_still_chosen_when_it_is_the_only_option() {
        init();
        let only_deep = [VideoFormat::Argb64Be, VideoFormat::Ayuv64];
        let best = only_deep
            .iter()
            .min_by_key(|&&f| conversion_loss(Some(VideoFormat::Rgba), f))
            .copied()
            .expect("menu is not empty");
        assert_eq!(best, VideoFormat::Argb64Be);
    }

    #[test]
    fn format_list_is_read_from_caps() {
        init();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", gst::List::new(["NV12", "I420"]))
            .build();
        let formats = formats_of(caps.structure(0).unwrap());
        assert_eq!(formats, vec![VideoFormat::Nv12, VideoFormat::I420]);
    }
}
