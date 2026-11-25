//! Replay mode of the media player.
//!
//! With `replay_group` set, the block does not play a playlist. It holds
//! decoded frames of a segmented recording around a cursor shared by every
//! player of the group, and outputs the frame under the cursor on every tick
//! of the flow clock. Jog, scrub, slow motion and reverse move the cursor.
//!
//! Modules, from source to output:
//! - `source`: a `SourceIndex` per protocol (local `index.json`, TAMS);
//! - `cache`: index window and coded bytes around the cursor, fetched async;
//! - `decode`: segment bytes to frames on the player's own TAI frame grid
//!   (`grid`, the source's rate);
//! - `timeline`: the group's shared cursor (a TAI instant), range and live
//!   edge; each player shows its latest frame at or before the cursor;
//! - `player`: decode scheduling, frame store and the output tick.

pub mod backoff;
pub mod cache;
pub mod decode;
pub mod grid;
pub mod player;
pub mod source;
pub mod timeline;

use crate::blocks::{
    live_bridge_appsrc, BlockBuildError, BlockBuildResult, BusMessageConnectFn,
    APPSRC_MAX_BYTES_VIDEO,
};
use crate::client_auth::AuthMethod;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use strom_types::element::ElementPadRef;
use strom_types::{FlowId, PropertyValue};
use tracing::info;
use uuid::Uuid;

pub const REPLAY_GROUP_PROPERTY: &str = "replay_group";
pub const REPLAY_SOURCE_PROPERTY: &str = "replay_source";
pub const REPLAY_CACHE_PROPERTY: &str = "replay_cache_s";
pub const REPLAY_PREFETCH_PROPERTY: &str = "replay_prefetch_s";
pub const REPLAY_AUTH_MODE_PROPERTY: &str = "replay_auth_mode";
pub const REPLAY_API_TOKEN_PROPERTY: &str = "replay_api_token";
pub const REPLAY_OSC_SERVICE_PROPERTY: &str = "replay_osc_service_id";
pub const REPLAY_OUTPUT_FRAMERATE_PROPERTY: &str = "replay_output_framerate";

/// Parse a frame rate: `N/D`, or a whole number. Empty = `None`.
pub fn parse_frame_rate(s: &str) -> Result<Option<(u32, u32)>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let bad = || format!("bad frame rate '{}', want e.g. 50/1 or 30000/1001", s);
    let (n, d) = match s.split_once('/') {
        Some((n, d)) => (n.trim().parse::<u32>(), d.trim().parse::<u32>()),
        None => (s.parse::<u32>(), Ok(1)),
    };
    match (n, d) {
        (Ok(n), Ok(d)) if n > 0 && d > 0 => Ok(Some((n, d))),
        _ => Err(bad()),
    }
}

fn prop_str(properties: &HashMap<String, PropertyValue>, key: &str) -> Option<String> {
    match properties.get(key) {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// The replay group, when the block is in replay mode.
pub fn replay_group(properties: &HashMap<String, PropertyValue>) -> Option<String> {
    prop_str(properties, REPLAY_GROUP_PROPERTY)
}

pub fn build(
    instance_id: &str,
    group: &str,
    properties: &HashMap<String, PropertyValue>,
) -> Result<BlockBuildResult, BlockBuildError> {
    let flow_id: FlowId = prop_str(properties, "_flow_id")
        .and_then(|s| Uuid::parse_str(&s).ok())
        .unwrap_or_else(Uuid::nil);
    let media_path = prop_str(properties, "_media_path").unwrap_or_else(|| "./media".to_string());
    let source = prop_str(properties, REPLAY_SOURCE_PROPERTY).ok_or_else(|| {
        BlockBuildError::InvalidProperty(
            "Media Player replay mode: replay_source is required (a segment directory or \
             tams+https://<gateway>/flows/<flow_id>)"
                .to_string(),
        )
    })?;
    let invalid =
        |e: String| BlockBuildError::InvalidProperty(format!("Media Player replay mode: {}", e));
    let uint = |key: &str, default: u64| match properties.get(key) {
        Some(PropertyValue::UInt(u)) => *u,
        Some(PropertyValue::Int(i)) if *i > 0 => *i as u64,
        _ => default,
    };
    let cache_ahead_s = uint(REPLAY_CACHE_PROPERTY, 3);
    let prefetch_s = uint(REPLAY_PREFETCH_PROPERTY, 10);
    let auth = match prop_str(properties, REPLAY_AUTH_MODE_PROPERTY).as_deref() {
        Some("osc") => source::AuthConfig::Osc {
            service_id: prop_str(properties, REPLAY_OSC_SERVICE_PROPERTY),
        },
        _ => source::AuthConfig::Fixed(
            match prop_str(properties, REPLAY_API_TOKEN_PROPERTY)
                .or_else(|| strom_types::env::var_opt("STROM_TAMS_API_TOKEN"))
            {
                Some(t) => AuthMethod::Bearer(t),
                None => AuthMethod::None,
            },
        ),
    };
    let output_rate = parse_frame_rate(
        &prop_str(properties, REPLAY_OUTPUT_FRAMERATE_PROPERTY).unwrap_or_default(),
    )
    .map_err(invalid)?;
    let spec = source::parse_source(&source, &media_path).map_err(invalid)?;
    let index = source::open(&spec, &auth, &flow_id.to_string()).map_err(invalid)?;
    info!(
        "Media Player {}: replay mode, group '{}', source {}",
        instance_id,
        group,
        index.describe()
    );

    let appsrc_id = format!("{}:appsrc_video", instance_id);
    let queue_id = format!("{}:queue_video", instance_id);
    let out_id = format!("{}:video_out", instance_id);
    let appsrc = live_bridge_appsrc(APPSRC_MAX_BYTES_VIDEO)
        .name(&appsrc_id)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();
    let queue = gst::ElementFactory::make("queue")
        .name(&queue_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", queue_id, e)))?;
    let out = gst::ElementFactory::make("identity")
        .name(&out_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", out_id, e)))?;

    let timeline = timeline::get_or_create(flow_id, group);
    let player = player::ReplayPlayer::new(
        instance_id,
        index,
        timeline,
        &appsrc,
        cache_ahead_s,
        prefetch_s,
        output_rate,
    );

    // The bus handler owns the player for the flow's run: when the flow
    // stops, the handler is disconnected and dropped, and the guard shuts the
    // player down. A flow built but never started drops the guard with the
    // unused handler.
    let guard = player::PlayerGuard(player);
    let bus_message_handler: BusMessageConnectFn = Box::new(move |bus: &gst::Bus, _, _| {
        guard.0.start();
        bus.connect_message(None, move |_, _| {
            let _ = &guard;
        })
    });

    Ok(BlockBuildResult {
        elements: vec![
            (appsrc_id.clone(), appsrc.upcast()),
            (queue_id.clone(), queue),
            (out_id.clone(), out),
        ],
        internal_links: vec![
            (
                ElementPadRef::pad(&appsrc_id, "src"),
                ElementPadRef::pad(&queue_id, "sink"),
            ),
            (
                ElementPadRef::pad(&queue_id, "src"),
                ElementPadRef::pad(&out_id, "sink"),
            ),
        ],
        bus_message_handler: Some(bus_message_handler),
        pad_properties: HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_frame_rate;

    #[test]
    fn frame_rates_parse() {
        assert_eq!(parse_frame_rate(""), Ok(None));
        assert_eq!(parse_frame_rate("50/1"), Ok(Some((50, 1))));
        assert_eq!(parse_frame_rate(" 30000/1001 "), Ok(Some((30000, 1001))));
        assert_eq!(parse_frame_rate("100"), Ok(Some((100, 1))));
        assert!(parse_frame_rate("0/1").is_err());
        assert!(parse_frame_rate("29.97").is_err());
        assert!(parse_frame_rate("50/").is_err());
    }
}
