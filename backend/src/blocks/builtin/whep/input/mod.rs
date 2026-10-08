//! WHEP Input - receives streams from external WHEP servers.
//!
//! - `whepclientsrc` (new): uses the signaller interface.
//! - `whepsrc` (stable): simpler implementation with direct properties.
//!
//! Handles dynamic pad creation by linking new audio streams to a liveadder mixer.

pub(crate) mod definition;
mod stream;
mod whepclientsrc;
mod whepsrc;

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::ice_preflight;
use std::collections::HashMap;
use strom_types::PropertyValue;
use tracing::debug;

use whepclientsrc::build_whepclientsrc;
use whepsrc::build_whepsrc;

/// WHEP Input block builder.
pub struct WHEPInputBuilder;

impl BlockBuilder for WHEPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Input block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHEP Input")?;

        // Get implementation choice (default to stable whepsrc)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whepclientsrc")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whepclientsrc(instance_id, properties, ctx)
        } else {
            build_whepsrc(instance_id, properties, ctx)
        }
    }
}
