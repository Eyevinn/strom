//! Time Offset block — shifts buffer timestamps via GStreamer pad-offset.
//!
//! A single `identity` element whose src pad has a GStreamer pad-offset
//! applied. The block does not buffer or jitter-buffer; it shifts the
//! `running_time` of buffers passing through, which is what aggregators
//! (compositor, audiomixer) and sinks use for synchronisation.
//!
//! The block is media-agnostic — caps pass through untouched, so the same
//! block can sit on an audio or video branch.
//!
//! Use cases:
//! - Lipsync: place a Time Offset block between an input and a mixer to
//!   align audio to video (or vice versa).
//! - Per-branch latency shaping: place one on a tee branch to give a
//!   downstream sink (e.g. WHEP PGM) more pipeline budget than another
//!   (e.g. WHEP multiview).
//!
//! `offset_ms` is signed: positive shifts buffers later (delay), negative
//! shifts them earlier (advance). With live sources only positive offsets
//! are physically meaningful — the pipeline cannot emit data ahead of the
//! source. Negative values are accepted but only have effect if upstream
//! buffering can absorb them.
//!
//! `offset_ms` is live-updatable. Updates flow through the standard exposed-
//! property path; `try_apply_live_offset` intercepts them and applies
//! `pad.set_offset()` instead of attempting an element-level property set.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    block::{
        BlockDefinition, BlockUIMetadata, ExposedProperty, ExternalPad, ExternalPads,
        PropertyMapping, PropertyType,
    },
    MediaType, PropertyValue,
};
use tracing::{debug, info, warn};

/// Suffix used for the internal identity element. Used by the live-update
/// interceptor to recognise a Time Offset block's element from its name.
const OFFSET_ELEMENT_SUFFIX: &str = "offset_identity";

/// Dotted form of the suffix for fast element-id matching without per-call allocation.
const OFFSET_ELEMENT_ID_TAIL: &str = ":offset_identity";

fn ms_to_ns(ms: f64) -> i64 {
    (ms * 1_000_000.0) as i64
}

fn parse_offset_ms(properties: &HashMap<String, PropertyValue>) -> f64 {
    properties
        .get("offset_ms")
        .and_then(|v| match v {
            PropertyValue::Float(f) => Some(*f),
            PropertyValue::Int(i) => Some(*i as f64),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// Try to handle a property update as a Time Offset block live-update.
///
/// Returns `true` if the element belongs to a Time Offset block and the
/// property was `offset_ms` — caller should skip the default property-set
/// path and trigger a latency recalculation. Returns `false` otherwise.
///
/// Coupling note: the matching keys (`offset_ms`, `:offset_identity`) mirror
/// the block's `PropertyMapping` and element naming. If those change in
/// `get_blocks()` / `build_offset()` this interceptor must be updated.
pub fn try_apply_live_offset(
    element: &gst::Element,
    element_id: &str,
    prop_name: &str,
    value: &PropertyValue,
) -> bool {
    if prop_name != "offset_ms" {
        return false;
    }
    if !element_id.ends_with(OFFSET_ELEMENT_ID_TAIL) {
        return false;
    }
    let ms = match value {
        PropertyValue::Float(f) => *f,
        PropertyValue::Int(i) => *i as f64,
        _ => {
            warn!(
                "Time Offset block '{}' received non-numeric offset_ms value {:?}",
                element_id, value
            );
            return false;
        }
    };
    let ns = ms_to_ns(ms);
    let Some(src_pad) = element.static_pad("src") else {
        return false;
    };
    src_pad.set_offset(ns);
    debug!(
        "Time Offset block '{}' offset_ms={} (offset_ns={})",
        element_id, ms, ns
    );
    true
}

fn is_offset_element(element_id: &str) -> bool {
    element_id.ends_with(OFFSET_ELEMENT_ID_TAIL)
}

fn read_offset_ms(element: &gst::Element) -> Option<PropertyValue> {
    let src_pad = element.static_pad("src")?;
    let ns = src_pad.offset();
    Some(PropertyValue::Float(ns as f64 / 1_000_000.0))
}

/// Mirror of `try_apply_live_offset` for the read path. Returns the current
/// `offset_ms` value derived from the src pad's offset if the element belongs
/// to a Time Offset block and `prop_name == "offset_ms"`. Returns `None`
/// otherwise so the caller falls through to the default element-property read.
pub fn try_read_live_offset(
    element: &gst::Element,
    element_id: &str,
    prop_name: &str,
) -> Option<PropertyValue> {
    if prop_name != "offset_ms" {
        return None;
    }
    if !is_offset_element(element_id) {
        return None;
    }
    read_offset_ms(element)
}

/// Returns the synthetic `(offset_ms, value)` entry to merge into a Time
/// Offset block's element-property listing. The underlying `identity`
/// element does not expose `offset_ms` as a GStreamer property — it lives
/// on the src pad — so callers listing all properties need this to surface
/// the block's exposed value.
pub fn live_offset_property_entry(
    element: &gst::Element,
    element_id: &str,
) -> Option<(String, PropertyValue)> {
    if !is_offset_element(element_id) {
        return None;
    }
    read_offset_ms(element).map(|v| ("offset_ms".to_string(), v))
}

fn build_offset(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
) -> Result<BlockBuildResult, BlockBuildError> {
    let offset_ms = parse_offset_ms(properties);
    let offset_ns = ms_to_ns(offset_ms);

    info!(
        "Building Time Offset block '{}': offset_ms={}, offset_ns={}",
        instance_id, offset_ms, offset_ns
    );

    let element_id = format!("{}:{}", instance_id, OFFSET_ELEMENT_SUFFIX);
    let identity = gst::ElementFactory::make("identity")
        .name(&element_id)
        .property("silent", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;

    if offset_ns != 0 {
        if let Some(src_pad) = identity.static_pad("src") {
            src_pad.set_offset(offset_ns);
        }
    }

    Ok(BlockBuildResult {
        elements: vec![(element_id, identity)],
        internal_links: vec![],
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

pub struct TimeOffsetBuilder;

impl BlockBuilder for TimeOffsetBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        build_offset(instance_id, properties)
    }
}

pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![BlockDefinition {
        id: "builtin.time_offset".to_string(),
        name: "Time Offset".to_string(),
        description: "Shifts buffer timestamps via GStreamer pad-offset. Media-agnostic — works for audio or video. Use for lipsync between audio and video, or to give different output branches different latency budgets. Live-updatable; the pipeline does not need to be restarted.".to_string(),
        category: "Effects".to_string(),
        exposed_properties: vec![ExposedProperty {
            name: "offset_ms".to_string(),
            label: "Offset (ms)".to_string(),
            description:
                "Buffer time-shift in milliseconds. Positive shifts the stream later (delay); negative shifts it earlier (advance, only meaningful when upstream buffering can absorb the shift)."
                    .to_string(),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(0.0)),
            mapping: PropertyMapping {
                element_id: OFFSET_ELEMENT_SUFFIX.to_string(),
                property_name: "offset_ms".to_string(),
                transform: None,
            },
            live: true,
        }],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "in".to_string(),
                media_type: MediaType::Generic,
                internal_element_id: OFFSET_ELEMENT_SUFFIX.to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
                name: "out".to_string(),
                media_type: MediaType::Generic,
                internal_element_id: OFFSET_ELEMENT_SUFFIX.to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("⏱".to_string()),
            width: Some(1.5),
            height: Some(1.0),
            ..Default::default()
        }),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        let _ = gst::init();
    }

    #[test]
    fn definition_is_generic() {
        let blocks = get_blocks();
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.id, "builtin.time_offset");
        assert_eq!(b.exposed_properties.len(), 1);
        assert_eq!(b.exposed_properties[0].name, "offset_ms");
        assert!(b.exposed_properties[0].live);
        assert!(matches!(
            b.external_pads.inputs[0].media_type,
            MediaType::Generic
        ));
        assert!(matches!(
            b.external_pads.outputs[0].media_type,
            MediaType::Generic
        ));
    }

    #[test]
    fn live_apply_sets_pad_offset_when_element_matches() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:offset_identity")
            .build()
            .expect("identity");

        let handled = try_apply_live_offset(
            &element,
            "blk1:offset_identity",
            "offset_ms",
            &PropertyValue::Float(50.0),
        );
        assert!(handled);
        let offset = element.static_pad("src").unwrap().offset();
        assert_eq!(offset, 50_000_000);
    }

    #[test]
    fn live_apply_handles_negative_offset() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:offset_identity")
            .build()
            .expect("identity");

        let handled = try_apply_live_offset(
            &element,
            "blk1:offset_identity",
            "offset_ms",
            &PropertyValue::Float(-50.0),
        );
        assert!(handled);
        let offset = element.static_pad("src").unwrap().offset();
        assert_eq!(offset, -50_000_000);
    }

    #[test]
    fn live_apply_skips_non_offset_element() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:something_else")
            .build()
            .expect("identity");

        let handled = try_apply_live_offset(
            &element,
            "blk1:something_else",
            "offset_ms",
            &PropertyValue::Float(50.0),
        );
        assert!(!handled);
    }

    #[test]
    fn live_read_returns_current_pad_offset() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:offset_identity")
            .build()
            .expect("identity");
        element.static_pad("src").unwrap().set_offset(75_000_000);

        let value = try_read_live_offset(&element, "blk1:offset_identity", "offset_ms")
            .expect("should return value for matching element/prop");
        assert!(matches!(value, PropertyValue::Float(v) if (v - 75.0).abs() < 1e-9));
    }

    #[test]
    fn live_read_reflects_apply() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:offset_identity")
            .build()
            .expect("identity");

        assert!(try_apply_live_offset(
            &element,
            "blk1:offset_identity",
            "offset_ms",
            &PropertyValue::Float(-30.0),
        ));

        let value = try_read_live_offset(&element, "blk1:offset_identity", "offset_ms")
            .expect("should round-trip the applied offset");
        assert!(matches!(value, PropertyValue::Float(v) if (v - (-30.0)).abs() < 1e-9));
    }

    #[test]
    fn live_read_skips_non_offset_element() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:something_else")
            .build()
            .expect("identity");
        assert!(try_read_live_offset(&element, "blk1:something_else", "offset_ms").is_none());
    }

    #[test]
    fn live_read_skips_other_property() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:offset_identity")
            .build()
            .expect("identity");
        assert!(try_read_live_offset(&element, "blk1:offset_identity", "other_prop").is_none());
    }

    #[test]
    fn property_entry_yields_offset_ms_for_offset_element() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:offset_identity")
            .build()
            .expect("identity");
        element.static_pad("src").unwrap().set_offset(42_000_000);

        let (name, value) = live_offset_property_entry(&element, "blk1:offset_identity")
            .expect("should produce an entry for offset elements");
        assert_eq!(name, "offset_ms");
        assert!(matches!(value, PropertyValue::Float(v) if (v - 42.0).abs() < 1e-9));
    }

    #[test]
    fn property_entry_skips_non_offset_element() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:something_else")
            .build()
            .expect("identity");
        assert!(live_offset_property_entry(&element, "blk1:something_else").is_none());
    }

    #[test]
    fn live_apply_skips_other_property() {
        init();
        let element = gst::ElementFactory::make("identity")
            .name("blk1:offset_identity")
            .build()
            .expect("identity");

        let handled = try_apply_live_offset(
            &element,
            "blk1:offset_identity",
            "other_prop",
            &PropertyValue::Float(50.0),
        );
        assert!(!handled);
    }

    #[test]
    fn build_applies_initial_offset() {
        init();
        let mut props = HashMap::new();
        props.insert("offset_ms".to_string(), PropertyValue::Float(120.0));
        let result = build_offset("blk2", &props).expect("build");
        assert_eq!(result.elements.len(), 1);
        let (id, element) = &result.elements[0];
        assert_eq!(id, "blk2:offset_identity");
        let offset = element.static_pad("src").unwrap().offset();
        assert_eq!(offset, 120_000_000);
    }
}
