//! What a video input needs in front of it, decided from caps.
//!
//! A block emits the memory type it naturally produces and the *consuming*
//! block adapts its own input. Several places learn only at runtime what a
//! producer delivers, each at its own moment: a CAPS-event probe on the WHEP
//! Output input ([`crate::gst::video_input_bridge`]), a link the linker retries
//! after it was refused ([`crate::gst::gl_link`]), and a query probe on a GL
//! consumer's input ([`crate::gst::gl_input_front`]). They keep their own
//! timing, and ask [`decide`] what to insert.
//!
//! | Consumer | Producer caps | Adapters |
//! |---|---|---|
//! | [`Consumer::GlUpload`] | raw video in CUDA memory only | [`Adapter::CudaDownload`], or [`MissingFactory`] when `cudadownload` is not installed |
//! | [`Consumer::GlUpload`] | anything else | none |
//! | [`Consumer::WebrtcSink`] | raw video in GL memory | [`Adapter::GlDownload`] |
//! | [`Consumer::WebrtcSink`] | raw video in system or GL memory, 8-bit, not NV12/I420 | [`Adapter::Convert`] to NV12, after any download |
//! | [`Consumer::Accepts`] | raw video in GL memory only, consumer takes system-memory raw video | [`Adapter::GlDownload`] |
//!
//! Encoded video, audio, `ANY` and `EMPTY` need nothing for any consumer, and
//! CUDA, NVMM, D3D11, VA and DMABuf memory are never downloaded: the consumers
//! that advertise those really do take them.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;

/// The caps feature that marks a buffer as living in GL memory.
pub const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// The caps feature that marks a buffer as living in CUDA memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// The caps feature negotiated caps carry for plain system memory.
const SYSTEM_MEMORY_FEATURE: &str = "memory:SystemMemory";

/// The element that downloads GL memory to system memory.
pub const GL_DOWNLOAD_FACTORY: &str = "gldownload";

/// The element that brings CUDA memory into GL (CUDA-GL interop, not a CPU
/// download). Ships with the GStreamer nvcodec plugin, so it exists on NVIDIA
/// builds only.
pub const CUDA_DOWNLOAD_FACTORY: &str = "cudadownload";

/// The format a [`Consumer::WebrtcSink`] input is converted to when it needs
/// converting at all.
///
/// `vtenc_h264`/`vtenc_h265` and the NVENC and VA encoders take it natively,
/// and `x264enc` lists it alongside I420. The VP9 and AV1 software encoders do
/// not accept NV12 at all (`vp9enc`, `av1enc`, `rav1enc` and `svtav1enc` list
/// I420), so their consumers still convert NV12 to I420 each. That costs a
/// couple of points against an encode that costs fifteen.
pub const NATIVE_FORMAT: &str = "NV12";

/// Formats left alone: 8-bit 4:2:0. Converting one into the other would cost
/// a pass over every frame and at best move the per-consumer conversion from
/// one set of encoders to another.
const NATIVE_FORMATS: &[&str] = &["NV12", "I420"];

/// The input being adapted.
#[derive(Debug, Clone, Copy)]
pub enum Consumer<'a> {
    /// A GL consumer's `glupload`. It takes system memory, GL memory and
    /// DMABuf, but not CUDA memory. The producer caps are what an ACCEPT_CAPS
    /// query or a filtered CAPS query offers.
    GlUpload,
    /// A `whepserversink` video pad. It advertises GL memory but cannot encode
    /// it, and builds one encoding chain per viewer, so the format is
    /// converted once in front of it. The producer caps are the negotiated
    /// (fixed) caps of the first CAPS event.
    WebrtcSink,
    /// A consumer that refused a direct link and accepts these caps, as its
    /// sink pad answers a caps query. The producer caps are the producer's
    /// answer to the same query.
    Accepts(&'a gst::CapsRef),
}

/// An element to put in front of the consumer, in link order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// `gldownload`: GL memory to system memory.
    GlDownload,
    /// `cudadownload`: CUDA memory to GL memory.
    CudaDownload,
    /// A converter followed by a capsfilter fixing `format`. The caller picks
    /// the converter element.
    Convert { format: &'static str },
}

/// An adaptation is called for, but the element it needs is not installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingFactory {
    pub factory: &'static str,
}

/// Decide what `consumer` needs in front of it to take `producer`.
///
/// An empty list means the input links as it is. `available` says whether an
/// element factory exists; it is consulted only for factories that may be
/// missing on a working install (`cudadownload`), and only when that adapter
/// is called for. See the module docs for the rules.
pub fn decide(
    producer: &gst::CapsRef,
    consumer: Consumer<'_>,
    available: impl Fn(&str) -> bool,
) -> Result<Vec<Adapter>, MissingFactory> {
    match consumer {
        Consumer::GlUpload => {
            if !offers_only_cuda_memory(producer) {
                return Ok(Vec::new());
            }
            if available(CUDA_DOWNLOAD_FACTORY) {
                Ok(vec![Adapter::CudaDownload])
            } else {
                Err(MissingFactory {
                    factory: CUDA_DOWNLOAD_FACTORY,
                })
            }
        }
        Consumer::WebrtcSink => {
            // A download comes first: the converter takes system memory only,
            // and the format it has to produce is the same either way.
            let mut adapters = Vec::new();
            if needs_gl_download(producer) {
                adapters.push(Adapter::GlDownload);
            }
            if needs_format_conversion(producer) {
                adapters.push(Adapter::Convert {
                    format: NATIVE_FORMAT,
                });
            }
            Ok(adapters)
        }
        Consumer::Accepts(accepted) => {
            if offers_only_gl_memory(producer) && accepts_system_memory_raw_video(accepted) {
                Ok(vec![Adapter::GlDownload])
            } else {
                Ok(Vec::new())
            }
        }
    }
}

/// True when `name` can be created from the registry.
pub fn factory_available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Create the elements for `adapters`, in link order, named after
/// `name_prefix`. `convert_factory` is the converter for [`Adapter::Convert`].
pub fn build_elements(
    adapters: &[Adapter],
    name_prefix: &str,
    convert_factory: &str,
) -> Result<Vec<gst::Element>, String> {
    let mut elements = Vec::new();
    for adapter in adapters {
        match adapter {
            Adapter::GlDownload => {
                elements.push(make(GL_DOWNLOAD_FACTORY, name_prefix, "gldownload")?)
            }
            Adapter::CudaDownload => {
                elements.push(make(CUDA_DOWNLOAD_FACTORY, name_prefix, "cudadownload")?)
            }
            Adapter::Convert { format } => {
                let convert = make(convert_factory, name_prefix, "videoconvert")?;
                // Threads the conversion on macOS, like every other converter
                // built from the convert mode; a no-op elsewhere.
                crate::gpu::configure_video_convert(&convert);
                elements.push(convert);
                let filter = make("capsfilter", name_prefix, "format")?;
                filter.set_property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", *format)
                        .build(),
                );
                elements.push(filter);
            }
        }
    }
    Ok(elements)
}

fn make(factory: &str, name_prefix: &str, suffix: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .name(format!("{}_{}", name_prefix, suffix))
        .build()
        .map_err(|e| format!("{} could not be created: {}", factory, e))
}

/// True when every structure in `caps` is raw video in CUDA memory.
fn offers_only_cuda_memory(caps: &gst::CapsRef) -> bool {
    if caps.is_any() || caps.is_empty() {
        return false;
    }
    caps.iter_with_features().all(|(structure, features)| {
        structure.name() == "video/x-raw" && features.contains(CUDA_MEMORY_FEATURE)
    })
}

/// True when `caps` describe raw video in GL memory.
///
/// Encoded video (`video/x-h264` and friends) is never a candidate: there is
/// nothing to download, and `gldownload` would not even link. Other GPU memory
/// types are not candidates either — see the module docs.
fn needs_gl_download(caps: &gst::CapsRef) -> bool {
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    if structure.name() != "video/x-raw" {
        return false;
    }
    caps.features(0)
        .is_some_and(|features| features.contains(GL_MEMORY_FEATURE))
}

/// True when the input should be converted to [`NATIVE_FORMAT`] before the
/// sink fans it out to its consumers.
///
/// GL memory says yes, because the download that precedes the converter lands
/// the frame in system memory in the same format it had on the GPU. Every
/// other memory feature says no: the encoder behind that pad consumes it
/// directly, and a `videoconvert` could not even link to it.
///
/// A format carrying more than 8 bits per component is left alone as well.
/// Converting it would silently drop the input's precision, and the encoders
/// that can use the extra bits would have kept them.
fn needs_format_conversion(caps: &gst::CapsRef) -> bool {
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    if structure.name() != "video/x-raw" {
        return false;
    }
    let convertible_memory = match caps.features(0) {
        None => true,
        Some(features) if features.is_any() => false,
        Some(features) => {
            features.is_empty()
                || features
                    .iter()
                    .all(|f| f == SYSTEM_MEMORY_FEATURE || f == GL_MEMORY_FEATURE)
        }
    };
    if !convertible_memory {
        return false;
    }
    let Ok(format) = structure.get::<String>("format") else {
        // An unfixed format means negotiation has not settled on one; there is
        // nothing to compare against and nothing to convert.
        return false;
    };
    if NATIVE_FORMATS.contains(&format.as_str()) {
        return false;
    }
    let Ok(parsed) = format.parse::<gst_video::VideoFormat>() else {
        return false;
    };
    let info = gst_video::VideoFormatInfo::from_format(parsed);
    info.depth().iter().copied().max().unwrap_or(8) <= 8
}

/// True when every format `caps` offers is raw video in GL memory.
///
/// A producer that also offers system memory would have linked without help,
/// so a download is not what it is missing.
fn offers_only_gl_memory(caps: &gst::CapsRef) -> bool {
    if caps.is_any() || caps.is_empty() {
        return false;
    }
    caps.iter_with_features().all(|(structure, features)| {
        // `video/x-raw(ANY)` contains every feature, GL included, but it offers
        // system memory too and would have linked on its own.
        structure.name() == "video/x-raw"
            && !features.is_any()
            && features.contains(GL_MEMORY_FEATURE)
    })
}

/// True when `caps` accept raw video in system memory.
///
/// This is the consumer half of the link test: a `videoconvert` with an
/// encoder behind it advertises plain `video/x-raw`, which is what a
/// `gldownload` produces. A consumer that takes only GPU memory of any kind,
/// or no raw video at all, is refusing the link for a reason a download does
/// not address.
fn accepts_system_memory_raw_video(caps: &gst::CapsRef) -> bool {
    if caps.is_empty() {
        return false;
    }
    if caps.is_any() {
        return true;
    }
    caps.iter_with_features().any(|(structure, features)| {
        structure.name() == "video/x-raw"
            && (features.is_any() || features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        // Caps parsing needs the type system registered.
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    fn all(_: &str) -> bool {
        true
    }

    fn none(_: &str) -> bool {
        false
    }

    const CUDA: &str = "video/x-raw(memory:CUDAMemory), format=NV12, width=1920, height=1080";

    const OTHER_GPU_MEMORY: [&str; 5] = [
        "memory:CUDAMemory",
        "memory:NVMM",
        "memory:D3D11Memory",
        "memory:VAMemory",
        "memory:DMABuf",
    ];

    const NOT_RAW_VIDEO: [&str; 7] = [
        "video/x-h264, stream-format=avc, alignment=au",
        "video/x-h265",
        "video/x-vp9",
        "video/x-av1",
        "audio/x-raw, rate=48000",
        "ANY",
        "EMPTY",
    ];

    // --- Consumer::GlUpload ---------------------------------------------

    #[test]
    fn gl_upload_cuda_memory_gets_cudadownload() {
        assert_eq!(
            decide(&caps(CUDA), Consumer::GlUpload, all),
            Ok(vec![Adapter::CudaDownload])
        );
    }

    #[test]
    fn gl_upload_cuda_memory_without_cudadownload_is_missing() {
        assert_eq!(
            decide(&caps(CUDA), Consumer::GlUpload, none),
            Err(MissingFactory {
                factory: CUDA_DOWNLOAD_FACTORY
            })
        );
    }

    /// `glupload` passes GL memory through and uploads system memory itself.
    #[test]
    fn gl_upload_takes_gl_and_system_memory_as_they_are() {
        for c in [
            "video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080",
            "video/x-raw, format=NV12, width=1920, height=1080",
            "video/x-raw(memory:SystemMemory), format=NV12",
            "video/x-raw(memory:DMABuf), format=DMA_DRM",
        ] {
            assert_eq!(decide(&caps(c), Consumer::GlUpload, all), Ok(vec![]), "{c}");
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload, none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    /// A producer that offers CUDA alongside something `glupload` takes — a
    /// decoder's template — settles on the latter during negotiation.
    #[test]
    fn gl_upload_cuda_among_other_memory_types_needs_nothing() {
        let offer = caps(&format!(
            "{}; video/x-raw(memory:GLMemory), format=NV12; video/x-raw, format=NV12",
            CUDA
        ));
        assert_eq!(decide(&offer, Consumer::GlUpload, all), Ok(vec![]));
        assert_eq!(decide(&offer, Consumer::GlUpload, none), Ok(vec![]));
    }

    /// Extra features next to CUDA memory (an overlay meta) do not change what
    /// the frames live in.
    #[test]
    fn gl_upload_cuda_memory_with_a_meta_feature_gets_cudadownload() {
        let c =
            caps("video/x-raw(memory:CUDAMemory, meta:GstVideoOverlayComposition), format=NV12");
        assert_eq!(
            decide(&c, Consumer::GlUpload, all),
            Ok(vec![Adapter::CudaDownload])
        );
    }

    #[test]
    fn gl_upload_not_raw_video_needs_nothing() {
        for c in NOT_RAW_VIDEO {
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload, none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    // --- Consumer::WebrtcSink -------------------------------------------

    #[test]
    fn webrtc_sink_gl_nv12_is_only_downloaded() {
        assert_eq!(
            decide(
                &caps("video/x-raw(memory:GLMemory), format=NV12, width=1280, height=720"),
                Consumer::WebrtcSink,
                none
            ),
            Ok(vec![Adapter::GlDownload])
        );
    }

    /// The download has to come first: the converter takes system memory.
    #[test]
    fn webrtc_sink_gl_rgba_is_downloaded_then_converted() {
        assert_eq!(
            decide(
                &caps("video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080"),
                Consumer::WebrtcSink,
                none
            ),
            Ok(vec![
                Adapter::GlDownload,
                Adapter::Convert { format: "NV12" }
            ])
        );
    }

    #[test]
    fn webrtc_sink_system_rgba_is_converted() {
        assert_eq!(
            decide(
                &caps("video/x-raw, format=RGBA, width=1920, height=1080"),
                Consumer::WebrtcSink,
                none
            ),
            Ok(vec![Adapter::Convert { format: "NV12" }])
        );
    }

    #[test]
    fn webrtc_sink_native_system_memory_needs_nothing() {
        for c in [
            "video/x-raw, format=NV12, width=1920, height=1080",
            "video/x-raw(memory:SystemMemory), format=I420",
        ] {
            assert_eq!(
                decide(&caps(c), Consumer::WebrtcSink, none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    /// The consumers that advertise these memory types encode them directly.
    #[test]
    fn webrtc_sink_other_gpu_memory_needs_nothing() {
        for feature in OTHER_GPU_MEMORY {
            let c = caps(&format!("video/x-raw({}), format=RGBA", feature));
            assert_eq!(
                decide(&c, Consumer::WebrtcSink, none),
                Ok(vec![]),
                "{feature}"
            );
        }
    }

    #[test]
    fn webrtc_sink_not_raw_video_needs_nothing() {
        for c in NOT_RAW_VIDEO {
            assert_eq!(
                decide(&caps(c), Consumer::WebrtcSink, none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    // --- Consumer::Accepts ----------------------------------------------

    const SYSTEM_CONSUMER: &str = "video/x-raw, format=(string){ NV12, I420 }";

    #[test]
    fn accepts_gl_only_producer_into_system_consumer_is_downloaded() {
        let consumer = caps(SYSTEM_CONSUMER);
        assert_eq!(
            decide(
                &caps("video/x-raw(memory:GLMemory), format=RGBA, width=1280, height=720"),
                Consumer::Accepts(&consumer),
                none
            ),
            Ok(vec![Adapter::GlDownload])
        );
    }

    /// It would have linked on its own, so the download is not what is missing.
    #[test]
    fn accepts_producer_that_also_offers_system_memory_needs_nothing() {
        let consumer = caps(SYSTEM_CONSUMER);
        assert_eq!(
            decide(
                &caps("video/x-raw(memory:GLMemory), format=RGBA; video/x-raw, format=NV12"),
                Consumer::Accepts(&consumer),
                none
            ),
            Ok(vec![])
        );
    }

    /// A `gldownload` produces system memory, so a consumer that takes only GPU
    /// memory, of whatever kind, or no raw video, cannot be fed by one.
    #[test]
    fn accepts_gpu_only_or_non_raw_consumer_needs_nothing() {
        let producer = caps("video/x-raw(memory:GLMemory), format=RGBA");
        for c in [
            "video/x-raw(memory:GLMemory), format=NV12",
            "video/x-raw(memory:CUDAMemory), format=NV12",
            "video/x-raw(memory:NVMM), format=NV12",
            "video/x-raw(memory:D3D11Memory), format=NV12",
            "video/x-raw(memory:VAMemory), format=NV12",
            "video/x-h264",
            "EMPTY",
        ] {
            let consumer = caps(c);
            assert_eq!(
                decide(&producer, Consumer::Accepts(&consumer), none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    #[test]
    fn accepts_any_consumer_and_raw_any_consumer_take_a_download() {
        let producer = caps("video/x-raw(memory:GLMemory), format=RGBA");
        for c in ["ANY", "video/x-raw(ANY)"] {
            let consumer = caps(c);
            assert_eq!(
                decide(&producer, Consumer::Accepts(&consumer), none),
                Ok(vec![Adapter::GlDownload]),
                "{c}"
            );
        }
    }

    #[test]
    fn accepts_other_gpu_memory_and_non_raw_producers_need_nothing() {
        let consumer = caps("ANY");
        for feature in OTHER_GPU_MEMORY {
            let c = caps(&format!("video/x-raw({}), format=NV12", feature));
            assert_eq!(
                decide(&c, Consumer::Accepts(&consumer), none),
                Ok(vec![]),
                "{feature}"
            );
        }
        for c in NOT_RAW_VIDEO.iter().chain(&["video/x-raw(ANY)"]) {
            assert_eq!(
                decide(&caps(c), Consumer::Accepts(&consumer), none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    // --- The WebrtcSink predicates in detail ----------------------------

    #[test]
    fn gl_memory_raw_video_needs_a_download() {
        assert!(needs_gl_download(&caps(
            "video/x-raw(memory:GLMemory), format=NV12, width=1280, height=720"
        )));
    }

    #[test]
    fn system_memory_raw_video_does_not() {
        assert!(!needs_gl_download(&caps(
            "video/x-raw, format=NV12, width=1280, height=720"
        )));
    }

    #[test]
    fn other_gpu_memory_types_are_not_downloaded() {
        for feature in OTHER_GPU_MEMORY {
            let c = caps(&format!("video/x-raw({}), format=NV12", feature));
            assert!(
                !needs_gl_download(&c),
                "{} should not be downloaded",
                feature
            );
        }
    }

    #[test]
    fn not_raw_video_is_never_downloaded_or_converted() {
        for c in NOT_RAW_VIDEO {
            assert!(!needs_gl_download(&caps(c)), "{c}");
            assert!(!needs_format_conversion(&caps(c)), "{c}");
        }
    }

    /// 8-bit 4:2:0: NV12 is what the hardware encoders take, I420 what the VP9
    /// and AV1 software encoders take.
    #[test]
    fn eight_bit_420_is_left_alone() {
        for format in ["NV12", "I420"] {
            assert!(
                !needs_format_conversion(&caps(&format!(
                    "video/x-raw, format={}, width=1920, height=1080",
                    format
                ))),
                "{} should be passed through",
                format
            );
        }
    }

    /// Everything webrtcsink would otherwise convert once per consumer: packed
    /// RGB from a mixer or a screen capture, packed and planar YUV that is not
    /// 4:2:0, and gray.
    #[test]
    fn other_eight_bit_formats_are_converted() {
        for format in [
            "RGBA", "BGRA", "RGB", "BGRx", "UYVY", "YUY2", "Y42B", "Y444", "GRAY8",
        ] {
            assert!(
                needs_format_conversion(&caps(&format!(
                    "video/x-raw, format={}, width=1920, height=1080",
                    format
                ))),
                "{} should be converted",
                format
            );
        }
    }

    /// A download lands the frame in system memory in the format it had on the
    /// GPU, so a GL input is judged on its format like any other.
    #[test]
    fn gl_memory_is_judged_on_its_format() {
        assert!(needs_format_conversion(&caps(
            "video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080"
        )));
        assert!(!needs_format_conversion(&caps(
            "video/x-raw(memory:GLMemory), format=NV12, width=1920, height=1080"
        )));
    }

    #[test]
    fn other_gpu_memory_is_not_converted() {
        for feature in OTHER_GPU_MEMORY {
            assert!(
                !needs_format_conversion(&caps(&format!("video/x-raw({}), format=RGBA", feature))),
                "{} should not be converted",
                feature
            );
        }
    }

    /// Converting these would drop precision the encoder behind the pad may
    /// well be able to keep.
    #[test]
    fn deeper_than_eight_bit_is_left_alone() {
        for format in ["P010_10LE", "I420_10LE", "AYUV64", "RGBA64_LE", "v210"] {
            assert!(
                !needs_format_conversion(&caps(&format!("video/x-raw, format={}", format))),
                "{} should be passed through",
                format
            );
        }
    }

    /// Before negotiation settles there is nothing to compare against.
    #[test]
    fn unfixed_format_is_left_alone() {
        assert!(!needs_format_conversion(&caps(
            "video/x-raw, width=1920, height=1080"
        )));
    }

    // --- build_elements -------------------------------------------------

    fn factories(elements: &[gst::Element]) -> Vec<String> {
        elements
            .iter()
            .map(|e| {
                e.factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn build_elements_creates_download_converter_and_filter_in_order() {
        let _ = gst::init();
        let elements = build_elements(
            &[Adapter::GlDownload, Adapter::Convert { format: "NV12" }],
            "in",
            "videoconvert",
        )
        .expect("elements");
        assert_eq!(
            factories(&elements),
            ["gldownload", "videoconvert", "capsfilter"]
        );
        let names: Vec<String> = elements.iter().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["in_gldownload", "in_videoconvert", "in_format"]);
        let filter_caps = elements[2].property::<gst::Caps>("caps");
        assert_eq!(filter_caps, caps("video/x-raw, format=NV12"));
    }
}
