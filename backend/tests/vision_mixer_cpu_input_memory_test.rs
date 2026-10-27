//! The software (CPU) Vision Mixer adapts GPU memory on its own inputs (#969).
//!
//! The CPU mixer's inputs used to put the global video convert mode's pick in
//! front of the compositor: `autovideoconvert` on a host with NVENC and CUDA-GL
//! interop, plain `videoconvert` elsewhere. GL memory into a plain
//! `videoconvert` reached the compositor only if the linker's GL-download
//! retry (#802) caught the refused link. Each input now has a front
//! (`gst::download_front`) that downloads what the compositor cannot take,
//! decided from the negotiated caps, and every converter is `videoconvert`.
//!
//! The binary forces the global mode to the GPU one before any block is
//! built, so the mixer would build `autovideoconvert` if it still asked the
//! mode. A GPU Vision Mixer upstream emits GL memory; `gltestsrc` stands in
//! for it.
//!
//! The tests run one at a time with the other GL tests (`serial(gl)`): GL
//! setup in parallel tests races on the Linux runner (#1095).

pub mod common;
pub mod vision_mixer_input_format;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::{Flow, PropertyValue as PV};
use vision_mixer_input_format::*;

fn force_gpu_convert_mode() {
    assert_eq!(
        strom::gpu::force_video_convert_mode(strom::gpu::VideoConvertMode::GpuAccelerated),
        strom::gpu::VideoConvertMode::GpuAccelerated,
        "the convert mode was set before this test could force it"
    );
}

/// The factory of every converter the mixer built itself (not spliced in by a
/// front), by element name.
fn built_converters(running: &Running, block_id: &str) -> Vec<(String, String)> {
    running
        .manager
        .pipeline()
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.name().starts_with(&format!("{block_id}:videoconvert")))
        .map(|e| {
            let factory = e
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();
            (e.name().to_string(), factory)
        })
        .collect()
}

/// The factories between input 0's front and its `videoconvert`: what the
/// front spliced in.
fn input_adapters(running: &Running, block_id: &str) -> Vec<String> {
    let front = format!("{block_id}:video_in_0_front");
    let mut factories = Vec::new();
    let mut next = running.feeder_of(&running.mixer_element("videoconvert_0"));
    while let Some(element) = next {
        if element.name() == front {
            return factories;
        }
        factories.insert(
            0,
            element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default(),
        );
        assert!(factories.len() < 5, "input 0 has a loop: {factories:?}");
        next = running.feeder_of(&element);
    }
    panic!("videoconvert_0 is not fed from input 0's front: {factories:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn gl_memory_on_an_input_is_downloaded_by_its_front() {
    force_gpu_convert_mode();
    gst::init().unwrap();
    let mut required = CPU_ELEMENTS.to_vec();
    required.extend(["gltestsrc", "gldownload"]);
    if !common::gl_available(&required) {
        return;
    }
    let block_id = "vm_cpu_gl_in";
    let mut flow = Flow::new("vm_cpu_gl_input");
    flow.blocks.push(mixer_block(block_id, Backend::Cpu));
    flow.elements.push(elem(
        "src",
        "gltestsrc",
        vec![
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "srccaps",
        "capsfilter",
        vec![(
            "caps",
            PV::String(format!(
                "video/x-raw(memory:GLMemory),format=RGBA,width={W},height={H},framerate=30/1"
            )),
        )],
    ));
    link(&mut flow, "src:src", "srccaps:sink");
    link(&mut flow, "srccaps:src", &format!("{block_id}:video_in_0"));
    add_pgm_tap(&mut flow, block_id);

    let running = Running::start(block_id, flow);
    running.wait_for_white_pgm("GL memory on video_in_0 of the CPU mixer", 3);

    // The linker's GL-download retry (#802) also rescues a refused link, so a
    // white PGM alone would not show which side adapted. The input's own
    // front has to.
    assert_eq!(
        input_adapters(&running, block_id),
        ["gldownload", "capsfilter"],
        "input 0's front must download the GL memory itself, pinned to system memory"
    );

    let not_plain: Vec<_> = built_converters(&running, block_id)
        .into_iter()
        .filter(|(_, factory)| factory != "videoconvert")
        .collect();
    assert!(
        not_plain.is_empty(),
        "the CPU mixer built {not_plain:?} in the GPU convert mode; its converters \
         must be plain videoconvert, with GPU memory left to each input's front"
    );
    running.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn system_memory_on_an_input_passes_without_a_download() {
    force_gpu_convert_mode();
    gst::init().unwrap();
    if !common::gl_available(CPU_ELEMENTS) {
        return;
    }
    let block_id = "vm_cpu_sys_in";
    let running = Running::start(
        block_id,
        testsrc_flow(block_id, Backend::Cpu, Front::Direct),
    );
    running.wait_for_white_pgm("system memory on video_in_0 of the CPU mixer", 3);
    assert!(
        input_adapters(&running, block_id).is_empty(),
        "system memory needs no download between input 0's front and its videoconvert"
    );
    running.stop();
}
