//! Initial compositor sink-pad geometry / alpha / zorder.
//!
//! These properties are applied AFTER linking, because the linker
//! auto-creates request pads sequentially via `link_pads(src, None)`.

use std::collections::HashMap;

use strom_types::{vision_mixer, PropertyValue};

use super::super::layout;
use super::PipelineParams;

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
    // Transforms are runtime-only (like zones) and caps are not negotiated
    // yet — both maps are empty at build time. The caps probe re-applies
    // aspect-correct geometry once each input's caps arrive.
    let layouts = strom_types::vision_mixer::resolve_zone_pads(
        rx,
        ry,
        rw,
        rh,
        zones,
        src_aspect,
        &strom_types::vision_mixer::PipTransforms::new(),
        &strom_types::vision_mixer::SourceAspects::new(),
    );
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
pub(super) fn build_pad_properties(
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
    // Fallback aspect for PiP-tile cell math at build time: caps are not
    // negotiated yet, so the slot grid and fits use the PGM canvas aspect.
    // The caps probe re-applies per-source aspect-correct geometry once each
    // input's caps arrive.
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
        // Explicit geometry: layout code aspect-fits every rect itself,
        // so the pad must render exactly (width, height). See aspect_fit_rect.
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("none".to_string()),
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
        // Explicit geometry: layout code aspect-fits every rect itself,
        // so the pad must render exactly (width, height). See aspect_fit_rect.
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("none".to_string()),
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
        // Explicit geometry: layout code aspect-fits every rect itself,
        // so the pad must render exactly (width, height). See aspect_fit_rect.
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("none".to_string()),
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
        // Explicit geometry: layout code aspect-fits every rect itself,
        // so the pad must render exactly (width, height). See aspect_fit_rect.
        props.insert(
            "sizing-policy".to_string(),
            PropertyValue::String("none".to_string()),
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
                PropertyValue::String("none".to_string()),
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
