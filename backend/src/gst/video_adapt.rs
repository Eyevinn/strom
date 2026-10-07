//! What a video input needs in front of it, decided from caps.
//!
//! A block emits the memory type it naturally produces and the *consuming*
//! block adapts its own input. Several places learn only at runtime what a
//! producer delivers, each at its own moment: a CAPS-event probe on the WHEP
//! Output input ([`crate::gst::video_input_bridge`]), a link the linker retries
//! after it was refused ([`crate::gst::gl_link`]), and a query probe on a GL
//! consumer's input ([`crate::gst::gl_input_front`]). They keep their own
//! timing, and ask [`decide`] what to insert. The thumbnail tap
//! ([`crate::gst::thumbnail_tap`]) asks too, from the caps already on its tee,
//! before it attaches a branch that reads frames on the CPU.
//!
//! | Consumer | Producer caps | Adapters |
//! |---|---|---|
//! | [`Consumer::GlUpload`] | raw video in CUDA memory only | [`Adapter::CudaDownload`], or [`Refusal::MissingFactory`] when `cudadownload` is not installed |
//! | [`Consumer::GlUpload`] | anything else | none |
//! | [`Consumer::Accepts`] | raw video in GL memory only, consumer takes system-memory raw video | [`Adapter::GlDownload`] |
//! | [`Consumer::SystemMemory`] | raw video in system memory | none |
//! | [`Consumer::SystemMemory`] | raw video in GL memory only | [`Adapter::GlDownload`] |
//! | [`Consumer::SystemMemory`] | raw video in CUDA memory only | [`Adapter::CudaDownload`], or [`Refusal::MissingFactory`] when `cudadownload` is not installed |
//! | [`Consumer::SystemMemory`] | raw video in any other memory | [`Refusal::UnsupportedMemory`] |
//! | [`Consumer::SystemMemory`] | encoded video, audio, anything but raw video | [`Refusal::NotRawVideo`] |
//!
//! Encoded video, audio, `ANY` and `EMPTY` need nothing for the other
//! consumers, and `ANY` and `EMPTY` need nothing for any consumer. For
//! [`Consumer::Accepts`], CUDA, NVMM, D3D11, VA and DMABuf memory are never
//! downloaded: the consumers that advertise those really do take them.

use gstreamer as gst;

/// The caps feature that marks a buffer as living in GL memory.
pub const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// The caps feature that marks a buffer as living in CUDA memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// The element that downloads GL memory to system memory.
pub const GL_DOWNLOAD_FACTORY: &str = "gldownload";

/// The element that takes CUDA memory out to GL memory (CUDA-GL interop) or
/// to system memory, whichever its consumer takes. Ships with the GStreamer
/// nvcodec plugin, so it exists on NVIDIA builds only.
pub const CUDA_DOWNLOAD_FACTORY: &str = "cudadownload";

/// The input being adapted.
#[derive(Debug, Clone, Copy)]
pub enum Consumer<'a> {
    /// A GL consumer's `glupload`. It takes system memory, GL memory and
    /// DMABuf, but not CUDA memory. The producer caps are what an ACCEPT_CAPS
    /// query or a filtered CAPS query offers.
    GlUpload,
    /// A consumer that takes these caps: what its sink pad answers to a caps
    /// query (for a link that was refused), or what it can really process
    /// where it advertises more. The producer caps are the producer's answer
    /// to the same query, or the negotiated caps.
    Accepts(&'a gst::CapsRef),
    /// A consumer that reads frames on the CPU and takes raw video in system
    /// memory only (the thumbnail tap's branch). The producer caps are the
    /// negotiated caps.
    SystemMemory,
}

/// An element to put in front of the consumer, in link order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// `gldownload`: GL memory to system memory.
    GlDownload,
    /// `cudadownload`: CUDA memory to GL or system memory.
    CudaDownload,
}

/// Why the consumer cannot be adapted to take the producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// An adaptation is called for, but the element it needs is not installed.
    MissingFactory { factory: &'static str },
    /// The producer offers raw video in a memory type nothing here downloads
    /// for this consumer. `feature` is the caps feature it offers.
    UnsupportedMemory { feature: String },
    /// The producer offers no raw video at all, to a consumer that reads raw
    /// frames. `media` is the media type it offers.
    NotRawVideo { media: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::MissingFactory { factory } => write!(f, "{} is not installed", factory),
            Refusal::UnsupportedMemory { feature } => {
                write!(f, "raw video in {} cannot be read on the CPU", feature)
            }
            Refusal::NotRawVideo { media } => write!(f, "{} is not raw video", media),
        }
    }
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
) -> Result<Vec<Adapter>, Refusal> {
    match consumer {
        Consumer::GlUpload => {
            if !offers_only_cuda_memory(producer) {
                return Ok(Vec::new());
            }
            cuda_download(available)
        }
        Consumer::Accepts(accepted) => {
            if offers_only_gl_memory(producer) && accepts_system_memory_raw_video(accepted) {
                Ok(vec![Adapter::GlDownload])
            } else {
                Ok(Vec::new())
            }
        }
        Consumer::SystemMemory => {
            if offers_only_gl_memory(producer) {
                return Ok(vec![Adapter::GlDownload]);
            }
            if offers_only_cuda_memory(producer) {
                return cuda_download(available);
            }
            if let Some(media) = offers_no_raw_video(producer) {
                return Err(Refusal::NotRawVideo { media });
            }
            match unreadable_memory(producer) {
                Some(feature) => Err(Refusal::UnsupportedMemory { feature }),
                None => Ok(Vec::new()),
            }
        }
    }
}

fn cuda_download(available: impl Fn(&str) -> bool) -> Result<Vec<Adapter>, Refusal> {
    if available(CUDA_DOWNLOAD_FACTORY) {
        Ok(vec![Adapter::CudaDownload])
    } else {
        Err(Refusal::MissingFactory {
            factory: CUDA_DOWNLOAD_FACTORY,
        })
    }
}

/// The memory feature of the first raw-video structure in `caps` that is not
/// in system memory, when `caps` offer no raw video in system memory at all.
///
/// `None` when the CPU can read what is offered, or when it is not raw video.
fn unreadable_memory(caps: &gst::CapsRef) -> Option<String> {
    if caps.is_any() || caps.is_empty() {
        return None;
    }
    let mut unreadable = None;
    for (structure, features) in caps.iter_with_features() {
        if structure.name() != "video/x-raw" {
            continue;
        }
        if features.is_any() || is_system_memory(features) {
            return None;
        }
        if unreadable.is_none() {
            unreadable = features
                .iter()
                .find(|f| f.starts_with("memory:"))
                .map(|f| f.to_string());
        }
    }
    unreadable
}

/// The media type of the first structure in `caps`, when none of them is raw
/// video. `None` for `ANY` and `EMPTY`, and whenever raw video is offered.
fn offers_no_raw_video(caps: &gst::CapsRef) -> Option<String> {
    if caps.is_any() || caps.is_empty() {
        return None;
    }
    if caps
        .iter()
        .any(|structure| structure.name() == "video/x-raw")
    {
        return None;
    }
    caps.structure(0)
        .map(|structure| structure.name().to_string())
}

/// True when `features` put the buffer in system memory: no memory feature, or
/// the system-memory one, with or without metas next to it.
fn is_system_memory(features: &gst::CapsFeaturesRef) -> bool {
    features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
        || !features.iter().any(|f| f.starts_with("memory:"))
}

/// True when `name` can be created from the registry.
pub fn factory_available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Create the elements for `adapters`, in link order, named after
/// `name_prefix`.
pub fn build_elements(
    adapters: &[Adapter],
    name_prefix: &str,
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
        // `video/x-raw(ANY)` contains every feature, CUDA included, but it
        // offers system memory too.
        structure.name() == "video/x-raw"
            && !features.is_any()
            && features.contains(CUDA_MEMORY_FEATURE)
    })
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
    use gstreamer::prelude::*;
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
            Err(Refusal::MissingFactory {
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
            "video/x-raw(ANY)",
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

    /// What a sink that takes system memory only (WHEP Output's corrected
    /// caps) gets for each producer: a download for GL memory, nothing else.
    #[test]
    fn accepts_plain_raw_video_consumer() {
        let consumer = caps("video/x-raw");
        for (producer, expected) in [
            (
                "video/x-raw(memory:GLMemory), format=NV12, width=1280, height=720",
                vec![Adapter::GlDownload],
            ),
            (
                "video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080",
                vec![Adapter::GlDownload],
            ),
            ("video/x-raw, format=NV12, width=1280, height=720", vec![]),
            ("video/x-raw(memory:SystemMemory), format=RGBA", vec![]),
        ] {
            assert_eq!(
                decide(&caps(producer), Consumer::Accepts(&consumer), none),
                Ok(expected),
                "{producer}"
            );
        }
        for feature in OTHER_GPU_MEMORY {
            let c = caps(&format!("video/x-raw({}), format=NV12", feature));
            assert_eq!(
                decide(&c, Consumer::Accepts(&consumer), none),
                Ok(vec![]),
                "{feature}"
            );
        }
        for c in NOT_RAW_VIDEO {
            assert_eq!(
                decide(&caps(c), Consumer::Accepts(&consumer), none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    // --- Consumer::SystemMemory ----------------------------------------

    #[test]
    fn system_memory_takes_system_memory_as_it_is() {
        for c in [
            "video/x-raw, format=NV12, width=1920, height=1080",
            "video/x-raw(memory:SystemMemory), format=RGBA",
            "video/x-raw(meta:GstVideoOverlayComposition), format=RGBA",
            "video/x-raw(ANY)",
        ] {
            assert_eq!(
                decide(&caps(c), Consumer::SystemMemory, none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    #[test]
    fn system_memory_gl_memory_gets_gldownload() {
        assert_eq!(
            decide(
                &caps("video/x-raw(memory:GLMemory), format=RGBA, width=1280, height=720"),
                Consumer::SystemMemory,
                none
            ),
            Ok(vec![Adapter::GlDownload])
        );
    }

    /// The thumbnail tap's defect: CUDA memory on its tee went to a CPU
    /// converter that refuses it.
    #[test]
    fn system_memory_cuda_memory_gets_cudadownload() {
        assert_eq!(
            decide(&caps(CUDA), Consumer::SystemMemory, all),
            Ok(vec![Adapter::CudaDownload])
        );
        assert_eq!(
            decide(
                &caps(
                    "video/x-raw(memory:CUDAMemory, meta:GstVideoOverlayComposition), format=NV12"
                ),
                Consumer::SystemMemory,
                all
            ),
            Ok(vec![Adapter::CudaDownload])
        );
    }

    #[test]
    fn system_memory_cuda_memory_without_cudadownload_is_missing() {
        assert_eq!(
            decide(&caps(CUDA), Consumer::SystemMemory, none),
            Err(Refusal::MissingFactory {
                factory: CUDA_DOWNLOAD_FACTORY
            })
        );
    }

    /// Memory nothing here downloads is refused, naming the feature, instead
    /// of being handed to a converter that would fail to negotiate.
    #[test]
    fn system_memory_other_gpu_memory_is_refused() {
        for feature in [
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
            "memory:DMABuf",
        ] {
            assert_eq!(
                decide(
                    &caps(&format!("video/x-raw({}), format=NV12", feature)),
                    Consumer::SystemMemory,
                    all
                ),
                Err(Refusal::UnsupportedMemory {
                    feature: feature.to_string()
                }),
                "{feature}"
            );
        }
    }

    #[test]
    fn system_memory_gpu_memory_alongside_system_memory_needs_nothing() {
        let offer = caps("video/x-raw(memory:NVMM), format=NV12; video/x-raw, format=NV12");
        assert_eq!(decide(&offer, Consumer::SystemMemory, none), Ok(vec![]));
    }

    /// A tap behind an encoder reads raw frames it will never get: refused,
    /// naming what arrived.
    #[test]
    fn system_memory_not_raw_video_is_refused() {
        for (c, media) in [
            (
                "video/x-h264, stream-format=avc, alignment=au",
                "video/x-h264",
            ),
            ("video/x-h265", "video/x-h265"),
            ("audio/x-raw, rate=48000", "audio/x-raw"),
        ] {
            assert_eq!(
                decide(&caps(c), Consumer::SystemMemory, all),
                Err(Refusal::NotRawVideo {
                    media: media.to_string()
                }),
                "{c}"
            );
        }
    }

    #[test]
    fn system_memory_any_and_empty_need_nothing() {
        for c in ["ANY", "EMPTY"] {
            assert_eq!(
                decide(&caps(c), Consumer::SystemMemory, none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    #[test]
    fn build_elements_names_the_download_after_the_prefix() {
        let _ = gst::init();
        let elements = build_elements(&[Adapter::GlDownload], "in").expect("elements");
        let names: Vec<String> = elements.iter().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["in_gldownload"]);
        assert_eq!(
            elements[0]
                .factory()
                .map(|f| f.name().to_string())
                .as_deref(),
            Some("gldownload")
        );
    }
}
