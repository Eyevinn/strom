//! Audio Bridge Output / Audio Bridge Input: the adaptive bridge between flows.
//!
//! Carries contributor audio from the flow that ingests it to a conversation
//! flow at a low, steady latency. The input side holds a small target backlog;
//! after a stall it plays the late audio slightly fast until it is back at the
//! target, instead of staying late or skipping. See `crate::gst::audio_bridge`.
//!
//! Time-scaled audio is for monitoring only. Never route an Audio Bridge Input
//! to a program output or a recording.
//!
//! ```text
//! Output:  audio_in → audioconvert → audioresample → capsfilter → stromaudiobridgesink
//! Input:   stromaudiobridgesrc → audio_out
//! ```

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::audio_bridge;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::audio_bridge::{
    DEFAULT_MAX_LATENCY_MS, DEFAULT_MAX_RATE_CHANGE_PERCENT, DEFAULT_TARGET_LATENCY_MS,
    MAX_MAX_LATENCY_MS, MAX_RATE_CHANGE_PERCENT, MAX_TARGET_LATENCY_MS, MIN_TARGET_LATENCY_MS,
};
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::info;

pub use strom_types::audio_bridge::{INPUT_BLOCK_ID, OUTPUT_BLOCK_ID};

/// Element id of the bridge source inside an Audio Bridge Input.
pub const BRIDGE_ELEMENT: &str = "bridge";

fn channel(properties: &HashMap<String, PropertyValue>) -> Result<String, BlockBuildError> {
    match properties.get("channel") {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(BlockBuildError::InvalidConfiguration(
            "channel name is required".to_string(),
        )),
    }
}

fn uint(properties: &HashMap<String, PropertyValue>, key: &str, default: u64) -> u64 {
    match properties.get(key) {
        Some(PropertyValue::UInt(u)) => *u,
        Some(PropertyValue::Int(i)) if *i >= 0 => *i as u64,
        _ => default,
    }
}

fn float(properties: &HashMap<String, PropertyValue>, key: &str, default: f64) -> f64 {
    match properties.get(key) {
        Some(PropertyValue::Float(f)) => *f,
        Some(PropertyValue::UInt(u)) => *u as f64,
        Some(PropertyValue::Int(i)) => *i as f64,
        _ => default,
    }
}

fn register() -> Result<(), BlockBuildError> {
    audio_bridge::register()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audio bridge elements: {e}")))
}

pub struct AudioBridgeOutputBuilder;

impl BlockBuilder for AudioBridgeOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        register()?;
        let channel = channel(properties)?;

        let make = |factory: &str, name: &str| {
            gst::ElementFactory::make(factory)
                .name(format!("{instance_id}:{name}"))
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{factory}: {e}")))
        };
        let convert = make("audioconvert", "convert")?;
        let resample = make("audioresample", "resample")?;
        let caps = make("capsfilter", "caps")?;
        caps.set_property("caps", audio_bridge::caps());
        let sink = make(audio_bridge::SINK_FACTORY, "sink")?;
        sink.set_property("channel", &channel);

        info!("Audio Bridge Output {instance_id} -> channel '{channel}'");

        let ids = ["convert", "resample", "caps", "sink"].map(|n| format!("{instance_id}:{n}"));
        let internal_links = ids
            .windows(2)
            .map(|w| {
                (
                    ElementPadRef::pad(&w[0], "src"),
                    ElementPadRef::pad(&w[1], "sink"),
                )
            })
            .collect();
        let [convert_id, resample_id, caps_id, sink_id] = ids;
        Ok(BlockBuildResult {
            elements: vec![
                (convert_id, convert),
                (resample_id, resample),
                (caps_id, caps),
                (sink_id, sink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

pub struct AudioBridgeInputBuilder;

impl BlockBuilder for AudioBridgeInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        register()?;
        if gst::ElementFactory::find("scaletempo").is_none() {
            return Err(BlockBuildError::ElementCreation(
                "scaletempo (gst-plugins-good) is not installed".to_string(),
            ));
        }
        let channel = channel(properties)?;
        let target = uint(properties, "target_latency_ms", DEFAULT_TARGET_LATENCY_MS)
            .clamp(MIN_TARGET_LATENCY_MS, MAX_TARGET_LATENCY_MS);
        let max_rate = float(
            properties,
            "max_rate_change_percent",
            DEFAULT_MAX_RATE_CHANGE_PERCENT,
        )
        .clamp(0.0, MAX_RATE_CHANGE_PERCENT);
        let max_latency = uint(properties, "max_latency_ms", DEFAULT_MAX_LATENCY_MS)
            .clamp(MIN_TARGET_LATENCY_MS, MAX_MAX_LATENCY_MS);

        let id = format!("{instance_id}:{BRIDGE_ELEMENT}");
        let bridge = gst::ElementFactory::make(audio_bridge::SRC_FACTORY)
            .name(&id)
            .property("channel", &channel)
            .property("target-latency", target as u32)
            .property("max-rate-change", max_rate)
            .property("max-latency", max_latency as u32)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audio bridge: {e}")))?;

        info!(
            "Audio Bridge Input {instance_id} <- channel '{channel}' \
             (target {target} ms, rate bound {max_rate} %, max {max_latency} ms)"
        );

        Ok(BlockBuildResult {
            elements: vec![(id, bridge)],
            internal_links: vec![],
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![output_definition(), input_definition()]
}

fn channel_property(description: &str) -> ExposedProperty {
    ExposedProperty {
        name: "channel".to_string(),
        label: "Channel".to_string(),
        description: description.to_string(),
        property_type: PropertyType::String,
        default_value: None,
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "channel".to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    }
}

fn ui_metadata(icon: &str) -> Option<BlockUIMetadata> {
    Some(BlockUIMetadata {
        icon: Some(icon.to_string()),
        width: Some(1.5),
        height: Some(1.5),
        // Inter-pipeline colours, like Inter Output / Inter Input.
        light_fill_color: Some("#FEF0E0".to_string()),
        light_stroke_color: Some("#C07020".to_string()),
        dark_fill_color: Some("#352A1E".to_string()),
        dark_stroke_color: Some("#E8A050".to_string()),
        ..Default::default()
    })
}

fn output_definition() -> BlockDefinition {
    BlockDefinition {
        id: OUTPUT_BLOCK_ID.to_string(),
        name: "Audio Bridge Output".to_string(),
        description: "Sends audio to an Audio Bridge Input in another flow, for low-latency \
            monitoring such as contributors hearing each other. Never holds up this flow."
            .to_string(),
        category: "Inter-Pipeline".to_string(),
        exposed_properties: vec![channel_property(
            "Channel name. The Audio Bridge Input with the same name receives this audio; \
             one output and one input per channel.",
        )],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "convert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: ui_metadata("📤"),
    }
}

fn input_definition() -> BlockDefinition {
    BlockDefinition {
        id: INPUT_BLOCK_ID.to_string(),
        name: "Audio Bridge Input".to_string(),
        description: "Receives audio from an Audio Bridge Output at a low target latency. \
            Covers a stall with silence and then plays slightly fast, without changing \
            pitch, until back at the target. For monitoring only: never route it to a \
            program output or a recording."
            .to_string(),
        category: "Inter-Pipeline".to_string(),
        exposed_properties: vec![
            channel_property("Channel name of the Audio Bridge Output to receive."),
            ExposedProperty {
                name: "target_latency_ms".to_string(),
                label: "Target Latency (ms)".to_string(),
                description: format!(
                    "Audio the bridge keeps in hand against late input, {MIN_TARGET_LATENCY_MS}-\
                     {MAX_TARGET_LATENCY_MS} ms. Input stalls shorter than this pass without a \
                     gap; longer ones leave a gap of stall minus target."
                ),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_TARGET_LATENCY_MS)),
                mapping: PropertyMapping {
                    element_id: BRIDGE_ELEMENT.to_string(),
                    property_name: "target-latency".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
            ExposedProperty {
                name: "max_rate_change_percent".to_string(),
                label: "Max Rate Change (%)".to_string(),
                description: format!(
                    "How far playback may speed up or slow down to return to the target, \
                     0-{MAX_RATE_CHANGE_PERCENT} %. At 5 % a 400 ms backlog drains in about \
                     8 s, at 10 % in about 4 s, but a faster drain leaves less in hand for the \
                     next hiccup, so a lossy link loses more audio. The catch-up is inaudible \
                     on speech at 5 % and at most very slightly noticeable at 10 %. 0 turns \
                     time-scaling off."
                ),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(DEFAULT_MAX_RATE_CHANGE_PERCENT)),
                mapping: PropertyMapping {
                    element_id: BRIDGE_ELEMENT.to_string(),
                    property_name: "max-rate-change".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
            ExposedProperty {
                name: "max_latency_ms".to_string(),
                label: "Max Latency (ms)".to_string(),
                description: "Backlog above which the bridge skips back to the target \
                    instead of draining it. Raised if it is set too close to the target \
                    to leave the bridge room to drain."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_MAX_LATENCY_MS)),
                mapping: PropertyMapping {
                    element_id: BRIDGE_ELEMENT.to_string(),
                    property_name: "max-latency".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: BRIDGE_ELEMENT.to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: ui_metadata("📥"),
    }
}
