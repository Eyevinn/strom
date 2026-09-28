//! WHEP (WebRTC-HTTP Egress Protocol) blocks.
//!
//! - [`input`]: WHEP Input receives a stream from an external WHEP server.
//! - [`output`]: WHEP Output hosts a WHEP server that clients play from.

mod profile_filter;

pub mod input;
pub mod output;

use strom_types::block::BlockDefinition;

pub use input::WHEPInputBuilder;
pub(crate) use output::explicit_track_count;
pub use output::{migrate_legacy_mode, WHEPOutputBuilder};

/// Get metadata for WHEP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![
        input::definition::whep_input_definition(),
        output::definition::whep_output_definition(),
    ]
}
