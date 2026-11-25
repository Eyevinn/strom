//! Authentication to a TAMS gateway as block properties, shared by every block
//! that talks to one (TAMS Output, the Media Player's timeline mode), so they
//! have the same properties, values and meaning.
//!
//! - `auth_mode`: `static` (default) or `osc`;
//! - `api_token`: the static bearer token; empty = `STROM_TAMS_API_TOKEN`, or
//!   no token;
//! - `osc_service_id`: the OSC service the SAT is scoped to; empty = derived
//!   from the gateway URL.

use crate::client_auth::AuthMethod;
use std::collections::HashMap;
use strom_types::block::{
    EnumValue, ExposedProperty, PropertyCondition, PropertyMapping, PropertyType,
};
use strom_types::PropertyValue;

pub const AUTH_MODE_PROPERTY: &str = "auth_mode";
pub const API_TOKEN_PROPERTY: &str = "api_token";
pub const OSC_SERVICE_ID_PROPERTY: &str = "osc_service_id";

/// The auth a block's properties ask for. The OSC variant is completed with a
/// credential key (the flow id) and the gateway URL where it is used.
#[derive(Clone)]
pub enum TamsAuth {
    /// A static bearer token, or none.
    Static(AuthMethod),
    /// OSC PAT/SAT, scoped to `service_id` (or one derived from the gateway).
    Osc { service_id: Option<String> },
}

fn prop_str(properties: &HashMap<String, PropertyValue>, key: &str) -> Option<String> {
    match properties.get(key) {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

impl TamsAuth {
    pub fn from_properties(properties: &HashMap<String, PropertyValue>) -> Self {
        match prop_str(properties, AUTH_MODE_PROPERTY).as_deref() {
            Some("osc") => TamsAuth::Osc {
                service_id: prop_str(properties, OSC_SERVICE_ID_PROPERTY),
            },
            _ => TamsAuth::Static(
                match prop_str(properties, API_TOKEN_PROPERTY)
                    .or_else(|| strom_types::env::var_opt("STROM_TAMS_API_TOKEN"))
                {
                    Some(t) => AuthMethod::Bearer(t),
                    None => AuthMethod::None,
                },
            ),
        }
    }
}

fn block_prop(
    name: &str,
    label: &str,
    description: &str,
    property_type: PropertyType,
    default: PropertyValue,
    visible_when: Option<PropertyCondition>,
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
        visible_when,
    }
}

/// The three auth properties. `visible_when` applies to all three: a block
/// that talks to a gateway only in some settings passes its condition.
pub fn auth_properties(visible_when: Option<PropertyCondition>) -> Vec<ExposedProperty> {
    vec![
        block_prop(
            AUTH_MODE_PROPERTY,
            "Authentication",
            "How to authenticate to the TAMS gateway. Static API Token = a bearer token. OSC PAT/SAT = Service Access Tokens minted from this Strom's OSC Personal Access Token.",
            PropertyType::Enum {
                values: vec![
                    EnumValue {
                        value: "static".to_string(),
                        label: Some("Static API Token".to_string()),
                        visible_when: None,
                    },
                    EnumValue {
                        value: "osc".to_string(),
                        label: Some("OSC PAT/SAT".to_string()),
                        visible_when: None,
                    },
                ],
            },
            PropertyValue::String("static".to_string()),
            visible_when.clone(),
        ),
        block_prop(
            API_TOKEN_PROPERTY,
            "API Token",
            "Static API Token mode: bearer token for the gateway. Empty = STROM_TAMS_API_TOKEN, or none (behind an access gate).",
            PropertyType::String,
            PropertyValue::String(String::new()),
            visible_when.clone(),
        ),
        block_prop(
            OSC_SERVICE_ID_PROPERTY,
            "OSC Service ID",
            "OSC PAT/SAT mode: the OSC service type the SAT is scoped to. Empty = derived from the gateway URL (*.osaas.io).",
            PropertyType::String,
            PropertyValue::String(String::new()),
            visible_when,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_osc_and_static_modes() {
        let mut p = HashMap::new();
        p.insert(AUTH_MODE_PROPERTY.to_string(), PropertyValue::from("osc"));
        p.insert(
            OSC_SERVICE_ID_PROPERTY.to_string(),
            PropertyValue::from("svc"),
        );
        assert!(matches!(
            TamsAuth::from_properties(&p),
            TamsAuth::Osc { service_id: Some(s) } if s == "svc"
        ));
        let mut p = HashMap::new();
        p.insert(API_TOKEN_PROPERTY.to_string(), PropertyValue::from("tok"));
        assert!(matches!(
            TamsAuth::from_properties(&p),
            TamsAuth::Static(AuthMethod::Bearer(t)) if t == "tok"
        ));
    }

    #[test]
    fn property_names_match_the_media_player_constants() {
        assert_eq!(
            AUTH_MODE_PROPERTY,
            strom_types::mediaplayer::AUTH_MODE_PROPERTY
        );
        assert_eq!(
            API_TOKEN_PROPERTY,
            strom_types::mediaplayer::API_TOKEN_PROPERTY
        );
        assert_eq!(
            OSC_SERVICE_ID_PROPERTY,
            strom_types::mediaplayer::OSC_SERVICE_ID_PROPERTY
        );
    }
}
