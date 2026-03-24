//! Pipeline construction for the vision mixer block.

use super::elements::{self, CompositorBackend};
use super::layout;
use super::overlay::{self, VisionMixerOverlayState};
use super::properties;
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use strom_types::vision_mixer;
use strom_types::{
    block::{ExternalPad, ExternalPads},
    element::ElementPadRef,
    MediaType, PropertyValue,
};
use tracing::info;

/// Vision Mixer block builder.
pub struct VisionMixerBuilder;

impl BlockBuilder for VisionMixerBuilder {
    fn get_external_pads(&self, props: &HashMap<String, PropertyValue>) -> Option<ExternalPads> {
        let num_inputs = properties::parse_num_inputs(props);
        let num_dsk = properties::parse_num_dsk_inputs(props);

        // Internal element IDs are bare names — block expansion adds the instance_id prefix
        let mut inputs: Vec<ExternalPad> = (0..num_inputs)
            .map(|i| {
                ExternalPad::with_label(
                    format!("video_in_{}", i),
                    format!("V{}", i),
                    MediaType::Video,
                    format!("queue_{}", i),
                    "sink",
                )
            })
            .collect();

        // DSK input pads
        for i in 0..num_dsk {
            inputs.push(ExternalPad::with_label(
                format!("dsk_in_{}", i),
                format!("DSK{}", i + 1),
                MediaType::Video,
                format!("queue_dsk_{}", i),
                "sink",
            ));
        }

        let outputs = vec![
            ExternalPad::with_label("pgm_out", "PGM", MediaType::Video, "capsfilter_dist", "src"),
            ExternalPad::with_label(
                "multiview_out",
                "MV",
                MediaType::Video,
                "capsfilter_mv",
                "src",
            ),
        ];

        Some(ExternalPads { inputs, outputs })
    }

    fn build(
        &self,
        instance_id: &str,
        props: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        let num_inputs = properties::parse_num_inputs(props);
        let pgm_input = properties::parse_initial_pgm(props, num_inputs);
        let pvw_input = properties::parse_initial_pvw(props, num_inputs);
        let labels = properties::parse_input_labels(props, num_inputs);
        let force_live = properties::parse_bool(props, "force_live", true);
        let latency_ms = properties::parse_u64(props, "latency", 200);
        let min_upstream_ms = properties::parse_u64(props, "min_upstream_latency", 200);
        let (pgm_w, pgm_h) = properties::parse_resolution(
            props,
            "pgm_resolution",
            vision_mixer::DEFAULT_PGM_RESOLUTION,
        );
        let (mv_w, mv_h) = properties::parse_resolution(
            props,
            "multiview_resolution",
            vision_mixer::DEFAULT_MULTIVIEW_RESOLUTION,
        );

        let num_dsk_inputs = properties::parse_num_dsk_inputs(props);

        let pref = props
            .get("compositor_preference")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");
        let backend = elements::select_backend(pref)?;

        info!(
            "Building vision mixer: {} inputs, PGM={}x{}, MV={}x{}, backend={:?}, pgm={}, pvw={}",
            num_inputs, pgm_w, pgm_h, mv_w, mv_h, backend, pgm_input, pvw_input
        );

        let p = PipelineParams {
            instance_id,
            num_inputs,
            num_dsk_inputs,
            pgm_input,
            pvw_input,
            labels: &labels,
            force_live,
            latency_ms,
            min_upstream_ms,
            pgm_w,
            pgm_h,
            mv_w,
            mv_h,
            backend,
        };

        match backend {
            CompositorBackend::OpenGL => build_gpu_pipeline(&p, ctx),
            CompositorBackend::Software => build_cpu_pipeline(&p, ctx),
        }
    }
}

/// Shared parameters for pipeline construction.
struct PipelineParams<'a> {
    instance_id: &'a str,
    num_inputs: usize,
    num_dsk_inputs: usize,
    pgm_input: usize,
    pvw_input: usize,
    labels: &'a [String],
    force_live: bool,
    latency_ms: u64,
    min_upstream_ms: u64,
    pgm_w: u32,
    pgm_h: u32,
    mv_w: u32,
    mv_h: u32,
    backend: CompositorBackend,
}

impl<'a> PipelineParams<'a> {
    /// Create a namespaced element ID.
    fn id(&self, name: &str) -> String {
        format!("{}:{}", self.instance_id, name)
    }
}

// ============================================================================
// GPU Pipeline
// ============================================================================

fn build_gpu_pipeline(
    p: &PipelineParams,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    let mut elems: Vec<(String, gst::Element)> = Vec::new();
    let mut links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

    // --- Create compositors (no pre-requested pads — the linker auto-creates them) ---
    let dist_comp =
        elements::make_dist_compositor(p.backend, p.force_live, p.latency_ms, p.min_upstream_ms)?;
    let mv_comp =
        elements::make_mv_compositor(p.backend, p.force_live, p.latency_ms, p.min_upstream_ms)?;

    dist_comp.set_property("name", p.id("mixer"));
    mv_comp.set_property("name", p.id("mv_comp"));

    let mixer_id = p.id("mixer");
    let mv_comp_id = p.id("mv_comp");
    elems.push((mixer_id.clone(), dist_comp));
    elems.push((mv_comp_id.clone(), mv_comp));

    // Compute multiview layout
    let mv_layout = layout::compute_layout(p.mv_w, p.mv_h, p.num_inputs);

    // --- Distribution output chain: mixer → tee_pgm → gldownload → capsfilter ---
    // tee_pgm splits the PGM output: one branch to distribution, one to multiview PGM display.
    let tee_pgm_id = p.id("tee_pgm");
    let tee_pgm = elements::make_tee(&tee_pgm_id)?;
    let dl_dist_id = p.id("gldownload_dist");
    let gldownload_dist = elements::make_element("gldownload", "gldownload_dist")?;
    gldownload_dist.set_property("name", &dl_dist_id);
    let cf_dist_id = p.id("capsfilter_dist");
    let capsfilter_dist = elements::make_capsfilter("capsfilter_dist", p.pgm_w, p.pgm_h)?;
    capsfilter_dist.set_property("name", &cf_dist_id);
    elems.push((tee_pgm_id.clone(), tee_pgm));
    elems.push((dl_dist_id.clone(), gldownload_dist));
    elems.push((cf_dist_id.clone(), capsfilter_dist));
    links.push((
        ElementPadRef::pad(&mixer_id, "src"),
        ElementPadRef::pad(&tee_pgm_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&tee_pgm_id, "src_0"),
        ElementPadRef::pad(&dl_dist_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&dl_dist_id, "src"),
        ElementPadRef::pad(&cf_dist_id, "sink"),
    ));

    // DSK input element chains (elements only, links to mixer added later after video inputs)
    for i in 0..p.num_dsk_inputs {
        let q_id = p.id(&format!("queue_dsk_{}", i));
        let up_id = p.id(&format!("glupload_dsk_{}", i));
        let cc_id = p.id(&format!("glcolorconvert_dsk_{}", i));

        let queue = elements::make_queue(&q_id)?;
        let glupload = elements::make_element("glupload", &up_id)?;
        let glcolorconvert = elements::make_element("glcolorconvert", &cc_id)?;

        elems.push((q_id.clone(), queue));
        elems.push((up_id.clone(), glupload));
        elems.push((cc_id.clone(), glcolorconvert));

        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&up_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&up_id, "src"),
            ElementPadRef::pad(&cc_id, "sink"),
        ));
        // NOTE: link to mixer is added later, after video input links, to ensure correct pad order
    }

    // --- Multiview output chain ---
    let dl_id = p.id("gldownload_mv");
    let vc_id = p.id("videoconvert_pre_cairo");
    let co_id = p.id("cairooverlay");
    let cf_mv_id = p.id("capsfilter_mv");

    let gldownload_mv = elements::make_element("gldownload", "gldownload_mv")?;
    gldownload_mv.set_property("name", &dl_id);
    let videoconvert_mv = elements::make_element("videoconvert", "videoconvert_pre_cairo")?;
    videoconvert_mv.set_property("name", &vc_id);
    let cairooverlay = elements::make_element("cairooverlay", "cairooverlay")?;
    cairooverlay.set_property("name", &co_id);
    let capsfilter_mv = elements::make_capsfilter("capsfilter_mv", p.mv_w, p.mv_h)?;
    capsfilter_mv.set_property("name", &cf_mv_id);

    elems.push((dl_id.clone(), gldownload_mv));
    elems.push((vc_id.clone(), videoconvert_mv));
    elems.push((co_id.clone(), cairooverlay.clone()));
    elems.push((cf_mv_id.clone(), capsfilter_mv));

    links.push((
        ElementPadRef::pad(&mv_comp_id, "src"),
        ElementPadRef::pad(&dl_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&dl_id, "src"),
        ElementPadRef::pad(&vc_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&vc_id, "src"),
        ElementPadRef::pad(&co_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&co_id, "src"),
        ElementPadRef::pad(&cf_mv_id, "sink"),
    ));

    // --- Per-input elements ---
    for i in 0..p.num_inputs {
        let q_id = p.id(&format!("queue_{}", i));
        let up_id = p.id(&format!("glupload_{}", i));
        let cc_id = p.id(&format!("glcolorconvert_{}", i));
        let tee_id = p.id(&format!("tee_{}", i));

        let queue = elements::make_queue(&q_id)?;
        let glupload = elements::make_element("glupload", &up_id)?;
        let glcolorconvert = elements::make_element("glcolorconvert", &cc_id)?;
        let tee = elements::make_tee(&tee_id)?;

        elems.push((q_id.clone(), queue));
        elems.push((up_id.clone(), glupload));
        elems.push((cc_id.clone(), glcolorconvert));
        elems.push((tee_id.clone(), tee));

        // queue → glupload → glcolorconvert → tee
        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&up_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&up_id, "src"),
            ElementPadRef::pad(&cc_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&cc_id, "src"),
            ElementPadRef::pad(&tee_id, "sink"),
        ));
    }

    // --- Compositor links (order matters: linker auto-creates sink pads sequentially) ---
    // Distribution compositor: video inputs first (sink_0..N-1), then DSK (sink_N..N+dsk-1)
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_0"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", i)),
        ));
    }
    // DSK inputs on dist compositor (after video inputs)
    for i in 0..p.num_dsk_inputs {
        let cc_id = p.id(&format!("glcolorconvert_dsk_{}", i));
        links.push((
            ElementPadRef::pad(&cc_id, "src"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", p.num_inputs + i)),
        ));
    }

    // Multiview compositor thumbnails: tee_i.src_1 → mv_comp (creates sink_0..sink_{N-1})
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_1"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", i)),
        ));
    }

    // Multiview PGM big display: tee_pgm.src_1 → mv_comp.sink_N
    // Shows the actual dist_comp output (transitions, DSK visible)
    links.push((
        ElementPadRef::pad(&tee_pgm_id, "src_1"),
        ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs)),
    ));

    // Multiview PVW big candidates: tee_i.src_2 → mv_comp.sink_{N+1+i}
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_2"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs + 1 + i)),
        ));
    }

    // --- Pad properties (applied after linking when auto-created pads exist) ---
    let pad_properties = build_pad_properties(p, &mv_layout);

    // --- Set up cairooverlay draw signal ---
    setup_cairo_overlay(p, &cairooverlay, &mv_layout, ctx);

    info!(
        "Vision mixer GPU pipeline built: {} inputs, PGM={}x{}, MV={}x{}",
        p.num_inputs, p.pgm_w, p.pgm_h, p.mv_w, p.mv_h
    );

    Ok(BlockBuildResult {
        elements: elems,
        internal_links: links,
        bus_message_handler: None,
        pad_properties,
    })
}

// ============================================================================
// CPU Pipeline
// ============================================================================

fn build_cpu_pipeline(
    p: &PipelineParams,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    let mut elems: Vec<(String, gst::Element)> = Vec::new();
    let mut links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

    let dist_comp =
        elements::make_dist_compositor(p.backend, p.force_live, p.latency_ms, p.min_upstream_ms)?;
    let mv_comp =
        elements::make_mv_compositor(p.backend, p.force_live, p.latency_ms, p.min_upstream_ms)?;

    dist_comp.set_property("name", p.id("mixer"));
    mv_comp.set_property("name", p.id("mv_comp"));

    let mixer_id = p.id("mixer");
    let mv_comp_id = p.id("mv_comp");
    elems.push((mixer_id.clone(), dist_comp));
    elems.push((mv_comp_id.clone(), mv_comp));

    let mv_layout = layout::compute_layout(p.mv_w, p.mv_h, p.num_inputs);

    // --- Distribution output chain: mixer → [dsk_comp →] tee_pgm → capsfilter ---
    let dsk_comp_id = p.id("dsk_comp");
    if p.num_dsk_inputs > 0 {
        let dsk_comp = elements::make_dist_compositor(
            p.backend,
            p.force_live,
            p.latency_ms,
            p.min_upstream_ms,
        )?;
        dsk_comp.set_property("name", dsk_comp_id.clone());
        elems.push((dsk_comp_id.clone(), dsk_comp));
        links.push((
            ElementPadRef::pad(&mixer_id, "src"),
            ElementPadRef::pad(&dsk_comp_id, "sink_0".to_string()),
        ));
    }

    let tee_pgm_id = p.id("tee_pgm");
    let tee_pgm = elements::make_tee(&tee_pgm_id)?;
    let cf_dist_id = p.id("capsfilter_dist");
    let capsfilter_dist = elements::make_capsfilter("capsfilter_dist", p.pgm_w, p.pgm_h)?;
    capsfilter_dist.set_property("name", &cf_dist_id);
    elems.push((tee_pgm_id.clone(), tee_pgm));
    elems.push((cf_dist_id.clone(), capsfilter_dist));

    links.push((
        ElementPadRef::pad(&mixer_id, "src"),
        ElementPadRef::pad(&tee_pgm_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&tee_pgm_id, "src_0"),
        ElementPadRef::pad(&cf_dist_id, "sink"),
    ));

    // DSK input element chains (links to mixer added later after video inputs)
    for i in 0..p.num_dsk_inputs {
        let q_id = p.id(&format!("queue_dsk_{}", i));
        let vc_id_dsk = p.id(&format!("videoconvert_dsk_{}", i));

        let queue = elements::make_queue(&q_id)?;
        let videoconvert = elements::make_element("videoconvert", &vc_id_dsk)?;

        elems.push((q_id.clone(), queue));
        elems.push((vc_id_dsk.clone(), videoconvert));

        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&vc_id_dsk, "sink"),
        ));
    }

    // --- Multiview output chain (no gldownload needed for CPU) ---
    let vc_id = p.id("videoconvert_pre_cairo");
    let co_id = p.id("cairooverlay");
    let cf_mv_id = p.id("capsfilter_mv");

    let videoconvert_mv = elements::make_element("videoconvert", &vc_id)?;
    let cairooverlay = elements::make_element("cairooverlay", &co_id)?;
    let capsfilter_mv = elements::make_capsfilter("capsfilter_mv", p.mv_w, p.mv_h)?;
    capsfilter_mv.set_property("name", &cf_mv_id);

    elems.push((vc_id.clone(), videoconvert_mv));
    elems.push((co_id.clone(), cairooverlay.clone()));
    elems.push((cf_mv_id.clone(), capsfilter_mv));

    links.push((
        ElementPadRef::pad(&mv_comp_id, "src"),
        ElementPadRef::pad(&vc_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&vc_id, "src"),
        ElementPadRef::pad(&co_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&co_id, "src"),
        ElementPadRef::pad(&cf_mv_id, "sink"),
    ));

    // --- Per-input elements ---
    for i in 0..p.num_inputs {
        let q_id = p.id(&format!("queue_{}", i));
        let vc_in_id = p.id(&format!("videoconvert_{}", i));
        let tee_id = p.id(&format!("tee_{}", i));

        let queue = elements::make_queue(&q_id)?;
        let videoconvert = elements::make_element("videoconvert", &vc_in_id)?;
        let tee = elements::make_tee(&tee_id)?;

        elems.push((q_id.clone(), queue));
        elems.push((vc_in_id.clone(), videoconvert));
        elems.push((tee_id.clone(), tee));

        // queue → videoconvert → tee
        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&vc_in_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&vc_in_id, "src"),
            ElementPadRef::pad(&tee_id, "sink"),
        ));
    }

    // --- Compositor links (grouped by compositor, order matters) ---
    // Distribution compositor: video inputs first, then DSK
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_0"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", i)),
        ));
    }
    for i in 0..p.num_dsk_inputs {
        let vc_id_dsk = p.id(&format!("videoconvert_dsk_{}", i));
        links.push((
            ElementPadRef::pad(&vc_id_dsk, "src"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", p.num_inputs + i)),
        ));
    }

    // Multiview compositor thumbnails
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_1"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", i)),
        ));
    }

    // Multiview PGM big display from tee_pgm (shows actual transitions)
    links.push((
        ElementPadRef::pad(&tee_pgm_id, "src_1"),
        ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs)),
    ));

    // Multiview PVW big candidates at offset N+1
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_2"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs + 1 + i)),
        ));
    }

    let pad_properties = build_pad_properties(p, &mv_layout);
    setup_cairo_overlay(p, &cairooverlay, &mv_layout, ctx);

    info!(
        "Vision mixer CPU pipeline built: {} inputs, PGM={}x{}, MV={}x{}",
        p.num_inputs, p.pgm_w, p.pgm_h, p.mv_w, p.mv_h
    );

    Ok(BlockBuildResult {
        elements: elems,
        internal_links: links,
        bus_message_handler: None,
        pad_properties,
    })
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Build pad_properties for compositor sink pads (applied after linking).
///
/// Since glvideomixerelement uses auto-created request pads (the linker uses
/// link_pads(src, None)), pads are created sequentially in link order.
/// We group links: dist sink_0..N-1, mv thumbnails sink_0..N-1, mv big sink_N..2N-1.
fn build_pad_properties(
    p: &PipelineParams,
    mv_layout: &layout::OverlayLayout,
) -> HashMap<String, HashMap<String, HashMap<String, PropertyValue>>> {
    let mut pad_props: HashMap<String, HashMap<String, HashMap<String, PropertyValue>>> =
        HashMap::new();

    let mixer_id = p.id("mixer");
    let mv_comp_id = p.id("mv_comp");
    let is_gl = p.backend == CompositorBackend::OpenGL;

    // --- Distribution compositor pad properties ---
    // Each input fills the full PGM canvas; only the active PGM input is visible (alpha=1)
    let dist_pads = pad_props.entry(mixer_id).or_default();
    for i in 0..p.num_inputs {
        let pad_name = format!("sink_{}", i);
        let props = dist_pads.entry(pad_name).or_default();
        let alpha = if i == p.pgm_input { 1.0 } else { 0.0 };
        props.insert("alpha".to_string(), PropertyValue::Float(alpha));
        props.insert("width".to_string(), PropertyValue::Int(p.pgm_w as i64));
        props.insert("height".to_string(), PropertyValue::Int(p.pgm_h as i64));
        if is_gl {
            props.insert(
                "sizing-policy".to_string(),
                PropertyValue::String("keep-aspect-ratio".to_string()),
            );
        }
    }

    // --- DSK pads on dist compositor (high zorder, above video inputs) ---
    for i in 0..p.num_dsk_inputs {
        let pad_name = format!("sink_{}", p.num_inputs + i);
        let props = dist_pads.entry(pad_name).or_default();
        props.insert("width".to_string(), PropertyValue::Int(p.pgm_w as i64));
        props.insert("height".to_string(), PropertyValue::Int(p.pgm_h as i64));
        props.insert("alpha".to_string(), PropertyValue::Float(1.0));
        props.insert("zorder".to_string(), PropertyValue::UInt(100 + i as u64));
        if is_gl {
            props.insert(
                "sizing-policy".to_string(),
                PropertyValue::String("keep-aspect-ratio".to_string()),
            );
        }
    }

    // --- Multiview compositor pad properties ---
    let mv_pads = pad_props.entry(mv_comp_id).or_default();

    // Thumbnail pads: sink_0..sink_{N-1}
    for i in 0..p.num_inputs {
        let pad_name = format!("sink_{}", i);
        let props = mv_pads.entry(pad_name).or_default();
        let (x, y, w, h) = layout::thumbnail_pad_position(mv_layout, i);
        props.insert("xpos".to_string(), PropertyValue::Int(x as i64));
        props.insert("ypos".to_string(), PropertyValue::Int(y as i64));
        props.insert("width".to_string(), PropertyValue::Int(w as i64));
        props.insert("height".to_string(), PropertyValue::Int(h as i64));
        props.insert("alpha".to_string(), PropertyValue::Float(1.0));
        props.insert("zorder".to_string(), PropertyValue::UInt(1));
        if is_gl {
            props.insert(
                "sizing-policy".to_string(),
                PropertyValue::String("keep-aspect-ratio".to_string()),
            );
        }
    }

    // PGM big display: sink_N (fed from tee_pgm, always visible at PGM position)
    {
        let pad_name = format!("sink_{}", p.num_inputs);
        let props = mv_pads.entry(pad_name).or_default();
        let (x, y, w, h) = layout::pgm_pad_position(mv_layout);
        props.insert("xpos".to_string(), PropertyValue::Int(x as i64));
        props.insert("ypos".to_string(), PropertyValue::Int(y as i64));
        props.insert("width".to_string(), PropertyValue::Int(w as i64));
        props.insert("height".to_string(), PropertyValue::Int(h as i64));
        props.insert("alpha".to_string(), PropertyValue::Float(1.0));
        props.insert("zorder".to_string(), PropertyValue::UInt(10));
        if is_gl {
            props.insert(
                "sizing-policy".to_string(),
                PropertyValue::String("keep-aspect-ratio".to_string()),
            );
        }
    }

    // PVW big display candidate pads: sink_{N+1}..sink_{2N}
    for i in 0..p.num_inputs {
        let pad_name = format!("sink_{}", p.num_inputs + 1 + i);
        let props = mv_pads.entry(pad_name).or_default();

        let (x, y, w, h) = if i == p.pvw_input {
            layout::pvw_pad_position(mv_layout)
        } else {
            (0, 0, 1, 1) // hidden
        };
        let alpha = if i == p.pvw_input { 1.0 } else { 0.0 };

        props.insert("xpos".to_string(), PropertyValue::Int(x as i64));
        props.insert("ypos".to_string(), PropertyValue::Int(y as i64));
        props.insert("width".to_string(), PropertyValue::Int(w as i64));
        props.insert("height".to_string(), PropertyValue::Int(h as i64));
        props.insert("alpha".to_string(), PropertyValue::Float(alpha));
        props.insert("zorder".to_string(), PropertyValue::UInt(10));
        if is_gl {
            props.insert(
                "sizing-policy".to_string(),
                PropertyValue::String("keep-aspect-ratio".to_string()),
            );
        }
    }

    pad_props
}

/// Set up the cairooverlay draw signal with shared state.
fn setup_cairo_overlay(
    p: &PipelineParams,
    cairooverlay: &gst::Element,
    mv_layout: &layout::OverlayLayout,
    ctx: &BlockBuildContext,
) {
    let overlay_state = Arc::new(VisionMixerOverlayState::new(
        p.num_inputs,
        p.pgm_input,
        p.pvw_input,
        p.labels.to_vec(),
        mv_layout.clone(),
    ));

    // Register the overlay state so the API layer can access it
    overlay::register_overlay_state(p.instance_id, Arc::clone(&overlay_state));

    let state_for_draw = Arc::clone(&overlay_state);
    let cairooverlay_clone = cairooverlay.clone();
    ctx.register_element_setup(Box::new(move |_flow_id, _events| {
        cairooverlay_clone.connect("draw", false, move |args| {
            // The cairooverlay "draw" signal provides the cairo context as the 2nd argument.
            // Extract raw pointer from GValue boxed type and wrap in cairo::Context.
            // SAFETY: The cairooverlay element guarantees the 2nd signal argument is a valid
            // cairo_t pointer during the "draw" callback. We borrow (from_raw_none) because
            // GStreamer owns the context and destroys it after the signal returns.
            unsafe {
                let value_ptr = args[1].as_ptr();
                let cr_ptr = gst::glib::gobject_ffi::g_value_get_boxed(value_ptr)
                    as *mut cairo::ffi::cairo_t;
                if !cr_ptr.is_null() {
                    let cr = cairo::Context::from_raw_none(cr_ptr);
                    overlay::draw_overlay(&state_for_draw, &cr);
                }
            }
            None
        });
    }));
}
