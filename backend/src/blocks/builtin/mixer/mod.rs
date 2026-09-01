//! Stereo Mixer block - a digital mixing console for audio.
//!
//! This block provides a mixer similar to digital consoles like Behringer X32:
//! - Configurable number of input channels (1-128)
//! - Per-channel: input gain, gate, compressor, 4-band parametric EQ, pan, fader, mute
//! - Aux sends (0-32 configurable aux buses, switchable pre/post fader)
//! - Groups (0-32 configurable, with output pads)
//! - Independent per-channel PFL (pre-fader) and AFL (post-fader) sends
//! - Per-bus AFL on every aux and group (post-master, post-mute tap into the
//!   solo bus). PFL on aux/group is intentionally not exposed today — bus
//!   faders are essentially always up in operation, so listening pre-master
//!   adds little. The property namespace (`*_afl`) leaves room for `*_pfl`
//!   later without a breaking change.
//! - Monitor bus that follows Main when no PFL/AFL is engaged and switches
//!   to the solo mix as soon as any channel/aux/group toggles PFL or AFL
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
//!
//! aux_M_out_tee  → aux_afl_volume_M  → aux_afl_queue_M  ─┐
//!                                                        ├→ solo_mixer
//! group_K_out_tee → group_afl_volume_K → group_afl_queue_K ─┘
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
//! `chN_pfl`/`chN_afl`/`auxN_afl`/`groupK_afl` write — no PFL/AFL active →
//! main_to_mon=1, solo_to_mon=0; any solo active → reversed. Clients only
//! write the bools; the gates are not part of the public API. monitor_mixer
//! feeds monitor_master_vol → monitor_level → monitor_out_tee → monitor_out
//! pad.
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
pub(crate) use elements::make_audiomixer;
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

/// Return `true` if `name` is one of the public solo-affecting Bool
/// properties: `chN_pfl`, `chN_afl`, `auxN_afl`, or `groupN_afl`. Used by the
/// state layer to detect when a block-properties batch needs to refresh the
/// monitor-source gates.
///
/// We don't return the index because the state layer keys its solo-intent
/// cache by the full property name — that lets channel/aux/group solos
/// coexist without colliding.
pub fn is_solo_property_name(name: &str) -> bool {
    if let Some(stripped) = name.strip_suffix("_pfl") {
        return stripped
            .strip_prefix("ch")
            .and_then(|s| s.parse::<usize>().ok())
            .is_some();
    }
    if let Some(stripped) = name.strip_suffix("_afl") {
        for prefix in ["ch", "aux", "group"] {
            if let Some(rest) = stripped.strip_prefix(prefix) {
                if rest.parse::<usize>().is_ok() {
                    return true;
                }
            }
        }
    }
    false
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
