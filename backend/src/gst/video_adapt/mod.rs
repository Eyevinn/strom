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
//! | [`Consumer::Exactly`] | raw video in GL, CUDA, D3D12 or D3D11 memory only, which the consumer's caps leave out, consumer takes system-memory raw video | [`Adapter::GlDownload`], [`Adapter::CudaDownload`], [`Adapter::D3d12Download`] or [`Adapter::D3d11Download`], then [`Adapter::SystemMemoryPin`] |
//! | [`Consumer::Exactly`] | as above, in a pixel format the download does not take | [`Refusal::NotDownloadable`] |
//! | [`Consumer::Exactly`] | raw video in any other memory the consumer's caps leave out | [`Refusal::UnsupportedMemory`] |
//! | [`Consumer::Exactly`] | anything but raw video, which the consumer's caps leave out | [`Refusal::NotRawVideo`] |
//! | [`Consumer::SystemMemory`] | raw video in system memory | none |
//! | [`Consumer::SystemMemory`] | raw video in GL memory only | [`Adapter::GlDownload`] |
//! | [`Consumer::SystemMemory`] | raw video in CUDA, D3D12 or D3D11 memory only | [`Adapter::CudaDownload`], [`Adapter::D3d12Download`] or [`Adapter::D3d11Download`] |
//! | [`Consumer::SystemMemory`] | raw video in any other memory | [`Refusal::UnsupportedMemory`] |
//! | [`Consumer::SystemMemory`] | encoded video, audio, anything but raw video | [`Refusal::NotRawVideo`] |
//! | [`Consumer::OtherPipeline`] | raw video in GL memory only | [`Adapter::GlDownload`], [`Adapter::SystemMemoryPin`] |
//! | [`Consumer::OtherPipeline`] | as above, in a pixel format `gldownload` does not take | [`Refusal::NotDownloadable`] |
//! | [`Consumer::OtherPipeline`] | as above, `gldownload` not installed | [`Refusal::MissingFactory`] |
//! | [`Consumer::OtherPipeline`] | anything else | none |
//! | [`Consumer::ScaleConvert`] | raw video in system memory | [`Adapter::VideoScale`], [`Adapter::VideoConvert`] |
//! | [`Consumer::ScaleConvert`] | raw video in GL, CUDA, D3D12 or D3D11 memory, which the output takes, with that GPU scaler installed and taking the pixel format | [`Adapter::GlScale`], [`Adapter::CudaScale`], [`Adapter::D3d12Scale`] or [`Adapter::D3d11Scale`] |
//! | [`Consumer::ScaleConvert`] | as above, but the output does not take that memory | the GPU scaler, its download, [`Adapter::SystemMemoryPin`], [`Adapter::VideoConvert`]: the smaller frame crosses the bus |
//! | [`Consumer::ScaleConvert`] | raw video in GPU memory whose scaler is missing or does not take the pixel format | its download, [`Adapter::SystemMemoryPin`], [`Adapter::VideoScale`], [`Adapter::VideoConvert`] |
//! | [`Consumer::ScaleConvert`] | as above, in a pixel format the download does not take | [`Refusal::NotDownloadable`] |
//! | [`Consumer::ScaleConvert`] | raw video in any other memory | [`Refusal::UnsupportedMemory`] |
//! | [`Consumer::ScaleConvert`] | encoded video, audio, anything but raw video | [`Refusal::NotRawVideo`] |
//!
//! A CUDA or D3D download that is called for but not installed is
//! [`Refusal::MissingFactory`], and so is any download
//! [`Consumer::ScaleConvert`] calls for. Those elements ship with platform plugins
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

mod adapter;
pub use adapter::*;

#[cfg(test)]
mod tests;

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
    /// memory only, with no scaling of its own (WHEP Output's bridge, for
    /// its CPU path). The producer caps are the negotiated caps.
    SystemMemory,
    /// An input fed from another pipeline that shares no context with this
    /// one (Inter Input, fed by another flow's Inter Output). GL memory there
    /// belongs to the other pipeline's GL context, and the first GL converter
    /// in this pipeline fails on it ("input memory OpenGL context is
    /// different"), so it is always downloaded. CUDA, D3D12 and D3D11 memory
    /// cross between pipelines without a shared context (#1099), so they and
    /// everything else pass as they are. The producer caps are the negotiated
    /// caps.
    OtherPipeline,
    /// A block that scales and converts video where it arrives (Video
    /// Format), between two passthrough elements. These caps are what its
    /// output takes now: the block's own caps, limited to what downstream
    /// takes. The producer caps are the negotiated caps, or what an
    /// ACCEPT_CAPS query offers. See [`Consumer::ScaleConvert`]'s rows in the
    /// module docs.
    ScaleConvert(&'a gst::CapsRef),
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
    /// The download called for does not take the producer's pixel format:
    /// the link would be made on templates and the caps then refused with a
    /// bare not-negotiated.
    NotDownloadable { download: Adapter },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::MissingFactory { factory } => write!(f, "{} is not installed", factory),
            Refusal::UnsupportedMemory { feature } => {
                write!(f, "raw video in {} cannot be read on the CPU", feature)
            }
            Refusal::NotRawVideo { media } => write!(f, "{} is not raw video", media),
            Refusal::NotDownloadable { download } => {
                write!(f, "{} does not take this pixel format", download.factory())
            }
        }
    }
}

/// Decide what `consumer` needs in front of it to take `producer`.
///
/// An empty list means the input links as it is. `available` says whether an
/// element factory exists; it is consulted only for factories that may be
/// missing on a working install (`cudadownload`, `d3d12download`,
/// `d3d11download`, and `gldownload` for [`Consumer::Exactly`],
/// [`Consumer::OtherPipeline`] and [`Consumer::ScaleConvert`], and the GPU
/// scalers' factories for
/// [`Consumer::ScaleConvert`]), and only when that adapter is called for.
/// Which pixel formats an adapter takes is read from the registry (in tests,
/// the sink templates can be stood in through `decide_with`; a scaler's src
/// template cannot), whatever `available` says: an element `available` claims but the registry lacks has
/// no known template, so a scaler counts as not taking anything and a
/// download as taking everything. See the module docs for the rules.
pub fn decide(
    producer: &gst::CapsRef,
    consumer: Consumer<'_>,
    available: impl Fn(&str) -> bool,
) -> Result<Vec<Adapter>, Refusal> {
    decide_with(producer, consumer, available, &|adapter| {
        adapter.sink_template().cloned()
    })
}

/// What an adapter's first element takes, in the adapter's own memory for a
/// scaler: [`Adapter::sink_template`], or a stand-in in tests.
type SinkTemplates<'a> = &'a dyn Fn(Adapter) -> Option<gst::Caps>;

/// [`decide`], with the adapters' sink templates from `templates`.
fn decide_with(
    producer: &gst::CapsRef,
    consumer: Consumer<'_>,
    available: impl Fn(&str) -> bool,
    templates: SinkTemplates<'_>,
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
                    pinned_download(download, producer, available, templates)
                }
                _ => match unreadable_memory(producer) {
                    Some(feature) => Err(Refusal::UnsupportedMemory { feature }),
                    // System memory the consumer does not take: a pixel
                    // format, which a download does not change.
                    None => Ok(Vec::new()),
                },
            }
        }
        Consumer::ScaleConvert(output) => scale_convert(producer, output, available, templates),
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
        // `gldownload` ships with the opengl plugin that made the GL memory,
        // so `available` refusing it is a broken install, named as such.
        Consumer::OtherPipeline if offers_only(producer, GL_MEMORY_FEATURE) => {
            pinned_download(Adapter::GlDownload, producer, available, templates)
        }
        Consumer::OtherPipeline => Ok(Vec::new()),
    }
}

/// Scale and convert `producer` where it is, for an output that takes
/// `output` (see [`Consumer::ScaleConvert`]).
fn scale_convert(
    producer: &gst::CapsRef,
    output: &gst::CapsRef,
    available: impl Fn(&str) -> bool,
    templates: SinkTemplates<'_>,
) -> Result<Vec<Adapter>, Refusal> {
    let (Some(structure), Some(features)) = (producer.structure(0), producer.features(0)) else {
        return Err(Refusal::NotRawVideo {
            media: "empty caps".to_string(),
        });
    };
    if structure.name() != "video/x-raw" {
        return Err(Refusal::NotRawVideo {
            media: structure.name().to_string(),
        });
    }
    if is_system_memory(features) {
        return Ok(CPU_SCALE.to_vec());
    }
    let Some(scaler) = Scaler::for_features(features) else {
        let feature = features
            .iter()
            .find(|f| f.starts_with("memory:"))
            .map(|f| f.to_string())
            .unwrap_or_else(|| "an unknown memory".to_string());
        return Err(Refusal::UnsupportedMemory { feature });
    };
    let download = scaler.download();
    // The GPU scaler must be installed and take this pixel format; otherwise
    // the frame is downloaded first and scaled on the CPU.
    let scaler_takes = scaler.adapter().factories().iter().all(|f| available(f))
        && templates(scaler.adapter()).is_some_and(|sink| producer.can_intersect(&sink));
    if !scaler_takes {
        let mut adapters = pinned_download(download, producer, &available, templates)?;
        adapters.extend(CPU_SCALE);
        return Ok(adapters);
    }
    // Downstream takes the scaler's output as it is: stay on the GPU.
    let in_scaler_memory = in_memory(output, Some(scaler.memory_feature()));
    let producible = scaler
        .template(gst::PadDirection::Src)
        .is_some_and(|src| in_scaler_memory.can_intersect(&src));
    if producible {
        return Ok(vec![scaler.adapter()]);
    }
    // Scaled on the GPU, then downloaded: the smaller frame crosses the bus.
    require(download, available)?;
    Ok(vec![
        scaler.adapter(),
        download,
        Adapter::SystemMemoryPin,
        Adapter::VideoConvert,
    ])
}

/// `download`, pinned to system memory, when its element is installed and
/// takes `producer`'s pixel format. A download whose template is unknown (a
/// test claiming an element that is not registered) is trusted.
fn pinned_download(
    download: Adapter,
    producer: &gst::CapsRef,
    available: impl Fn(&str) -> bool,
    templates: SinkTemplates<'_>,
) -> Result<Vec<Adapter>, Refusal> {
    require(download, available)?;
    match templates(download) {
        Some(template) if !producer.can_intersect(&template) => {
            Err(Refusal::NotDownloadable { download })
        }
        _ => Ok(vec![download, Adapter::SystemMemoryPin]),
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
