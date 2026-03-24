//! Vision Mixer block — TV broadcast production switcher.
//!
//! A source selector with PVW/PGM workflow and transition support.
//! Takes configurable video inputs (2-10), outputs a high-res PGM stream
//! and a broadcast multiview with cairo-drawn overlays.
//!
//! Internally tees each input to two compositors:
//! - **Distribution compositor** ("mixer"): full-res PGM output with transitions
//! - **Multiview compositor** ("mv_comp"): 2-5-5 grid with PVW/PGM large panels,
//!   thumbnails, borders, labels, and clock via cairooverlay
//!
//! GPU pipeline (per input):
//! ```text
//! queue_i → glupload_i → glcolorconvert_i → tee_i
//!   tee_i.src_0 → dist_comp.sink_i
//!   tee_i.src_1 → mv_comp.sink_i          (thumbnail)
//!   tee_i.src_2 → mv_comp.sink_{N+i}      (PVW/PGM big display candidate)
//! ```
//!
//! Distribution output: `dist_comp → capsfilter_dist → [pgm_out]`
//! Multiview output: `mv_comp → gldownload_mv → videoconvert_pre_cairo → cairooverlay → capsfilter_mv → [multiview_out]`

mod builder;
mod definition;
mod elements;
mod layout;
pub mod overlay;
pub(crate) mod properties;
#[cfg(test)]
mod tests;

// Public API
pub use builder::VisionMixerBuilder;
pub use definition::get_blocks;
pub use overlay::VisionMixerOverlayState;
