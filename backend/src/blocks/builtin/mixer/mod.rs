//! Stereo Mixer block - a digital mixing console for audio.
//!
//! This block provides a mixer similar to digital consoles like Behringer X32:
//! - Configurable number of input channels (1-128)
//! - Per-channel: input gain, gate, compressor, 4-band parametric EQ, pan, fader, mute
//! - Aux sends (0-32 configurable aux buses, switchable pre/post fader)
//! - Groups (0-32 configurable, with output pads)
//! - Independent per-channel PFL (pre-fader) and AFL (post-fader) sends
//! - Monitor bus that follows Main when no PFL/AFL is engaged and switches
//!   to the solo mix as soon as any channel toggles PFL or AFL
//! - Main stereo bus with compressor, EQ, limiter, and master fader
//! - Per-channel and bus metering
//!
//! Pipeline structure per channel:
//! ```text
//! input_N → audioconvert → capsfilter(F32LE) → gain → hpf → gate → compressor → EQ →
//!           pre_fader_tee → audiopanorama_N → volume_N → post_fader_tee →
//!           level_N → [group or main audiomixer]
//!
//! pre_fader_tee  → pfl_volume_N → pfl_queue_N ─┐
//!                                              ├→ solo_mixer
//! post_fader_tee → afl_volume_N → afl_queue_N ─┘
//!
//! (pre_fader_tee | post_fader_tee) → aux_send_N_M → aux_queue_N_M → aux_M_mixer
//! ```
//!
//! Main bus: audiomixer → main_comp → main_eq → main_limiter → main_volume → main_level → main_out_tee
//!
//! Monitor bus: the solo_mixer and a tap from main_out_tee both feed a
//! monitor_mixer through two volume-gate elements (solo_to_mon and
//! main_to_mon). The frontend toggles those gates whenever any ch{N}_pfl or
//! ch{N}_afl property changes — no PFL/AFL active → main_to_mon=1,
//! solo_to_mon=0; any solo active → reversed. monitor_mixer feeds
//! monitor_master_vol → monitor_level → monitor_out_tee → monitor_out pad.
//!
//! All output buses terminate in a tee with allow-not-linked=true, so unconnected
//! output pads don't cause NOT_LINKED flow errors. Audiomixer elements use
//! force-live=true so unconnected input pads don't stall the pipeline.
//!
//! Processing uses LSP LV2 plugins when available. Falls back to identity passthrough
//! when LV2 plugins are not installed.

mod builder;
mod definition;
mod elements;
mod metering;
mod properties;
#[cfg(test)]
mod tests;

use strom_types::mixer::{
    DEFAULT_CHANNELS, MAX_AUX_BUSES, MAX_CHANNELS, MAX_GROUPS, MIN_KNEE_LINEAR,
};
/// Level meter interval in nanoseconds (100ms)
const METER_INTERVAL_NS: u64 = 100_000_000;
/// EQ band type for Peaking/Bell filter (lsp-rs-equalizer enum value)
const EQ_BAND_TYPE_BELL: i32 = 7;

// Public API
pub use builder::MixerBuilder;
pub use definition::get_blocks;
pub use properties::translate_property_for_element;

// Crate-internal re-imports (accessible via super::* in tests)
#[cfg(test)]
use definition::mixer_definition;
#[cfg(test)]
use elements::*;
#[cfg(test)]
use metering::*;
#[cfg(test)]
use properties::*;
