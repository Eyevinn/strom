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
    let c = caps("video/x-raw(memory:CUDAMemory, meta:GstVideoOverlayComposition), format=NV12");
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

// --- Consumer::OtherPipeline ---------------------------------------

/// GL memory from another flow's context is downloaded, and pinned to
/// system memory. Without `gldownload` it is refused, naming it.
#[test]
fn other_pipeline_gl_memory_gets_gldownload() {
    let gl = caps("video/x-raw(memory:GLMemory), format=RGBA, width=1280, height=720");
    assert_eq!(
        decide(&gl, Consumer::OtherPipeline, all),
        Ok(vec![Adapter::GlDownload, Adapter::SystemMemoryPin])
    );
    assert_eq!(
        decide(&gl, Consumer::OtherPipeline, none),
        Err(Refusal::MissingFactory {
            factory: GL_DOWNLOAD_FACTORY
        })
    );
}

/// Only GL needs the other pipeline's context. CUDA and D3D memory cross
/// as they are, and so does everything that is not GPU memory.
#[test]
fn other_pipeline_passes_everything_but_gl_memory() {
    let mut passing = vec![
        CUDA,
        "video/x-raw, format=NV12, width=1920, height=1080",
        "video/x-raw(memory:GLMemory), format=RGBA; video/x-raw, format=RGBA",
        "video/x-h264, stream-format=byte-stream",
        "audio/x-raw, format=F32LE, rate=48000, channels=2",
        "ANY",
        "EMPTY",
    ];
    passing.extend(D3D.iter().map(|(c, _, _)| *c));
    for c in passing {
        assert_eq!(
            decide(&caps(c), Consumer::OtherPipeline, none),
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
            &caps("video/x-raw(memory:CUDAMemory, meta:GstVideoOverlayComposition), format=NV12"),
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
            Ok(vec![download, Adapter::SystemMemoryPin]),
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
        Ok(vec![Adapter::CudaDownload, Adapter::SystemMemoryPin])
    );
}

/// A `cudadownload` that takes NV12 only: a download whose element does
/// not list every pixel format.
fn nv12_only_cudadownload(adapter: Adapter) -> Option<gst::Caps> {
    (adapter == Adapter::CudaDownload).then(|| caps("video/x-raw(memory:CUDAMemory), format=NV12"))
}

/// A download whose element does not take the pixel format is refused,
/// not a yes that fails not-negotiated once the CAPS event arrives: for an
/// encoder's input, and for a scaler that is not installed.
#[test]
fn a_download_that_does_not_take_the_pixel_format_is_refused() {
    let p010 = caps("video/x-raw(memory:CUDAMemory), format=P010_10LE, width=320, height=240");
    let refused = Err(Refusal::NotDownloadable {
        download: Adapter::CudaDownload,
    });
    let encoder = caps(SOFTWARE_ENCODER);
    assert_eq!(
        decide_with(
            &p010,
            Consumer::Exactly(&encoder),
            all,
            &nv12_only_cudadownload
        ),
        refused
    );
    let output = caps("video/x-raw, width=160, height=120");
    assert_eq!(
        decide_with(
            &p010,
            Consumer::ScaleConvert(&output),
            all,
            &nv12_only_cudadownload
        ),
        refused
    );
    // The format it does take is downloaded.
    assert_eq!(
        decide_with(
            &caps(CUDA),
            Consumer::Exactly(&encoder),
            all,
            &nv12_only_cudadownload
        ),
        Ok(vec![Adapter::CudaDownload, Adapter::SystemMemoryPin])
    );
}

/// Inter Input's input: GL memory in a pixel format `gldownload` does not
/// take is refused, not linked into a bare not-negotiated.
#[test]
fn other_pipeline_gl_memory_gldownload_does_not_take_is_refused() {
    let rgba_only = |adapter: Adapter| {
        (adapter == Adapter::GlDownload).then(|| caps("video/x-raw(memory:GLMemory), format=RGBA"))
    };
    let nv12 = caps("video/x-raw(memory:GLMemory), format=NV12, width=320, height=240");
    assert_eq!(
        decide_with(&nv12, Consumer::OtherPipeline, all, &rgba_only),
        Err(Refusal::NotDownloadable {
            download: Adapter::GlDownload
        })
    );
}

/// `sink_template` caches one slot per entry of `ALL`, and an adapter
/// left out has no template. The match fails to compile when a variant is
/// added, as a reminder to list it; the count catches one listed twice.
#[test]
fn every_adapter_is_listed_in_all() {
    for adapter in Adapter::ALL {
        assert_eq!(
            Adapter::ALL.iter().filter(|a| **a == adapter).count(),
            1,
            "{adapter:?}"
        );
        match adapter {
            Adapter::GlDownload
            | Adapter::CudaDownload
            | Adapter::D3d12Download
            | Adapter::D3d11Download
            | Adapter::VideoConvert
            | Adapter::VideoScale
            | Adapter::GlScale
            | Adapter::CudaScale
            | Adapter::D3d12Scale
            | Adapter::D3d11Scale
            | Adapter::SystemMemoryPin => {}
        }
    }
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
        Ok(vec![Adapter::D3d11Download, Adapter::SystemMemoryPin])
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
