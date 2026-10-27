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

/// The factories between the front of the input `pad` and the `videoconvert`
/// named `convert`: what the front spliced in.
fn input_adapters(running: &Running, block_id: &str, pad: &str, convert: &str) -> Vec<String> {
    let front = format!("{block_id}:{pad}_front");
    let mut factories = Vec::new();
    let mut next = running.feeder_of(&running.mixer_element(convert));
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
        assert!(factories.len() < 5, "{pad} has a loop: {factories:?}");
        next = running.feeder_of(&element);
    }
    panic!("{convert} is not fed from {pad}'s front: {factories:?}");
}

/// A `gltestsrc` emitting GL RGBA, named `id`, linked to `to`.
fn add_gl_source(flow: &mut Flow, id: &str, to: &str) {
    flow.elements.push(elem(
        id,
        "gltestsrc",
        vec![
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    let caps = format!("{id}_caps");
    flow.elements.push(elem(
        &caps,
        "capsfilter",
        vec![(
            "caps",
            PV::String(format!(
                "video/x-raw(memory:GLMemory),format=RGBA,width={W},height={H},framerate=30/1"
            )),
        )],
    ));
    link(flow, &format!("{id}:src"), &format!("{caps}:sink"));
    link(flow, &format!("{caps}:src"), to);
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
    add_gl_source(&mut flow, "src", &format!("{block_id}:video_in_0"));
    add_pgm_tap(&mut flow, block_id);

    let running = Running::start(block_id, flow);
    running.wait_for_white_pgm("GL memory on video_in_0 of the CPU mixer", 3);

    // The linker's GL-download retry (#802) also rescues a refused link, so a
    // white PGM alone would not show which side adapted. The input's own
    // front has to.
    assert_eq!(
        input_adapters(&running, block_id, "video_in_0", "videoconvert_0"),
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
        input_adapters(&running, block_id, "video_in_0", "videoconvert_0").is_empty(),
        "system memory needs no download between input 0's front and its videoconvert"
    );
    running.stop();
}

/// The DSK and stinger inputs have their own fronts, and different consumers
/// behind their `videoconvert`: a compositor pad for a DSK, `videocrop` for the
/// stinger, whose sink template also lists `video/x-raw(ANY)`. GL memory on
/// either is downloaded by that input's front.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn gl_memory_on_a_dsk_or_stinger_input_is_downloaded_by_its_front() {
    force_gpu_convert_mode();
    gst::init().unwrap();
    let mut required = CPU_ELEMENTS.to_vec();
    required.extend(["gltestsrc", "gldownload"]);
    if !common::gl_available(&required) {
        return;
    }
    let block_id = "vm_cpu_gl_dsk_stinger";
    let mut flow = testsrc_flow(block_id, Backend::Cpu, Front::Direct);
    let mixer = &mut flow.blocks[0].properties;
    mixer.insert("num_dsk_inputs".to_string(), PV::String("1".into()));
    mixer.insert(
        strom_types::stinger::ENABLE_STINGER_PROPERTY.to_string(),
        PV::Bool(true),
    );
    // DSK and stinger pads depend on the properties, so the flow carries them
    // as the API would.
    flow.blocks[0].computed_external_pads =
        strom::blocks::builtin::get_builder("builtin.vision_mixer")
            .and_then(|builder| builder.get_external_pads(&flow.blocks[0].properties));
    add_gl_source(&mut flow, "dsksrc", &format!("{block_id}:dsk_in_0"));
    add_gl_source(
        &mut flow,
        "stingersrc",
        &format!("{block_id}:{}", strom_types::stinger::STINGER_INPUT_PAD),
    );

    let running = Running::start(block_id, flow);
    running.wait_for_white_pgm("video_in_0 beside GL DSK and stinger inputs", 3);
    for (pad, convert) in [
        ("dsk_in_0", "videoconvert_dsk_0"),
        (
            strom_types::stinger::STINGER_INPUT_PAD,
            "videoconvert_stinger",
        ),
    ] {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut adapters = input_adapters(&running, block_id, pad, convert);
        while adapters != ["gldownload", "capsfilter"] && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            adapters = input_adapters(&running, block_id, pad, convert);
        }
        assert_eq!(
            adapters,
            ["gldownload", "capsfilter"],
            "{pad}'s front must download the GL memory itself, pinned to system memory"
        );
    }
    running.stop();
}
