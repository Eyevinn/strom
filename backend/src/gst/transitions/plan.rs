//! Pure decision logic for pad transitions — no GStreamer dependencies.
//!
//! [`plan_transition`] takes two compositions (old + new) and returns a
//! per-pad [`PadAction`] that the [`super::TransitionController`] then drives
//! into actual compositor properties.

use std::collections::{HashMap, HashSet};

use super::{PadAction, PadTarget, ZHandling};

/// Returns true if `outer` fully contains `inner`.
fn rect_contains(outer: (i32, i32, i32, i32), inner: (i32, i32, i32, i32)) -> bool {
    outer.0 <= inner.0
        && outer.1 <= inner.1
        && outer.0 + outer.2 >= inner.0 + inner.2
        && outer.1 + outer.3 >= inner.1 + inner.3
}

/// Decide per-pad actions for a transition between two compositions. Pure
/// function with no GStreamer dependencies — drives the `animate_pad_transition`
/// test matrix.
///
/// The core rule: **what's visible needs a transition; what's hidden doesn't.**
///   - A non-shared incoming pad whose rect is fully *covered* by some morphing
///     pad at the start of the animation can sit at `alpha=1` immediately
///     (`HoldFullAlpha`) — it's hidden behind the morph until the morph reveals
///     it by shrinking/moving.
///   - A non-shared incoming pad that would be *visible* at t=0 (not covered by
///     any morph) needs a smooth `FadeIn` so it doesn't pop in.
///   - Symmetric for outgoing pads: covered at t=end → `StepOffAtEnd`; visible
///     at t=end → `FadeOut`.
///
/// Same-position swaps (e.g. two different PiP bgs both fullscreen) always
/// cross-fade so the swap is smooth.
pub fn plan_transition(outgoing: &[PadTarget], incoming: &[PadTarget]) -> Vec<(usize, PadAction)> {
    let outgoing_map: HashMap<usize, PadTarget> =
        outgoing.iter().map(|t| (t.pad_idx, *t)).collect();
    let incoming_map: HashMap<usize, PadTarget> =
        incoming.iter().map(|t| (t.pad_idx, *t)).collect();

    // Collect start/end rects of every morphing (shared + position-changing) pad.
    let mut morph_start_rects: Vec<(i32, i32, i32, i32)> = Vec::new();
    let mut morph_end_rects: Vec<(i32, i32, i32, i32)> = Vec::new();
    for t in incoming {
        if let Some(o) = outgoing_map.get(&t.pad_idx) {
            if o.x != t.x || o.y != t.y || o.w != t.w || o.h != t.h {
                morph_start_rects.push((o.x, o.y, o.w, o.h));
                morph_end_rects.push((t.x, t.y, t.w, t.h));
            }
        }
    }

    let outgoing_nonshared_positions: HashSet<(i32, i32, i32, i32)> = outgoing
        .iter()
        .filter(|t| !incoming_map.contains_key(&t.pad_idx))
        .map(|t| (t.x, t.y, t.w, t.h))
        .collect();
    let incoming_nonshared_positions: HashSet<(i32, i32, i32, i32)> = incoming
        .iter()
        .filter(|t| !outgoing_map.contains_key(&t.pad_idx))
        .map(|t| (t.x, t.y, t.w, t.h))
        .collect();

    let mut plan = Vec::new();

    for t in incoming {
        if let Some(o) = outgoing_map.get(&t.pad_idx) {
            let morphing = o.x != t.x || o.y != t.y || o.w != t.w || o.h != t.h;
            if morphing {
                let new_has_overlays_above_me = incoming.iter().any(|i_t| {
                    i_t.pad_idx != t.pad_idx
                        && !outgoing_map.contains_key(&i_t.pad_idx)
                        && i_t.zorder > t.zorder
                });
                let z_handling = if new_has_overlays_above_me {
                    ZHandling::SnapToNew(t.zorder)
                } else {
                    ZHandling::LiftAndStep { new_z: t.zorder }
                };
                plan.push((
                    t.pad_idx,
                    PadAction::Morph {
                        from_x: o.x,
                        from_y: o.y,
                        from_w: o.w,
                        from_h: o.h,
                        to_x: t.x,
                        to_y: t.y,
                        to_w: t.w,
                        to_h: t.h,
                        z_handling,
                    },
                ));
            } else {
                plan.push((
                    t.pad_idx,
                    PadAction::AffirmStatic {
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        zorder: t.zorder,
                    },
                ));
            }
        } else {
            let pad_rect = (t.x, t.y, t.w, t.h);
            let same_pos_outgoing = outgoing_nonshared_positions.contains(&pad_rect);
            let covered_at_start = morph_start_rects
                .iter()
                .any(|r| rect_contains(*r, pad_rect));
            // Cross-fade when there's a same-position swap (smooth same-rect blend)
            // OR when the pad is visible at t=0 (no morph covers it).
            // Hold at full alpha only when fully hidden behind a morphing pad.
            if !same_pos_outgoing && covered_at_start {
                plan.push((
                    t.pad_idx,
                    PadAction::HoldFullAlpha {
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        zorder: t.zorder,
                    },
                ));
            } else {
                plan.push((
                    t.pad_idx,
                    PadAction::FadeIn {
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        zorder: t.zorder,
                    },
                ));
            }
        }
    }

    for t in outgoing {
        if incoming_map.contains_key(&t.pad_idx) {
            continue;
        }
        let pad_rect = (t.x, t.y, t.w, t.h);
        let same_pos_incoming = incoming_nonshared_positions.contains(&pad_rect);
        let covered_at_end = morph_end_rects.iter().any(|r| rect_contains(*r, pad_rect));
        if !same_pos_incoming && covered_at_end {
            plan.push((t.pad_idx, PadAction::StepOffAtEnd));
        } else {
            plan.push((t.pad_idx, PadAction::FadeOut));
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // plan_transition matrix
    // ------------------------------------------------------------------------
    //
    // Tests below treat input indices as pad indices (pad_base=0). The exact
    // zorder values are not critical — what matters is the relative ordering
    // (PGM=1, OVL=2) that drives the `has_overlays_above_me` decision.

    const PGM_Z: u32 = strom_types::vision_mixer::DIST_PGM_ZORDER;
    const OVL_Z: u32 = strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER;

    fn pad(idx: usize, x: i32, y: i32, w: i32, h: i32, z: u32) -> PadTarget {
        PadTarget {
            pad_idx: idx,
            x,
            y,
            w,
            h,
            zorder: z,
        }
    }

    /// Look up a plan entry for a pad; panics if not present.
    fn action_of(plan: &[(usize, PadAction)], idx: usize) -> PadAction {
        plan.iter()
            .find(|(i, _)| *i == idx)
            .unwrap_or_else(|| panic!("no plan entry for pad {} in {:?}", idx, plan))
            .1
    }

    fn fullscreen(idx: usize, z: u32) -> PadTarget {
        pad(idx, 0, 0, 1920, 1080, z)
    }
    fn ovl_a(idx: usize) -> PadTarget {
        // overlay cell 0 (left half, vertically centered for 16:9)
        pad(idx, 0, 270, 960, 540, OVL_Z)
    }
    fn ovl_b(idx: usize) -> PadTarget {
        pad(idx, 960, 270, 960, 540, OVL_Z)
    }

    #[test]
    fn input_to_input_pure_crossfade() {
        let old = vec![fullscreen(0, PGM_Z)];
        let new = vec![fullscreen(1, PGM_Z)];
        let plan = plan_transition(&old, &new);
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 1), PadAction::FadeIn { .. }));
    }

    #[test]
    fn input_to_pip_with_same_input_as_bg_keeps_pad_static() {
        // Input(0) fullscreen → Pip{bg=0, overlays=[1]}: 0 stays at same fullscreen position
        let old = vec![fullscreen(0, PGM_Z)];
        let new = vec![fullscreen(0, PGM_Z), fullscreen(1, OVL_Z)];
        let plan = plan_transition(&old, &new);
        // Shared pad 0 with same geometry → AffirmStatic
        assert!(matches!(
            action_of(&plan, 0),
            PadAction::AffirmStatic { .. }
        ));
        // Incoming-only pad 1 → no morphing pad in plan → FadeIn
        assert!(matches!(action_of(&plan, 1), PadAction::FadeIn { .. }));
    }

    #[test]
    fn input_to_pip_source_becomes_overlay_morphs_lifted() {
        // Input(0) fullscreen → Pip{bg=1, overlays=[0]}:
        //   0 morphs from fullscreen to overlay (gets covered/revealed by going up)
        let old = vec![fullscreen(0, PGM_Z)];
        let new = vec![fullscreen(1, PGM_Z), ovl_a(0)];
        let plan = plan_transition(&old, &new);
        // 0 morphs; new state has no pad above pad 0 (pad 1 is at PGM_Z=1, pad 0 is at OVL_Z=2)
        // → LiftAndStep so 0 stays on top while moving
        match action_of(&plan, 0) {
            PadAction::Morph { z_handling, .. } => match z_handling {
                ZHandling::LiftAndStep { new_z } => assert_eq!(new_z, OVL_Z),
                _ => panic!("expected LiftAndStep, got {:?}", z_handling),
            },
            other => panic!("expected Morph, got {:?}", other),
        }
        // pad 1 is non-shared incoming, has_morphing_pad=true → HoldFullAlpha
        assert!(matches!(
            action_of(&plan, 1),
            PadAction::HoldFullAlpha { .. }
        ));
    }

    #[test]
    fn pip_to_input_overlay_zooms_to_fullscreen_lifted() {
        // Pip{bg=0, overlays=[1,2]} → Input(1) fullscreen
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1), ovl_b(2)];
        let new = vec![fullscreen(1, PGM_Z)];
        let plan = plan_transition(&old, &new);
        // pad 1 morphs; new state has no other pads → LiftAndStep
        match action_of(&plan, 1) {
            PadAction::Morph { z_handling, .. } => {
                assert_eq!(z_handling, ZHandling::LiftAndStep { new_z: PGM_Z })
            }
            other => panic!("expected Morph, got {:?}", other),
        }
        // pad 0 (old bg, fullscreen) is outgoing-only; no same-position incoming → StepOffAtEnd
        assert!(matches!(action_of(&plan, 0), PadAction::StepOffAtEnd));
        assert!(matches!(action_of(&plan, 2), PadAction::StepOffAtEnd));
    }

    #[test]
    fn pip_to_pip_no_overlap_pure_crossfade() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=2, overlays=[3]}: no shared sources
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(2, PGM_Z), ovl_a(3)];
        let plan = plan_transition(&old, &new);
        // No morphing pad → bg-vs-bg same-position FadeIn/FadeOut
        // overlays at same position → also FadeIn/FadeOut
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 1), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
        assert!(matches!(action_of(&plan, 3), PadAction::FadeIn { .. }));
    }

    #[test]
    fn pip_to_pip_overlay_becomes_bg_slides_under() {
        // Pip{bg=0, overlays=[1,2]} → Pip{bg=1, overlays=[3,4]}:
        //   pad 1 morphs from overlay → bg with overlays above → SnapToNew.
        //   pad 4 is at cell-b which is also pad 2's old position → cross-fade.
        //   pad 3 is at cell-a which had pad 1 (shared, excluded) → HoldFullAlpha.
        //   pad 0 (old bg fullscreen) — no non-shared incoming at fullscreen → StepOffAtEnd.
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1), ovl_b(2)];
        let new = vec![fullscreen(1, PGM_Z), ovl_a(3), ovl_b(4)];
        let plan = plan_transition(&old, &new);
        match action_of(&plan, 1) {
            PadAction::Morph { z_handling, .. } => assert_eq!(
                z_handling,
                ZHandling::SnapToNew(PGM_Z),
                "becoming bg with overlays above → snap z",
            ),
            other => panic!("expected Morph, got {:?}", other),
        }
        // pad 3 has no same-position outgoing partner (cell-a was the morphing
        // pad's old position; shared pads don't count) → HoldFullAlpha.
        assert!(matches!(
            action_of(&plan, 3),
            PadAction::HoldFullAlpha { .. }
        ));
        // pad 4 at cell-b has same-position partner (pad 2 outgoing) → cross-fade.
        assert!(matches!(action_of(&plan, 4), PadAction::FadeIn { .. }));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeOut));
        // pad 0 fullscreen has no same-position incoming → step off.
        assert!(matches!(action_of(&plan, 0), PadAction::StepOffAtEnd));
    }

    #[test]
    fn pip_to_pip_bg_becomes_overlay_lifts() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=2, overlays=[0]}: pad 0 bg→overlay
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(2, PGM_Z), ovl_a(0)];
        let plan = plan_transition(&old, &new);
        // pad 0 morphs; new state has no pad above pad 0's new zorder (OVL_Z) — pad 2 is at PGM_Z below
        // → LiftAndStep
        match action_of(&plan, 0) {
            PadAction::Morph { z_handling, .. } => {
                assert_eq!(z_handling, ZHandling::LiftAndStep { new_z: OVL_Z })
            }
            other => panic!("expected Morph, got {:?}", other),
        }
        assert!(matches!(
            action_of(&plan, 2),
            PadAction::HoldFullAlpha { .. }
        ));
        assert!(matches!(action_of(&plan, 1), PadAction::StepOffAtEnd));
    }

    #[test]
    fn pip_to_pip_shared_static_bg_crossfade_overlays() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=0, overlays=[2]}: 0 stationary, 1 out, 2 in
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(0, PGM_Z), ovl_a(2)];
        let plan = plan_transition(&old, &new);
        assert!(matches!(
            action_of(&plan, 0),
            PadAction::AffirmStatic { .. }
        ));
        // overlays at same position (cell-0) but different inputs → cross-fade
        assert!(matches!(action_of(&plan, 1), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
    }

    #[test]
    fn pip_to_pip_different_bg_same_overlay_crossfades_bg() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=2, overlays=[1]}: bg cross-fades, overlay static
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(2, PGM_Z), ovl_a(1)];
        let plan = plan_transition(&old, &new);
        // No morphing pad — pad 1 has same position. So no morph mode.
        assert!(matches!(
            action_of(&plan, 1),
            PadAction::AffirmStatic { .. }
        ));
        // Both bgs at fullscreen — same-position partners → cross-fade
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
    }

    #[test]
    fn incoming_outside_morph_start_fades_in_not_hold() {
        // Pip{overlays=[1]} → Pip{bg=2, overlays=[1 moved to cell-b], 3 at cell-c}
        // pad 1 morphs from cell-a → cell-b. morph_start = cell-a.
        // pad 2 (new bg) at fullscreen — NOT covered by cell-a → should FadeIn
        //   (the user's "background syns innan transition är klar" case).
        // pad 3 doesn't exist here; we use cell-c logic implicitly.
        let cell_a = pad(1, 0, 0, 480, 270, OVL_Z);
        let cell_b = pad(1, 480, 0, 480, 270, OVL_Z);
        let old = vec![cell_a];
        let new = vec![fullscreen(2, PGM_Z), cell_b];
        let plan = plan_transition(&old, &new);
        // pad 1 morphs cell-a → cell-b
        assert!(matches!(action_of(&plan, 1), PadAction::Morph { .. }));
        // pad 2 fullscreen incoming — not covered by morph_start (cell-a) → FadeIn
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
    }

    #[test]
    fn outgoing_outside_morph_end_fades_out_not_step() {
        // Pip{bg=0, overlays=[1 at cell-a]} → Input(1) but positioned at cell-c
        //   (hypothetical — destination not fullscreen).
        // pad 0 (outgoing fullscreen) is NOT covered by morph_end (cell-c) → FadeOut.
        let old = vec![fullscreen(0, PGM_Z), pad(1, 0, 0, 480, 270, OVL_Z)];
        // pad 1 morphs to a small destination (not fullscreen)
        let new = vec![pad(1, 100, 100, 600, 400, PGM_Z)];
        let plan = plan_transition(&old, &new);
        assert!(matches!(action_of(&plan, 1), PadAction::Morph { .. }));
        // pad 0 fullscreen — not covered by morph_end (600×400) → FadeOut
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
    }

    #[test]
    fn morph_with_same_position_partners_crossfades_those_partners() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=3, overlays=[1 moved]} where the bgs share
        // fullscreen position. pad 1 morphs (different overlay slot), so we ARE
        // in morph mode — but the bgs should still cross-fade because same-pos.
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(3, PGM_Z), ovl_b(1)];
        let plan = plan_transition(&old, &new);
        // pad 1 morphs from cell-0 to cell-1
        assert!(matches!(action_of(&plan, 1), PadAction::Morph { .. }));
        // Bgs at fullscreen — same-position partners → cross-fade even in morph mode
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 3), PadAction::FadeIn { .. }));
    }
}
