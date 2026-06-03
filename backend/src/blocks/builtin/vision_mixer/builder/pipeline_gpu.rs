//! GPU pipeline build path: glvideomixer-based, with optional GL-memory passthrough.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use strom_types::element::ElementPadRef;
use tracing::info;

use super::super::{elements, layout};
use super::{audio_meter, pad_layout, setup_overlay_renderer, PipelineParams};
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult};

pub(super) fn build_gpu_pipeline(
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
    // Weak handles for the caps probes registered below — the probes must
    // not hold strong element references (pads own their probes; a strong
    // ref would create a cycle that leaks the pipeline on restart).
    let dist_weak = dist_comp.downgrade();
    let dist_weak_pgm = dist_comp.downgrade();
    let mv_weak_overlay = mv_comp.downgrade();
    let mv_weak = mv_comp.downgrade();
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

    // --- PGM graphics overlay (zone borders): appsrc → glupload → dist mixer ---
    // Mixer-state-driven decorations drawn by the mixer itself (borders track
    // live pad geometry). Sits below the DSK stack on the dist compositor.
    let appsrc_pgm_ov_id = p.id("appsrc_pgm_overlay");
    let pgm_ov_caps_str = format!(
        "video/x-raw,format=RGBA,width={},height={},pixel-aspect-ratio=1/1,framerate={}/{},interlace-mode=progressive,multiview-mode=mono",
        p.pgm_w, p.pgm_h, p.pgm_framerate.0, p.pgm_framerate.1
    );
    let pgm_ov_caps: gst::Caps = pgm_ov_caps_str
        .parse()
        .map_err(|e| BlockBuildError::ElementCreation(format!("pgm overlay caps: {}", e)))?;
    let appsrc_pgm_ov = gst_app::AppSrc::builder()
        .name(&appsrc_pgm_ov_id)
        .format(gst::Format::Time)
        .is_live(false)
        .automatic_eos(false)
        .do_timestamp(true)
        .max_buffers(2)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();
    let q_pgm_ov_id = p.id("queue_pgm_overlay");
    let up_pgm_ov_id = p.id("glupload_pgm_overlay");
    elems.push((appsrc_pgm_ov_id.clone(), appsrc_pgm_ov.clone().upcast()));
    elems.push((q_pgm_ov_id.clone(), elements::make_queue(&q_pgm_ov_id)?));
    elems.push((
        up_pgm_ov_id.clone(),
        elements::make_element("glupload", &up_pgm_ov_id)?,
    ));
    links.push((
        ElementPadRef::pad(&appsrc_pgm_ov_id, "src"),
        ElementPadRef::pad(&q_pgm_ov_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&q_pgm_ov_id, "src"),
        ElementPadRef::pad(&up_pgm_ov_id, "sink"),
    ));
    // Link to the dist mixer is added after the DSK links (pad ordering).

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

    // PGM graphics overlay pad — after the DSK links so it lands at
    // sink_{num_inputs + num_dsk_inputs}.
    links.push((
        ElementPadRef::pad(&up_pgm_ov_id, "src"),
        ElementPadRef::pad(
            &mixer_id,
            format!("sink_{}", p.num_inputs + p.num_dsk_inputs),
        ),
    ));

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
    let pad_properties = pad_layout::build_pad_properties(p, &mv_layout);

    // --- Audio metering branches (per-input + PGM) ---
    audio_meter::append_audio_meter_chains(p, &mut elems, &mut links)?;

    // --- Set up overlay appsrc renderer ---
    let overlay_state = setup_overlay_renderer(
        p,
        &appsrc_overlay,
        &overlay_caps,
        &mv_layout,
        mv_weak_overlay,
        ctx,
    );

    // --- PGM graphics overlay renderer (zone borders) ---
    super::super::pgm_overlay::setup_pgm_overlay_renderer(
        p.instance_id,
        &appsrc_pgm_ov,
        &pgm_ov_caps,
        std::sync::Arc::clone(&overlay_state),
        dist_weak_pgm,
        p.pgm_w as i32,
        p.pgm_h as i32,
        p.pgm_framerate,
        ctx,
    );

    // --- Reactive explicit geometry ---
    // Input pads run sizing-policy=none; aspect-correct rects are re-applied
    // whenever an input's caps arrive or change. Probes attach at element
    // setup time (after linking, when the request pads exist).
    {
        let block_id = p.instance_id.to_string();
        let num_inputs = p.num_inputs;
        let num_pips = p.num_pips;
        ctx.register_element_setup(Box::new(move |_flow_id, _events| {
            let (Some(mixer), Some(mv_comp)) = (dist_weak.upgrade(), mv_weak.upgrade()) else {
                return;
            };
            super::super::geometry::install_caps_probes(
                &block_id, &mixer, &mv_comp, num_inputs, num_pips,
            );
        }));
    }

    // --- Bus handler that pipes `level` messages into the overlay state ---
    let bus_message_handler = Some(audio_meter::build_meter_bus_handler(
        p.instance_id,
        overlay_state,
    ));

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
