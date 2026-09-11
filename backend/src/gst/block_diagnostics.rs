//! Degradation a block reports about itself.
//!
//! The stalled-pad-task scan in `gst::pipeline::health` sees pads in the flow
//! pipeline and nothing else. A block whose failure happens inside a
//! third-party element's private pipeline has to report it, because there is no
//! pad in the flow pipeline that shows the failure and no bus message carrying
//! it. `whepserversink`'s codec discovery is the case this exists for: it runs
//! one throwaway pipeline per codec on a bus whose sync handler drops every
//! message, and drops any codec that fails.
//!
//! Only degradation belongs here. A block that has stopped passing data
//! entirely stalls a pad task in the flow pipeline, which the scan already
//! finds.

use std::sync::Arc;

/// A block-supplied check for degradation that is invisible from the outside.
pub trait BlockDiagnostic: Send + Sync {
    /// Block instance ID this diagnostic reports for.
    fn block_id(&self) -> &str;

    /// `Some(detail)` once the block has settled into a degraded state, `None`
    /// while it is healthy or still settling.
    ///
    /// Polled from the health scan every couple of seconds, so it must return
    /// promptly and must not wait on the streaming threads.
    fn degraded(&self) -> Option<String>;
}

/// Diagnostics collected from every block in one flow.
pub type BlockDiagnostics = Vec<Arc<dyn BlockDiagnostic>>;
