//! Regression tests for the per-block ICE transport policy override.
//!
//! The server-wide `server.ice_transport_policy` decides which ICE candidates
//! every WebRTC block may use. One ingest endpoint can sit behind a network
//! that host and server-reflexive candidates cannot cross, and forcing the
//! whole server onto TURN to serve it puts every other block on relay too.
//!
//! The block's own `ice_transport_policy` property overrides the server
//! setting, and an unset property must keep inheriting it — the override may
//! not change how an existing flow negotiates.
//!
//! Each block is checked where its resolved value actually lands:
//! - WHIP Input stores it in the `WhipEndpointConfig` it registers, which the
//!   session manager hands to every session's webrtcbin.
//! - WHIP Output (both implementations), WHEP Input (`whepsrc`) and WHEP Output
//!   set it on the element they build, which forwards it to the webrtcbin it
//!   owns, so the element can be read back straight after the build.
//!
//! `whepclientsrc` (WHEP Input, new implementation) is the one path left
//! unchecked: it exposes no such property, so its policy is applied to the
//! child webrtcbin from a `deep-element-added` handler that needs a negotiated
//! session to observe.

use std::collections::HashMap;

use strom::blocks::builtin::{get_builder, whip::build_whipserversrc};
use strom::blocks::BlockBuildContext;
use strom_types::PropertyValue;

/// Elements the slot chain needs. `whipserversrc` is deliberately not among
/// them: the chain built here is plain core GStreamer, so this runs on a CI
/// image without `gst-plugins-rs`.
const REQUIRED: &[&str] = &[
    "appsrc",
    "decodebin",
    "audioconvert",
    "audioresample",
    "capsfilter",
    "tee",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    init_gst();

    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gstreamer::ElementFactory::find(e).is_none())
        .collect();

    if missing.is_empty() {
        return true;
    }

    if std::env::var("STROM_REQUIRE_GST_PLUGINS").is_ok() {
        panic!("required GStreamer elements missing: {:?}", missing);
    }

    eprintln!("SKIP: required GStreamer elements missing: {:?}", missing);
    false
}

/// The WHIP/WHEP elements come from `gst-plugins-rs`, which is linked into the
/// binary and registered at startup. A test binary has to register it itself.
fn init_gst() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gstreamer::init().expect("gst init");
        gstwebrtchttp::plugin_register_static().expect("register webrtchttp plugins");
        gstrswebrtc::plugin_register_static().expect("register webrtc plugins");
    });
}

fn props(policy: Option<&str>) -> HashMap<String, PropertyValue> {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    // Audio only keeps the built chain small; the policy is per-endpoint and
    // has nothing to do with which media types the block carries.
    props.insert(
        "mode".to_string(),
        PropertyValue::String("audio".to_string()),
    );
    props.insert("max_sessions".to_string(), PropertyValue::Int(1));
    props.insert(
        "endpoint_id".to_string(),
        PropertyValue::String("ice-policy".to_string()),
    );
    if let Some(policy) = policy {
        props.insert(
            "ice_transport_policy".to_string(),
            PropertyValue::String(policy.to_string()),
        );
    }
    props
}

/// Build a WHIP Input block against a server configured with `server_policy`
/// and return the policy it registered for its sessions.
fn registered_policy(server_policy: &str, block_policy: Option<&str>) -> String {
    let ctx = BlockBuildContext::new(vec![], server_policy.to_string());
    build_whipserversrc("whip_in", &props(block_policy), &ctx).expect("WHIP Input block builds");

    let mut configs = ctx.take_whip_endpoint_configs();
    assert_eq!(configs.len(), 1, "WHIP Input registers one endpoint config");
    configs.remove(0).1.ice_transport_policy
}

#[test]
fn block_property_forces_relay_on_a_server_that_allows_all_candidates() {
    if !plugins_available() {
        return;
    }
    assert_eq!(registered_policy("all", Some("relay")), "relay");
}

#[test]
fn unset_block_property_inherits_the_server_policy() {
    if !plugins_available() {
        return;
    }
    assert_eq!(registered_policy("all", None), "all");
    assert_eq!(registered_policy("relay", None), "relay");
}

#[test]
fn block_property_can_widen_a_relay_only_server() {
    if !plugins_available() {
        return;
    }
    assert_eq!(registered_policy("relay", Some("all")), "all");
}

/// The value reaches webrtcbin through `set_property_from_str`, which panics on
/// a nick the enum does not know. Properties arrive over the API, where any
/// string is possible.
#[test]
fn unknown_block_property_falls_back_to_the_server_policy() {
    if !plugins_available() {
        return;
    }
    assert_eq!(registered_policy("all", Some("turn-only")), "all");
}

// ---------------------------------------------------------------------------
// WHIP Output, WHEP Input and WHEP Output: the policy is set on the element
// the block builds, which hands it to the webrtcbin it owns.
// ---------------------------------------------------------------------------

/// Elements whose own `ice-transport-policy` carries the block's setting.
/// Without `gst-plugins-rs` these blocks refuse to build at all.
const WEBRTC_ELEMENTS: &[&str] = &["whipsink", "whipclientsink", "whepsrc", "whepserversink"];

fn webrtc_elements_available() -> bool {
    init_gst();

    let missing: Vec<&str> = WEBRTC_ELEMENTS
        .iter()
        .copied()
        .filter(|e| gstreamer::ElementFactory::find(e).is_none())
        .collect();

    if missing.is_empty() {
        return true;
    }

    if std::env::var("STROM_REQUIRE_GST_PLUGINS").is_ok() {
        panic!("required GStreamer elements missing: {:?}", missing);
    }

    eprintln!("SKIP: required GStreamer elements missing: {:?}", missing);
    false
}

/// Build `block_id` with the given properties and read the ICE transport policy
/// back off the element whose id ends in `element_suffix`.
fn built_element_policy(
    block_id: &str,
    server_policy: &str,
    mut properties: HashMap<String, PropertyValue>,
    element_suffix: &str,
) -> String {
    use gstreamer::prelude::*;

    properties.insert(
        "whip_endpoint".to_string(),
        PropertyValue::String("http://192.0.2.10/whip".to_string()),
    );
    properties.insert(
        "whep_endpoint".to_string(),
        PropertyValue::String("http://192.0.2.10/whep".to_string()),
    );

    let ctx = BlockBuildContext::new(
        vec!["turn:user:secret@192.0.2.20:3478".to_string()],
        server_policy.to_string(),
    );
    let builder = get_builder(block_id).unwrap_or_else(|| panic!("no builder for {}", block_id));
    let built = builder
        .build("ice_policy", &properties, &ctx)
        .unwrap_or_else(|e| panic!("{} builds: {:?}", block_id, e));

    let (_, element) = built
        .elements
        .iter()
        .find(|(id, _)| id.ends_with(element_suffix))
        .unwrap_or_else(|| panic!("{} has no {} element", block_id, element_suffix));

    element
        .property_value("ice-transport-policy")
        .serialize()
        .expect("ice-transport-policy serializes")
        .to_string()
}

fn impl_props(implementation: &str, policy: Option<&str>) -> HashMap<String, PropertyValue> {
    let mut props = HashMap::new();
    props.insert(
        "implementation".to_string(),
        PropertyValue::String(implementation.to_string()),
    );
    if let Some(policy) = policy {
        props.insert(
            "ice_transport_policy".to_string(),
            PropertyValue::String(policy.to_string()),
        );
    }
    props
}

#[test]
fn whip_output_whipsink_carries_the_block_policy() {
    if !webrtc_elements_available() {
        return;
    }
    assert_eq!(
        built_element_policy(
            "builtin.whip_output",
            "all",
            impl_props("whipsink", Some("relay")),
            ":whipsink"
        ),
        "relay"
    );
    assert_eq!(
        built_element_policy(
            "builtin.whip_output",
            "all",
            impl_props("whipsink", None),
            ":whipsink"
        ),
        "all"
    );
}

#[test]
fn whip_output_whipclientsink_carries_the_block_policy() {
    if !webrtc_elements_available() {
        return;
    }
    assert_eq!(
        built_element_policy(
            "builtin.whip_output",
            "all",
            impl_props("whipclientsink", Some("relay")),
            ":whipclientsink"
        ),
        "relay"
    );
    assert_eq!(
        built_element_policy(
            "builtin.whip_output",
            "all",
            impl_props("whipclientsink", None),
            ":whipclientsink"
        ),
        "all"
    );
}

#[test]
fn whep_input_whepsrc_carries_the_block_policy() {
    if !webrtc_elements_available() {
        return;
    }
    assert_eq!(
        built_element_policy(
            "builtin.whep_input",
            "all",
            impl_props("whepsrc", Some("relay")),
            ":whepsrc"
        ),
        "relay"
    );
    assert_eq!(
        built_element_policy(
            "builtin.whep_input",
            "all",
            impl_props("whepsrc", None),
            ":whepsrc"
        ),
        "all"
    );
}

#[test]
fn whep_output_carries_the_block_policy() {
    if !webrtc_elements_available() {
        return;
    }
    assert_eq!(
        built_element_policy(
            "builtin.whep_output",
            "all",
            impl_props("", Some("relay")),
            ":whepserversink"
        ),
        "relay"
    );
    assert_eq!(
        built_element_policy(
            "builtin.whep_output",
            "all",
            impl_props("", None),
            ":whepserversink"
        ),
        "all"
    );
}

/// A relay-only server with no block override must still reach the element.
#[test]
fn server_policy_reaches_the_element_when_the_block_does_not_override_it() {
    if !webrtc_elements_available() {
        return;
    }
    assert_eq!(
        built_element_policy(
            "builtin.whip_output",
            "relay",
            impl_props("whipsink", None),
            ":whipsink"
        ),
        "relay"
    );
}
