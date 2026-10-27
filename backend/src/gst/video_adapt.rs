//! What a video input needs in front of it, decided from caps.
//!
//! A block emits the memory type it naturally produces and the *consuming*
//! block adapts its own input. Several places learn only at runtime what a
//! producer delivers, each at its own moment: a CAPS-event probe on the WHEP
//! Output input ([`crate::gst::video_input_bridge`]), a link the linker retries
//! after it was refused ([`crate::gst::gl_link`]), query and CAPS-event
//! probes on a GL consumer's input ([`crate::gst::gl_input_front`]), and query
//! and CAPS-event probes on an encoder's input ([`crate::gst::download_front`]).
//! They keep their own timing, and ask [`decide`] what to insert. The thumbnail tap
//! ([`crate::gst::thumbnail_tap`]) asks too, from the caps already on its tee,
//! before it attaches a branch that reads frames on the CPU.
//!
//! | Consumer | Producer caps | Adapters |
//! |---|---|---|
//! | [`Consumer::GlUpload`] | raw video in CUDA memory only | [`Adapter::CudaDownload`] |
//! | [`Consumer::GlUpload`] | raw video in D3D12 or D3D11 memory only | [`Adapter::D3d12Download`] or [`Adapter::D3d11Download`] |
//! | [`Consumer::GlUpload`] | raw video in system memory only, in no pixel format `glupload` uploads | [`Adapter::VideoConvert`] |
//! | [`Consumer::GlUpload`] | anything else | none |
//! | [`Consumer::Accepts`] | raw video in GL memory only, consumer takes system-memory raw video | [`Adapter::GlDownload`] |
//! | [`Consumer::Accepts`] | raw video in D3D12 or D3D11 memory only, consumer takes system-memory raw video and not that memory | [`Adapter::D3d12Download`] or [`Adapter::D3d11Download`] |
//! | [`Consumer::Exactly`] | anything the consumer's caps take as it is | none |
//! | [`Consumer::Exactly`] | raw video in GL, CUDA, D3D12 or D3D11 memory only, which the consumer's caps leave out, consumer takes system-memory raw video | [`Adapter::GlDownload`], [`Adapter::CudaDownload`], [`Adapter::D3d12Download`] or [`Adapter::D3d11Download`] |
//! | [`Consumer::Exactly`] | raw video in any other memory the consumer's caps leave out | [`Refusal::UnsupportedMemory`] |
//! | [`Consumer::Exactly`] | anything but raw video, which the consumer's caps leave out | [`Refusal::NotRawVideo`] |
//! | [`Consumer::SystemMemory`] | raw video in system memory | none |
//! | [`Consumer::SystemMemory`] | raw video in GL memory only | [`Adapter::GlDownload`] |
//! | [`Consumer::SystemMemory`] | raw video in CUDA, D3D12 or D3D11 memory only | [`Adapter::CudaDownload`], [`Adapter::D3d12Download`] or [`Adapter::D3d11Download`] |
//! | [`Consumer::SystemMemory`] | raw video in any other memory | [`Refusal::UnsupportedMemory`] |
//! | [`Consumer::SystemMemory`] | encoded video, audio, anything but raw video | [`Refusal::NotRawVideo`] |
//!
//! A CUDA or D3D download that is called for but not installed is
//! [`Refusal::MissingFactory`]. Those elements ship with platform plugins
//! (nvcodec, d3d12, d3d11), so a working install can lack them. For
//! [`Consumer::Exactly`] the same goes for `gldownload`.
//!
//! Encoded video, audio, `ANY` and `EMPTY` need nothing for the other
//! consumers, and `ANY` and `EMPTY` need nothing for any consumer. For
//! [`Consumer::Accepts`], CUDA, NVMM, VA and DMABuf memory are never
//! downloaded: the consumers that advertise those really do take them. D3D
//! memory is downloaded only when the consumer's caps leave it out, as
//! WHEP Output's corrected caps do.

use gstreamer as gst;

/// The caps feature that marks a buffer as living in GL memory.
pub const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// The caps feature that marks a buffer as living in CUDA memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// The element that converts raw video in system memory between pixel formats.
pub const VIDEO_CONVERT_FACTORY: &str = "videoconvert";

/// The element that downloads GL memory to system memory.
pub const GL_DOWNLOAD_FACTORY: &str = "gldownload";

/// The element that takes CUDA memory out to GL memory (CUDA-GL interop) or
/// to system memory, whichever its consumer takes. Ships with the GStreamer
/// nvcodec plugin, so it exists on NVIDIA builds only.
pub const CUDA_DOWNLOAD_FACTORY: &str = "cudadownload";

/// The caps feature that marks a buffer as living in Direct3D 12 memory.
pub const D3D12_MEMORY_FEATURE: &str = "memory:D3D12Memory";

/// The caps feature that marks a buffer as living in Direct3D 11 memory.
pub const D3D11_MEMORY_FEATURE: &str = "memory:D3D11Memory";

/// The element that downloads D3D12 memory to system memory. Ships with the
/// GStreamer d3d12 plugin, so it exists on Windows builds only.
pub const D3D12_DOWNLOAD_FACTORY: &str = "d3d12download";

/// The element that downloads D3D11 memory to system memory. Ships with the
/// GStreamer d3d11 plugin, so it exists on Windows builds only.
pub const D3D11_DOWNLOAD_FACTORY: &str = "d3d11download";

/// The input being adapted.
#[derive(Debug, Clone, Copy)]
pub enum Consumer<'a> {
    /// A GL consumer's `glupload`. It takes system memory, GL memory and
    /// DMABuf, but not CUDA or D3D memory, and in system memory only the pixel
    /// formats it can upload: these caps, its sink template's system-memory
    /// entries ([`gl_upload_system_caps`]). Which formats those are depends on
    /// the GStreamer version (10-bit 4:4:4 with alpha is missing from older
    /// ones, `AYUV64` from all of them so far). The producer caps are what an
    /// ACCEPT_CAPS query or a filtered CAPS query offers.
    GlUpload(&'a gst::CapsRef),
    /// A consumer that takes these caps: what its sink pad answers to a caps
    /// query (for a link that was refused), or what it can really process
    /// where it advertises more. The producer caps are the producer's answer
    /// to the same query, or the negotiated caps.
    Accepts(&'a gst::CapsRef),
    /// A consumer that takes exactly what these caps list, in every memory
    /// type: what the converter in front of an encoder answers to a caps
    /// query, which lists the GPU memory types the encoder takes and system
    /// memory in any format. Memory it lists passes as it is (CUDA into
    /// `nvh264enc` stays on the GPU); GL, CUDA or D3D memory it leaves out is
    /// downloaded. The producer caps are the negotiated caps, or what an
    /// ACCEPT_CAPS query offers.
    ///
    /// Unlike [`Consumer::Accepts`], this trusts the caps for every memory
    /// type: WHEP Output's corrected caps leave out the GPU memory its sink
    /// takes, so they cannot be read this way.
    Exactly(&'a gst::CapsRef),
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
    /// `d3d12download`: D3D12 memory to system memory.
    D3d12Download,
    /// `d3d11download`: D3D11 memory to system memory.
    D3d11Download,
    /// `videoconvert`: a system-memory pixel format the consumer cannot take
    /// to one it can.
    VideoConvert,
}

impl Adapter {
    /// The element factory the adapter is made from.
    pub fn factory(self) -> &'static str {
        match self {
            Adapter::GlDownload => GL_DOWNLOAD_FACTORY,
            Adapter::CudaDownload => CUDA_DOWNLOAD_FACTORY,
            Adapter::D3d12Download => D3D12_DOWNLOAD_FACTORY,
            Adapter::D3d11Download => D3D11_DOWNLOAD_FACTORY,
            Adapter::VideoConvert => VIDEO_CONVERT_FACTORY,
        }
    }

    /// The downloads, each out of one GPU memory type.
    pub const DOWNLOADS: [Adapter; 4] = [
        Adapter::GlDownload,
        Adapter::CudaDownload,
        Adapter::D3d12Download,
        Adapter::D3d11Download,
    ];

    /// The download made from `factory`, if it is one.
    pub fn download_from(factory: &str) -> Option<Adapter> {
        Adapter::DOWNLOADS
            .into_iter()
            .find(|adapter| adapter.factory() == factory)
    }

    /// The caps feature of the memory a download takes. `None` for
    /// [`Adapter::VideoConvert`], which takes system memory.
    pub fn memory_feature(self) -> Option<&'static str> {
        match self {
            Adapter::GlDownload => Some(GL_MEMORY_FEATURE),
            Adapter::CudaDownload => Some(CUDA_MEMORY_FEATURE),
            Adapter::D3d12Download => Some(D3D12_MEMORY_FEATURE),
            Adapter::D3d11Download => Some(D3D11_MEMORY_FEATURE),
            Adapter::VideoConvert => None,
        }
    }

    /// The GStreamer plugin the adapter's element ships with, for an error
    /// that says what to install.
    pub fn plugin(self) -> &'static str {
        match self {
            Adapter::GlDownload => "opengl",
            Adapter::CudaDownload => "nvcodec",
            Adapter::D3d12Download => "d3d12",
            Adapter::D3d11Download => "d3d11",
            Adapter::VideoConvert => "videoconvertscale",
        }
    }

    /// What the adapter's element takes on its sink pad: its sink template.
    /// `None` when the element is not installed. Read once per adapter, and
    /// only once found, so a test element registered later is still seen.
    pub fn sink_template(self) -> Option<&'static gst::Caps> {
        static TEMPLATES: [std::sync::OnceLock<gst::Caps>; 5] = [
            std::sync::OnceLock::new(),
            std::sync::OnceLock::new(),
            std::sync::OnceLock::new(),
            std::sync::OnceLock::new(),
            std::sync::OnceLock::new(),
        ];
        let slot = &TEMPLATES[self as usize];
        if let Some(caps) = slot.get() {
            return Some(caps);
        }
        let caps = gst::ElementFactory::find(self.factory())?
            .static_pad_templates()
            .into_iter()
            .find(|t| t.direction() == gst::PadDirection::Sink)?
            .caps();
        Some(slot.get_or_init(|| caps))
    }

    /// True when the adapter's element takes `caps` on its sink pad. `false`
    /// when it is not installed.
    pub fn sink_takes(self, caps: &gst::CapsRef) -> bool {
        self.sink_template()
            .is_some_and(|template| caps.can_intersect(template))
    }

    /// True for the downloads out of a platform GPU memory (CUDA, D3D12,
    /// D3D11), whose elements a working install can lack.
    pub fn downloads_device_memory(self) -> bool {
        matches!(
            self,
            Adapter::CudaDownload | Adapter::D3d12Download | Adapter::D3d11Download
        )
    }
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
/// missing on a working install (`cudadownload`, `d3d12download`,
/// `d3d11download`, and `gldownload` for [`Consumer::Exactly`]), and only when
/// that adapter is called for. See the module docs for the rules.
pub fn decide(
    producer: &gst::CapsRef,
    consumer: Consumer<'_>,
    available: impl Fn(&str) -> bool,
) -> Result<Vec<Adapter>, Refusal> {
    match consumer {
        Consumer::GlUpload(uploads) => {
            if let Some(download) = device_download(producer) {
                return require(download, available);
            }
            // Compared by structure: a meta feature next to system memory
            // does not change which pixel formats can be uploaded.
            if !uploads.is_any()
                && offers_only_system_memory_raw_video(producer)
                && !producer
                    .iter()
                    .any(|offered| uploads.iter().any(|taken| offered.can_intersect(taken)))
            {
                return Ok(vec![Adapter::VideoConvert]);
            }
            Ok(Vec::new())
        }
        Consumer::Accepts(accepted) => {
            if !accepts_system_memory_raw_video(accepted) {
                return Ok(Vec::new());
            }
            if offers_only(producer, GL_MEMORY_FEATURE) {
                return Ok(vec![Adapter::GlDownload]);
            }
            match d3d_download(producer) {
                // A consumer that lists D3D memory takes it as it is.
                Some(download) if !accepted.can_intersect(producer) => require(download, available),
                _ => Ok(Vec::new()),
            }
        }
        Consumer::Exactly(takes) => {
            if producer.is_any()
                || producer.is_empty()
                || takes.is_any()
                || takes.can_intersect(producer)
            {
                return Ok(Vec::new());
            }
            if let Some(media) = offers_no_raw_video(producer) {
                return Err(Refusal::NotRawVideo { media });
            }
            let download = if offers_only(producer, GL_MEMORY_FEATURE) {
                Some(Adapter::GlDownload)
            } else {
                device_download(producer)
            };
            match download {
                // A download produces system memory, so it helps only a
                // consumer that takes that.
                Some(download) if accepts_system_memory_raw_video(takes) => {
                    require(download, available)
                }
                _ => match unreadable_memory(producer) {
                    Some(feature) => Err(Refusal::UnsupportedMemory { feature }),
                    // System memory the consumer does not take: a pixel
                    // format, which a download does not change.
                    None => Ok(Vec::new()),
                },
            }
        }
        Consumer::SystemMemory => {
            if offers_only(producer, GL_MEMORY_FEATURE) {
                return Ok(vec![Adapter::GlDownload]);
            }
            if let Some(download) = device_download(producer) {
                return require(download, available);
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

/// `download`, when its element is installed.
fn require(download: Adapter, available: impl Fn(&str) -> bool) -> Result<Vec<Adapter>, Refusal> {
    if available(download.factory()) {
        Ok(vec![download])
    } else {
        Err(Refusal::MissingFactory {
            factory: download.factory(),
        })
    }
}

/// The download out of the platform GPU memory `caps` offer and nothing else:
/// CUDA, D3D12 or D3D11.
fn device_download(caps: &gst::CapsRef) -> Option<Adapter> {
    if offers_only(caps, CUDA_MEMORY_FEATURE) {
        return Some(Adapter::CudaDownload);
    }
    d3d_download(caps)
}

/// The download out of the D3D memory `caps` offer and nothing else.
fn d3d_download(caps: &gst::CapsRef) -> Option<Adapter> {
    if offers_only(caps, D3D12_MEMORY_FEATURE) {
        Some(Adapter::D3d12Download)
    } else if offers_only(caps, D3D11_MEMORY_FEATURE) {
        Some(Adapter::D3d11Download)
    } else {
        None
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
pub(crate) fn is_system_memory(features: &gst::CapsFeaturesRef) -> bool {
    features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
        || !features.iter().any(|f| f.starts_with("memory:"))
}

/// A capsfilter named `<name_prefix>_system_memory` that pins what a download
/// outputs to system memory: `gldownload` and `cudadownload` also output GL
/// memory, which a consumer that lists GL memory would otherwise be handed.
pub fn system_memory_pin(name_prefix: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make("capsfilter")
        .name(format!("{}_system_memory", name_prefix))
        .property("caps", gst::Caps::new_empty_simple("video/x-raw"))
        .build()
        .map_err(|e| format!("capsfilter could not be created: {}", e))
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
    adapters
        .iter()
        .map(|adapter| make(adapter.factory(), name_prefix))
        .collect()
}

/// An element of `factory` named `<name_prefix>_<factory>`.
fn make(factory: &str, name_prefix: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .name(format!("{}_{}", name_prefix, factory))
        .build()
        .map_err(|e| format!("{} could not be created: {}", factory, e))
}

/// What `glupload` takes in system memory: the system-memory raw video
/// entries of its sink template, with their features dropped so they match
/// producer caps written either way. `None` when `glupload` is not installed.
/// Read once: the registry does not change under a running process.
pub fn gl_upload_system_caps() -> Option<&'static gst::Caps> {
    static CAPS: std::sync::OnceLock<Option<gst::Caps>> = std::sync::OnceLock::new();
    CAPS.get_or_init(|| {
        let factory = gst::ElementFactory::find("glupload")?;
        let template = factory
            .static_pad_templates()
            .into_iter()
            .find(|t| t.direction() == gst::PadDirection::Sink)?
            .caps();
        let mut system = gst::Caps::new_empty();
        {
            let system = system.get_mut().expect("new caps are writable");
            for (structure, features) in template.iter_with_features() {
                if structure.name() == "video/x-raw" && is_system_memory(features) {
                    system.append_structure(structure.to_owned());
                }
            }
        }
        (!system.is_empty()).then_some(system)
    })
    .as_ref()
}

/// True when every structure in `caps` is raw video in system memory.
fn offers_only_system_memory_raw_video(caps: &gst::CapsRef) -> bool {
    if caps.is_any() || caps.is_empty() {
        return false;
    }
    caps.iter_with_features().all(|(structure, features)| {
        structure.name() == "video/x-raw" && !features.is_any() && is_system_memory(features)
    })
}

/// True when every format `caps` offers is raw video in the memory `feature`
/// names.
///
/// A producer that also offers system memory would have linked without help,
/// so a download is not what it is missing.
fn offers_only(caps: &gst::CapsRef, feature: &str) -> bool {
    if caps.is_any() || caps.is_empty() {
        return false;
    }
    caps.iter_with_features().all(|(structure, features)| {
        // `video/x-raw(ANY)` contains every feature, but it offers system
        // memory too and would have linked on its own.
        structure.name() == "video/x-raw" && !features.is_any() && features.contains(feature)
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

    /// D3D memory and the download out of it: what `d3d12h264dec` and
    /// `d3d11h264dec` decode to on Windows.
    const D3D: [(&str, Adapter, &str); 2] = [
        (
            "video/x-raw(memory:D3D12Memory), format=NV12, width=1920, height=1080",
            Adapter::D3d12Download,
            D3D12_DOWNLOAD_FACTORY,
        ),
        (
            "video/x-raw(memory:D3D11Memory), format=NV12, width=1920, height=1080",
            Adapter::D3d11Download,
            D3D11_DOWNLOAD_FACTORY,
        ),
    ];

    const OTHER_GPU_MEMORY: [&str; 4] = [
        "memory:CUDAMemory",
        "memory:NVMM",
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

    /// Stands in for `glupload`'s system-memory formats, which differ between
    /// GStreamer versions.
    fn uploads() -> gst::Caps {
        caps("video/x-raw, format=(string){ RGBA, BGRA, NV12, I420 }")
    }

    /// ProRes 4444 decodes to 10- or 16-bit 4:4:4 with alpha (`A444_10LE`
    /// from `avdec_prores`, `AYUV64` from VideoToolbox in system memory),
    /// which `glupload` does not upload: a converter goes in front.
    #[test]
    fn gl_upload_a_format_it_cannot_upload_gets_videoconvert() {
        for c in [
            "video/x-raw, format=AYUV64, width=1920, height=1080",
            "video/x-raw(memory:SystemMemory), format=A444_10LE, width=320, height=180",
            "video/x-raw(memory:SystemMemory, meta:GstVideoOverlayComposition), format=AYUV64",
        ] {
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload(&uploads()), none),
                Ok(vec![Adapter::VideoConvert]),
                "{c}"
            );
        }
    }

    /// A query that offers several formats, one of which uploads, settles on
    /// that one; a format in GPU memory is not converted on the CPU.
    #[test]
    fn gl_upload_an_offer_with_an_uploadable_format_needs_nothing() {
        for c in [
            "video/x-raw, format={ AYUV64, RGBA }",
            "video/x-raw, format=AYUV64; video/x-raw, format=NV12",
            "video/x-raw(memory:SystemMemory, meta:GstVideoOverlayComposition), format=NV12",
            "video/x-raw(memory:GLMemory), format=AYUV64",
            "video/x-raw, format=AYUV64; video/x-raw(memory:GLMemory), format=RGBA",
        ] {
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload(&uploads()), none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    /// The real `glupload` takes the common 8-bit formats and not `AYUV64`.
    #[test]
    fn gl_upload_system_caps_come_from_the_registry() {
        let _ = gst::init();
        let Some(uploads) = gl_upload_system_caps() else {
            return;
        };
        assert!(caps("video/x-raw, format=RGBA").can_intersect(uploads));
        assert!(!caps("video/x-raw, format=AYUV64").can_intersect(uploads));
    }

    #[test]
    fn gl_upload_cuda_memory_gets_cudadownload() {
        assert_eq!(
            decide(&caps(CUDA), Consumer::GlUpload(&uploads()), all),
            Ok(vec![Adapter::CudaDownload])
        );
    }

    #[test]
    fn gl_upload_cuda_memory_without_cudadownload_is_missing() {
        assert_eq!(
            decide(&caps(CUDA), Consumer::GlUpload(&uploads()), none),
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
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload(&uploads()), all),
                Ok(vec![]),
                "{c}"
            );
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload(&uploads()), none),
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
        assert_eq!(
            decide(&offer, Consumer::GlUpload(&uploads()), all),
            Ok(vec![])
        );
        assert_eq!(
            decide(&offer, Consumer::GlUpload(&uploads()), none),
            Ok(vec![])
        );
    }

    /// Extra features next to CUDA memory (an overlay meta) do not change what
    /// the frames live in.
    #[test]
    fn gl_upload_cuda_memory_with_a_meta_feature_gets_cudadownload() {
        let c =
            caps("video/x-raw(memory:CUDAMemory, meta:GstVideoOverlayComposition), format=NV12");
        assert_eq!(
            decide(&c, Consumer::GlUpload(&uploads()), all),
            Ok(vec![Adapter::CudaDownload])
        );
    }

    /// A hardware decoder on Windows leaves its frames in D3D memory, which
    /// `glupload` does not take: the GPU Vision Mixer's input never
    /// negotiated with a Media Player playing H.264.
    #[test]
    fn gl_upload_d3d_memory_gets_its_download() {
        for (c, download, _) in D3D {
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload(&uploads()), all),
                Ok(vec![download]),
                "{c}"
            );
        }
        let with_meta =
            caps("video/x-raw(memory:D3D12Memory, meta:GstVideoOverlayComposition), format=NV12");
        assert_eq!(
            decide(&with_meta, Consumer::GlUpload(&uploads()), all),
            Ok(vec![Adapter::D3d12Download])
        );
    }

    #[test]
    fn gl_upload_d3d_memory_without_its_download_is_missing() {
        for (c, _, factory) in D3D {
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload(&uploads()), none),
                Err(Refusal::MissingFactory { factory }),
                "{c}"
            );
        }
    }

    /// A decoder's template offers D3D memory next to system memory, and
    /// settles on system memory with `glupload`.
    #[test]
    fn gl_upload_d3d_among_system_memory_needs_nothing() {
        let offer = caps("video/x-raw(memory:D3D12Memory), format=NV12; video/x-raw, format=NV12");
        assert_eq!(
            decide(&offer, Consumer::GlUpload(&uploads()), none),
            Ok(vec![])
        );
    }

    #[test]
    fn gl_upload_not_raw_video_needs_nothing() {
        for c in NOT_RAW_VIDEO {
            assert_eq!(
                decide(&caps(c), Consumer::GlUpload(&uploads()), none),
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

    /// WHEP Output's corrected caps take system memory only, so D3D memory is
    /// downloaded rather than handed to a sink that cannot encode it.
    #[test]
    fn accepts_d3d_memory_into_a_system_consumer_is_downloaded() {
        for consumer in ["video/x-raw", SYSTEM_CONSUMER] {
            let consumer = caps(consumer);
            for (c, download, factory) in D3D {
                assert_eq!(
                    decide(&caps(c), Consumer::Accepts(&consumer), all),
                    Ok(vec![download]),
                    "{c}"
                );
                assert_eq!(
                    decide(&caps(c), Consumer::Accepts(&consumer), none),
                    Err(Refusal::MissingFactory { factory }),
                    "{c}"
                );
            }
        }
    }

    /// A consumer that lists D3D memory takes it as it is.
    #[test]
    fn accepts_d3d_memory_into_a_consumer_that_lists_it_needs_nothing() {
        for consumer in [
            "video/x-raw(memory:D3D12Memory); video/x-raw(memory:D3D11Memory); video/x-raw",
            "ANY",
            "video/x-raw(ANY)",
        ] {
            let consumer = caps(consumer);
            for (c, _, _) in D3D {
                assert_eq!(
                    decide(&caps(c), Consumer::Accepts(&consumer), all),
                    Ok(vec![]),
                    "{c} into {consumer}"
                );
            }
        }
    }

    /// What a sink that takes system memory only (WHEP Output's corrected
    /// caps) gets for each producer: a download for GL memory, nothing for
    /// the memory types its encoders may take.
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
    fn system_memory_d3d_memory_gets_its_download() {
        for (c, download, factory) in D3D {
            assert_eq!(
                decide(&caps(c), Consumer::SystemMemory, all),
                Ok(vec![download]),
                "{c}"
            );
            assert_eq!(
                decide(&caps(c), Consumer::SystemMemory, none),
                Err(Refusal::MissingFactory { factory }),
                "{c}"
            );
        }
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
        for feature in ["memory:NVMM", "memory:VAMemory", "memory:DMABuf"] {
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

    // --- Consumer::Exactly ----------------------------------------------

    /// What `videoconvert` in front of `x264enc` answers: system memory, any
    /// format.
    const SOFTWARE_ENCODER: &str = "video/x-raw, width=[1, 32767], height=[1, 32767]";

    /// What `videoconvert` in front of `nvh264enc` answers: CUDA and GL memory
    /// in the formats the encoder takes, system memory in any.
    const NVENC: &str = "video/x-raw(memory:CUDAMemory), format={ NV12, Y444, RGBA, BGRA }; \
         video/x-raw(memory:GLMemory), format={ NV12, Y444, RGBA, BGRA }; \
         video/x-raw";

    /// The defect in #1104: `nvh264dec` decodes to CUDA memory, and an encoder
    /// that lists CUDA memory takes it with no download and no converter.
    #[test]
    fn exactly_memory_the_consumer_lists_passes_as_it_is() {
        let consumer = caps(NVENC);
        for c in [
            CUDA,
            "video/x-raw(memory:GLMemory), format=RGBA, width=1280, height=720",
            "video/x-raw, format=NV12, width=1280, height=720",
            "video/x-raw, format=P010_10LE, width=1280, height=720",
        ] {
            assert_eq!(
                decide(&caps(c), Consumer::Exactly(&consumer), none),
                Ok(vec![]),
                "{c}"
            );
        }
    }

    /// A software encoder takes system memory only: every GPU memory a
    /// download exists for is downloaded.
    #[test]
    fn exactly_memory_the_consumer_leaves_out_is_downloaded() {
        let consumer = caps(SOFTWARE_ENCODER);
        let mut cases = vec![
            (
                "video/x-raw(memory:GLMemory), format=RGBA, width=1280, height=720",
                Adapter::GlDownload,
                GL_DOWNLOAD_FACTORY,
            ),
            (CUDA, Adapter::CudaDownload, CUDA_DOWNLOAD_FACTORY),
            (
                "video/x-raw(memory:CUDAMemory, meta:GstVideoOverlayComposition), format=NV12",
                Adapter::CudaDownload,
                CUDA_DOWNLOAD_FACTORY,
            ),
        ];
        cases.extend(D3D);
        for (c, download, factory) in cases {
            assert_eq!(
                decide(&caps(c), Consumer::Exactly(&consumer), all),
                Ok(vec![download]),
                "{c}"
            );
            assert_eq!(
                decide(&caps(c), Consumer::Exactly(&consumer), none),
                Err(Refusal::MissingFactory { factory }),
                "{c}"
            );
        }
    }

    /// CUDA memory in a format the encoder does not take in CUDA memory is
    /// downloaded, so the converter can change its format on the CPU.
    #[test]
    fn exactly_gpu_memory_in_a_format_the_consumer_does_not_list_is_downloaded() {
        let consumer = caps(NVENC);
        assert_eq!(
            decide(
                &caps("video/x-raw(memory:CUDAMemory), format=P010_10LE"),
                Consumer::Exactly(&consumer),
                all
            ),
            Ok(vec![Adapter::CudaDownload])
        );
    }

    /// Windows: `nvh264enc` takes D3D12 memory but not D3D11.
    #[test]
    fn exactly_d3d_memory_follows_the_consumer_list() {
        let consumer = caps("video/x-raw(memory:D3D12Memory), format=NV12; video/x-raw");
        assert_eq!(
            decide(&caps(D3D[0].0), Consumer::Exactly(&consumer), none),
            Ok(vec![])
        );
        assert_eq!(
            decide(&caps(D3D[1].0), Consumer::Exactly(&consumer), all),
            Ok(vec![Adapter::D3d11Download])
        );
    }

    #[test]
    fn exactly_memory_nothing_downloads_is_refused() {
        let consumer = caps(SOFTWARE_ENCODER);
        for feature in ["memory:NVMM", "memory:VAMemory", "memory:DMABuf"] {
            assert_eq!(
                decide(
                    &caps(&format!("video/x-raw({}), format=NV12", feature)),
                    Consumer::Exactly(&consumer),
                    all
                ),
                Err(Refusal::UnsupportedMemory {
                    feature: feature.to_string()
                }),
                "{feature}"
            );
        }
    }

    /// A consumer that takes no system memory cannot be fed by a download.
    #[test]
    fn exactly_a_consumer_without_system_memory_gets_no_download() {
        let consumer = caps("video/x-raw(memory:VAMemory), format=NV12");
        assert_eq!(
            decide(&caps(CUDA), Consumer::Exactly(&consumer), all),
            Err(Refusal::UnsupportedMemory {
                feature: CUDA_MEMORY_FEATURE.to_string()
            })
        );
    }

    #[test]
    fn exactly_encoded_video_the_consumer_leaves_out_is_refused() {
        let consumer = caps(SOFTWARE_ENCODER);
        assert_eq!(
            decide(
                &caps("video/x-h264, stream-format=avc, alignment=au"),
                Consumer::Exactly(&consumer),
                all
            ),
            Err(Refusal::NotRawVideo {
                media: "video/x-h264".to_string()
            })
        );
    }

    #[test]
    fn exactly_any_empty_and_unconstrained_need_nothing() {
        let consumer = caps(SOFTWARE_ENCODER);
        for c in ["ANY", "EMPTY", "video/x-raw(ANY)"] {
            assert_eq!(
                decide(&caps(c), Consumer::Exactly(&consumer), none),
                Ok(vec![]),
                "{c}"
            );
        }
        let any = caps("ANY");
        assert_eq!(
            decide(&caps(CUDA), Consumer::Exactly(&any), none),
            Ok(vec![])
        );
    }

    #[test]
    fn every_download_names_its_memory_and_plugin() {
        for download in Adapter::DOWNLOADS {
            assert!(download.memory_feature().is_some(), "{download:?}");
            assert_eq!(Adapter::download_from(download.factory()), Some(download));
        }
        assert_eq!(Adapter::download_from(VIDEO_CONVERT_FACTORY), None);
        assert_eq!(Adapter::CudaDownload.plugin(), "nvcodec");
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

    #[test]
    fn every_adapter_is_made_from_its_factory() {
        for adapter in [
            Adapter::GlDownload,
            Adapter::CudaDownload,
            Adapter::D3d12Download,
            Adapter::D3d11Download,
            Adapter::VideoConvert,
        ] {
            assert_eq!(
                adapter.downloads_device_memory(),
                matches!(
                    adapter.factory(),
                    CUDA_DOWNLOAD_FACTORY | D3D12_DOWNLOAD_FACTORY | D3D11_DOWNLOAD_FACTORY
                ),
                "{adapter:?}"
            );
        }
        assert_eq!(Adapter::D3d12Download.factory(), "d3d12download");
        assert_eq!(Adapter::D3d11Download.factory(), "d3d11download");
    }
}
