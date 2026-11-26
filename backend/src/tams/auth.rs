//! Authentication to a TAMS gateway as block properties, shared by every block
//! that talks to one (TAMS Output, the Media Player's timeline mode), so they
//! have the same properties, values and meaning.
//!
//! - `auth_mode`: `static` (default) or `osc`;
//! - `api_token`: the static bearer token; empty = `STROM_TAMS_API_TOKEN` when
//!   the gateway is allow-listed, or no token;
//! - `osc_service_id`: the OSC service the SAT is scoped to; empty = derived
//!   from the gateway URL. A SAT only goes to an HTTPS OSC host of that service,
//!   or to an allow-listed gateway.

use super::client::{env_api_token, osc_service_id};
use crate::client_auth::AuthMethod;
use std::collections::HashMap;
use strom_types::block::{
    EnumValue, ExposedProperty, PropertyCondition, PropertyMapping, PropertyType,
};
use strom_types::PropertyValue;

pub const AUTH_MODE_PROPERTY: &str = "auth_mode";
pub const API_TOKEN_PROPERTY: &str = "api_token";
pub const OSC_SERVICE_ID_PROPERTY: &str = "osc_service_id";

/// The auth a block's properties ask for, checked against the gateway it is
/// used with. The instance's own credentials only go where they may:
/// the `STROM_TAMS_API_TOKEN` fallback only to an allow-listed gateway
/// ([`env_api_token`]), and an OSC SAT only to an OSC host of that service
/// or an allow-listed gateway ([`osc_service_id`]).
#[derive(Clone)]
pub enum TamsAuth {
    /// A static bearer token, or none.
    Static(AuthMethod),
    /// OSC PAT/SAT, scoped to `service_id`. Completed with a credential key
    /// (the flow id) where it is used.
    Osc { service_id: String },
}

fn prop_str(properties: &HashMap<String, PropertyValue>, key: &str) -> Option<String> {
    match properties.get(key) {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

impl TamsAuth {
    /// The auth for `gateway` from a block's properties. Fails in OSC mode
    /// when no service id is set or derivable, or when the SAT may not go to
    /// `gateway`.
    pub fn from_properties(
        properties: &HashMap<String, PropertyValue>,
        gateway: &str,
    ) -> Result<Self, String> {
        match prop_str(properties, AUTH_MODE_PROPERTY).as_deref() {
            Some("osc") => Ok(TamsAuth::Osc {
                service_id: osc_service_id(
                    gateway,
                    prop_str(properties, OSC_SERVICE_ID_PROPERTY).as_deref(),
                )?,
            }),
            _ => Ok(TamsAuth::Static(
                match prop_str(properties, API_TOKEN_PROPERTY).or_else(|| env_api_token(gateway)) {
                    Some(t) => AuthMethod::Bearer(t),
                    None => AuthMethod::None,
                },
            )),
        }
    }

    /// The concrete auth method; OSC SATs are minted from the PAT registered
    /// under `credential_key`.
    pub fn method(&self, credential_key: &str) -> AuthMethod {
        match self {
            TamsAuth::Static(auth) => auth.clone(),
            TamsAuth::Osc { service_id } => AuthMethod::Osc {
                provider: crate::osc::sat_provider(),
                service_id: service_id.clone(),
                credential_key: credential_key.to_string(),
            },
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
            "How to authenticate to the TAMS gateway. Static API Token = a bearer token. OSC PAT/SAT = short-lived Service Access Tokens minted from the OSC Personal Access Token configured on this Strom instance (STROM_OSC_PAT / OSC_ACCESS_TOKEN).",
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
            "Static API Token mode: bearer token for the gateway. Empty = the instance's STROM_TAMS_API_TOKEN, which is sent only to STROM_TAMS_GATEWAY_URL or a gateway listed in STROM_TAMS_ALLOWED_GATEWAYS; otherwise no token (behind an access gate).",
            PropertyType::String,
            PropertyValue::String(String::new()),
            visible_when.clone(),
        ),
        block_prop(
            OSC_SERVICE_ID_PROPERTY,
            "OSC Service ID",
            "OSC PAT/SAT mode: the OSC service type the SAT is scoped to. Empty = derived from the gateway URL (the service-type label of an *.osaas.io host). A SAT is sent only to an HTTPS OSC host of that service type, or to a gateway listed in STROM_TAMS_ALLOWED_GATEWAYS.",
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
        let gw = "https://acme-tams.eyevinn-tams-gateway.auto.prod.osaas.io";
        let mut p = HashMap::new();
        p.insert(AUTH_MODE_PROPERTY.to_string(), PropertyValue::from("osc"));
        assert!(matches!(
            TamsAuth::from_properties(&p, gw),
            Ok(TamsAuth::Osc { service_id }) if service_id == "eyevinn-tams-gateway"
        ));
        let mut p = HashMap::new();
        p.insert(API_TOKEN_PROPERTY.to_string(), PropertyValue::from("tok"));
        assert!(matches!(
            TamsAuth::from_properties(&p, gw),
            Ok(TamsAuth::Static(AuthMethod::Bearer(t))) if t == "tok"
        ));
    }

    #[test]
    fn osc_sat_does_not_go_to_another_host() {
        let mut p = HashMap::new();
        p.insert(AUTH_MODE_PROPERTY.to_string(), PropertyValue::from("osc"));
        p.insert(
            OSC_SERVICE_ID_PROPERTY.to_string(),
            PropertyValue::from("eyevinn-tams-gateway"),
        );
        assert!(TamsAuth::from_properties(&p, "https://tams.example.com").is_err());
        // User info that looks like an OSC host: the request would go to
        // evil.example. Neither the explicit nor the derived service id passes.
        let evil = "https://x.eyevinn-tams-gateway.auto.prod.osaas.io:pw@evil.example/";
        assert!(TamsAuth::from_properties(&p, evil).is_err());
        p.remove(OSC_SERVICE_ID_PROPERTY);
        assert!(TamsAuth::from_properties(&p, evil).is_err());
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
