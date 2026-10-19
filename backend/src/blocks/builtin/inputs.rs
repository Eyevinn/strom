//! Helpers for a block's input tracks: how many a block declares, and which of
//! them a flow actually links.

use crate::blocks::FlowLinkTargets;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::PropertyValue;

/// A track-count property (`num_audio_tracks`, `num_video_tracks`, ...) as the
/// block was given it, clamped to 0..=8, or `None` when it is absent.
///
/// Absent is not the same as 0: saved flows leave out properties at their
/// default value, so each block applies its own default to `None`.
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

/// Whether the flow feeds a block's input element.
///
/// True when the input's sink pad is linked, or when a flow link points into
/// it that construction could not make yet: a link from a sometimes pad (a raw
/// `decodebin` element's, say) waits in `pending_links` and is made when the
/// pad appears, after the setup hooks have run. Without the second half such
/// an input reads as unconnected and its block requests no muxer or sink pad
/// for it, so the late link has nowhere to go.
///
/// Call it from a `register_element_setup` hook, with the handle from
/// `ctx.flow_link_targets()`: the hooks run after construction's linking pass
/// and block expansion has filled the targets by then.
pub(crate) fn input_is_connected(
    input: &gst::glib::WeakRef<gst::Element>,
    targets: &FlowLinkTargets,
) -> bool {
    let Some(input) = input.upgrade() else {
        return false;
    };
    input.static_pad("sink").is_some_and(|p| p.is_linked()) || targets.contains(&input.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_count_is_clamped_and_absent_stays_absent() {
        let mut p = HashMap::new();
        assert_eq!(explicit_track_count(&p, "n"), None);
        p.insert("n".to_string(), PropertyValue::UInt(99));
        assert_eq!(explicit_track_count(&p, "n"), Some(8));
        p.insert("n".to_string(), PropertyValue::Int(-3));
        assert_eq!(explicit_track_count(&p, "n"), Some(0));
        p.insert("n".to_string(), PropertyValue::Int(2));
        assert_eq!(explicit_track_count(&p, "n"), Some(2));
    }

    #[test]
    fn an_input_is_connected_when_linked_or_a_flow_link_targets_it() {
        gst::init().expect("gst init");
        let src = gst::ElementFactory::make("fakesrc").build().unwrap();
        let input = gst::ElementFactory::make("identity")
            .name("blk:input")
            .build()
            .unwrap();
        let weak = input.downgrade();
        let targets = FlowLinkTargets::default();
        assert!(!input_is_connected(&weak, &targets));
        src.link(&input).unwrap();
        assert!(input_is_connected(&weak, &targets));
        src.unlink(&input);
        assert!(!input_is_connected(&weak, &targets));

        // A flow link that waits for a sometimes pad counts as connected.
        targets.insert("blk:input");
        assert!(input_is_connected(&weak, &targets));

        drop(input);
        assert!(
            !input_is_connected(&weak, &targets),
            "a dropped input is not connected"
        );
    }
}
