//! Live pipeline effects — runtime mutations of a playing pipeline.
//!
//! Sub-modules (single responsibility each, mirroring the builder split):
//!   - [`take`] — the PGM take engine: `trigger_transition` with the
//!     PiP-aware and classic single-input paths.
//!   - [`mixer_ops`] — vision mixer runtime operations: bus selection,
//!     DSK, FTB, overlay alpha, PiP configuration, input resolutions.
//!   - [`mixer_layout`] — pad geometry/crop appliers shared by the take
//!     engine, the mixer ops and the reactive geometry probe.
//!   - [`shader_fx`] — shader FX engine ops: per-source looks, master
//!     effects, wipe takes and master envelopes (GPU backend only).
//!   - [`misc`] — block-generic effects (input animation, loudness reset,
//!     recorder split, thumbnail capture).

use super::PipelineManager;
use gstreamer as gst;
use gstreamer::prelude::*;

mod misc;
mod mixer_layout;
mod mixer_ops;
mod shader_fx;
mod take;

pub(crate) use mixer_layout::{apply_input_group_to_region, apply_pip_layout_to_region, find_pad};

impl PipelineManager {
    /// Get the distribution compositor canvas size from its capsfilter.
    fn dist_canvas_size(&self, block_instance_id: &str) -> (i32, i32) {
        let default =
            strom_types::parse_resolution_string(strom_types::vision_mixer::DEFAULT_PGM_RESOLUTION)
                .map(|(w, h)| (w as i32, h as i32))
                .expect("DEFAULT_PGM_RESOLUTION must be valid");

        let capsfilter_id = format!("{}:capsfilter_dist", block_instance_id);
        self.elements
            .get(&capsfilter_id)
            .and_then(|cf| cf.property::<Option<gst::Caps>>("caps"))
            .and_then(|caps| {
                let s = caps.structure(0)?;
                Some((
                    s.get::<i32>("width").unwrap_or(default.0),
                    s.get::<i32>("height").unwrap_or(default.1),
                ))
            })
            .unwrap_or(default)
    }
}
