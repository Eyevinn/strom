//! Replay mode of the media player.
//!
//! With `replay_group` set, the block does not play a playlist. It holds
//! decoded frames of a segmented recording around a cursor shared by every
//! player of the group, and outputs the frame under the cursor on every tick
//! of the flow clock. Jog, scrub, slow motion and reverse move the cursor; see
//! `timeline` and `player`.

pub mod player;
pub mod source;
pub mod timeline;

use crate::blocks::{
    live_bridge_appsrc, BlockBuildError, BlockBuildResult, BusMessageConnectFn,
    APPSRC_MAX_BYTES_VIDEO,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::Arc;
use strom_types::element::ElementPadRef;
use strom_types::{FlowId, PropertyValue};
use tracing::info;
use uuid::Uuid;

pub const REPLAY_GROUP_PROPERTY: &str = "replay_group";
pub const REPLAY_SOURCE_PROPERTY: &str = "replay_source";
pub const REPLAY_CACHE_PROPERTY: &str = "replay_cache_s";

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
            "Media Player replay mode: replay_source (segment directory) is required".to_string(),
        )
    })?;
    let dir = {
        let p = std::path::PathBuf::from(&source);
        if p.is_absolute() {
            p
        } else {
            std::path::PathBuf::from(media_path).join(p)
        }
    };
    let cache_ahead_s = match properties.get(REPLAY_CACHE_PROPERTY) {
        Some(PropertyValue::UInt(u)) => *u,
        Some(PropertyValue::Int(i)) if *i > 0 => *i as u64,
        _ => 3,
    };
    let index = source::SegmentIndex::load(&dir).map_err(|e| {
        BlockBuildError::InvalidProperty(format!("Media Player replay mode: {}", e))
    })?;
    info!(
        "Media Player {}: replay mode, group '{}', {} segments / {} frames from {}",
        instance_id,
        group,
        index.segments.len(),
        index.total_frames,
        dir.display()
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
        Arc::new(index),
        timeline,
        &appsrc,
        cache_ahead_s,
    );

    let bus_message_handler: BusMessageConnectFn = Box::new(move |bus: &gst::Bus, _, _| {
        player.start();
        let keep = Arc::clone(&player);
        bus.connect_message(None, move |_, _| {
            let _ = &keep;
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
