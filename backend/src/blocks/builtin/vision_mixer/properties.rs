//! Property parsing helpers for vision mixer block.

use std::collections::HashMap;
use strom_types::vision_mixer::{
    DEFAULT_DSK_INPUTS, DEFAULT_NUM_INPUTS, MAX_DSK_INPUTS, MAX_NUM_INPUTS, MIN_NUM_INPUTS,
};
use strom_types::PropertyValue;

/// Parse the number of DSK inputs from block properties (0-2).
pub fn parse_num_dsk_inputs(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_dsk_inputs")
        .and_then(|v| match v {
            PropertyValue::String(s) => s.parse::<usize>().ok(),
            PropertyValue::UInt(n) => Some(*n as usize),
            PropertyValue::Int(n) => Some(*n as usize),
            _ => None,
        })
        .unwrap_or(DEFAULT_DSK_INPUTS)
        .min(MAX_DSK_INPUTS)
}

/// Parse the number of inputs from block properties, clamped to valid range.
pub fn parse_num_inputs(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_inputs")
        .and_then(|v| match v {
            PropertyValue::String(s) => s.parse::<usize>().ok(),
            PropertyValue::UInt(n) => Some(*n as usize),
            PropertyValue::Int(n) => Some(*n as usize),
            _ => None,
        })
        .unwrap_or(DEFAULT_NUM_INPUTS)
        .clamp(MIN_NUM_INPUTS, MAX_NUM_INPUTS)
}

/// Parse the initial PGM input index from block properties.
pub fn parse_initial_pgm(properties: &HashMap<String, PropertyValue>, num_inputs: usize) -> usize {
    properties
        .get("initial_pgm_input")
        .and_then(|v| match v {
            PropertyValue::UInt(n) => Some(*n as usize),
            PropertyValue::Int(n) => Some(*n as usize),
            PropertyValue::String(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(strom_types::vision_mixer::DEFAULT_PGM_INPUT)
        .min(num_inputs.saturating_sub(1))
}

/// Parse the initial PVW input index from block properties.
pub fn parse_initial_pvw(properties: &HashMap<String, PropertyValue>, num_inputs: usize) -> usize {
    properties
        .get("initial_pvw_input")
        .and_then(|v| match v {
            PropertyValue::UInt(n) => Some(*n as usize),
            PropertyValue::Int(n) => Some(*n as usize),
            PropertyValue::String(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(strom_types::vision_mixer::DEFAULT_PVW_INPUT)
        .min(num_inputs.saturating_sub(1))
}

/// Parse input labels from block properties, falling back to "In N" defaults.
pub fn parse_input_labels(
    properties: &HashMap<String, PropertyValue>,
    num_inputs: usize,
) -> Vec<String> {
    (0..num_inputs)
        .map(|i| {
            properties
                .get(&format!("input_{}_label", i))
                .and_then(|v| match v {
                    PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| format!("In {}", i + 1))
        })
        .collect()
}

/// Parse a resolution string property, returning (width, height).
pub fn parse_resolution(
    properties: &HashMap<String, PropertyValue>,
    key: &str,
    default: &str,
) -> (u32, u32) {
    let s = properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or(default);
    strom_types::parse_resolution_string(s).unwrap_or((1920, 1080))
}

/// Parse a boolean property with a default.
pub fn parse_bool(properties: &HashMap<String, PropertyValue>, key: &str, default: bool) -> bool {
    properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(default)
}

/// Parse a u64 property with a default.
pub fn parse_u64(properties: &HashMap<String, PropertyValue>, key: &str, default: u64) -> u64 {
    properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::UInt(n) => Some(*n),
            PropertyValue::Int(n) => Some(*n as u64),
            PropertyValue::String(s) => s.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(default)
}
