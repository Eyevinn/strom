//! Conditional visibility for block properties and enum options.
//!
//! An [`ExposedProperty`] or an [`EnumValue`] may carry a
//! [`PropertyCondition`] in `visible_when`. The condition names one other
//! property of the same block and the values that make it true: the property
//! or option applies only while that property's current value (its stored
//! value, or its default when unset) is one of them.
//!
//! What a false condition means:
//! - Property: the UI hides it, and the backend ignores its stored value when
//!   it builds the block (the block sees the property as unset and uses its
//!   default). The stored value stays in the flow, so switching back
//!   restores it.
//! - Enum option: the UI leaves it out of the dropdown. A stored value whose
//!   option is false is an error when the block is built.
//!
//! Keep conditions flat: point a condition at a property that has no
//! condition of its own. Conditions are evaluated against the stored values,
//! not against what is left after hiding.
//!
//! Shared by backend and frontend so both read a condition the same way.

use crate::block::{EnumValue, ExposedProperty, PropertyType};
use crate::PropertyValue;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// True while the property named `property` has one of the values in `one_of`.
///
/// The property's current value is its stored value, or its default when the
/// block does not store one. A property with neither never matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PropertyCondition {
    /// Name of another exposed property of the same block.
    pub property: String,
    /// Values of that property that make the condition true.
    pub one_of: Vec<PropertyValue>,
}

impl PropertyCondition {
    /// Condition on a property with string (enum) values.
    pub fn one_of_strings(property: &str, values: &[&str]) -> Self {
        Self {
            property: property.to_string(),
            one_of: values
                .iter()
                .map(|v| PropertyValue::String(v.to_string()))
                .collect(),
        }
    }

    /// Whether the condition holds for a block's stored `properties`, with
    /// defaults taken from the block definition's `exposed` properties.
    pub fn is_met(
        &self,
        properties: &HashMap<String, PropertyValue>,
        exposed: &[ExposedProperty],
    ) -> bool {
        let current = properties.get(&self.property).or_else(|| {
            exposed
                .iter()
                .find(|p| p.name == self.property)
                .and_then(|p| p.default_value.as_ref())
        });
        match current {
            Some(value) => self.one_of.iter().any(|v| values_match(v, value)),
            None => false,
        }
    }

    /// Human-readable form for error messages, e.g. `codec is one of h264, h265`.
    pub fn describe(&self) -> String {
        let values: Vec<String> = self.one_of.iter().map(value_text).collect();
        format!("{} is one of {}", self.property, values.join(", "))
    }
}

impl ExposedProperty {
    /// Whether this property applies with the block's current settings.
    pub fn is_visible(
        &self,
        properties: &HashMap<String, PropertyValue>,
        exposed: &[ExposedProperty],
    ) -> bool {
        self.visible_when
            .as_ref()
            .is_none_or(|c| c.is_met(properties, exposed))
    }
}

impl EnumValue {
    /// Whether this option applies with the block's current settings.
    pub fn is_visible(
        &self,
        properties: &HashMap<String, PropertyValue>,
        exposed: &[ExposedProperty],
    ) -> bool {
        self.visible_when
            .as_ref()
            .is_none_or(|c| c.is_met(properties, exposed))
    }
}

/// The properties a block is built with: the stored `properties` without the
/// ones whose `visible_when` is false. Keys that are not exposed properties
/// (internal `_`-prefixed values, for example) pass through.
///
/// Errors when a visible enum property holds a value whose own `visible_when`
/// is false, naming the property, the value and the condition.
pub fn effective_properties(
    exposed: &[ExposedProperty],
    properties: &HashMap<String, PropertyValue>,
) -> Result<HashMap<String, PropertyValue>, String> {
    let mut effective = properties.clone();
    for prop in exposed {
        if !prop.is_visible(properties, exposed) {
            effective.remove(&prop.name);
            continue;
        }
        let PropertyType::Enum { values } = &prop.property_type else {
            continue;
        };
        let Some(PropertyValue::String(current)) = properties.get(&prop.name) else {
            continue;
        };
        let Some(option) = values.iter().find(|v| &v.value == current) else {
            continue;
        };
        if let Some(condition) = &option.visible_when {
            if !condition.is_met(properties, exposed) {
                return Err(format!(
                    "property '{}' has value '{}', which applies only when {}",
                    prop.name,
                    current,
                    condition.describe()
                ));
            }
        }
    }
    Ok(effective)
}

/// Loose equality between a condition value and a stored value. Stored values
/// may arrive with a different numeric variant (`4` vs `"4"`, `UInt` vs `Int`)
/// depending on where they came from, so numbers compare by value.
fn values_match(want: &PropertyValue, have: &PropertyValue) -> bool {
    match (want, have) {
        (PropertyValue::String(a), PropertyValue::String(b)) => a == b,
        (PropertyValue::Bool(a), PropertyValue::Bool(b)) => a == b,
        (PropertyValue::Bool(_), _) | (_, PropertyValue::Bool(_)) => false,
        _ => match (as_number(want), as_number(have)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
    }
}

fn as_number(v: &PropertyValue) -> Option<f64> {
    match v {
        PropertyValue::Int(i) => Some(*i as f64),
        PropertyValue::UInt(u) => Some(*u as f64),
        PropertyValue::Float(f) => Some(*f),
        PropertyValue::String(s) => s.trim().parse().ok(),
        PropertyValue::Bool(_) => None,
    }
}

fn value_text(v: &PropertyValue) -> String {
    match v {
        PropertyValue::String(s) => s.clone(),
        PropertyValue::Int(i) => i.to_string(),
        PropertyValue::UInt(u) => u.to_string(),
        PropertyValue::Float(f) => f.to_string(),
        PropertyValue::Bool(b) => b.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enum_prop(name: &str, values: Vec<EnumValue>, default: &str) -> ExposedProperty {
        ExposedProperty {
            name: name.to_string(),
            property_type: PropertyType::Enum { values },
            default_value: Some(PropertyValue::String(default.to_string())),
            ..Default::default()
        }
    }

    fn option(value: &str, visible_when: Option<PropertyCondition>) -> EnumValue {
        EnumValue {
            value: value.to_string(),
            label: None,
            visible_when,
        }
    }

    /// codec (h264 default), profile (only for h264/h265; "high" h264 only,
    /// "main-10" h265 only), and a plain bitrate.
    fn exposed() -> Vec<ExposedProperty> {
        let codec = enum_prop(
            "codec",
            vec![
                option("h264", None),
                option("h265", None),
                option("vp9", None),
            ],
            "h264",
        );
        let mut profile = enum_prop(
            "profile",
            vec![
                option("auto", None),
                option(
                    "high",
                    Some(PropertyCondition::one_of_strings("codec", &["h264"])),
                ),
                option(
                    "main-10",
                    Some(PropertyCondition::one_of_strings("codec", &["h265"])),
                ),
            ],
            "auto",
        );
        profile.visible_when = Some(PropertyCondition::one_of_strings(
            "codec",
            &["h264", "h265"],
        ));
        let bitrate = ExposedProperty {
            name: "bitrate".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(4000)),
            ..Default::default()
        };
        vec![codec, profile, bitrate]
    }

    fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn s(v: &str) -> PropertyValue {
        PropertyValue::String(v.to_string())
    }

    #[test]
    fn condition_uses_the_stored_value_or_the_default() {
        let exposed = exposed();
        let cond = PropertyCondition::one_of_strings("codec", &["h264", "h265"]);
        // Unset: default h264.
        assert!(cond.is_met(&HashMap::new(), &exposed));
        assert!(cond.is_met(&props(&[("codec", s("h265"))]), &exposed));
        assert!(!cond.is_met(&props(&[("codec", s("vp9"))]), &exposed));
    }

    #[test]
    fn condition_on_a_property_without_value_or_default_is_false() {
        let cond = PropertyCondition::one_of_strings("missing", &["x"]);
        assert!(!cond.is_met(&HashMap::new(), &exposed()));
    }

    #[test]
    fn numbers_match_across_variants() {
        let cond = PropertyCondition {
            property: "n".to_string(),
            one_of: vec![PropertyValue::UInt(4)],
        };
        for stored in [
            PropertyValue::UInt(4),
            PropertyValue::Int(4),
            PropertyValue::Float(4.0),
            s("4"),
        ] {
            assert!(
                cond.is_met(&props(&[("n", stored.clone())]), &[]),
                "{stored:?}"
            );
        }
        assert!(!cond.is_met(&props(&[("n", PropertyValue::UInt(5))]), &[]));
        assert!(!cond.is_met(&props(&[("n", PropertyValue::Bool(true))]), &[]));
    }

    #[test]
    fn property_and_option_visibility() {
        let exposed = exposed();
        let profile = &exposed[1];
        let PropertyType::Enum { values } = &profile.property_type else {
            unreachable!()
        };
        let h265 = props(&[("codec", s("h265"))]);
        assert!(profile.is_visible(&h265, &exposed));
        assert!(!profile.is_visible(&props(&[("codec", s("vp9"))]), &exposed));
        let visible: Vec<&str> = values
            .iter()
            .filter(|v| v.is_visible(&h265, &exposed))
            .map(|v| v.value.as_str())
            .collect();
        assert_eq!(visible, ["auto", "main-10"]);
        // Unconditioned property is always visible.
        assert!(exposed[2].is_visible(&h265, &exposed));
    }

    #[test]
    fn effective_properties_drops_hidden_properties_and_keeps_the_rest() {
        let exposed = exposed();
        let stored = props(&[
            ("codec", s("vp9")),
            ("profile", s("high")),
            ("bitrate", PropertyValue::UInt(2000)),
            ("_flow_id", s("f")),
        ]);
        let effective = effective_properties(&exposed, &stored).unwrap();
        assert!(!effective.contains_key("profile"));
        assert!(effective.contains_key("codec"));
        assert!(effective.contains_key("bitrate"));
        assert!(effective.contains_key("_flow_id"));
        // The stored map is untouched: switching back restores the value.
        assert!(stored.contains_key("profile"));
    }

    #[test]
    fn effective_properties_errors_on_a_hidden_enum_value() {
        let exposed = exposed();
        let err = effective_properties(
            &exposed,
            &props(&[("codec", s("h265")), ("profile", s("high"))]),
        )
        .unwrap_err();
        assert!(err.contains("'profile'"), "{err}");
        assert!(err.contains("'high'"), "{err}");
        assert!(err.contains("codec is one of h264"), "{err}");
    }

    #[test]
    fn effective_properties_accepts_a_visible_enum_value() {
        let exposed = exposed();
        let stored = props(&[("codec", s("h265")), ("profile", s("main-10"))]);
        let effective = effective_properties(&exposed, &stored).unwrap();
        assert!(effective.contains_key("profile"));
    }

    #[test]
    fn visible_when_round_trips_and_is_omitted_when_unset() {
        let exposed = exposed();
        let json = serde_json::to_value(&exposed).unwrap();
        assert!(json[0].get("visible_when").is_none());
        assert_eq!(json[1]["visible_when"]["property"], "codec");
        assert!(json[0]["property_type"]["enum"]["values"][0]
            .get("visible_when")
            .is_none());
        let back: Vec<ExposedProperty> = serde_json::from_value(json).unwrap();
        assert!(back[1].visible_when.is_some());
        assert!(back[0].visible_when.is_none());
    }
}
