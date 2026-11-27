//! Timeline mode of the media player (`mode=timeline`), used for replay.
//!
//! In this mode the block does not play a playlist. It holds
//! decoded frames of a segmented recording around a cursor shared by every
//! player of the group, and outputs the frame under the cursor on every tick
//! of the flow clock. Jog, scrub, slow motion and reverse move the cursor.
//!
//! Modules, from source to output:
//! - `source`: a `SourceIndex` per protocol (TAMS; an experimental local `index.json`);
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

use super::bridge::NetworkBuffer;
use crate::blocks::{
    live_bridge_appsrc, BlockBuildError, BlockBuildResult, BusMessageConnectFn,
    APPSRC_MAX_BYTES_VIDEO,
};
use crate::client_auth::AuthMethod;
use crate::tams::auth::TamsAuth;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use strom_types::element::ElementPadRef;
use strom_types::mediaplayer::{
    timeline_group, DECODED_AHEAD_PROPERTY, DEFAULT_DECODED_AHEAD_S, GROUP_PROPERTY,
    OUTPUT_FRAMERATE_PROPERTY, PRIVATE_GROUP_PREFIX, READ_AHEAD_PROPERTY, SOURCE_PROPERTY,
};
use strom_types::{FlowId, PropertyValue};
use tracing::{info, warn};
use uuid::Uuid;

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

/// Build a timeline player. `properties` are current names (migrated).
pub fn build(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
) -> Result<BlockBuildResult, BlockBuildError> {
    let flow_id: FlowId = prop_str(properties, "_flow_id")
        .and_then(|s| Uuid::parse_str(&s).ok())
        .unwrap_or_else(Uuid::nil);
    let media_path = prop_str(properties, "_media_path").unwrap_or_else(|| "./media".to_string());
    let invalid =
        |e: String| BlockBuildError::InvalidProperty(format!("Media Player timeline mode: {}", e));
    let source = prop_str(properties, SOURCE_PROPERTY).ok_or_else(|| {
        invalid(
            "source is required (a segment directory or \
             tams+https://<gateway>/flows/<flow_id>)"
                .to_string(),
        )
    })?;
    if prop_str(properties, GROUP_PROPERTY).is_some_and(|g| g.starts_with(PRIVATE_GROUP_PREFIX)) {
        return Err(invalid(format!(
            "group may not start with '{}' (reserved for a player's own group)",
            PRIVATE_GROUP_PREFIX
        )));
    }
    let group = timeline_group(properties, instance_id);
    let uint = |key: &str, default: u64| match properties.get(key) {
        Some(PropertyValue::UInt(u)) => *u,
        Some(PropertyValue::Int(i)) if *i >= 0 => *i as u64,
        _ => default,
    };
    let decoded_ahead_s =
        uint(DECODED_AHEAD_PROPERTY, DEFAULT_DECODED_AHEAD_S).min(player::MAX_DECODE_AHEAD_S);
    let read_ahead_s = uint(READ_AHEAD_PROPERTY, NetworkBuffer::DEFAULT_READ_AHEAD_S)
        .min(NetworkBuffer::MAX_READ_AHEAD_S);
    // The fetcher must hold what the decoder reads ahead (both capped, so
    // this stays within MAX_READ_AHEAD_S).
    if read_ahead_s < decoded_ahead_s + 2 {
        warn!(
            "Media Player {}: read_ahead_s {} is less than decoded_ahead_s + 2; \
             fetching {} s ahead",
            instance_id,
            read_ahead_s,
            decoded_ahead_s + 2
        );
    }
    let output_rate =
        parse_frame_rate(&prop_str(properties, OUTPUT_FRAMERATE_PROPERTY).unwrap_or_default())
            .map_err(invalid)?;
    let spec = source::parse_source(&source, &media_path).map_err(invalid)?;
    // Gateway auth is checked against the gateway it goes to (env token and
    // OSC SAT only to allowed gateways).
    let auth = match &spec {
        source::SourceSpec::Tams { gateway, .. } => TamsAuth::from_properties(properties, gateway)
            .map_err(invalid)?
            .method(&flow_id.to_string()),
        source::SourceSpec::Local(_) => AuthMethod::None,
    };
    let index = source::open(&spec, auth, std::path::Path::new(&media_path)).map_err(invalid)?;
    info!(
        "Media Player {}: timeline mode, group '{}', source {}",
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

    let timeline = timeline::get_or_create(flow_id, &group);
    let player = player::ReplayPlayer::new(
        instance_id,
        index,
        timeline,
        &appsrc,
        decoded_ahead_s,
        read_ahead_s,
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
