//! Regression tests for #674: a Vision Mixer input takes the pixel format its
//! source delivers, on the CPU and the GPU backend.
//!
//! Each test builds a real Vision Mixer flow through `PipelineManager` with a
//! white source on `video_in_0` and waits for PGM to turn white. The CPU
//! backend converts on each input. The GPU backend uploads with `glupload`,
//! which takes a fixed list of system-memory formats: for any other format its
//! input front splices a `videoconvert` in, and takes it out again once the
//! source delivers something `glupload` (or `cudadownload`) takes directly.
//!
//! `AYUV64` and `GBR_10LE` are the formats `glupload` does not take, on the
//! GStreamer CI runs (1.24.2) and on current releases (1.28) alike.
//! `A444_10LE`, a ProRes 4444 decode, uploads on both, so it is not used here.
//!
//! The CUDA tests here use a `cudadownload` stand-in on hosts without nvcodec
//! (see `stand_in`). The one with real NVDEC output is ignored by default;
//! see `nvdec_cuda_after_a_converted_format_stays_on_the_gpu`.

pub mod common;
pub mod vision_mixer_input_format;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::time::{Duration, Instant};
use vision_mixer_input_format::*;

// --- #674 on the CPU backend ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_videotestsrc_direct() {
    assert_input_reaches_pgm("vmfmt_cpu_direct", Backend::Cpu, Front::Direct, false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_videotestsrc_via_videoconvert() {
    assert_input_reaches_pgm(
        "vmfmt_cpu_conv",
        Backend::Cpu,
        Front::ViaVideoconvert,
        false,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_i420() {
    assert_input_reaches_pgm("vmfmt_cpu_i420", Backend::Cpu, Front::Format("I420"), false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_nv12() {
    assert_input_reaches_pgm("vmfmt_cpu_nv12", Backend::Cpu, Front::Format("NV12"), false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_ayuv64() {
    assert_input_reaches_pgm(
        "vmfmt_cpu_ayuv64",
        Backend::Cpu,
        Front::Format("AYUV64"),
        false,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_gbr_10le() {
    assert_input_reaches_pgm(
        "vmfmt_cpu_gbr10",
        Backend::Cpu,
        Front::Format("GBR_10LE"),
        false,
    );
}

// --- #674 on the GPU backend ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_videotestsrc_direct() {
    assert_input_reaches_pgm("vmfmt_gpu_direct", Backend::Gpu, Front::Direct, false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_videotestsrc_via_videoconvert() {
    assert_input_reaches_pgm(
        "vmfmt_gpu_conv",
        Backend::Gpu,
        Front::ViaVideoconvert,
        false,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_i420() {
    assert_input_reaches_pgm("vmfmt_gpu_i420", Backend::Gpu, Front::Format("I420"), false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_nv12() {
    assert_input_reaches_pgm("vmfmt_gpu_nv12", Backend::Gpu, Front::Format("NV12"), false);
}

/// `glupload` does not upload `AYUV64`: a converter goes in front of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_ayuv64() {
    assert_input_reaches_pgm(
        "vmfmt_gpu_ayuv64",
        Backend::Gpu,
        Front::Format("AYUV64"),
        true,
    );
}

/// Nor `GBR_10LE`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_gbr_10le() {
    assert_input_reaches_pgm(
        "vmfmt_gpu_gbr10",
        Backend::Gpu,
        Front::Format("GBR_10LE"),
        true,
    );
}

// --- The GPU converter is transient --------------------------------------

/// One GPU input switches from a format `glupload` takes, to one it does not,
/// and back. The converter goes in for the middle part only: once the source
/// is back on an uploadable format, the frames go straight into `glupload`
/// again, and PGM keeps showing the source throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_converter_comes_out_when_the_format_switches_back() {
    if !ready(Backend::Gpu) {
        return;
    }
    let block_id = "vmfmt_gpu_switch";
    let running = Running::start(
        block_id,
        testsrc_flow(block_id, Backend::Gpu, Front::Format("I420")),
    );
    running.wait_for_white_pgm("the I420 source", 3);
    assert!(running.gpu_input_adapters().is_empty());

    for (format, converted) in [
        ("GBR_10LE", true),
        ("NV12", false),
        ("AYUV64", true),
        ("I420", false),
    ] {
        running
            .element("fmt")
            .set_property("caps", format_caps(format).parse::<gst::Caps>().unwrap());
        running.wait_for_input_format(format);
        running.wait_for_white_pgm(&format!("the {format} source"), 15);
        if converted {
            assert_eq!(
                running.gpu_input_adapters(),
                ["videoconvert"],
                "{format}: glupload does not take it, so a converter has to be in"
            );
        } else {
            assert!(
                running.gpu_input_adapters().is_empty(),
                "{format}: glupload takes it, but the input still runs {:?}",
                running.gpu_input_adapters()
            );
            assert!(
                running.spliced_converters().is_empty(),
                "{format}: a removed converter is still in the pipeline: {:?}",
                running.spliced_converters()
            );
            running.wait_for_upload_format(
                format,
                &format!("{format}: glupload does not receive the source's frames as they are"),
            );
        }
    }
    running.stop();
}

/// While the converter is in, the input's answer to a CAPS query lists what
/// `glupload` takes directly ahead of what only the converter takes, so a
/// source that can choose picks a format with no CPU conversion. Checked on
/// the answer itself, and on a source that is told only "raw video".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_converter_answers_with_direct_formats_first() {
    if !ready(Backend::Gpu) {
        return;
    }
    let uploads = strom::gst::video_adapt::gl_upload_system_caps()
        .expect("glupload's formats")
        .clone();
    let block_id = "vmfmt_gpu_answer";
    let running = Running::start(
        block_id,
        testsrc_flow(block_id, Backend::Gpu, Front::Format("AYUV64")),
    );
    running.wait_for_white_pgm("the AYUV64 source", 3);
    assert_eq!(running.gpu_input_adapters(), ["videoconvert"]);

    let answer = running
        .mixer_element("queue_0")
        .static_pad("sink")
        .unwrap()
        .query_caps(None);
    // GL memory goes into `glupload` as it is, and CUDA memory through
    // `cudadownload` (on a host with nvcodec): neither needs the converter.
    let direct = |s: &gst::StructureRef, f: &gst::CapsFeaturesRef| {
        if f.contains("memory:GLMemory") || f.contains("memory:CUDAMemory") {
            return true;
        }
        if !s.has_field("format") {
            return false;
        }
        let mut one = gst::Caps::new_empty();
        one.make_mut().append_structure(s.to_owned());
        one.is_subset(&uploads)
    };
    let first_converted = answer
        .iter_with_features()
        .position(|(s, f)| !direct(s, f))
        .unwrap_or_else(|| panic!("the answer lists nothing only the converter takes: {answer}"));
    let first_system_direct = answer
        .iter_with_features()
        .position(|(s, f)| {
            !f.contains("memory:GLMemory") && !f.contains("memory:CUDAMemory") && direct(s, f)
        })
        .unwrap_or_else(|| {
            panic!("the answer lists no system-memory format glupload uploads: {answer}")
        });
    assert!(
        first_system_direct < first_converted,
        "the answer lists what only the converter takes (entry {first_converted}) ahead \
         of what glupload uploads directly (entry {first_system_direct}): {answer}"
    );

    // A source told only "raw video" settles on what the answer lists first,
    // which glupload takes: the converter comes out.
    running.element("fmt").set_property(
        "caps",
        format!("video/x-raw,width={W},height={H},framerate=30/1")
            .parse::<gst::Caps>()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while running.input_format().as_deref() == Some("AYUV64") {
        assert!(Instant::now() < deadline, "the source never renegotiated");
        std::thread::sleep(Duration::from_millis(20));
    }
    running.wait_for_white_pgm("the unpinned source", 5);
    let format = running.input_format().unwrap();
    let settled: gst::Caps = format!("video/x-raw,format={format}").parse().unwrap();
    assert!(
        settled.is_subset(&uploads) || settled.can_intersect(&uploads),
        "the source settled on {format}, which glupload does not upload"
    );
    assert!(
        running.gpu_input_adapters().is_empty(),
        "the source settled on {format}, but the input still runs {:?}",
        running.gpu_input_adapters()
    );
    running.stop();
}
