//! Pipeline construction for the vision mixer block.
//!
//! Sub-modules:
//!   - [`pipeline_gpu`] — `build_gpu_pipeline` (glvideomixer-based)
//!   - [`pipeline_cpu`] — `build_cpu_pipeline` (compositor-based, no GL)
//!   - [`audio_meter`] — per-input + PGM `level` element chains and bus handler
//!   - [`pad_layout`]  — compositor sink-pad initial geometry/zorder/alpha

use super::elements::CompositorBackend;
use super::layout;
use super::overlay::{self, OverlayRenderer, VisionMixerOverlayState};
use super::properties;
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use strom_types::vision_mixer;
use strom_types::{
    block::{ExternalPad, ExternalPads},
    MediaType, PropertyValue,
};
use tracing::info;

mod audio_meter;
mod pad_layout;
mod pipeline_cpu;
mod pipeline_gpu;

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
        // Zones are runtime-only — the operator configures them via the
        // /pip endpoint after the pipeline is built.
        let pip_zones: Vec<Vec<strom_types::vision_mixer::Zone>> =
            (0..num_pips).map(|_| Vec::new()).collect();

        let output_format = properties::parse_output_format(props);
        let gl_download =
            properties::parse_bool(props, "gl_download", vision_mixer::DEFAULT_GL_DOWNLOAD);
        let show_vu_meters = properties::parse_show_vu_meters(props);
        let enable_fx = properties::parse_bool(props, "enable_fx", vision_mixer::DEFAULT_ENABLE_FX);
        let swap_pvw_pgm =
            properties::parse_bool(props, "swap_pvw_pgm", vision_mixer::DEFAULT_SWAP_PVW_PGM);

        let pref = props
            .get("compositor_preference")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");
        let backend = super::elements::select_backend(pref)?;

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
            enable_fx,
            swap_pvw_pgm,
        };

        match backend {
            CompositorBackend::OpenGL => pipeline_gpu::build_gpu_pipeline(&p, ctx),
            CompositorBackend::Software => pipeline_cpu::build_cpu_pipeline(&p, ctx),
        }
    }
}

/// Shared parameters for pipeline construction.
pub(super) struct PipelineParams<'a> {
    pub(super) instance_id: &'a str,
    pub(super) num_inputs: usize,
    pub(super) num_dsk_inputs: usize,
    pub(super) num_pips: usize,
    pub(super) pgm_input: usize,
    pub(super) pvw_input: usize,
    pub(super) pgm_source: strom_types::vision_mixer::Source,
    pub(super) pvw_source: strom_types::vision_mixer::Source,
    pub(super) pip_bg_inputs: &'a [Option<usize>],
    pub(super) pip_zones: &'a [Vec<strom_types::vision_mixer::Zone>],
    pub(super) labels: &'a [String],
    pub(super) latency_ms: u64,
    pub(super) min_upstream_ms: u64,
    pub(super) pgm_w: u32,
    pub(super) pgm_h: u32,
    pub(super) mv_w: u32,
    pub(super) mv_h: u32,
    pub(super) pgm_framerate: (i32, i32),
    pub(super) mv_framerate: (i32, i32),
    pub(super) backend: CompositorBackend,
    pub(super) output_format: Option<String>,
    pub(super) gl_download: bool,
    pub(super) show_vu_meters: bool,
    /// Build shader FX slots (GPU pipeline only; the CPU builder ignores it).
    pub(super) enable_fx: bool,
    /// Mirror the multiview layout (PGM left, PVW right).
    pub(super) swap_pvw_pgm: bool,
}

impl<'a> PipelineParams<'a> {
    /// Create a namespaced element ID.
    pub(super) fn id(&self, name: &str) -> String {
        format!("{}:{}", self.instance_id, name)
    }

    /// Raw video caps with resolution, framerate and an optional pixel format.
    fn raw_caps(&self, w: u32, h: u32, framerate: (i32, i32), format: Option<&str>) -> gst::Caps {
        let mut builder = gst::Caps::builder("video/x-raw")
            .field("width", w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(framerate.0, framerate.1))
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
        if let Some(fmt) = format {
            builder = builder.field("format", fmt);
        }
        builder.build()
    }

    /// Build PGM output caps with resolution, framerate, and optional pixel format.
    pub(super) fn pgm_caps(&self) -> gst::Caps {
        self.raw_caps(
            self.pgm_w,
            self.pgm_h,
            self.pgm_framerate,
            self.output_format.as_deref(),
        )
    }

    /// Build multiview output caps with resolution, framerate, and optional pixel format.
    pub(super) fn mv_caps(&self) -> gst::Caps {
        self.raw_caps(
            self.mv_w,
            self.mv_h,
            self.mv_framerate,
            self.output_format.as_deref(),
        )
    }

    /// PGM caps with `format` instead of `output_format` — the software
    /// compositor's blend space, see [`Self::alpha_blend_format`].
    pub(super) fn pgm_blend_caps(&self, format: &str) -> gst::Caps {
        self.raw_caps(self.pgm_w, self.pgm_h, self.pgm_framerate, Some(format))
    }

    /// Multiview caps with `format` instead of `output_format` — the software
    /// compositor's blend space, see [`Self::alpha_blend_format`].
    pub(super) fn mv_blend_caps(&self, format: &str) -> gst::Caps {
        self.raw_caps(self.mv_w, self.mv_h, self.mv_framerate, Some(format))
    }

    /// The format the software compositor has to blend in so that keyed pads
    /// keep their per-pixel alpha, or `None` when `output_format` needs no
    /// substitute.
    ///
    /// `compositor` blends in its own output format and converts every sink
    /// pad to it first, so an alpha-less `output_format` (I420, NV12, RGB, ...)
    /// strips the alpha of DSK graphics, the multiview overlay and the border
    /// underlays. Blending in the alpha-carrying counterpart and converting
    /// once on the way out keeps the key and still hands `output_format`
    /// downstream. Alpha-carrying and unknown formats are left alone.
    pub(super) fn alpha_blend_format(&self) -> Option<&'static str> {
        let fmt = self.output_format.as_deref()?;
        let format = fmt.parse::<gst_video::VideoFormat>().ok()?;
        let info = gst_video::VideoFormatInfo::from_format(format);
        if info.has_alpha() {
            return None;
        }
        // Match the chroma subsampling of the requested format so the extra
        // hop costs nothing the output would not have cost anyway.
        Some(match fmt {
            "YUY2" | "UYVY" => "A422",
            "v210" => "A422_10LE",
            _ if info.is_rgb() || info.is_gray() => "RGBA",
            _ => "A420",
        })
    }

    /// Build PGM output caps for the GL-memory passthrough path.
    /// Constrains framerate/resolution without forcing a download to system memory.
    pub(super) fn pgm_caps_glmem(&self) -> gst::Caps {
        let s = format!(
            "video/x-raw(memory:GLMemory),width={},height={},framerate={}/{},pixel-aspect-ratio=1/1",
            self.pgm_w, self.pgm_h, self.pgm_framerate.0, self.pgm_framerate.1
        );
        s.parse().expect("valid GL memory caps for PGM")
    }

    /// Build multiview output caps for the GL-memory passthrough path.
    pub(super) fn mv_caps_glmem(&self) -> gst::Caps {
        let s = format!(
            "video/x-raw(memory:GLMemory),width={},height={},framerate={}/{},pixel-aspect-ratio=1/1",
            self.mv_w, self.mv_h, self.mv_framerate.0, self.mv_framerate.1
        );
        s.parse().expect("valid GL memory caps for MV")
    }
}

/// Set up the overlay renderer: creates shared state, registers it, and starts
/// a 1Hz timer that pushes overlay frames via appsrc when state changes.
///
/// Returns the shared overlay state so callers can install additional
/// consumers (e.g. a bus handler that writes audio meter values into it).
pub(super) fn setup_overlay_renderer(
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

    // pgm_input/pvw_input must match what the compositor actually renders.
    // For a single Input source it's that input; for a PiP source we initialize
    // from the PiP's bg input so transitions and overlay rendering stay consistent
    // (Take from PiP-PGM falls back to bg-as-single-input).
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
        p.pgm_w,
        p.pgm_h,
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
