//! WHEP Output - hosts a WHEP server for clients to connect and receive streams.
//!
//! `whepserversink` hosts the HTTP endpoint; clients connect via WHEP to receive.

pub(crate) mod definition;
mod whepserversink;

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::ice_preflight;
use std::collections::HashMap;
use strom_types::block::StreamMode;
use strom_types::{block::*, PropertyValue, *};
use tracing::debug;

use whepserversink::build_whepserversink;

/// WHEP Output block builder (hosts WHEP server).
pub struct WHEPOutputBuilder;

impl BlockBuilder for WHEPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Output block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHEP Output")?;
        build_whepserversink(instance_id, properties, ctx)
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let (num_audio_tracks, num_video_tracks) = resolve_track_counts(properties);
        let has_video = num_video_tracks > 0;
        let has_audio = num_audio_tracks > 0;

        let mut inputs = Vec::new();

        for slot in 0..num_video_tracks {
            // Slot 0 keeps the unsuffixed names (video_in / video_queue) so
            // existing flows continue to link without modification when
            // num_video_tracks grows past 1.
            let (pad_name, queue_id) = if slot == 0 {
                ("video_in".to_string(), "video_queue".to_string())
            } else {
                (
                    format!("video_in_{}", slot),
                    format!("video_queue_{}", slot),
                )
            };

            let label = if has_audio || num_video_tracks > 1 {
                Some(format!("V{}", slot))
            } else {
                None
            };

            inputs.push(ExternalPad {
                label,
                name: pad_name,
                media_type: MediaType::Video,
                internal_element_id: queue_id,
                internal_pad_name: "sink".to_string(),
            });
        }

        for slot in 0..num_audio_tracks {
            // Slot 0 keeps the unsuffixed names (audio_in / audio_queue) so
            // existing flows continue to link without modification when
            // num_audio_tracks grows past 1. Other blocks use audio_in_0 for
            // slot 0; this asymmetry is deliberate for backwards compat.
            let (pad_name, queue_id) = if slot == 0 {
                ("audio_in".to_string(), "audio_queue".to_string())
            } else {
                (
                    format!("audio_in_{}", slot),
                    format!("audio_queue_{}", slot),
                )
            };

            let label = if has_video || num_audio_tracks > 1 {
                Some(format!("A{}", slot))
            } else {
                None
            };

            inputs.push(ExternalPad {
                label,
                name: pad_name,
                media_type: MediaType::Audio,
                internal_element_id: queue_id,
                internal_pad_name: "sink".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }
}

/// Resolve audio and video track counts from block properties.
///
/// Returns `(num_audio_tracks, num_video_tracks)`. 0 means the media type is
/// disabled on this endpoint; 1..=8 produces that many request pads.
///
/// Resolution order per media type:
/// 1. Explicit `num_audio_tracks` / `num_video_tracks` property (clamped to 0..=8).
/// 2. Legacy `mode` enum (`"audio"` / `"video"` / `"audio_video"`): translated
///    to 0 or 1 based on which media types it enabled. Allows old flows saved
///    before the count-based API to keep working.
/// 3. Default `1` when no property is present, matching the previous behaviour
///    where a missing `mode` was treated as audio+video.
pub(super) fn resolve_track_counts(properties: &HashMap<String, PropertyValue>) -> (usize, usize) {
    let explicit_audio = explicit_track_count(properties, "num_audio_tracks");
    let explicit_video = explicit_track_count(properties, "num_video_tracks");
    let legacy_mode = properties.get("mode").and_then(|v| match v {
        PropertyValue::String(s) => Some(StreamMode::parse(s)),
        _ => None,
    });

    let num_audio = match (explicit_audio, &legacy_mode) {
        (Some(n), _) => n,
        (None, Some(m)) => {
            if m.has_audio() {
                1
            } else {
                0
            }
        }
        (None, None) => 1,
    };
    let num_video = match (explicit_video, &legacy_mode) {
        (Some(n), _) => n,
        (None, Some(m)) => {
            if m.has_video() {
                1
            } else {
                0
            }
        }
        (None, None) => 1,
    };

    (num_audio, num_video)
}

pub(crate) fn explicit_track_count(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
) -> Option<usize> {
    properties.get(name).and_then(|v| match v {
        PropertyValue::UInt(u) => Some((*u as usize).min(8)),
        PropertyValue::Int(i) => Some((*i).clamp(0, 8) as usize),
        _ => None,
    })
}

/// Migrate a legacy `mode` property on a WHEP Output block to explicit
/// `num_audio_tracks` / `num_video_tracks` counts, and drop `mode`.
///
/// Without this, the property panel shows `num_video_tracks = 1` (the schema
/// default) for an audio-only flow, but the backend keeps computing 0 video
/// pads from the still-present `mode: "audio"`. Setting the value back to its
/// default 1 in the UI is a no-op (frontend stores only diffs against the
/// default), so the block diagram never grows a video port.
///
/// Only count fields not already present are filled in, so partial migrations
/// don't clobber user-set values. Idempotent.
///
/// Returns `true` when something changed.
pub fn migrate_legacy_mode(properties: &mut HashMap<String, PropertyValue>) -> bool {
    let Some(PropertyValue::String(mode_str)) = properties.get("mode").cloned() else {
        return false;
    };
    let mode = StreamMode::parse(&mode_str);

    // Only insert counts that diverge from the schema default (UInt(1)). This
    // keeps the saved properties minimal and lets the UI's "reset to default"
    // still work intuitively.
    if !mode.has_audio() {
        properties
            .entry("num_audio_tracks".to_string())
            .or_insert(PropertyValue::UInt(0));
    }
    if !mode.has_video() {
        properties
            .entry("num_video_tracks".to_string())
            .or_insert(PropertyValue::UInt(0));
    }

    properties.remove("mode");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a property map. `legacy_mode` populates the old "mode" enum
    /// (`Some("audio")` etc.) to exercise the migration path; pass `None` for
    /// new flows. Count values pass through `num_audio_tracks` /
    /// `num_video_tracks`.
    fn props(
        legacy_mode: Option<&str>,
        num_audio_tracks: Option<i64>,
        num_video_tracks: Option<i64>,
    ) -> HashMap<String, PropertyValue> {
        let mut p = HashMap::new();
        if let Some(m) = legacy_mode {
            p.insert("mode".to_string(), PropertyValue::String(m.to_string()));
        }
        if let Some(c) = num_audio_tracks {
            p.insert("num_audio_tracks".to_string(), PropertyValue::Int(c));
        }
        if let Some(c) = num_video_tracks {
            p.insert("num_video_tracks".to_string(), PropertyValue::Int(c));
        }
        p
    }

    fn audio_pad_names(pads: &ExternalPads) -> Vec<String> {
        pads.inputs
            .iter()
            .filter(|p| matches!(p.media_type, MediaType::Audio))
            .map(|p| p.name.clone())
            .collect()
    }

    fn video_pad_names(pads: &ExternalPads) -> Vec<String> {
        pads.inputs
            .iter()
            .filter(|p| matches!(p.media_type, MediaType::Video))
            .map(|p| p.name.clone())
            .collect()
    }

    #[test]
    fn external_pads_default_is_one_video_and_one_audio() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, None, None))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
    }

    #[test]
    fn external_pads_zero_audio_disables_audio() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(0), None))
            .expect("expected pads");
        assert!(audio_pad_names(&pads).is_empty());
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
    }

    #[test]
    fn external_pads_zero_video_disables_video() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, None, Some(0)))
            .expect("expected pads");
        assert!(video_pad_names(&pads).is_empty());
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
    }

    #[test]
    fn external_pads_count_clamped_to_max_eight() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(99), Some(99)))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads).len(), 8);
        assert_eq!(video_pad_names(&pads).len(), 8);
    }

    #[test]
    fn external_pads_count_clamped_to_min_zero() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(-5), Some(-1)))
            .expect("expected pads");
        assert!(audio_pad_names(&pads).is_empty());
        assert!(video_pad_names(&pads).is_empty());
    }

    #[test]
    fn external_pads_audio_count_four() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(4), Some(0)))
            .expect("expected pads");
        // Slot 0 keeps the unsuffixed name; subsequent slots are suffixed.
        assert_eq!(
            audio_pad_names(&pads),
            vec!["audio_in", "audio_in_1", "audio_in_2", "audio_in_3"],
        );
        assert!(video_pad_names(&pads).is_empty());
    }

    #[test]
    fn external_pads_video_count_four() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(0), Some(4)))
            .expect("expected pads");
        assert_eq!(
            video_pad_names(&pads),
            vec!["video_in", "video_in_1", "video_in_2", "video_in_3"],
        );
        assert!(audio_pad_names(&pads).is_empty());
    }

    #[test]
    fn external_pads_video_three_with_audio_one() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(1), Some(3)))
            .expect("expected pads");
        assert_eq!(
            video_pad_names(&pads),
            vec!["video_in", "video_in_1", "video_in_2"],
        );
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
    }

    // --- Legacy mode migration ---

    #[test]
    fn legacy_mode_audio_video_with_no_counts_yields_one_plus_one() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("audio_video"), None, None))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
    }

    #[test]
    fn legacy_mode_audio_only_with_no_counts_disables_video() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("audio"), None, None))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
        assert!(video_pad_names(&pads).is_empty());
    }

    #[test]
    fn legacy_mode_video_only_with_no_counts_disables_audio() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("video"), None, None))
            .expect("expected pads");
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
        assert!(audio_pad_names(&pads).is_empty());
    }

    #[test]
    fn explicit_counts_override_legacy_mode() {
        // Counts are authoritative when present; legacy mode is ignored.
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("audio"), Some(2), Some(3)))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads).len(), 2);
        assert_eq!(video_pad_names(&pads).len(), 3);
    }

    // --- migrate_legacy_mode ---

    #[test]
    fn migrate_audio_only_inserts_zero_video_and_drops_mode() {
        let mut p = props(Some("audio"), None, None);
        let changed = migrate_legacy_mode(&mut p);
        assert!(changed);
        assert!(!p.contains_key("mode"));
        // num_audio_tracks defaults to 1 — leave it implicit
        assert!(!p.contains_key("num_audio_tracks"));
        // num_video_tracks must be explicit 0 to match audio-only intent
        assert!(matches!(
            p.get("num_video_tracks"),
            Some(PropertyValue::UInt(0))
        ));
    }

    #[test]
    fn migrate_video_only_inserts_zero_audio_and_drops_mode() {
        let mut p = props(Some("video"), None, None);
        let changed = migrate_legacy_mode(&mut p);
        assert!(changed);
        assert!(!p.contains_key("mode"));
        assert!(matches!(
            p.get("num_audio_tracks"),
            Some(PropertyValue::UInt(0))
        ));
        assert!(!p.contains_key("num_video_tracks"));
    }

    #[test]
    fn migrate_audio_video_just_drops_mode() {
        let mut p = props(Some("audio_video"), None, None);
        let changed = migrate_legacy_mode(&mut p);
        assert!(changed);
        assert!(!p.contains_key("mode"));
        // Both default to 1 — nothing explicit needed
        assert!(!p.contains_key("num_audio_tracks"));
        assert!(!p.contains_key("num_video_tracks"));
    }

    #[test]
    fn migrate_is_noop_when_no_mode() {
        let mut p = props(None, Some(2), Some(3));
        let before = p.clone();
        let changed = migrate_legacy_mode(&mut p);
        assert!(!changed);
        assert_eq!(p.len(), before.len());
    }

    #[test]
    fn migrate_preserves_explicit_counts() {
        // mode says audio-only but explicit num_video_tracks=2 was set —
        // user wins, migration must not overwrite it.
        let mut p = props(Some("audio"), None, Some(2));
        migrate_legacy_mode(&mut p);
        assert!(!p.contains_key("mode"));
        assert!(matches!(
            p.get("num_video_tracks"),
            Some(PropertyValue::Int(2))
        ));
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut p = props(Some("audio"), None, None);
        migrate_legacy_mode(&mut p);
        let snapshot = p.clone();
        let changed = migrate_legacy_mode(&mut p);
        assert!(!changed);
        assert_eq!(p.len(), snapshot.len());
    }

    #[test]
    fn migrated_audio_then_resolve_yields_no_video() {
        // After migration the explicit num_video_tracks=0 keeps the legacy
        // intent without depending on the now-removed mode property.
        let mut p = props(Some("audio"), None, None);
        migrate_legacy_mode(&mut p);
        let pads = WHEPOutputBuilder
            .get_external_pads(&p)
            .expect("expected pads");
        assert!(video_pad_names(&pads).is_empty());
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
    }
}
