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
//!   option is false is ignored when the block is built: the block sees the
//!   property as unset and uses its default. A flow saved before the
//!   condition existed (or a codec switched after a profile was picked) keeps
//!   starting.
//!
//! In both cases [`effective_properties`] reports what it ignored, so the
//! backend can log it. A live write that a false condition rules out is
//! refused ([`write_refusal`]).
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
        match self.current(properties, exposed) {
            Some(value) => self.one_of.iter().any(|v| values_match(v, value)),
            None => false,
        }
    }

    /// The value the condition is evaluated against: the stored value of
    /// `property`, or its default.
    pub fn current<'a>(
        &self,
        properties: &'a HashMap<String, PropertyValue>,
        exposed: &'a [ExposedProperty],
    ) -> Option<&'a PropertyValue> {
        properties.get(&self.property).or_else(|| {
            exposed
                .iter()
                .find(|p| p.name == self.property)
                .and_then(|p| p.default_value.as_ref())
        })
    }

    /// [`Self::describe`] plus the current value, e.g.
    /// `codec is one of h264 (codec is vp9)`.
    fn describe_with_current(
        &self,
        properties: &HashMap<String, PropertyValue>,
        exposed: &[ExposedProperty],
    ) -> String {
        match self.current(properties, exposed) {
            Some(v) => format!(
                "{} ({} is {})",
                self.describe(),
                self.property,
                value_text(v)
            ),
            None => format!("{} ({} is unset)", self.describe(), self.property),
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

/// A stored value that [`effective_properties`] left out, and why.
#[derive(Debug, Clone)]
pub struct IgnoredValue {
    /// Name of the property whose stored value was left out.
    pub property: String,
    /// The stored value.
    pub value: PropertyValue,
    /// The condition that is false: the property's own `visible_when`, or the
    /// `visible_when` of the enum option the value names.
    pub condition: PropertyCondition,
    /// True when the condition is the enum option's, false when it is the
    /// property's.
    pub option: bool,
}

impl IgnoredValue {
    /// Human-readable form for a log line.
    pub fn describe(&self) -> String {
        let subject = if self.option {
            format!(
                "property '{}' has value '{}', which applies",
                self.property,
                value_text(&self.value)
            )
        } else {
            format!(
                "property '{}' (value '{}') applies",
                self.property,
                value_text(&self.value)
            )
        };
        format!(
            "{} only when {}; ignored, the default applies",
            subject,
            self.condition.describe()
        )
    }
}

/// The properties a block is built with: the stored `properties` without the
/// values a false `visible_when` rules out. That is a property whose own
/// condition is false, and an enum property whose stored value names an
/// option whose condition is false. Either way the block sees the property as
/// unset and uses its default. Keys that are not exposed properties
/// (internal `_`-prefixed values, for example) pass through.
///
/// Returns the left-out values too, so the caller can log them.
pub fn effective_properties(
    exposed: &[ExposedProperty],
    properties: &HashMap<String, PropertyValue>,
) -> (HashMap<String, PropertyValue>, Vec<IgnoredValue>) {
    let mut effective = properties.clone();
    let mut ignored = Vec::new();
    for prop in exposed {
        let Some(value) = properties.get(&prop.name) else {
            continue;
        };
        if let Some(condition) = prop
            .visible_when
            .as_ref()
            .filter(|c| !c.is_met(properties, exposed))
        {
            effective.remove(&prop.name);
            ignored.push(IgnoredValue {
                property: prop.name.clone(),
                value: value.clone(),
                condition: condition.clone(),
                option: false,
            });
            continue;
        }
        if let Some(condition) = ruled_out_option(prop, value, properties, exposed) {
            effective.remove(&prop.name);
            ignored.push(IgnoredValue {
                property: prop.name.clone(),
                value: value.clone(),
                condition: condition.clone(),
                option: true,
            });
        }
    }
    (effective, ignored)
}

/// Why a live write of `value` to the property `name` does not apply, or
/// `None` when it does. `properties` are the block's values as they would be
/// after the write (stored values with the rest of the batch applied).
///
/// Refused: a write to a property whose `visible_when` is false, and a write
/// of an enum value whose option's `visible_when` is false.
pub fn write_refusal(
    exposed: &[ExposedProperty],
    name: &str,
    value: &PropertyValue,
    properties: &HashMap<String, PropertyValue>,
) -> Option<String> {
    let prop = exposed.iter().find(|p| p.name == name)?;
    if let Some(condition) = prop
        .visible_when
        .as_ref()
        .filter(|c| !c.is_met(properties, exposed))
    {
        return Some(format!(
            "applies only when {}",
            condition.describe_with_current(properties, exposed)
        ));
    }
    ruled_out_option(prop, value, properties, exposed).map(|condition| {
        format!(
            "value '{}' applies only when {}",
            value_text(value),
            condition.describe_with_current(properties, exposed)
        )
    })
}

/// The condition of the enum option that `value` names, when it is false.
/// Enum values may be stored as numbers (`4` for `"4"`), so the option is
/// found by loose comparison.
fn ruled_out_option<'a>(
    prop: &'a ExposedProperty,
    value: &PropertyValue,
    properties: &HashMap<String, PropertyValue>,
    exposed: &[ExposedProperty],
) -> Option<&'a PropertyCondition> {
    let PropertyType::Enum { values } = &prop.property_type else {
        return None;
    };
    values
        .iter()
        .find(|v| values_match(&PropertyValue::String(v.value.clone()), value))?
        .visible_when
        .as_ref()
        .filter(|c| !c.is_met(properties, exposed))
}

/// Loose equality between a condition value and a stored value. Stored values
/// may arrive with a different variant (`4` vs `"4"`, `UInt` vs `Int`, `true`
/// vs `"true"`) depending on where they came from, so numbers compare by
/// value and bools match their string form.
fn values_match(want: &PropertyValue, have: &PropertyValue) -> bool {
    match (want, have) {
        (PropertyValue::String(a), PropertyValue::String(b)) => a == b,
        (PropertyValue::Bool(a), PropertyValue::Bool(b)) => a == b,
        (PropertyValue::Bool(b), PropertyValue::String(s))
        | (PropertyValue::String(s), PropertyValue::Bool(b)) => {
            s.trim().parse::<bool>().is_ok_and(|parsed| parsed == *b)
        }
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
        let (effective, ignored) = effective_properties(&exposed, &stored);
        assert!(!effective.contains_key("profile"));
        assert!(effective.contains_key("codec"));
        assert!(effective.contains_key("bitrate"));
        assert!(effective.contains_key("_flow_id"));
        // The stored map is untouched: switching back restores the value.
        assert!(stored.contains_key("profile"));
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].property, "profile");
        assert!(!ignored[0].option);
        let msg = ignored[0].describe();
        assert!(msg.contains("'high'"), "{msg}");
        assert!(msg.contains("codec is one of h264, h265"), "{msg}");
    }

    #[test]
    fn effective_properties_drops_a_ruled_out_enum_value() {
        let exposed = exposed();
        let (effective, ignored) = effective_properties(
            &exposed,
            &props(&[("codec", s("h265")), ("profile", s("high"))]),
        );
        assert!(!effective.contains_key("profile"));
        assert_eq!(ignored.len(), 1);
        assert!(ignored[0].option);
        let msg = ignored[0].describe();
        assert!(msg.contains("'profile'"), "{msg}");
        assert!(msg.contains("'high'"), "{msg}");
        assert!(msg.contains("codec is one of h264"), "{msg}");
    }

    #[test]
    fn effective_properties_accepts_a_visible_enum_value() {
        let exposed = exposed();
        let stored = props(&[("codec", s("h265")), ("profile", s("main-10"))]);
        let (effective, ignored) = effective_properties(&exposed, &stored);
        assert!(effective.contains_key("profile"));
        assert!(ignored.is_empty());
    }

    /// Enum values that look numeric may be stored as numbers; the option
    /// is still found and its condition still applies.
    #[test]
    fn effective_properties_checks_numeric_enum_values() {
        let channels = enum_prop(
            "channels",
            vec![
                option("2", None),
                option(
                    "8",
                    Some(PropertyCondition::one_of_strings("codec", &["h265"])),
                ),
            ],
            "2",
        );
        let exposed = vec![exposed().remove(0), channels];
        for stored in [PropertyValue::UInt(8), PropertyValue::Int(8), s("8")] {
            let (effective, ignored) =
                effective_properties(&exposed, &props(&[("channels", stored.clone())]));
            assert!(!effective.contains_key("channels"), "{stored:?}");
            assert_eq!(ignored.len(), 1, "{stored:?}");
            let refusal = write_refusal(&exposed, "channels", &stored, &HashMap::new());
            assert!(refusal.is_some(), "{stored:?}");
        }
        let (effective, _) =
            effective_properties(&exposed, &props(&[("channels", PropertyValue::UInt(2))]));
        assert!(effective.contains_key("channels"));
    }

    #[test]
    fn bools_match_their_string_form() {
        let cond = PropertyCondition {
            property: "on".to_string(),
            one_of: vec![PropertyValue::Bool(true)],
        };
        assert!(cond.is_met(&props(&[("on", s("true"))]), &[]));
        assert!(cond.is_met(&props(&[("on", PropertyValue::Bool(true))]), &[]));
        assert!(!cond.is_met(&props(&[("on", s("false"))]), &[]));
        assert!(!cond.is_met(&props(&[("on", s("yes"))]), &[]));
        let cond = PropertyCondition {
            property: "on".to_string(),
            one_of: vec![s("false")],
        };
        assert!(cond.is_met(&props(&[("on", PropertyValue::Bool(false))]), &[]));
        assert!(!cond.is_met(&props(&[("on", PropertyValue::Bool(true))]), &[]));
    }

    #[test]
    fn write_refusal_names_the_condition_and_current_value() {
        let exposed = exposed();
        // Hidden property.
        let reason = write_refusal(
            &exposed,
            "profile",
            &s("auto"),
            &props(&[("codec", s("vp9"))]),
        )
        .expect("refused");
        assert!(reason.contains("codec is one of h264, h265"), "{reason}");
        assert!(reason.contains("(codec is vp9)"), "{reason}");
        // Ruled-out option.
        let reason = write_refusal(
            &exposed,
            "profile",
            &s("high"),
            &props(&[("codec", s("h265"))]),
        )
        .expect("refused");
        assert!(reason.contains("'high'"), "{reason}");
        assert!(reason.contains("codec is one of h264"), "{reason}");
        // Allowed writes.
        let h265 = props(&[("codec", s("h265"))]);
        assert!(write_refusal(&exposed, "profile", &s("main-10"), &h265).is_none());
        assert!(write_refusal(&exposed, "bitrate", &PropertyValue::UInt(1), &h265).is_none());
        assert!(write_refusal(&exposed, "unknown", &s("x"), &h265).is_none());
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
