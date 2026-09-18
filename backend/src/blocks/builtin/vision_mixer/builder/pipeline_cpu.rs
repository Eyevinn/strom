//! CPU pipeline build path: compositor-based, no GL.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use strom_types::element::ElementPadRef;
use tracing::info;

use super::super::{elements, layout};
use super::{audio_meter, pad_layout, setup_overlay_renderer, PipelineParams};
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult};
use crate::gpu;

pub(super) fn build_cpu_pipeline(
    p: &PipelineParams,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    let mut elems: Vec<(String, gst::Element)> = Vec::new();
    let mut links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

    // Conversion element for this host — see the DSK chain below for why.
    let vc_factory = gpu::video_convert_mode().element_name();

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
    let mv_weak = mv_comp.downgrade();
    elems.push((mixer_id.clone(), dist_comp));
    elems.push((mv_comp_id.clone(), mv_comp));

    let source_aspect = if p.pgm_h > 0 {
        p.pgm_w as f64 / p.pgm_h as f64
    } else {
        16.0 / 9.0
    };
    let mv_layout = layout::compute_layout(
        p.mv_w,
        p.mv_h,
        p.num_inputs,
        p.num_pips,
        source_aspect,
        p.swap_pvw_pgm,
    );

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

    // Keyed pads on the dist compositor: DSK graphics, and the border
    // underlays that carry #RRGGBBAA colors. When they are present and
    // output_format cannot hold alpha, blend in the alpha-carrying
    // counterpart and convert to output_format after the mixer — pinning
    // output_format on the mixer itself would flatten the key.
    let dist_keyed = p.num_dsk_inputs > 0 || p.num_pips > 0;
    let dist_blend_format = if dist_keyed {
        p.alpha_blend_format()
    } else {
        None
    };
    if let Some(blend_fmt) = dist_blend_format {
        let cf_blend_id = p.id("capsfilter_dist_blend");
        let capsfilter_blend = gst::ElementFactory::make("capsfilter")
            .name(&cf_blend_id)
            .property("caps", p.pgm_blend_caps(blend_fmt))
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("capsfilter_dist_blend: {}", e))
            })?;
        let vc_dist_out_id = p.id("videoconvert_dist_out");
        let videoconvert_dist_out = elements::make_element(vc_factory, &vc_dist_out_id)?;
        elems.push((cf_blend_id.clone(), capsfilter_blend));
        elems.push((vc_dist_out_id.clone(), videoconvert_dist_out));
        info!(
            "Vision mixer {}: blending in {} for keyed pads, converting to {} on output",
            p.instance_id,
            blend_fmt,
            p.output_format.as_deref().unwrap_or("auto")
        );

        links.push((
            ElementPadRef::pad(&mixer_id, "src"),
            ElementPadRef::pad(&cf_blend_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&cf_blend_id, "src"),
            ElementPadRef::pad(&vc_dist_out_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&vc_dist_out_id, "src"),
            ElementPadRef::pad(&cf_dist_id, "sink"),
        ));
    } else {
        links.push((
            ElementPadRef::pad(&mixer_id, "src"),
            ElementPadRef::pad(&cf_dist_id, "sink"),
        ));
    }
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
    for i in 0..p.num_dsk_inputs {
        let q_id = p.id(&format!("queue_dsk_{}", i));
        let vc_id_dsk = p.id(&format!("videoconvert_dsk_{}", i));

        let queue = elements::make_queue(&q_id)?;
        // Uses autovideoconvert on hosts with working CUDA-GL interop, plain
        // videoconvert elsewhere — same pattern as videoenc/videoformat. Needed
        // so that upstream GPU-memory sources (e.g. nvh264dec from efpsrt_input)
        // negotiate correctly against the CPU compositor backend.
        let videoconvert = elements::make_element(vc_factory, &vc_id_dsk)?;
        gpu::configure_video_convert(&videoconvert);

        elems.push((q_id.clone(), queue));
        elems.push((vc_id_dsk.clone(), videoconvert));

        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&vc_id_dsk, "sink"),
        ));

        // No output_format capsfilter here. A DSK is a keyer: forcing an
        // alpha-less format (I420/NV12) ahead of the compositor silently
        // flattens the graphic's per-pixel alpha and paints its RGB opaquely
        // over the program. Video inputs need the pin because their tee feeds
        // two independently negotiating compositors; a DSK links straight to
        // one compositor pad, so its convert pad handles the conversion.
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

    // The multiview overlay (labels, VU meters, tallies) is an RGBA pad on
    // mv_comp, so the multiview always has a keyed pad — same treatment as the
    // dist compositor above.
    if let Some(blend_fmt) = p.alpha_blend_format() {
        let cf_blend_id = p.id("capsfilter_mv_blend");
        let capsfilter_blend = gst::ElementFactory::make("capsfilter")
            .name(&cf_blend_id)
            .property("caps", p.mv_blend_caps(blend_fmt))
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter_mv_blend: {}", e)))?;
        let vc_mv_out_id = p.id("videoconvert_mv_out");
        let videoconvert_mv_out = elements::make_element(vc_factory, &vc_mv_out_id)?;
        elems.push((cf_blend_id.clone(), capsfilter_blend));
        elems.push((vc_mv_out_id.clone(), videoconvert_mv_out));

        links.push((
            ElementPadRef::pad(&mv_comp_id, "src"),
            ElementPadRef::pad(&cf_blend_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&cf_blend_id, "src"),
            ElementPadRef::pad(&vc_mv_out_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&vc_mv_out_id, "src"),
            ElementPadRef::pad(&cf_mv_id, "sink"),
        ));
    } else {
        links.push((
            ElementPadRef::pad(&mv_comp_id, "src"),
            ElementPadRef::pad(&cf_mv_id, "sink"),
        ));
    }
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

    // No output_format capsfilter here either: the overlay is RGBA with
    // per-pixel alpha, and pinning it to an alpha-less format would composite
    // its transparent areas opaquely over the multiview.
    links.push((
        ElementPadRef::pad(&appsrc_overlay_id, "src"),
        ElementPadRef::pad(&q_overlay_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&q_overlay_id, "src"),
        ElementPadRef::pad(&vc_overlay_id, "sink"),
    ));
    let overlay_last_id = vc_overlay_id;
    // Link to mv_comp is added AFTER all other mv_comp links (pad ordering matters)

    // --- Border underlay sources ---
    // Zone borders render as solid-color compositor pads directly beneath
    // their content pads (see `gst::underlay`). One tiny videotestsrc per
    // (region, input); the border color is set at runtime via
    // `foreground-color`. Non-live → contributes no latency. Only built when
    // PiPs are configured — zones (and thus borders) cannot exist without
    // them.
    if p.num_pips > 0 {
        let underlay_chains: Vec<String> = (0..p.num_inputs)
            .map(|i| format!("underlay_dist_{}", i))
            .chain((0..p.num_inputs).map(|i| format!("underlay_pvw_{}", i)))
            .chain((0..p.num_pips).flat_map(|pip| {
                (0..p.num_inputs)
                    .map(move |i| format!("underlay_pip_{}_{}", pip, i))
                    .collect::<Vec<_>>()
            }))
            .collect();
        // Always RGBA — the compositor's convert pads handle per-pad format
        // conversion, and an alpha-less forced output_format (I420/NV12)
        // would silently drop the alpha of #RRGGBBAA border colors. Low
        // framerate: the color only changes on border edits and the
        // compositor keeps compositing the latest buffer between pushes.
        let underlay_caps: gst::Caps =
            "video/x-raw,format=RGBA,width=16,height=16,framerate=5/1,pixel-aspect-ratio=1/1"
                .parse()
                .map_err(|e| BlockBuildError::ElementCreation(format!("underlay caps: {}", e)))?;
        for name in &underlay_chains {
            let src_id = p.id(&format!("{}_src", name));
            let cf_id = p.id(&format!("{}_caps", name));
            let src = gst::ElementFactory::make("videotestsrc")
                .name(&src_id)
                .property("is-live", false)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", src_id, e)))?;
            src.set_property_from_str("pattern", "solid-color");
            let cf = gst::ElementFactory::make("capsfilter")
                .name(&cf_id)
                .property("caps", &underlay_caps)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", cf_id, e)))?;
            elems.push((src_id.clone(), src));
            elems.push((cf_id.clone(), cf));
            links.push((
                ElementPadRef::pad(&src_id, "src"),
                ElementPadRef::pad(&cf_id, "sink"),
            ));
        }
    }
    // Links to the compositor pads are added below in pad-index order.

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

        // The CPU `compositor` has no crop pad properties, so every croppable
        // branch (dist, PVW, PiP — not thumbnails, which never crop) gets a
        // videocrop element directly upstream of its compositor sink pad.
        // Zero crop = passthrough. `set_pad_crop` finds it via pad.peer().
        let crop_dist_id = p.id(&format!("videocrop_dist_{}", i));
        let crop_pvw_id = p.id(&format!("videocrop_pvw_{}", i));
        elems.push((
            crop_dist_id.clone(),
            elements::make_element("videocrop", &crop_dist_id)?,
        ));
        elems.push((
            crop_pvw_id.clone(),
            elements::make_element("videocrop", &crop_pvw_id)?,
        ));

        // One queue (+ videocrop) per PiP tile per input — feeds the virtual
        // PiP thumbnail pads on mv_comp.
        for pip_idx in 0..p.num_pips {
            let q_pip_id = p.id(&format!("queue_to_mv_pip_{}_{}", pip_idx, i));
            elems.push((q_pip_id.clone(), elements::make_queue(&q_pip_id)?));
            let crop_pip_id = p.id(&format!("videocrop_pip_{}_{}", pip_idx, i));
            elems.push((
                crop_pip_id.clone(),
                elements::make_element("videocrop", &crop_pip_id)?,
            ));
        }
    }

    // --- Compositor links (grouped by compositor, order matters) ---
    // Distribution compositor: video inputs first, then DSK
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        let q_dist_id = p.id(&format!("queue_to_dist_{}", i));
        let crop_dist_id = p.id(&format!("videocrop_dist_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_0"),
            ElementPadRef::pad(&q_dist_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_dist_id, "src"),
            ElementPadRef::pad(&crop_dist_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&crop_dist_id, "src"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", i)),
        ));
    }
    for i in 0..p.num_dsk_inputs {
        let last_dsk_elem = p.id(&format!("videoconvert_dsk_{}", i));
        links.push((
            ElementPadRef::pad(&last_dsk_elem, "src"),
            ElementPadRef::pad(&mixer_id, format!("sink_{}", p.num_inputs + i)),
        ));
    }

    // Dist border underlays — after the DSK links so input i's underlay
    // lands at sink_{num_inputs + num_dsk_inputs + i}.
    if p.num_pips > 0 {
        for i in 0..p.num_inputs {
            let cf_id = p.id(&format!("underlay_dist_{}_caps", i));
            links.push((
                ElementPadRef::pad(&cf_id, "src"),
                ElementPadRef::pad(
                    &mixer_id,
                    format!("sink_{}", p.num_inputs + p.num_dsk_inputs + i),
                ),
            ));
        }
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

    // Multiview PVW big candidates: tee_i.src_2 → queue → videocrop → mv_comp.sink_{N+1+i}
    for i in 0..p.num_inputs {
        let tee_id = p.id(&format!("tee_{}", i));
        let q_pvw_id = p.id(&format!("queue_to_mv_pvw_{}", i));
        let crop_pvw_id = p.id(&format!("videocrop_pvw_{}", i));
        links.push((
            ElementPadRef::pad(&tee_id, "src_2"),
            ElementPadRef::pad(&q_pvw_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&q_pvw_id, "src"),
            ElementPadRef::pad(&crop_pvw_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&crop_pvw_id, "src"),
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
            let crop_pip_id = p.id(&format!("videocrop_pip_{}_{}", pip_idx, i));
            let tee_src = format!("src_{}", 3 + pip_idx);
            let sink_idx = 2 * p.num_inputs + 1 + pip_idx * p.num_inputs + i;
            links.push((
                ElementPadRef::pad(&tee_id, &tee_src),
                ElementPadRef::pad(&q_pip_id, "sink"),
            ));
            links.push((
                ElementPadRef::pad(&q_pip_id, "src"),
                ElementPadRef::pad(&crop_pip_id, "sink"),
            ));
            links.push((
                ElementPadRef::pad(&crop_pip_id, "src"),
                ElementPadRef::pad(&mv_comp_id, format!("sink_{}", sink_idx)),
            ));
        }
    }

    // Overlay pad: last overlay element → mv_comp.
    let overlay_pad_idx = 2 * p.num_inputs + 1 + p.num_pips * p.num_inputs;
    links.push((
        ElementPadRef::pad(&overlay_last_id, "src"),
        ElementPadRef::pad(&mv_comp_id, format!("sink_{}", overlay_pad_idx)),
    ));

    // Multiview border underlays — after the overlay pad: PVW underlays at
    // sink_{2N+2+P+i}, then tile underlays at sink_{2N+2+P+N+pip*N+i}.
    if p.num_pips > 0 {
        let mv_underlay_base = overlay_pad_idx + 1;
        for i in 0..p.num_inputs {
            let cf_id = p.id(&format!("underlay_pvw_{}_caps", i));
            links.push((
                ElementPadRef::pad(&cf_id, "src"),
                ElementPadRef::pad(&mv_comp_id, format!("sink_{}", mv_underlay_base + i)),
            ));
        }
        for pip_idx in 0..p.num_pips {
            for i in 0..p.num_inputs {
                let cf_id = p.id(&format!("underlay_pip_{}_{}_caps", pip_idx, i));
                let sink_idx = mv_underlay_base + p.num_inputs * (1 + pip_idx) + i;
                links.push((
                    ElementPadRef::pad(&cf_id, "src"),
                    ElementPadRef::pad(&mv_comp_id, format!("sink_{}", sink_idx)),
                ));
            }
        }
    }

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

    let pad_properties = pad_layout::build_pad_properties(p, &mv_layout);

    // Audio metering branches (per-input + PGM)
    audio_meter::append_audio_meter_chains(p, &mut elems, &mut links)?;

    let overlay_state = setup_overlay_renderer(p, &appsrc_overlay, &overlay_caps, &mv_layout, ctx);

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
    let bus_message_handler = Some(audio_meter::build_meter_bus_handler(
        p.instance_id,
        overlay_state,
    ));

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
