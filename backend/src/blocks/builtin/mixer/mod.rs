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
//! - Per-channel and bus metering. Convention: channel (input) meters are
//!   tapped pre-fader; bus (output) meters are tapped post-master.
//!
//! Pipeline structure per channel:
//! ```text
//! input_N → audioconvert → capsfilter(F32LE) → gain → hpf → gate → compressor → EQ →
//!           level_N → pre_fader_tee → audiopanorama_N → volume_N → post_fader_tee →
//!           routing_tee_N → [group or main audiomixer]
//!
//! pre_fader_tee  → pfl_volume_N → pfl_queue_N ─┐
//!                                              ├→ solo_mixer
//! post_fader_tee → afl_volume_N → afl_queue_N ─┘
//!
//! (pre_fader_tee | post_fader_tee) → aux_send_N_M → aux_queue_N_M → aux_M_mixer
//! ```
//! `level_N` sits pre-fader so the channel meter shows the signal hitting the
//! fader regardless of fader position or mute. Bus meters (`main_level`,
//! `monitor_level`, `auxN_level`, `groupN_level`) sit on the bus output,
//! post-master.
//!
//! Main bus: audiomixer → main_comp → main_eq → main_limiter → main_volume → main_level → main_out_tee
//!
//! Monitor bus: the solo_mixer and a tap from main_out_tee both feed a
//! monitor_mixer through two volume-gate elements (solo_to_mon and
//! main_to_mon). The state layer flips those gates as a side effect of any
//! `chN_pfl`/`chN_afl` write — no PFL/AFL active → main_to_mon=1,
//! solo_to_mon=0; any solo active → reversed. Clients only write the bools;
//! the gates are not part of the public API. monitor_mixer feeds
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

/// Block definition ID for the audio mixer. Used by state-layer hooks that
/// need to recognize mixer instances (e.g. the PFL/AFL → monitor-gate derivation).
pub const MIXER_BLOCK_ID: &str = "builtin.mixer";

/// Element IDs (relative to the block instance) for the two monitor-source gates.
/// These are pure derived state driven by PFL/AFL — external clients never
/// write them directly.
pub const SOLO_TO_MON_ELEMENT: &str = "solo_to_mon";
pub const MAIN_TO_MON_ELEMENT: &str = "main_to_mon";

/// Return the 1-indexed channel number if `name` matches a `chN_pfl` / `chN_afl`
/// exposed property, otherwise `None`. Used to detect solo-affecting writes in
/// the block-properties batch path.
pub fn parse_solo_property_name(name: &str) -> Option<usize> {
    let stripped = name
        .strip_suffix("_pfl")
        .or_else(|| name.strip_suffix("_afl"))?;
    stripped.strip_prefix("ch")?.parse::<usize>().ok()
}

// Crate-internal re-imports (accessible via super::* in tests)
#[cfg(test)]
use definition::mixer_definition;
#[cfg(test)]
use elements::*;
#[cfg(test)]
use metering::*;
#[cfg(test)]
use properties::*;
