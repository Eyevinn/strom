//! Property parsing helpers for vision mixer block.

use std::collections::HashMap;
use strom_types::vision_mixer::{
    DEFAULT_DSK_INPUTS, DEFAULT_NUM_INPUTS, DEFAULT_SHOW_VU_METERS, MAX_DSK_INPUTS, MAX_NUM_INPUTS,
    MIN_NUM_INPUTS,
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
    strom_types::parse_resolution_string(s).unwrap_or_else(|| {
        strom_types::parse_resolution_string(default).expect("default resolution must be valid")
    })
}

/// Parse the `show_vu_meters` flag from block properties.
pub fn parse_show_vu_meters(properties: &HashMap<String, PropertyValue>) -> bool {
    parse_bool(properties, "show_vu_meters", DEFAULT_SHOW_VU_METERS)
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

/// Parse the output pixel format. Returns None for "Auto" (empty string).
pub fn parse_output_format(properties: &HashMap<String, PropertyValue>) -> Option<String> {
    properties.get("output_format").and_then(|v| match v {
        PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    })
}

/// Parse a framerate string "N/D" into (numerator, denominator).
pub fn parse_framerate(
    properties: &HashMap<String, PropertyValue>,
    key: &str,
    default: &str,
) -> (i32, i32) {
    let s = properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or(default);
    parse_framerate_string(s).unwrap_or_else(|| {
        parse_framerate_string(default).expect("default framerate must be valid")
    })
}

fn parse_framerate_string(s: &str) -> Option<(i32, i32)> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2 {
        let n = parts[0].parse::<i32>().ok()?;
        let d = parts[1].parse::<i32>().ok()?;
        if n > 0 && d > 0 {
            return Some((n, d));
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_framerate_string_valid_fractions() {
        assert_eq!(parse_framerate_string("25/1"), Some((25, 1)));
        assert_eq!(parse_framerate_string("30000/1001"), Some((30000, 1001)));
        assert_eq!(parse_framerate_string("1/1"), Some((1, 1)));
    }

    #[test]
    fn parse_framerate_string_rejects_invalid() {
        assert_eq!(parse_framerate_string("0/1"), None);
        assert_eq!(parse_framerate_string("25/0"), None);
        assert_eq!(parse_framerate_string("-1/1"), None);
        assert_eq!(parse_framerate_string("25"), None);
        assert_eq!(parse_framerate_string(""), None);
        assert_eq!(parse_framerate_string("abc/def"), None);
    }

    #[test]
    fn parse_framerate_uses_property_value() {
        let mut props = HashMap::new();
        props.insert("fps".to_string(), PropertyValue::String("60/1".to_string()));
        assert_eq!(parse_framerate(&props, "fps", "25/1"), (60, 1));
    }

    #[test]
    fn parse_framerate_falls_back_to_default() {
        let props = HashMap::new();
        assert_eq!(parse_framerate(&props, "fps", "25/1"), (25, 1));
    }

    #[test]
    fn parse_framerate_invalid_value_falls_back_to_default() {
        let mut props = HashMap::new();
        props.insert("fps".to_string(), PropertyValue::String("nope".to_string()));
        assert_eq!(parse_framerate(&props, "fps", "30/1"), (30, 1));
    }
}
