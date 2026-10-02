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
//! | [`Consumer::Accepts`] | raw video in GL memory only, consumer takes system-memory raw video | [`Adapter::GlDownload`] |
//!
//! Encoded video, audio, `ANY` and `EMPTY` need nothing for any consumer, and
//! CUDA, NVMM, D3D11, VA and DMABuf memory are never downloaded: the consumers
//! that advertise those really do take them.

use gstreamer as gst;

/// The caps feature that marks a buffer as living in GL memory.
pub const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// The caps feature that marks a buffer as living in CUDA memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// The element that downloads GL memory to system memory.
pub const GL_DOWNLOAD_FACTORY: &str = "gldownload";

/// The element that brings CUDA memory into GL (CUDA-GL interop, not a CPU
/// download). Ships with the GStreamer nvcodec plugin, so it exists on NVIDIA
/// builds only.
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
}

/// An element to put in front of the consumer, in link order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// `gldownload`: GL memory to system memory.
    GlDownload,
    /// `cudadownload`: CUDA memory to GL memory.
    CudaDownload,
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
        structure.name() == "video/x-raw" && features.contains(CUDA_MEMORY_FEATURE)
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
