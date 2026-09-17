//! Regression test for the per-block ICE transport policy override on WHIP Input.
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
//! WHIP Input is the one path where the resolved value is observable without a
//! live session: it is stored in the `WhipEndpointConfig` the block registers,
//! and the session manager hands it to each session's webrtcbin. The other
//! WHIP/WHEP builders pass the same resolved value into a
//! `deep-element-added` closure, which needs a negotiated session to observe.

use std::collections::HashMap;

use strom::blocks::builtin::whip::build_whipserversrc;
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
    gstreamer::init().expect("gst init");

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
