//! WHIP Output - sends media to an external WHIP server.
//!
//! - `whipclientsink` (new): uses the signaller interface, handles encoding internally.
//! - `whipsink` (legacy): simpler implementation, requires pre-encoded RTP input.

pub(crate) mod definition;
mod whipclientsink;
mod whipsink;

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::ice_preflight;
use std::collections::HashMap;
use strom_types::PropertyValue;
use tracing::debug;

use whipclientsink::build_whipclientsink;
use whipsink::build_whipsink;

/// WHIP Output block builder.
pub struct WHIPOutputBuilder;

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
