//! Reference Timestamp block: stamps each buffer with its wall-clock time as a
//! `GstReferenceTimestampMeta`, and optionally burns that time into the picture.
//!
//! The meta is the carrier for capture time in a flow: TAMS Output anchors its
//! timeline on it, and other consumers can read it. The stamp is the pipeline
//! clock's time for the buffer (see [`stamper`]), so place the block where the
//! stream runs on the clock: after a live source, or after `clocksync`.
//!
//! With `burn_in`, the same meta is rendered into the frame as UTC text and a
//! machine-readable code row (see [`burn_in`]). That makes the time a store gives
//! a frame checkable against the picture. Burn-in needs raw video; the block
//! converts what arrives to a format it can draw on.

pub mod burn_in;
pub mod stamper;

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::video_adapt::VIDEO_CONVERT_FACTORY;
use gstreamer as gst;
use gstreamer::prelude::*;
use stamper::{ReferenceTimestamper, Timescale};
use std::collections::HashMap;
use strom_types::{
    block::{
        BlockDefinition, BlockUIMetadata, EnumValue, ExposedProperty, ExternalPad, ExternalPads,
        PropertyMapping, PropertyType,
    },
    element::ElementPadRef,
    MediaType, PropertyValue,
};
use tracing::info;

pub const BLOCK_ID: &str = "builtin.reference_timestamp";
const STAMP_SUFFIX: &str = "stamp";
const CONVERT_SUFFIX: &str = "convert";
const BURN_IN_SUFFIX: &str = "burn_in";

fn timescale(properties: &HashMap<String, PropertyValue>) -> Timescale {
    match properties.get("timescale") {
        Some(PropertyValue::String(s)) if s == "tai" => Timescale::Tai,
        _ => Timescale::Utc,
    }
}

fn burn_in_enabled(properties: &HashMap<String, PropertyValue>) -> bool {
    matches!(properties.get("burn_in"), Some(PropertyValue::Bool(true)))
}

pub struct ReferenceTimestampBuilder;

impl BlockBuilder for ReferenceTimestampBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        let timescale = timescale(properties);
        let burn_in = burn_in_enabled(properties);
        info!(
            "Building Reference Timestamp block '{}': timescale {}, burn_in {}",
            instance_id,
            timescale.caps_name(),
            burn_in
        );

        let stamp_id = format!("{}:{}", instance_id, STAMP_SUFFIX);
        let stamper = ReferenceTimestamper::new(&stamp_id, timescale);
        // Share the flow's running time -> TAI mapping, known only at start.
        let weak = stamper.downgrade();
        ctx.register_element_setup(Box::new(move |flow_id, _events| {
            if let Some(s) = weak.upgrade() {
                s.set_flow_id(flow_id);
            }
        }));

        let mut elements = vec![(stamp_id.clone(), stamper.upcast::<gst::Element>())];
        let mut internal_links = Vec::new();
        if burn_in {
            let convert_id = format!("{}:{}", instance_id, CONVERT_SUFFIX);
            let burn_id = format!("{}:{}", instance_id, BURN_IN_SUFFIX);
            let convert = gst::ElementFactory::make(VIDEO_CONVERT_FACTORY)
                .name(&convert_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("videoconvert: {}", e)))?;
            let burn = burn_in::TimestampBurnIn::new(&burn_id);
            internal_links.push((
                ElementPadRef::pad(&stamp_id, "src"),
                ElementPadRef::pad(&convert_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&burn_id, "sink"),
            ));
            elements.push((convert_id, convert));
            elements.push((burn_id, burn.upcast()));
        }

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        Some(external_pads(burn_in_enabled(properties)))
    }
}

fn external_pads(burn_in: bool) -> ExternalPads {
    let (media_type, out_element) = if burn_in {
        (MediaType::Video, BURN_IN_SUFFIX)
    } else {
        (MediaType::Generic, STAMP_SUFFIX)
    };
    ExternalPads {
        inputs: vec![ExternalPad {
            label: None,
            name: "in".to_string(),
            media_type,
            internal_element_id: STAMP_SUFFIX.to_string(),
            internal_pad_name: "sink".to_string(),
        }],
        outputs: vec![ExternalPad {
            label: None,
            name: "out".to_string(),
            media_type,
            internal_element_id: out_element.to_string(),
            internal_pad_name: "src".to_string(),
        }],
    }
}

fn block_prop(
    name: &str,
    label: &str,
    description: &str,
    property_type: PropertyType,
    default: PropertyValue,
) -> ExposedProperty {
    ExposedProperty {
        name: name.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        property_type,
        default_value: Some(default),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: name.to_string(),
            transform: None,
        },
        live: false,
        persist: None,
        visible_when: None,
    }
}

pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![BlockDefinition {
        id: BLOCK_ID.to_string(),
        name: "Reference Timestamp".to_string(),
        description: "Stamps each buffer with its wall-clock time (pipeline clock) as a reference timestamp meta, which TAMS Output uses as capture time. Optionally burns the time into the video as UTC text and a machine-readable code.".to_string(),
        category: "Video".to_string(),
        exposed_properties: vec![
            block_prop(
                "timescale",
                "Timescale",
                "Timescale of the stamp: UTC (timestamp/x-unix) or TAI (timestamp/x-tai).",
                PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "utc".to_string(),
                            label: Some("UTC".to_string()),
                            visible_when: None,
                        },
                        EnumValue {
                            value: "tai".to_string(),
                            label: Some("TAI".to_string()),
                            visible_when: None,
                        },
                    ],
                },
                PropertyValue::String("utc".to_string()),
            ),
            block_prop(
                "burn_in",
                "Burn In",
                "Draw the time into the picture as UTC text and a code row. Needs raw video.",
                PropertyType::Bool,
                PropertyValue::Bool(false),
            ),
        ],
        external_pads: external_pads(false),
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🕒".to_string()),
            width: Some(1.8),
            height: Some(1.0),
            ..Default::default()
        }),
    }]
}

#[cfg(test)]
mod tests;
