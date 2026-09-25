//! WHIP (WebRTC-HTTP Ingestion Protocol) block builders.
//!
//! WHIP Output - Sends media to an external WHIP server:
//! - `whipclientsink` (new): Uses signaller interface, handles encoding internally
//! - `whipsink` (legacy): Simpler implementation, requires pre-encoded RTP input
//!
//! WHIP Input - Hosts a WHIP server for clients to connect and send media:
//! - `whipserversrc`: One element per WHIP client session, created dynamically
//!   by the WhipSessionManager when a client POSTs an SDP offer.
//!   Each session is assigned to a numbered slot with independent output chains
//!   (appsrc → decodebin → convert → tee per slot).

mod definition;
mod output;
mod session;
mod slot_chain;
mod watchdog;

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::ice_preflight;
use std::collections::HashMap;
use strom_types::block::StreamMode;
use strom_types::{block::*, PropertyValue, *};
use tracing::debug;

use output::{build_whipclientsink, build_whipsink};

pub use definition::get_blocks;
pub use session::{create_whipserversrc_for_session, CreatedSession};
pub use slot_chain::build_whipserversrc;

/// WHIP Output block builder.
pub struct WHIPOutputBuilder;

/// WHIP Input block builder (hosts WHIP server).
pub struct WHIPInputBuilder;

impl BlockBuilder for WHIPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Output block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHIP Output")?;

        // Get implementation choice (default to stable whipsink)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whipclientsink")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whipclientsink(instance_id, properties, ctx)
        } else {
            build_whipsink(instance_id, properties, ctx)
        }
    }
}

impl BlockBuilder for WHIPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Input block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHIP Input")?;
        build_whipserversrc(instance_id, properties, ctx)
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or(StreamMode::AudioVideo);

        let max_sessions = properties
            .get("max_sessions")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some((*i).max(1) as usize),
                _ => None,
            })
            .unwrap_or(1);

        let mut outputs = Vec::new();

        for slot in 0..max_sessions {
            // Slot 0 always uses unsuffixed names (video_out, audio_out) so existing
            // connections are preserved when max_sessions is increased.
            // Additional slots use numbered names (video_out_1, audio_out_1, ...).
            let (video_name, audio_name) = if slot == 0 {
                ("video_out".to_string(), "audio_out".to_string())
            } else {
                (format!("video_out_{}", slot), format!("audio_out_{}", slot))
            };

            if mode.has_video() {
                outputs.push(ExternalPad {
                    label: Some(format!("V{}", slot)),
                    name: video_name,
                    media_type: MediaType::Video,
                    internal_element_id: format!("video_out_tee_{}", slot),
                    internal_pad_name: "src_%u".to_string(),
                });
            }

            if mode.has_audio() {
                outputs.push(ExternalPad {
                    label: Some(format!("A{}", slot)),
                    name: audio_name,
                    media_type: MediaType::Audio,
                    internal_element_id: format!("audio_out_tee_{}", slot),
                    internal_pad_name: "src_%u".to_string(),
                });
            }
        }

        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }
}
