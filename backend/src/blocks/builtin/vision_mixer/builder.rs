//! Pipeline construction for the vision mixer block.

use super::elements::{self, CompositorBackend};
use super::layout;
use super::overlay::{self, OverlayRenderer, VisionMixerOverlayState};
use super::properties;
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use crate::gpu;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use strom_types::vision_mixer;
use strom_types::{
    block::{ExternalPad, ExternalPads},
    element::ElementPadRef,
    FlowId, MediaType, PropertyValue,
};
use tracing::{info, trace};

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

        // Audio input pads (one per video input, plus a dedicated PGM audio pad).
        // Each audio branch feeds a level element so the multiview overlay can
        // render per-input VU meters.
        for i in 0..num_inputs {
            inputs.push(ExternalPad::with_label(
                format!("audio_in_{}", i),
                format!("A{}", i),
                MediaType::Audio,
                format!("queue_audio_{}", i),
                "sink",
            ));
        }
        inputs.push(ExternalPad::with_label(
            "pgm_audio_in",
            "PGM Audio",
            MediaType::Audio,
            "queue_audio_pgm",
            "sink",
        ));

        let outputs = vec![
            ExternalPad::with_label("pgm_out", "PGM", MediaType::Video, "queue_dist_out", "src"),
            ExternalPad::with_label(
                "multiview_out",
                "MV",
                MediaType::Video,
                "queue_mv_out",
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
        let num_pips = properties::parse_num_pips(props);
        let pgm_input = properties::parse_initial_pgm(props, num_inputs);
        let pvw_input = properties::parse_initial_pvw(props, num_inputs);
        let pgm_source = properties::parse_initial_pgm_source(props, num_inputs, num_pips);
        let pvw_source = properties::parse_initial_pvw_source(props, num_inputs, num_pips);
        let labels = properties::parse_input_labels(props, num_inputs);
        let latency_ms = properties::parse_u64(props, "latency", vision_mixer::DEFAULT_LATENCY_MS);
        let min_upstream_ms = properties::parse_u64(
            props,
            "min_upstream_latency",
            vision_mixer::DEFAULT_MIN_UPSTREAM_LATENCY_MS,
        );
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
        let pgm_framerate = properties::parse_framerate(
            props,
            "pgm_framerate",
            vision_mixer::DEFAULT_PGM_FRAMERATE,
        );
        let mv_framerate = properties::parse_framerate(
            props,
            "multiview_framerate",
            vision_mixer::DEFAULT_MULTIVIEW_FRAMERATE,
        );

        let num_dsk_inputs = properties::parse_num_dsk_inputs(props);

        let pip_bg_inputs: Vec<Option<usize>> = (0..num_pips)
            .map(|i| properties::parse_pip_bg(props, i, num_inputs))
            .collect();
        // Legacy block properties expose `pip_X_overlays` as a flat input list
        // with auto-tile semantics. We hoist that into a single zone per PiP so
        // existing saved flows keep their overlays without explicit zone config.
        let pip_zones: Vec<Vec<strom_types::vision_mixer::Zone>> = (0..num_pips)
            .map(|i| {
                let sources = properties::parse_pip_overlays(
                    props,
                    i,
                    num_inputs,
                    pip_bg_inputs.get(i).copied().flatten(),
                );
                if sources.is_empty() {
                    Vec::new()
                } else {
                    vec![strom_types::vision_mixer::Zone {
                        rect: None,
                        capacity: None,
                        sources,
                    }]
                }
            })
            .collect();

        let output_format = properties::parse_output_format(props);
        let gl_download =
            properties::parse_bool(props, "gl_download", vision_mixer::DEFAULT_GL_DOWNLOAD);
        let show_vu_meters = properties::parse_show_vu_meters(props);

        let pref = props
            .get("compositor_preference")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");
        let backend = elements::select_backend(pref)?;

        info!(
            "Building vision mixer: {} inputs, PGM={}x{}@{}/{}, MV={}x{}@{}/{}, backend={:?}, pgm={}, pvw={}",
            num_inputs, pgm_w, pgm_h, pgm_framerate.0, pgm_framerate.1,
            mv_w, mv_h, mv_framerate.0, mv_framerate.1,
            backend, pgm_input, pvw_input
        );

        let p = PipelineParams {
            instance_id,
            num_inputs,
            num_dsk_inputs,
            num_pips,
            pgm_input,
            pvw_input,
            pgm_source,
            pvw_source,
            pip_bg_inputs: &pip_bg_inputs,
            pip_zones: &pip_zones,
            labels: &labels,
            latency_ms,
            min_upstream_ms,
            pgm_w,
            pgm_h,
            mv_w,
            mv_h,
            pgm_framerate,
            mv_framerate,
            backend,
            output_format,
            gl_download,
            show_vu_meters,
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
    num_pips: usize,
    pgm_input: usize,
    pvw_input: usize,
    pgm_source: strom_types::vision_mixer::Source,
    pvw_source: strom_types::vision_mixer::Source,
    pip_bg_inputs: &'a [Option<usize>],
    pip_zones: &'a [Vec<strom_types::vision_mixer::Zone>],
    labels: &'a [String],
    latency_ms: u64,
    min_upstream_ms: u64,
    pgm_w: u32,
    pgm_h: u32,
    mv_w: u32,
    mv_h: u32,
    pgm_framerate: (i32, i32),
    mv_framerate: (i32, i32),
    backend: CompositorBackend,
    output_format: Option<String>,
    gl_download: bool,
    show_vu_meters: bool,
}

impl<'a> PipelineParams<'a> {
    /// Create a namespaced element ID.
    fn id(&self, name: &str) -> String {
        format!("{}:{}", self.instance_id, name)
    }

    /// Build PGM output caps with resolution, framerate, and optional pixel format.
    fn pgm_caps(&self) -> gst::Caps {
        let mut builder = gst::Caps::builder("video/x-raw")
            .field("width", self.pgm_w as i32)
            .field("height", self.pgm_h as i32)
            .field(
                "framerate",
                gst::Fraction::new(self.pgm_framerate.0, self.pgm_framerate.1),
            )
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
        if let Some(ref fmt) = self.output_format {
            builder = builder.field("format", fmt.as_str());
        }
        builder.build()
    }

    /// Build multiview output caps with resolution, framerate, and optional pixel format.
    fn mv_caps(&self) -> gst::Caps {
        let mut builder = gst::Caps::builder("video/x-raw")
            .field("width", self.mv_w as i32)
            .field("height", self.mv_h as i32)
            .field(
                "framerate",
                gst::Fraction::new(self.mv_framerate.0, self.mv_framerate.1),
            )
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
        if let Some(ref fmt) = self.output_format {
            builder = builder.field("format", fmt.as_str());
        }
        builder.build()
    }

    /// Build PGM output caps for the GL-memory passthrough path.
    /// Constrains framerate/resolution without forcing a download to system memory.
    fn pgm_caps_glmem(&self) -> gst::Caps {
        let s = format!(
            "video/x-raw(memory:GLMemory),width={},height={},framerate={}/{},pixel-aspect-ratio=1/1",
            self.pgm_w, self.pgm_h, self.pgm_framerate.0, self.pgm_framerate.1
        );
        s.parse().expect("valid GL memory caps for PGM")
    }

    /// Build multiview output caps for the GL-memory passthrough path.
    fn mv_caps_glmem(&self) -> gst::Caps {
        let s = format!(
            "video/x-raw(memory:GLMemory),width={},height={},framerate={}/{},pixel-aspect-ratio=1/1",
            self.mv_w, self.mv_h, self.mv_framerate.0, self.mv_framerate.1
        );
        s.parse().expect("valid GL memory caps for MV")
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
    let dist_comp = elements::make_dist_compositor(p.backend, p.latency_ms, p.min_upstream_ms)?;
    let mv_comp = elements::make_mv_compositor(p.backend, p.latency_ms, p.min_upstream_ms)?;

    dist_comp.set_property("name", p.id("mixer"));
    mv_comp.set_property("name", p.id("mv_comp"));

    let mixer_id = p.id("mixer");
    let mv_comp_id = p.id("mv_comp");
    elems.push((mixer_id.clone(), dist_comp));
    elems.push((mv_comp_id.clone(), mv_comp));

    // Compute multiview layout
    let source_aspect = if p.pgm_h > 0 {
        p.pgm_w as f64 / p.pgm_h as f64
    } else {
        16.0 / 9.0
    };
    let mv_layout = layout::compute_layout(p.mv_w, p.mv_h, p.num_inputs, p.num_pips, source_aspect);

    // --- Distribution output chain ---
    // queue_post_dist decouples the compositor from downstream processing.
    // With gl_download=true:  mixer → queue_post_dist → tee_pgm → gldownload → capsfilter → queue_dist_out
    // With gl_download=false: mixer → queue_post_dist → tee_pgm → capsfilter(GLMemory) → queue_dist_out
    // The capsfilter on the false path enforces pgm_framerate/resolution while
    // keeping memory in GL — without it, the framerate property is silently ignored.
    let q_post_dist_id = p.id("queue_post_dist");
    let queue_post_dist = elements::make_queue(&q_post_dist_id)?;
    let tee_pgm_id = p.id("tee_pgm");
    let tee_pgm = elements::make_tee(&tee_pgm_id)?;
    let q_dist_out_id = p.id("queue_dist_out");
    let queue_dist_out = elements::make_queue(&q_dist_out_id)?;
    elems.push((q_post_dist_id.clone(), queue_post_dist));
    elems.push((tee_pgm_id.clone(), tee_pgm));
    elems.push((q_dist_out_id.clone(), queue_dist_out));
    links.push((
        ElementPadRef::pad(&mixer_id, "src"),
        ElementPadRef::pad(&q_post_dist_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&q_post_dist_id, "src"),
        ElementPadRef::pad(&tee_pgm_id, "sink"),
    ));
    if p.gl_download {
        let dl_dist_id = p.id("gldownload_dist");
        let gldownload_dist = elements::make_element("gldownload", "gldownload_dist")?;
        gldownload_dist.set_property("name", &dl_dist_id);
        let cf_dist_id = p.id("capsfilter_dist");
        let capsfilter_dist = gst::ElementFactory::make("capsfilter")
            .name(&cf_dist_id)
            .property("caps", p.pgm_caps())
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_dist: {}", e)))?;
        elems.push((dl_dist_id.clone(), gldownload_dist));
        elems.push((cf_dist_id.clone(), capsfilter_dist));
        links.push((
            ElementPadRef::pad(&tee_pgm_id, "src_0"),
            ElementPadRef::pad(&dl_dist_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&dl_dist_id, "src"),
            ElementPadRef::pad(&cf_dist_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&cf_dist_id, "src"),
            ElementPadRef::pad(&q_dist_out_id, "sink"),
        ));
    } else {
        // GL passthrough: insert capsfilter with GLMemory feature so pgm_framerate
        // is enforced without a download. The compositor's src pad will negotiate
        // to this rate via downstream caps propagation.
        let cf_dist_id = p.id("capsfilter_dist");
        let capsfilter_dist = gst::ElementFactory::make("capsfilter")
            .name(&cf_dist_id)
            .property("caps", p.pgm_caps_glmem())
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_dist: {}", e)))?;
        elems.push((cf_dist_id.clone(), capsfilter_dist));
        links.push((
            ElementPadRef::pad(&tee_pgm_id, "src_0"),
            ElementPadRef::pad(&cf_dist_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&cf_dist_id, "src"),
            ElementPadRef::pad(&q_dist_out_id, "sink"),
        ));
    }

    // Queue to decouple tee_pgm from the multiview compositor (separate thread)
    let q_pgm_mv_id = p.id("queue_pgm_mv");
    let queue_pgm_mv = elements::make_queue(&q_pgm_mv_id)?;
    elements::suppress_latency_query(&queue_pgm_mv);
    elems.push((q_pgm_mv_id.clone(), queue_pgm_mv));

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
    // queue_post_mv decouples the compositor from downstream processing.
    // With gl_download=true:  mv_comp → queue_post_mv → gldownload → capsfilter → tee_mv → queue_mv_out
    // With gl_download=false: mv_comp → queue_post_mv → capsfilter(GLMemory) → tee_mv → queue_mv_out
    // The capsfilter on the false path enforces multiview_framerate/resolution while
    // keeping memory in GL — without it, the framerate property is silently ignored.
    let q_post_mv_id = p.id("queue_post_mv");
    let queue_post_mv = elements::make_queue(&q_post_mv_id)?;
    let tee_mv_id = p.id("tee_mv");
    let tee_mv = elements::make_tee(&tee_mv_id)?;
    let q_mv_out_id = p.id("queue_mv_out");
    let queue_mv_out = elements::make_queue(&q_mv_out_id)?;

    elems.push((q_post_mv_id.clone(), queue_post_mv));
    elems.push((tee_mv_id.clone(), tee_mv));
    elems.push((q_mv_out_id.clone(), queue_mv_out));

    links.push((
        ElementPadRef::pad(&mv_comp_id, "src"),
        ElementPadRef::pad(&q_post_mv_id, "sink"),
    ));
    if p.gl_download {
        let dl_id = p.id("gldownload_mv");
        let gldownload_mv = elements::make_element("gldownload", "gldownload_mv")?;
        gldownload_mv.set_property("name", &dl_id);
        let cf_mv_id = p.id("capsfilter_mv");
        let capsfilter_mv = gst::ElementFactory::make("capsfilter")
            .name(&cf_mv_id)
            .property("caps", p.mv_caps())
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_mv: {}", e)))?;
        elems.push((dl_id.clone(), gldownload_mv));
        elems.push((cf_mv_id.clone(), capsfilter_mv));
        links.push((
            ElementPadRef::pad(&q_post_mv_id, "src"),
            ElementPadRef::pad(&dl_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&dl_id, "src"),
            ElementPadRef::pad(&cf_mv_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&cf_mv_id, "src"),
            ElementPadRef::pad(&tee_mv_id, "sink"),
        ));
    } else {
        // GL passthrough: insert capsfilter with GLMemory feature so mv_framerate
        // is enforced without a download.
        let cf_mv_id = p.id("capsfilter_mv");
        let capsfilter_mv = gst::ElementFactory::make("capsfilter")
            .name(&cf_mv_id)
            .property("caps", p.mv_caps_glmem())
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_mv: {}", e)))?;
        elems.push((cf_mv_id.clone(), capsfilter_mv));
        links.push((
            ElementPadRef::pad(&q_post_mv_id, "src"),
            ElementPadRef::pad(&cf_mv_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&cf_mv_id, "src"),
            ElementPadRef::pad(&tee_mv_id, "sink"),
        ));
    }
    links.push((
        ElementPadRef::pad(&tee_mv_id, "src_0"),
        ElementPadRef::pad(&q_mv_out_id, "sink"),
    ));

    // --- Overlay appsrc → glupload → mv_comp (composited in GPU at high zorder) ---
    let appsrc_overlay_id = p.id("appsrc_overlay");
    let overlay_caps_str = format!(
        "video/x-raw,format=RGBA,width={},height={},pixel-aspect-ratio=1/1,framerate={}/{},interlace-mode=progressive,multiview-mode=mono",
        p.mv_w, p.mv_h, p.mv_framerate.0, p.mv_framerate.1
    );
    let overlay_caps: gst::Caps = overlay_caps_str
        .parse()
        .map_err(|e| BlockBuildError::ElementCreation(format!("overlay caps: {}", e)))?;
    let appsrc_overlay = gst_app::AppSrc::builder()
        .name(&appsrc_overlay_id)
        .format(gst::Format::Time)
        .is_live(false)
        .automatic_eos(false)
        .do_timestamp(true)
        .max_buffers(2)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();

    // Overlay appsrc → queue → glupload → mv_comp.
    // No caps set on appsrc at build time — caps are pushed with the first sample
    // after the pipeline is PLAYING (GL context available). Same pattern as WHIP inputs.
    let q_overlay_id = p.id("queue_overlay");
    let up_overlay_id = p.id("glupload_overlay");
    let queue_overlay = elements::make_queue(&q_overlay_id)?;
    let glupload_overlay = elements::make_element("glupload", &up_overlay_id)?;

    elems.push((appsrc_overlay_id.clone(), appsrc_overlay.clone().upcast()));
    elems.push((q_overlay_id.clone(), queue_overlay));
    elems.push((up_overlay_id.clone(), glupload_overlay));

    links.push((
        ElementPadRef::pad(&appsrc_overlay_id, "src"),
        ElementPadRef::pad(&q_overlay_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&q_overlay_id, "src"),
        ElementPadRef::pad(&up_overlay_id, "sink"),
    ));
    // Link to mv_comp is added AFTER all other mv_comp links (pad ordering matters)

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

        // Queues after tee decouple input processing from compositor backpressure.
        // Without these, the tee pushes synchronously to all 3 compositors — if any
        // compositor blocks, glupload/glcolorconvert stall and the input queue fills.
        let q_dist_id = p.id(&format!("queue_to_dist_{}", i));
        let q_thumb_id = p.id(&format!("queue_to_mv_thumb_{}", i));
        let q_pvw_id = p.id(&format!("queue_to_mv_pvw_{}", i));
        elems.push((q_dist_id.clone(), elements::make_queue(&q_dist_id)?));
        elems.push((q_thumb_id.clone(), elements::make_queue(&q_thumb_id)?));
        elems.push((q_pvw_id.clone(), elements::make_queue(&q_pvw_id)?));

        // One queue per PiP tile per input — feeds the virtual PiP thumbnail pads on mv_comp.
        for pip_idx in 0..p.num_pips {
            let q_pip_id = p.id(&format!("queue_to_mv_pip_{}_{}", pip_idx, i));
            elems.push((q_pip_id.clone(), elements::make_queue(&q_pip_id)?));
        }

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
        let q_dist_id = p.id(&format!("queue_to_dist_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_0"),
            ElementPadRef::pad(&q_dist_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_dist_id, "src"),
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

    // Multiview compositor thumbnails: tee_i.src_1 → queue → mv_comp
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        let q_thumb_id = p.id(&format!("queue_to_mv_thumb_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_1"),
            ElementPadRef::pad(&q_thumb_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_thumb_id, "src"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", i)),
        ));
    }

    // Multiview PGM big display: tee_pgm.src_1 → queue_pgm_mv → mv_comp.sink_N
    links.push((
        ElementPadRef::pad(&tee_pgm_id, "src_1"),
        ElementPadRef::pad(&q_pgm_mv_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&q_pgm_mv_id, "src"),
        ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs)),
    ));

    // Multiview PVW big candidates: tee_i.src_2 → queue → mv_comp.sink_{N+1+i}
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        let q_pvw_id = p.id(&format!("queue_to_mv_pvw_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_2"),
            ElementPadRef::pad(&q_pvw_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_pvw_id, "src"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs + 1 + i)),
        ));
    }

    // PiP candidates: for each PiP tile p_idx, each input i gets one mv_comp pad
    // at sink_{2N+1 + p_idx*N + i}. Pad role (bg/insert/hidden) is decided by
    // alpha+geometry in build_pad_properties.
    for pip_idx in 0..p.num_pips {
        for i in 0..p.num_inputs {
            let tee_id = p.id(&format!("tee_{}", i));
            let q_pip_id = p.id(&format!("queue_to_mv_pip_{}_{}", pip_idx, i));
            let tee_src = format!("src_{}", 3 + pip_idx);
            let sink_idx = 2 * p.num_inputs + 1 + pip_idx * p.num_inputs + i;
            links.push((
                ElementPadRef::pad(&tee_id, &tee_src),
                ElementPadRef::pad(&q_pip_id, "sink"),
            ));
            links.push((
                ElementPadRef::pad(&q_pip_id, "src"),
                ElementPadRef::pad(&mv_comp_id, format!("sink_{}", sink_idx)),
            ));
        }
    }

    // Overlay pad: glupload_overlay → mv_comp (must be last link to get correct pad index)
    let overlay_pad_idx = 2 * p.num_inputs + 1 + p.num_pips * p.num_inputs;
    links.push((
        ElementPadRef::pad(&up_overlay_id, "src"),
        ElementPadRef::pad(&mv_comp_id, format!("sink_{}", overlay_pad_idx)),
    ));

    // --- Pad properties (applied after linking when auto-created pads exist) ---
    let pad_properties = build_pad_properties(p, &mv_layout);

    // --- Audio metering branches (per-input + PGM) ---
    append_audio_meter_chains(p, &mut elems, &mut links)?;

    // --- Set up overlay appsrc renderer ---
    let overlay_state = setup_overlay_renderer(p, &appsrc_overlay, &overlay_caps, &mv_layout, ctx);

    // --- Bus handler that pipes `level` messages into the overlay state ---
    let bus_message_handler = Some(build_meter_bus_handler(p.instance_id, overlay_state));

    info!(
        "Vision mixer GPU pipeline built: {} inputs, PGM={}x{}, MV={}x{}",
        p.num_inputs, p.pgm_w, p.pgm_h, p.mv_w, p.mv_h
    );

    Ok(BlockBuildResult {
        elements: elems,
        internal_links: links,
        bus_message_handler,
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

    let dist_comp = elements::make_dist_compositor(p.backend, p.latency_ms, p.min_upstream_ms)?;
    let mv_comp = elements::make_mv_compositor(p.backend, p.latency_ms, p.min_upstream_ms)?;

    dist_comp.set_property("name", p.id("mixer"));
    mv_comp.set_property("name", p.id("mv_comp"));

    let mixer_id = p.id("mixer");
    let mv_comp_id = p.id("mv_comp");
    elems.push((mixer_id.clone(), dist_comp));
    elems.push((mv_comp_id.clone(), mv_comp));

    let source_aspect = if p.pgm_h > 0 {
        p.pgm_w as f64 / p.pgm_h as f64
    } else {
        16.0 / 9.0
    };
    let mv_layout = layout::compute_layout(p.mv_w, p.mv_h, p.num_inputs, p.num_pips, source_aspect);

    // --- Distribution output chain: mixer → capsfilter_dist → tee_pgm → queue_dist_out ---
    // DSK inputs are composited on the main mixer (same as GPU path).
    // capsfilter_dist forces resolution (and optional pixel format) on the compositor output.
    let cf_dist_id = p.id("capsfilter_dist");
    let capsfilter_dist = gst::ElementFactory::make("capsfilter")
        .name(&cf_dist_id)
        .property("caps", p.pgm_caps())
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_dist: {}", e)))?;
    let tee_pgm_id = p.id("tee_pgm");
    let tee_pgm = elements::make_tee(&tee_pgm_id)?;
    let q_dist_out_id = p.id("queue_dist_out");
    let queue_dist_out = elements::make_queue(&q_dist_out_id)?;
    elems.push((cf_dist_id.clone(), capsfilter_dist));
    elems.push((tee_pgm_id.clone(), tee_pgm));
    elems.push((q_dist_out_id.clone(), queue_dist_out));

    links.push((
        ElementPadRef::pad(&mixer_id, "src"),
        ElementPadRef::pad(&cf_dist_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&cf_dist_id, "src"),
        ElementPadRef::pad(&tee_pgm_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&tee_pgm_id, "src_0"),
        ElementPadRef::pad(&q_dist_out_id, "sink"),
    ));

    // Queue + capsfilter to decouple tee_pgm from the multiview compositor.
    // The capsfilter breaks the caps negotiation cycle: PGM compositor → tee →
    // queue_pgm_mv → mv_comp → (feedback). Without it, tee forwards caps queries
    // to mv_comp which deadlocks waiting for its own src to negotiate.
    // leaky=upstream drops old buffers while mv_comp is still starting.
    let q_pgm_mv_id = p.id("queue_pgm_mv");
    let queue_pgm_mv = elements::make_queue(&q_pgm_mv_id)?;
    queue_pgm_mv.set_property_from_str("leaky", "upstream");
    queue_pgm_mv.set_property("max-size-buffers", 1u32);
    elements::suppress_latency_query(&queue_pgm_mv);
    let cf_pgm_mv_id = p.id("capsfilter_pgm_mv");
    let capsfilter_pgm_mv = gst::ElementFactory::make("capsfilter")
        .name(&cf_pgm_mv_id)
        .property("caps", p.pgm_caps())
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_pgm_mv: {}", e)))?;
    elems.push((q_pgm_mv_id.clone(), queue_pgm_mv));
    elems.push((cf_pgm_mv_id.clone(), capsfilter_pgm_mv));

    // DSK input element chains (links to mixer added later after video inputs)
    let vc_factory = gpu::video_convert_mode().element_name();
    for i in 0..p.num_dsk_inputs {
        let q_id = p.id(&format!("queue_dsk_{}", i));
        let vc_id_dsk = p.id(&format!("videoconvert_dsk_{}", i));

        let queue = elements::make_queue(&q_id)?;
        // Uses autovideoconvert on hosts with working CUDA-GL interop, plain
        // videoconvert elsewhere — same pattern as videoenc/videoformat. Needed
        // so that upstream GPU-memory sources (e.g. nvh264dec from efpsrt_input)
        // negotiate correctly against the CPU compositor backend.
        let videoconvert = elements::make_element(vc_factory, &vc_id_dsk)?;

        elems.push((q_id.clone(), queue));
        elems.push((vc_id_dsk.clone(), videoconvert));

        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&vc_id_dsk, "sink"),
        ));

        // When output_format is specified, force DSK inputs to match — same as video inputs.
        if let Some(ref fmt) = p.output_format {
            let cf_dsk_id = p.id(&format!("capsfilter_dsk_{}", i));
            let capsfilter_dsk = gst::ElementFactory::make("capsfilter")
                .name(&cf_dsk_id)
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", fmt.as_str())
                        .build(),
                )
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("capsfilter_dsk_{}: {}", i, e))
                })?;
            elems.push((cf_dsk_id.clone(), capsfilter_dsk));
            links.push((
                ElementPadRef::pad(&vc_id_dsk, "src"),
                ElementPadRef::pad(&cf_dsk_id, "sink"),
            ));
        }
    }

    // --- Multiview output chain (no gldownload needed for CPU) ---
    // Overlay is composited by mv_comp via appsrc pad (see below).
    // capsfilter_mv forces resolution (and optional pixel format) on the mv compositor output.
    let cf_mv_id = p.id("capsfilter_mv");
    let capsfilter_mv = gst::ElementFactory::make("capsfilter")
        .name(&cf_mv_id)
        .property("caps", p.mv_caps())
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_mv: {}", e)))?;
    let tee_mv_id = p.id("tee_mv");
    let tee_mv = elements::make_tee(&tee_mv_id)?;
    let q_mv_out_id = p.id("queue_mv_out");
    let queue_mv_out = elements::make_queue(&q_mv_out_id)?;

    elems.push((cf_mv_id.clone(), capsfilter_mv));
    elems.push((tee_mv_id.clone(), tee_mv));
    elems.push((q_mv_out_id.clone(), queue_mv_out));

    links.push((
        ElementPadRef::pad(&mv_comp_id, "src"),
        ElementPadRef::pad(&cf_mv_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&cf_mv_id, "src"),
        ElementPadRef::pad(&tee_mv_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&tee_mv_id, "src_0"),
        ElementPadRef::pad(&q_mv_out_id, "sink"),
    ));

    // --- Overlay appsrc → mv_comp (CPU compositor accepts raw BGRA directly) ---
    let appsrc_overlay_id = p.id("appsrc_overlay");
    let overlay_caps_str = format!(
        "video/x-raw,format=RGBA,width={},height={},pixel-aspect-ratio=1/1,framerate={}/{},interlace-mode=progressive,multiview-mode=mono",
        p.mv_w, p.mv_h, p.mv_framerate.0, p.mv_framerate.1
    );
    let overlay_caps: gst::Caps = overlay_caps_str
        .parse()
        .map_err(|e| BlockBuildError::ElementCreation(format!("overlay caps: {}", e)))?;
    let appsrc_overlay = gst_app::AppSrc::builder()
        .name(&appsrc_overlay_id)
        .format(gst::Format::Time)
        .is_live(false)
        .automatic_eos(false)
        .do_timestamp(true)
        .max_buffers(2)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();

    let q_overlay_id = p.id("queue_overlay");
    let queue_overlay = elements::make_queue(&q_overlay_id)?;
    let vc_overlay_id = p.id("videoconvert_overlay");
    let videoconvert_overlay = elements::make_element(vc_factory, &vc_overlay_id)?;

    elems.push((appsrc_overlay_id.clone(), appsrc_overlay.clone().upcast()));
    elems.push((q_overlay_id.clone(), queue_overlay));
    elems.push((vc_overlay_id.clone(), videoconvert_overlay));

    // Optional capsfilter to match compositor output format
    let overlay_last_id = if let Some(ref fmt) = p.output_format {
        let cf_overlay_id = p.id("capsfilter_overlay");
        let capsfilter_overlay = gst::ElementFactory::make("capsfilter")
            .name(&cf_overlay_id)
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", fmt.as_str())
                    .build(),
            )
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_overlay: {}", e)))?;
        elems.push((cf_overlay_id.clone(), capsfilter_overlay));

        links.push((
            ElementPadRef::pad(&appsrc_overlay_id, "src"),
            ElementPadRef::pad(&q_overlay_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_overlay_id, "src"),
            ElementPadRef::pad(&vc_overlay_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&vc_overlay_id, "src"),
            ElementPadRef::pad(&cf_overlay_id, "sink"),
        ));
        cf_overlay_id
    } else {
        links.push((
            ElementPadRef::pad(&appsrc_overlay_id, "src"),
            ElementPadRef::pad(&q_overlay_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_overlay_id, "src"),
            ElementPadRef::pad(&vc_overlay_id, "sink"),
        ));
        vc_overlay_id.clone()
    };
    // Link to mv_comp is added AFTER all other mv_comp links (pad ordering matters)

    // --- Per-input elements ---
    for i in 0..p.num_inputs {
        let q_id = p.id(&format!("queue_{}", i));
        let vc_in_id = p.id(&format!("videoconvert_{}", i));
        let tee_id = p.id(&format!("tee_{}", i));

        let queue = elements::make_queue(&q_id)?;
        let videoconvert = elements::make_element(vc_factory, &vc_in_id)?;
        let tee = elements::make_tee(&tee_id)?;

        elems.push((q_id.clone(), queue));
        elems.push((vc_in_id.clone(), videoconvert));

        // Capsfilter after videoconvert forces all inputs to the same format before tee split.
        // Without this, the two compositors negotiate independently and tee can't satisfy both.
        if let Some(ref fmt) = p.output_format {
            let cf_in_id = p.id(&format!("capsfilter_in_{}", i));
            let capsfilter_in = gst::ElementFactory::make("capsfilter")
                .name(&cf_in_id)
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", fmt.as_str())
                        .build(),
                )
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("capsfilter_in_{}: {}", i, e))
                })?;
            elems.push((cf_in_id.clone(), capsfilter_in));
            elems.push((tee_id.clone(), tee));

            links.push((
                ElementPadRef::pad(&q_id, "src"),
                ElementPadRef::pad(&vc_in_id, "sink"),
            ));
            links.push((
                ElementPadRef::pad(&vc_in_id, "src"),
                ElementPadRef::pad(&cf_in_id, "sink"),
            ));
            links.push((
                ElementPadRef::pad(&cf_in_id, "src"),
                ElementPadRef::pad(&tee_id, "sink"),
            ));
        } else {
            elems.push((tee_id.clone(), tee));

            links.push((
                ElementPadRef::pad(&q_id, "src"),
                ElementPadRef::pad(&vc_in_id, "sink"),
            ));
            links.push((
                ElementPadRef::pad(&vc_in_id, "src"),
                ElementPadRef::pad(&tee_id, "sink"),
            ));
        }

        // Queues after tee decouple input processing from compositor backpressure
        let q_dist_id = p.id(&format!("queue_to_dist_{}", i));
        let q_thumb_id = p.id(&format!("queue_to_mv_thumb_{}", i));
        let q_pvw_id = p.id(&format!("queue_to_mv_pvw_{}", i));
        elems.push((q_dist_id.clone(), elements::make_queue(&q_dist_id)?));
        elems.push((q_thumb_id.clone(), elements::make_queue(&q_thumb_id)?));
        elems.push((q_pvw_id.clone(), elements::make_queue(&q_pvw_id)?));

        // One queue per PiP tile per input — feeds the virtual PiP thumbnail pads on mv_comp.
        for pip_idx in 0..p.num_pips {
            let q_pip_id = p.id(&format!("queue_to_mv_pip_{}_{}", pip_idx, i));
            elems.push((q_pip_id.clone(), elements::make_queue(&q_pip_id)?));
        }
    }

    // --- Compositor links (grouped by compositor, order matters) ---
    // Distribution compositor: video inputs first, then DSK
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        let q_dist_id = p.id(&format!("queue_to_dist_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_0"),
            ElementPadRef::pad(&q_dist_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_dist_id, "src"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", i)),
        ));
    }
    for i in 0..p.num_dsk_inputs {
        let last_dsk_elem = if p.output_format.is_some() {
            p.id(&format!("capsfilter_dsk_{}", i))
        } else {
            p.id(&format!("videoconvert_dsk_{}", i))
        };
        links.push((
            ElementPadRef::pad(&last_dsk_elem, "src"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", p.num_inputs + i)),
        ));
    }

    // Multiview compositor thumbnails: tee_i.src_1 → queue → mv_comp
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        let q_thumb_id = p.id(&format!("queue_to_mv_thumb_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_1"),
            ElementPadRef::pad(&q_thumb_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_thumb_id, "src"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", i)),
        ));
    }

    // Multiview PVW big candidates: tee_i.src_2 → queue → mv_comp.sink_{N+1+i}
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        let q_pvw_id = p.id(&format!("queue_to_mv_pvw_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_2"),
            ElementPadRef::pad(&q_pvw_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_pvw_id, "src"),
            ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs + 1 + i)),
        ));
    }

    // PiP candidates: for each PiP tile p_idx, each input i gets one mv_comp pad
    // at sink_{2N+1 + p_idx*N + i}. Pad role (bg/insert/hidden) is decided by
    // alpha+geometry in build_pad_properties.
    for pip_idx in 0..p.num_pips {
        for i in 0..p.num_inputs {
            let tee_id = p.id(&format!("tee_{}", i));
            let q_pip_id = p.id(&format!("queue_to_mv_pip_{}_{}", pip_idx, i));
            let tee_src = format!("src_{}", 3 + pip_idx);
            let sink_idx = 2 * p.num_inputs + 1 + pip_idx * p.num_inputs + i;
            links.push((
                ElementPadRef::pad(&tee_id, &tee_src),
                ElementPadRef::pad(&q_pip_id, "sink"),
            ));
            links.push((
                ElementPadRef::pad(&q_pip_id, "src"),
                ElementPadRef::pad(&mv_comp_id, format!("sink_{}", sink_idx)),
            ));
        }
    }

    // Overlay pad: last overlay element → mv_comp (must be last link for correct pad index)
    let overlay_pad_idx = 2 * p.num_inputs + 1 + p.num_pips * p.num_inputs;
    links.push((
        ElementPadRef::pad(&overlay_last_id, "src"),
        ElementPadRef::pad(&mv_comp_id, format!("sink_{}", overlay_pad_idx)),
    ));

    // Multiview PGM big display: tee_pgm.src_1 → queue_pgm_mv → capsfilter_pgm_mv → mv_comp.sink_N
    // (capsfilter breaks caps query cycle back to PGM compositor)
    links.push((
        ElementPadRef::pad(&tee_pgm_id, "src_1"),
        ElementPadRef::pad(&q_pgm_mv_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&q_pgm_mv_id, "src"),
        ElementPadRef::pad(&cf_pgm_mv_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&cf_pgm_mv_id, "src"),
        ElementPadRef::pad(&mv_comp_id, format!("sink_{}", p.num_inputs)),
    ));

    let pad_properties = build_pad_properties(p, &mv_layout);

    // Audio metering branches (per-input + PGM)
    append_audio_meter_chains(p, &mut elems, &mut links)?;

    let overlay_state = setup_overlay_renderer(p, &appsrc_overlay, &overlay_caps, &mv_layout, ctx);
    let bus_message_handler = Some(build_meter_bus_handler(p.instance_id, overlay_state));

    info!(
        "Vision mixer CPU pipeline built: {} inputs, PGM={}x{}, MV={}x{}",
        p.num_inputs, p.pgm_w, p.pgm_h, p.mv_w, p.mv_h
    );

    Ok(BlockBuildResult {
        elements: elems,
        internal_links: links,
        bus_message_handler,
        pad_properties,
    })
}

// ============================================================================
// Audio metering chains
// ============================================================================

/// Append audio metering chains to the pipeline: one per video input plus a
/// dedicated PGM audio branch. Each chain is:
///   queue_audio_{i} → audioconvert → level_audio_{i} → fakesink_audio_{i}
///
/// The external pads (`audio_in_{i}` / `pgm_audio_in`) target the queue sinks,
/// so the chains are self-contained and don't need caps negotiation up-front.
fn append_audio_meter_chains(
    p: &PipelineParams,
    elems: &mut Vec<(String, gst::Element)>,
    links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<(), BlockBuildError> {
    for i in 0..p.num_inputs {
        let q_id = p.id(&format!("queue_audio_{}", i));
        let conv_id = p.id(&format!("audioconvert_audio_{}", i));
        let level_id = p.id(&format!("level_audio_{}", i));
        let sink_id = p.id(&format!("fakesink_audio_{}", i));

        elems.push((q_id.clone(), elements::make_queue(&q_id)?));
        elems.push((
            conv_id.clone(),
            elements::make_element("audioconvert", &conv_id)?,
        ));
        elems.push((level_id.clone(), elements::make_level(&level_id)?));
        elems.push((sink_id.clone(), elements::make_meter_fakesink(&sink_id)?));

        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&conv_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&conv_id, "src"),
            ElementPadRef::pad(&level_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&level_id, "src"),
            ElementPadRef::pad(&sink_id, "sink"),
        ));
    }

    // PGM audio branch.
    let q_id = p.id("queue_audio_pgm");
    let conv_id = p.id("audioconvert_audio_pgm");
    let level_id = p.id("level_audio_pgm");
    let sink_id = p.id("fakesink_audio_pgm");

    elems.push((q_id.clone(), elements::make_queue(&q_id)?));
    elems.push((
        conv_id.clone(),
        elements::make_element("audioconvert", &conv_id)?,
    ));
    elems.push((level_id.clone(), elements::make_level(&level_id)?));
    elems.push((sink_id.clone(), elements::make_meter_fakesink(&sink_id)?));

    links.push((
        ElementPadRef::pad(&q_id, "src"),
        ElementPadRef::pad(&conv_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&conv_id, "src"),
        ElementPadRef::pad(&level_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&level_id, "src"),
        ElementPadRef::pad(&sink_id, "sink"),
    ));

    Ok(())
}

/// Build the bus message handler that forwards `level` element messages into
/// the overlay state so VU meters can be rendered.
///
/// The handler owns an `Arc<VisionMixerOverlayState>` — never a `gst::Element`
/// or `gst::Pipeline` — so it doesn't create a reference cycle.
fn build_meter_bus_handler(
    instance_id: &str,
    overlay_state: Arc<VisionMixerOverlayState>,
) -> crate::blocks::BusMessageConnectFn {
    let input_level_prefix = format!("{}:level_audio_", instance_id);
    let pgm_level_name = format!("{}:level_audio_pgm", instance_id);

    Box::new(
        move |bus: &gst::Bus,
              _flow_id: FlowId,
              _events: EventBroadcaster|
              -> gst::glib::SignalHandlerId {
            bus.add_signal_watch();
            let state = overlay_state;
            bus.connect_message(None, move |_bus, msg| {
                use gst::MessageView;
                let MessageView::Element(element_msg) = msg.view() else {
                    return;
                };
                let Some(s) = element_msg.structure() else {
                    return;
                };
                if s.name() != "level" {
                    return;
                }
                let Some(src) = msg.src() else { return };
                let src_name = src.name();

                let peak = extract_level_array(s, "peak");
                let decay = extract_level_array(s, "decay");
                if peak.is_empty() {
                    return;
                }
                let peak_db = max_f64(&peak);
                // decay may be missing on some level implementations; fall back
                // to peak so the tick still tracks something sensible.
                let decay_db = if decay.is_empty() {
                    peak_db
                } else {
                    max_f64(&decay)
                };

                if src_name == pgm_level_name.as_str() {
                    trace!(
                        "vision_mixer PGM meter peak={:.1} decay={:.1}",
                        peak_db,
                        decay_db
                    );
                    state.set_pgm_levels(peak_db, decay_db);
                    return;
                }
                if let Some(rest) = src_name.strip_prefix(input_level_prefix.as_str()) {
                    if let Ok(idx) = rest.parse::<usize>() {
                        trace!(
                            "vision_mixer input {} meter peak={:.1} decay={:.1}",
                            idx,
                            peak_db,
                            decay_db
                        );
                        state.set_input_levels(idx, peak_db, decay_db);
                    }
                }
            })
        },
    )
}

fn extract_level_array(s: &gst::StructureRef, field: &str) -> Vec<f64> {
    use gstreamer::glib;
    s.get::<glib::ValueArray>(field)
        .map(|arr| arr.iter().filter_map(|v| v.get::<f64>().ok()).collect())
        .unwrap_or_default()
}

fn max_f64(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Compute initial pad geometry for `input` inside a PiP-rendered region.
///
/// Returns `(xpos, ypos, width, height, alpha, zorder)`. If `input` is the
/// PiP's bg → full region at `bg_zorder`. If `input` appears in any zone →
/// auto-tiled rect inside the zone at `overlay_zorder + slot_offset`.
/// Otherwise hidden (alpha=0).
fn initial_pad_geom_for_input(
    p: &PipelineParams,
    pip_idx: usize,
    input: usize,
    region: (i32, i32, i32, i32),
    bg_zorder: u32,
    overlay_zorder: u32,
    src_aspect: f64,
) -> (i32, i32, i32, i32, f64, u64) {
    let (rx, ry, rw, rh) = region;
    let bg = p.pip_bg_inputs.get(pip_idx).copied().flatten();
    if Some(input) == bg {
        return (rx, ry, rw, rh, 1.0, bg_zorder as u64);
    }
    let zones = p.pip_zones.get(pip_idx).map(Vec::as_slice).unwrap_or(&[]);
    let layouts = strom_types::vision_mixer::resolve_zone_pads(rx, ry, rw, rh, zones, src_aspect);
    if let Some(l) = layouts.iter().find(|l| l.input == input) {
        (
            l.x,
            l.y,
            l.w,
            l.h,
            1.0,
            (overlay_zorder + l.zorder_offset) as u64,
        )
    } else {
        (0, 0, 1, 1, 0.0, bg_zorder as u64)
    }
}

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

    // --- Distribution compositor pad properties ---
    // Each input has its alpha/geometry set based on the initial PGM source:
    //   Source::Input(active) → sink_active fills the canvas (alpha=1), others hidden
    //   Source::Pip(p)        → bg input fills the canvas, zone sources auto-tile on top
    use strom_types::vision_mixer::Source;
    let canvas_w = p.pgm_w as i32;
    let canvas_h = p.pgm_h as i32;
    // Source aspect for PiP-tile cell math (assumes inputs share the PGM aspect,
    // which is typical for broadcast workflows). resolve_zone_pads sizes each
    // tile to this aspect so `keep-aspect-ratio` pads fill cleanly without
    // transparent letterbox bands letting the bg peek through.
    let pgm_aspect = if canvas_h > 0 {
        canvas_w as f64 / canvas_h as f64
    } else {
        16.0 / 9.0
    };

    let dist_pads = pad_props.entry(mixer_id).or_default();
    for i in 0..p.num_inputs {
        let pad_name = format!("sink_{}", i);
        let props = dist_pads.entry(pad_name).or_default();

        let (x, y, w, h, alpha, zorder) = match p.pgm_source {
            Source::Input(active) => {
                let alpha = if i == active { 1.0 } else { 0.0 };
                (
                    0,
                    0,
                    canvas_w,
                    canvas_h,
                    alpha,
                    vision_mixer::DIST_PGM_ZORDER as u64,
                )
            }
            Source::Pip(pip_idx) => initial_pad_geom_for_input(
                p,
                pip_idx,
                i,
                (0, 0, canvas_w, canvas_h),
                vision_mixer::DIST_PGM_ZORDER,
                vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                pgm_aspect,
            ),
        };

        props.insert("alpha".to_string(), PropertyValue::Float(alpha));
        props.insert("xpos".to_string(), PropertyValue::Int(x as i64));
        props.insert("ypos".to_string(), PropertyValue::Int(y as i64));
        props.insert("width".to_string(), PropertyValue::Int(w as i64));
        props.insert("height".to_string(), PropertyValue::Int(h as i64));
        props.insert("zorder".to_string(), PropertyValue::UInt(zorder));
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("keep-aspect-ratio".to_string()),
        );
    }

    // --- DSK pads on dist compositor (high zorder, above video inputs) ---
    for i in 0..p.num_dsk_inputs {
        let pad_name = format!("sink_{}", p.num_inputs + i);
        let props = dist_pads.entry(pad_name).or_default();
        props.insert("width".to_string(), PropertyValue::Int(p.pgm_w as i64));
        props.insert("height".to_string(), PropertyValue::Int(p.pgm_h as i64));
        props.insert("alpha".to_string(), PropertyValue::Float(0.0));
        props.insert(
            "zorder".to_string(),
            PropertyValue::UInt(vision_mixer::DIST_DSK_BASE_ZORDER as u64 + i as u64),
        );
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("keep-aspect-ratio".to_string()),
        );
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
        props.insert(
            "zorder".to_string(),
            PropertyValue::UInt(vision_mixer::MV_THUMBNAIL_ZORDER as u64),
        );
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("keep-aspect-ratio".to_string()),
        );
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
        props.insert(
            "zorder".to_string(),
            PropertyValue::UInt(vision_mixer::MV_BIG_DISPLAY_ZORDER as u64),
        );
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("keep-aspect-ratio".to_string()),
        );
    }

    // PVW big display candidate pads: sink_{N+1}..sink_{2N}
    // Same dual treatment as the dist compositor: an Input source activates one
    // pad at the PVW rect; a Pip source uses bg+zone auto-tile inside the PVW rect.
    let pvw_rect = layout::pvw_pad_position(mv_layout);
    let (pvw_x, pvw_y, pvw_w, pvw_h) = pvw_rect;

    for i in 0..p.num_inputs {
        let pad_name = format!("sink_{}", p.num_inputs + 1 + i);
        let props = mv_pads.entry(pad_name).or_default();

        let (x, y, w, h, alpha, zorder) = match p.pvw_source {
            Source::Input(active) => {
                if i == active {
                    (
                        pvw_x,
                        pvw_y,
                        pvw_w,
                        pvw_h,
                        1.0,
                        vision_mixer::MV_BIG_DISPLAY_ZORDER as u64,
                    )
                } else {
                    (0, 0, 1, 1, 0.0, vision_mixer::MV_BIG_DISPLAY_ZORDER as u64)
                }
            }
            Source::Pip(pip_idx) => initial_pad_geom_for_input(
                p,
                pip_idx,
                i,
                (pvw_x, pvw_y, pvw_w, pvw_h),
                vision_mixer::MV_BIG_DISPLAY_ZORDER,
                vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                pgm_aspect,
            ),
        };

        props.insert("xpos".to_string(), PropertyValue::Int(x as i64));
        props.insert("ypos".to_string(), PropertyValue::Int(y as i64));
        props.insert("width".to_string(), PropertyValue::Int(w as i64));
        props.insert("height".to_string(), PropertyValue::Int(h as i64));
        props.insert("alpha".to_string(), PropertyValue::Float(alpha));
        props.insert("zorder".to_string(), PropertyValue::UInt(zorder));
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("keep-aspect-ratio".to_string()),
        );
    }

    // PiP candidate pads: sink_{2N+1 + pip_idx*N + i}. One per (PiP tile, input).
    // For each PiP tile, the bg input fills the tile (low zorder) and the zone
    // sources are auto-tiled in sub-rects on top (higher zorder, see
    // `resolve_zone_pads`). All other PiP pads stay alpha=0.
    for pip_idx in 0..p.num_pips {
        let (bg_x, bg_y, bg_w, bg_h) = layout::pip_bg_pad_position(mv_layout, pip_idx);

        for i in 0..p.num_inputs {
            let sink_idx = 2 * p.num_inputs + 1 + pip_idx * p.num_inputs + i;
            let pad_name = format!("sink_{}", sink_idx);
            let props = mv_pads.entry(pad_name).or_default();

            let (x, y, w, h, alpha, zorder) = initial_pad_geom_for_input(
                p,
                pip_idx,
                i,
                (bg_x, bg_y, bg_w, bg_h),
                vision_mixer::MV_PIP_BG_ZORDER,
                vision_mixer::MV_PIP_OVERLAY_ZORDER,
                pgm_aspect,
            );

            props.insert("xpos".to_string(), PropertyValue::Int(x as i64));
            props.insert("ypos".to_string(), PropertyValue::Int(y as i64));
            props.insert("width".to_string(), PropertyValue::Int(w as i64));
            props.insert("height".to_string(), PropertyValue::Int(h as i64));
            props.insert("alpha".to_string(), PropertyValue::Float(alpha));
            props.insert("zorder".to_string(), PropertyValue::UInt(zorder));
            props.insert(
                "sizing-policy".to_string(),
                PropertyValue::String("keep-aspect-ratio".to_string()),
            );
        }
    }

    // --- Overlay pad: fullscreen, highest zorder ---
    {
        let overlay_pad_name = format!("sink_{}", 2 * p.num_inputs + 1 + p.num_pips * p.num_inputs);
        let props = mv_pads.entry(overlay_pad_name).or_default();
        props.insert("xpos".to_string(), PropertyValue::Int(0));
        props.insert("ypos".to_string(), PropertyValue::Int(0));
        props.insert("width".to_string(), PropertyValue::Int(p.mv_w as i64));
        props.insert("height".to_string(), PropertyValue::Int(p.mv_h as i64));
        props.insert("alpha".to_string(), PropertyValue::Float(1.0));
        props.insert(
            "zorder".to_string(),
            PropertyValue::UInt(vision_mixer::MV_OVERLAY_ZORDER as u64),
        );
    }

    pad_props
}

/// Set up the overlay renderer: creates shared state, registers it, and starts
/// a 1Hz timer that pushes overlay frames via appsrc when state changes.
///
/// Returns the shared overlay state so callers can install additional
/// consumers (e.g. a bus handler that writes audio meter values into it).
fn setup_overlay_renderer(
    p: &PipelineParams,
    appsrc: &gst_app::AppSrc,
    overlay_caps: &gst::Caps,
    mv_layout: &layout::OverlayLayout,
    ctx: &BlockBuildContext,
) -> Arc<VisionMixerOverlayState> {
    let pip_initial = overlay::PipInitialState {
        num_pips: p.num_pips,
        pip_bgs: (0..p.num_pips)
            .map(|i| p.pip_bg_inputs.get(i).copied().flatten())
            .collect(),
        pip_zones: (0..p.num_pips)
            .map(|i| p.pip_zones.get(i).cloned().unwrap_or_default())
            .collect(),
        pgm_pip: p.pgm_source.as_pip(),
        pvw_pip: p.pvw_source.as_pip(),
    };

    // pgm_group/pvw_group must match what the compositor actually renders.
    // For a single Input source it's that input; for a PiP source the legacy
    // group reflects the PiP's bg input so transitions and overlay rendering
    // stay consistent (Take from PiP-PGM falls back to bg-as-single-input).
    let initial_pgm_input = p
        .pgm_source
        .as_pip()
        .and_then(|p_idx| pip_initial.pip_bgs.get(p_idx).copied().flatten())
        .unwrap_or(p.pgm_input);
    let initial_pvw_input = p
        .pvw_source
        .as_pip()
        .and_then(|p_idx| pip_initial.pip_bgs.get(p_idx).copied().flatten())
        .unwrap_or(p.pvw_input);

    let overlay_state = Arc::new(VisionMixerOverlayState::new(
        p.num_inputs,
        p.num_dsk_inputs,
        initial_pgm_input,
        initial_pvw_input,
        p.labels.to_vec(),
        mv_layout.clone(),
        p.show_vu_meters,
        pip_initial,
    ));

    // Register the overlay state so the API layer can access it
    overlay::register_overlay_state(p.instance_id, Arc::clone(&overlay_state));

    let renderer = Arc::new(Mutex::new(OverlayRenderer::new(
        appsrc.clone(),
        overlay_caps.clone(),
        Arc::clone(&overlay_state),
        p.mv_w as i32,
        p.mv_h as i32,
    )));

    let block_id = p.instance_id.to_string();
    overlay::register_overlay_renderer(&block_id, Arc::clone(&renderer));

    let block_id_for_timer = block_id.clone();
    let renderer_for_timer = Arc::clone(&renderer);
    let mv_framerate = p.mv_framerate;
    ctx.register_element_setup(Box::new(move |_flow_id, _events| {
        overlay::start_overlay_timer(
            block_id_for_timer.clone(),
            renderer_for_timer.clone(),
            mv_framerate,
        );
    }));

    overlay_state
}
