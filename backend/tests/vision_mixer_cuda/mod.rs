//! The Media Player -> GPU Vision Mixer harness shared by the CUDA input tests.
//!
//! Only the Media Player's elements are started: the mixer stays in NULL, so no
//! GL context is needed. The queries that decide the input front happen while
//! the Media Player's `identity` settles its caps, before any CAPS event or
//! buffer reaches the mixer.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};

/// The GL elements the GPU Vision Mixer is built from. Their absence is a CI
/// regression, not a reason to skip: a skip would pass green guarding nothing.
pub const GL_ELEMENTS: &[&str] = &["glupload", "glcolorconvert", "glvideomixerelement"];

pub const MIXER: &str = "vm";
pub const PLAYER: &str = "player";

pub const CUDA_CAPS: &str = "video/x-raw(memory:CUDAMemory),format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";
pub const SYSTEM_CAPS: &str = "video/x-raw,format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";

pub fn block(
    id: &str,
    definition: &str,
    properties: Vec<(&str, PV)>,
) -> strom_types::BlockInstance {
    let properties: HashMap<String, PV> = properties
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let computed_external_pads = strom::blocks::builtin::get_builder(definition)
        .and_then(|builder| builder.get_external_pads(&properties));
    strom_types::BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads,
    }
}

/// A Media Player (decode mode, empty playlist) feeding `mixer_input` of a GPU
/// Vision Mixer.
pub fn player_into_mixer(name: &str, mixer_input: &str) -> Flow {
    let mut flow = Flow::new(name.to_string());
    flow.blocks.push(block(
        MIXER,
        "builtin.vision_mixer",
        vec![
            ("compositor_preference", PV::String("gpu".to_string())),
            ("num_inputs", PV::UInt(2)),
            ("num_dsk_inputs", PV::String("1".to_string())),
            ("pgm_resolution", PV::String("320x240".to_string())),
            ("multiview_resolution", PV::String("320x240".to_string())),
        ],
    ));
    flow.blocks.push(block(
        PLAYER,
        "builtin.media_player",
        vec![("decode", PV::Bool(true))],
    ));
    flow.links.push(strom_types::Link {
        from: format!("{}:video_out", PLAYER),
        to: format!("{}:{}", MIXER, mixer_input),
    });
    flow
}

pub fn build_manager(flow: &Flow) -> PipelineManager {
    gst::init().unwrap();
    strom::gpu::detect_gpu_capabilities();
    crate::common::manager::build(flow).expect("GPU vision mixer flow should build")
}

pub fn element(pipeline: &gst::Pipeline, name: &str) -> gst::Element {
    pipeline
        .by_name(name)
        .unwrap_or_else(|| panic!("{} exists", name))
}

/// The element feeding `name`'s sink pad.
pub fn feeder_of(pipeline: &gst::Pipeline, name: &str) -> Option<gst::Element> {
    element(pipeline, name)
        .static_pad("sink")?
        .peer()?
        .parent_element()
}

pub fn factory_name(element: &gst::Element) -> String {
    element
        .factory()
        .map(|f| f.name().to_string())
        .unwrap_or_default()
}

/// What the Media Player's output did with one frame.
pub struct Outcome {
    /// The caps its `video_out` settled on, if it negotiated with the mixer.
    pub negotiated: Option<gst::Caps>,
    /// Every error on the bus, as (source name, message, debug).
    pub errors: Vec<(String, String, String)>,
}

/// Start only the Media Player's main-pipeline elements and push one frame
/// with `caps` from its `appsrc`, the way its bridge does with each decoded
/// sample. The mixer stays in NULL. Waits until the Media Player's output has
/// negotiated or the bus carries an error, at most 5 s.
pub fn push_from_player(pipeline: &gst::Pipeline, caps: &str) -> Outcome {
    let names = ["appsrc_video", "queue_video", "video_out"].map(|n| format!("{}:{}", PLAYER, n));
    for name in names.iter().rev() {
        let e = element(pipeline, name);
        e.set_locked_state(true);
        e.set_state(gst::State::Playing)
            .unwrap_or_else(|e| panic!("{} could not start: {:?}", name, e));
    }

    let caps: gst::Caps = caps.parse().unwrap();
    let mut buffer = gst::Buffer::with_size(320 * 240 * 3 / 2).unwrap();
    buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let sample = gst::Sample::builder().buffer(&buffer).caps(&caps).build();
    let appsrc = element(pipeline, &names[0])
        .downcast::<gstreamer_app::AppSrc>()
        .unwrap();
    appsrc
        .push_sample(&sample)
        .expect("appsrc takes the sample");

    let video_out_src = element(pipeline, &names[2]).static_pad("src").unwrap();
    let bus = pipeline.bus().unwrap();
    let mut errors = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut settle_until = None;
    loop {
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(err) = msg.view() {
                errors.push((
                    msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    err.error().to_string(),
                    err.debug().map(|d| d.to_string()).unwrap_or_default(),
                ));
            }
        }
        if video_out_src.current_caps().is_some() && settle_until.is_none() {
            break;
        }
        // After the first error give the others (the Media Player's own
        // not-negotiated) a moment to land too.
        if !errors.is_empty() && settle_until.is_none() {
            settle_until = Some(Instant::now() + Duration::from_millis(200));
        }
        let now = Instant::now();
        if now >= deadline || settle_until.is_some_and(|t| now >= t) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let negotiated = video_out_src.current_caps();

    for name in &names {
        let e = element(pipeline, name);
        let _ = e.set_state(gst::State::Null);
        e.set_locked_state(false);
    }
    Outcome { negotiated, errors }
}
