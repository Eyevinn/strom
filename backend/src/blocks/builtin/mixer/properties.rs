use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::{parse_common_audio_sample_rate, PropertyValue, DEFAULT_AUDIO_SAMPLE_RATE};
use tracing::warn;

use super::{DEFAULT_CHANNELS, MAX_AUX_BUSES, MAX_CHANNELS, MAX_GROUPS, MIN_KNEE_LINEAR};

/// Read a count property. A negative integer counts as zero so the caller's
/// clamp lands on its minimum; `as usize` would wrap it to `usize::MAX`.
fn parse_count(value: &PropertyValue) -> Option<usize> {
    match value {
        PropertyValue::Int(i) => Some(usize::try_from(*i).unwrap_or(0)),
        PropertyValue::UInt(u) => Some(usize::try_from(*u).unwrap_or(usize::MAX)),
        PropertyValue::String(s) => s.parse::<usize>().ok(),
        _ => None,
    }
}

/// Parse number of channels from properties.
pub(super) fn parse_num_channels(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_channels")
        .and_then(parse_count)
        .unwrap_or(DEFAULT_CHANNELS)
        .clamp(1, MAX_CHANNELS)
}

/// Parse number of aux buses from properties.
pub(super) fn parse_num_aux_buses(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_aux_buses")
        .and_then(parse_count)
        .unwrap_or(0)
        .clamp(0, MAX_AUX_BUSES)
}

/// Parse number of groups from properties.
pub(super) fn parse_num_groups(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_groups")
        .and_then(parse_count)
        .unwrap_or(0)
        .clamp(0, MAX_GROUPS)
}

/// Parse the rate every bus runs at. An unset property gives
/// `DEFAULT_AUDIO_SAMPLE_RATE`; a value outside the common rates is logged
/// and falls back to the default too.
pub(super) fn parse_sample_rate(properties: &HashMap<String, PropertyValue>) -> u32 {
    match properties.get("sample_rate") {
        None => DEFAULT_AUDIO_SAMPLE_RATE,
        Some(v) => parse_common_audio_sample_rate(v).unwrap_or_else(|| {
            warn!(
                "Unsupported mixer sample_rate {:?}, using {}",
                v, DEFAULT_AUDIO_SAMPLE_RATE
            );
            DEFAULT_AUDIO_SAMPLE_RATE
        }),
    }
}

/// Get a float property with default.
pub(super) fn get_float_prop(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: f64,
) -> f64 {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::Float(f) => Some(*f),
            PropertyValue::Int(i) => Some(*i as f64),
            _ => None,
        })
        .unwrap_or(default)
}

/// Get a u64 property with default.
pub(super) fn get_u64_prop(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: u64,
) -> u64 {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u),
            PropertyValue::Int(i) => Some(*i as u64),
            PropertyValue::Float(f) => Some(*f as u64),
            PropertyValue::String(s) => s.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(default)
}

/// Get a bool property with default.
pub(super) fn get_bool_prop(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: bool,
) -> bool {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(default)
}

/// Get a string property with default.
pub(super) fn get_string_prop<'a>(
    properties: &'a HashMap<String, PropertyValue>,
    name: &str,
    default: &'a str,
) -> &'a str {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or(default)
}

/// Convert dB to linear scale.
pub(super) fn db_to_linear(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

/// Convert linear scale to dB.
pub(super) fn linear_to_db(linear: f64) -> f64 {
    if linear <= 0.0 {
        -120.0 // floor
    } else {
        20.0 * linear.log10()
    }
}

/// Translate a property name and value from LV2 conventions to lsp-rs conventions.
///
/// The ExposedProperty mappings use LV2 property names (gt, at, rt, al, cr, mk, kn, th, f-N, g-N, q-N).
/// When the target element is from lsp-plugins-rs, this function translates the property name
/// and adjusts the value format where needed (e.g., LV2 uses linear gain, Rust uses dB).
///
/// Returns a list of (translated_prop_name, translated_value) pairs, or empty if no translation needed.
/// May return multiple pairs when one LV2 property maps to multiple Rust properties.
pub fn translate_property_for_element(
    element: &gst::Element,
    prop_name: &str,
    value: &PropertyValue,
) -> Vec<(String, PropertyValue)> {
    // Use GObject type name instead of factory() which can SIGSEGV
    // when static plugins and LV2 plugins coexist.
    let type_name = element.type_().name();

    if type_name == "LspRsGate" {
        let pairs = match prop_name {
            "gt" => {
                // LV2: gt is linear (already transformed by db_to_linear).
                // Rust: open-threshold/close-threshold are dB. Reverse the transform.
                let db_val = match value {
                    PropertyValue::Float(v) => linear_to_db(*v),
                    _ => return vec![],
                };
                vec![
                    ("open-threshold".to_string(), PropertyValue::Float(db_val)),
                    ("close-threshold".to_string(), PropertyValue::Float(db_val)),
                ]
            }
            "at" => vec![("attack".to_string(), value.clone())],
            "rt" => vec![("release".to_string(), value.clone())],
            "enabled" => return vec![], // same name, no translation needed
            _ => return vec![],
        };
        return pairs;
    }

    if type_name == "LspRsCompressor" {
        let pair = match prop_name {
            "al" => ("threshold".to_string(), value.clone()),
            "cr" => ("ratio".to_string(), value.clone()),
            "at" => ("attack".to_string(), value.clone()),
            "rt" => ("release".to_string(), value.clone()),
            "mk" => ("makeup-gain".to_string(), value.clone()),
            "kn" => {
                // Same clamp as the build path, so a live write lands on the
                // value the next restart would build.
                let knee = match value {
                    PropertyValue::Float(v) => v.clamp(MIN_KNEE_LINEAR, 1.0),
                    _ => return vec![],
                };
                ("knee".to_string(), PropertyValue::Float(knee))
            }
            "enabled" => return vec![],
            _ => return vec![],
        };
        return vec![pair];
    }

    if type_name == "LspRsEqualizer" {
        // EQ band properties: f-N -> bandN-frequency, g-N -> bandN-gain, q-N -> bandN-q
        if let Some(band) = prop_name.strip_prefix("f-") {
            return vec![(format!("band{}-frequency", band), value.clone())];
        }
        if let Some(band) = prop_name.strip_prefix("g-") {
            // LV2: g-N is linear (already transformed by db_to_linear).
            // Rust: bandN-gain is dB. Reverse the transform.
            let db_val = match value {
                PropertyValue::Float(v) => linear_to_db(*v),
                _ => return vec![],
            };
            return vec![(format!("band{}-gain", band), PropertyValue::Float(db_val))];
        }
        if let Some(band) = prop_name.strip_prefix("q-") {
            return vec![(format!("band{}-q", band), value.clone())];
        }
        return vec![];
    }

    if type_name == "LspRsLimiter" {
        let pair = match prop_name {
            "th" => {
                // LV2: th is linear (already transformed by db_to_linear).
                // Rust: threshold is dB. Reverse the transform.
                let db_val = match value {
                    PropertyValue::Float(v) => linear_to_db(*v),
                    _ => return vec![],
                };
                ("threshold".to_string(), PropertyValue::Float(db_val))
            }
            "enabled" => return vec![],
            _ => return vec![],
        };
        return vec![pair];
    }

    vec![]
}

#[cfg(test)]
mod clamping_tests {
    use super::*;

    fn props(key: &str, value: PropertyValue) -> HashMap<String, PropertyValue> {
        HashMap::from([(key.to_string(), value)])
    }

    #[test]
    fn negative_counts_clamp_to_the_minimum() {
        // `-1 as usize` wraps to usize::MAX, which the clamp then turned into
        // the largest mixer the block can build.
        assert_eq!(
            parse_num_channels(&props("num_channels", PropertyValue::Int(-1))),
            1
        );
        assert_eq!(
            parse_num_aux_buses(&props("num_aux_buses", PropertyValue::Int(-1))),
            0
        );
        assert_eq!(
            parse_num_groups(&props("num_groups", PropertyValue::Int(-5))),
            0
        );
        assert_eq!(
            parse_num_channels(&props("num_channels", PropertyValue::Int(4))),
            4
        );
    }

    fn live_knee(linear: f64) -> f64 {
        let _ = gst::init();
        let _ = gst_plugins_lsp::plugin_register_static();
        let comp = gst::ElementFactory::make("lsp-rs-compressor")
            .build()
            .expect("lsp-rs-compressor is statically registered");
        let out = translate_property_for_element(&comp, "kn", &PropertyValue::Float(linear));
        match out.as_slice() {
            [(name, PropertyValue::Float(v))] if name == "knee" => *v,
            other => panic!("kn should translate to one Float knee, got {:?}", other),
        }
    }

    #[test]
    fn live_knee_is_clamped_like_the_build_path() {
        // The builder clamps the knee to MIN_KNEE_LINEAR..=1.0. A live write
        // must land on the same value: otherwise -30 dB is applied live but
        // the next restart builds -24 dB, and anything above 0 dB is rejected
        // by the element's param spec (0.0..=1.0) instead of being clamped.
        assert_eq!(live_knee(db_to_linear(-30.0)), MIN_KNEE_LINEAR);
        assert_eq!(live_knee(db_to_linear(6.0)), 1.0);
        let mid = db_to_linear(-6.0);
        assert_eq!(live_knee(mid), mid);
    }
}
