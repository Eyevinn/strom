//! Block properties shared by the WHIP and WHEP blocks.

use std::collections::HashMap;
use strom_types::PropertyValue;

/// Parse do_retransmission from properties (default: true). Shared with the
/// WHIP blocks.
///
/// On `whepserversink`, which is send-only, this is a bandwidth/quality
/// tunable only.
pub(super) fn parse_do_retransmission(properties: &HashMap<String, PropertyValue>) -> bool {
    properties
        .get("do_retransmission")
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(true)
}

/// Parse drop_on_latency from properties (default: true). Shared with the
/// WHIP blocks.
///
/// True works around a GStreamer rtpjitterbuffer bug (see `build_whepsrc`'s
/// iterate_recurse). False keeps late packets for a downstream WebRTC endpoint
/// that buffers adaptively, and reinstates the stall.
pub(super) fn parse_drop_on_latency(properties: &HashMap<String, PropertyValue>) -> bool {
    properties
        .get("drop_on_latency")
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a property map from explicit key/value pairs.
    fn raw_props(entries: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn webrtc_bool_properties_default_true_and_honour_explicit_values() {
        // Shared by the WHIP and WHEP blocks.
        type Parser = fn(&HashMap<String, PropertyValue>) -> bool;
        let parsers: [(&str, Parser); 2] = [
            ("do_retransmission", parse_do_retransmission),
            ("drop_on_latency", parse_drop_on_latency),
        ];
        for (key, parse) in parsers {
            assert!(parse(&raw_props(&[])), "{} defaults to true", key);
            assert!(
                parse(&raw_props(&[(key, PropertyValue::Bool(true))])),
                "{}",
                key
            );
            assert!(
                !parse(&raw_props(&[(key, PropertyValue::Bool(false))])),
                "{}",
                key
            );
            // Not a Bool: falls back to the default
            assert!(
                parse(&raw_props(&[(key, PropertyValue::String("false".into()))])),
                "{}",
                key
            );
        }
    }
}
