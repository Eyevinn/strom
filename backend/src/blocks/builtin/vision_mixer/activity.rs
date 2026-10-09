//! Per-input media activity for the vision mixer.
//!
//! The compositor repeats an input's last frame forever once that input stops
//! delivering, so a participant who drops looks identical on air to one who is
//! sitting still. These probes stamp the arrival of every input buffer, which
//! is what lets `input_media_age_ms` tell those two apart.
//!
//! These ARE per-buffer probes — the hottest path in the pipeline. The
//! callback reads the monotonic clock and stores one relaxed atomic: no locks,
//! no allocation, no formatting, no state lookup. The overlay state is
//! captured as an `Arc`, not looked up per buffer, and it is not a GStreamer
//! object, so it forms no reference cycle with the pipeline.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::Arc;
use tracing::debug;

use super::overlay::VisionMixerOverlayState;
use crate::gst::pipeline::effects::find_pad;

/// Install a buffer probe on each of the dist compositor's input sink pads so
/// the overlay state knows when each input last delivered a frame.
///
/// Must run after linking, when the request pads exist. `sink_i` on the dist
/// compositor carries input `i` regardless of what is on air — visibility is
/// an alpha property, so a hidden input still stamps activity.
pub fn install_input_activity_probes(
    block_id: &str,
    mixer: &gst::Element,
    state: &Arc<VisionMixerOverlayState>,
    num_inputs: usize,
) {
    let mut installed = 0;
    for i in 0..num_inputs {
        let Some(pad) = find_pad(mixer, &format!("sink_{}", i)) else {
            continue;
        };
        let state = Arc::clone(state);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            state.note_input_buffer(i);
            gst::PadProbeReturn::Ok
        });
        installed += 1;
    }
    debug!(
        "Vision mixer {}: installed {} input activity probes",
        block_id, installed
    );
}
