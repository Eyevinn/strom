//! WHIP (WebRTC-HTTP Ingestion Protocol) blocks.
//!
//! - [`input`]: WHIP Input hosts a WHIP server that clients publish into.
//! - [`output`]: WHIP Output publishes to an external WHIP server.

pub mod input;
pub mod output;

use strom_types::block::BlockDefinition;

pub use input::{
    attach_session_branch, build_whipserversrc, create_whipserversrc_for_session, CreatedSession,
    WHIPInputBuilder,
};
pub use output::WHIPOutputBuilder;

/// Get metadata for WHIP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![
        output::definition::whip_output_definition(),
        input::definition::whip_input_definition(),
    ]
}
