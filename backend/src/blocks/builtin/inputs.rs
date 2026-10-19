//! Helpers for a block's input tracks: how many a block declares, and which of
//! them a flow actually links.

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

/// Whether a block's input element has something linked to its sink pad.
///
/// Call it from a `register_element_setup` hook: those run after construction's
/// linking pass and before the pipeline leaves NULL, so the answer is exact for
/// every link resolved there. A link construction could not make at once (a
/// sometimes pad, such as a raw `decodebin` element's) is deferred to
/// `pending_links`, resolves after the hook, and reads as unconnected here.
pub(crate) fn input_is_connected(input: &gst::glib::WeakRef<gst::Element>) -> bool {
    input
        .upgrade()
        .and_then(|e| e.static_pad("sink"))
        .map(|p| p.is_linked())
        .unwrap_or(false)
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
    fn an_input_is_connected_only_while_its_sink_pad_is_linked() {
        gst::init().expect("gst init");
        let src = gst::ElementFactory::make("fakesrc").build().unwrap();
        let input = gst::ElementFactory::make("identity").build().unwrap();
        let weak = input.downgrade();
        assert!(!input_is_connected(&weak));
        src.link(&input).unwrap();
        assert!(input_is_connected(&weak));
        src.unlink(&input);
        assert!(!input_is_connected(&weak));
        drop(input);
        assert!(
            !input_is_connected(&weak),
            "a dropped input is not connected"
        );
    }
}
