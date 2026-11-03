//! Default values for the Audio Processor block (`builtin.audioprocessor`).
//!
//! The block wraps strom-dsp's `stromdsp-broadcastprocessor` (spike guard,
//! loudness AGC, multiband compressor with expander and bass clipper, final
//! clipper, true-peak limiter). With a named preset the element's
//! own preset tuning applies. With the `custom` preset the block sets every
//! element property itself, from these values or the operator's, so Strom's
//! sound never depends on the element's defaults.
//!
//! Single source of truth shared by both backend and frontend.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Block definition id.
pub const BLOCK_ID: &str = "builtin.audioprocessor";

/// Block property that picks the preset.
pub const PRESET_PROPERTY: &str = "preset";

/// Block properties that apply only with the `custom` preset. With a named
/// preset the element's preset tuning sets them, so the UI hides them and a
/// live write to one is refused.
pub const CUSTOM_ONLY_PROPERTIES: &[&str] = &[
    "input_gain",
    "target_loudness",
    "true_peak_ceiling",
    "agc_enabled",
    "agc_max_gain",
    "agc_max_cut",
    "agc_rate_up",
    "agc_rate_down",
    "agc_gate",
    "spike_guard_enabled",
    "spike_threshold",
    "multiband_enabled",
    "multiband_drive",
    "expander_hold",
    "bass_clipper_enabled",
    "bass_clip_threshold",
    "clipper_enabled",
    "clipper_bandwidth",
    "governor_enabled",
    "clip_budget",
    "limiter_enabled",
    "limiter_release",
];

/// Preset that applies the block's own (Strom's) values for every setting.
pub const PRESET_CUSTOM: &str = "custom";

/// Presets the block offers, as the element's enum nicks, with a short label
/// for the dropdown. The targets are in the preset property's description.
/// `custom` is last: it is the only one that uses the block's own settings.
pub const PRESETS: &[(&str, &str)] = &[
    ("ebu-r128", "EBU R 128"),
    ("atsc-a85", "ATSC A/85"),
    ("streaming", "Streaming"),
    ("speech", "Speech"),
    (PRESET_CUSTOM, "Custom"),
];

/// Preset a new block starts with.
pub const DEFAULT_PRESET: &str = "ebu-r128";

// ── Always applied, whatever the preset ─────────────────────────────
pub const DEFAULT_ENABLED: bool = true;
/// Number of bands in the multiband compressor (1..=5). Construction-time only.
pub const DEFAULT_NUM_BANDS: u32 = 4;
pub const MIN_NUM_BANDS: u32 = 1;
pub const MAX_NUM_BANDS: u32 = 5;
/// How often the element posts its meter message (ms). 0 turns meters off.
pub const DEFAULT_METER_INTERVAL_MS: u32 = 100;

/// Latency modes, as the element's `latency-mode` enum nicks, with a label
/// for the dropdown. Construction-time only; presets do not set it.
pub const LATENCY_MODES: &[(&str, &str)] = &[
    ("standard", "Standard (about 5.6 ms)"),
    ("low", "Low (about 2.8 ms)"),
];
pub const DEFAULT_LATENCY_MODE: &str = "standard";

// ── Custom preset: exposed settings ─────────────────────────────────
pub const DEFAULT_INPUT_GAIN_DB: f32 = 0.0;
pub const DEFAULT_TARGET_LOUDNESS_LUFS: f32 = -23.0;
pub const DEFAULT_TRUE_PEAK_CEILING_DBTP: f32 = -1.0;

pub const DEFAULT_AGC_ENABLED: bool = true;
/// AGC range (dB). Every element preset uses +/-24 dB since strom-dsp
/// v0.1.1, so custom does too.
pub const DEFAULT_AGC_MAX_GAIN_DB: f32 = 24.0;
pub const DEFAULT_AGC_MAX_CUT_DB: f32 = 24.0;
/// How fast the AGC raises gain (dB/s).
pub const DEFAULT_AGC_RATE_UP_DB_S: f32 = 1.0;
/// How fast the AGC lowers gain (dB/s).
pub const DEFAULT_AGC_RATE_DOWN_DB_S: f32 = 3.0;
/// Below this loudness the AGC holds its gain instead of raising it (LUFS).
pub const DEFAULT_AGC_GATE_LUFS: f32 = -50.0;

pub const DEFAULT_MULTIBAND_ENABLED: bool = true;
/// Gain into the multiband compressor (dB). More drive, more compression.
pub const DEFAULT_MULTIBAND_DRIVE_DB: f32 = 0.0;

/// Spike guard: removes short, isolated peaks (clicks) ahead of the AGC.
pub const DEFAULT_SPIKE_GUARD_ENABLED: bool = true;
/// How far over the programme's own peak envelope a peak must be to count
/// as a spike (dB).
pub const DEFAULT_SPIKE_THRESHOLD_DB: f32 = 8.0;

/// Hold of the per-band downward expander before it closes (ms).
pub const DEFAULT_EXPANDER_HOLD_MS: f32 = 100.0;

/// Bass clipper on the lowest band.
pub const DEFAULT_BASS_CLIPPER_ENABLED: bool = true;
/// Bass clipper threshold (dB, band 0 envelope): the target + 9 dB, as the
/// element's presets set it.
pub const DEFAULT_BASS_CLIP_THRESHOLD_DB: f32 = -14.0;

/// Final clipper ahead of the limiter.
pub const DEFAULT_CLIPPER_ENABLED: bool = true;
/// Bandwidth of what the final clipper removes (Hz).
pub const DEFAULT_CLIPPER_BANDWIDTH_HZ: f32 = 20000.0;

/// Clip budget governor.
pub const DEFAULT_GOVERNOR_ENABLED: bool = true;
/// How much clipper and limiter work the governor allows (dB of budget).
pub const DEFAULT_CLIP_BUDGET_DB: f32 = 2.0;

pub const DEFAULT_LIMITER_ENABLED: bool = true;
pub const DEFAULT_LIMITER_RELEASE_MS: f32 = 80.0;
/// Limiter lookahead (ms) of the standard latency mode. Construction-time
/// only. `latency-mode` sets the lookahead itself (1.5 ms standard, 0.6 ms
/// low), so the block writes `limiter_lookahead` only when the flow stores
/// a value, as an override after the latency mode.
pub const DEFAULT_LIMITER_LOOKAHEAD_MS: f32 = 1.5;

// ── Custom preset: band settings the block sets but does not expose ──
// Gentle starting points: low ratios, no makeup, slower time constants in
// the low bands than in the high ones. Index N is `bandN-*` / `crossoverN-*`;
// with fewer than five bands the element uses the first ones.

/// Crossover frequencies (Hz), strictly ascending.
pub const DEFAULT_CROSSOVER_FREQUENCIES_HZ: [f32; 4] = [120.0, 800.0, 4000.0, 10000.0];
pub const DEFAULT_BAND_THRESHOLD_DB: [f32; 5] = [-24.0, -22.0, -20.0, -20.0, -20.0];
pub const DEFAULT_BAND_RATIO: [f32; 5] = [2.0, 2.0, 2.0, 1.8, 1.5];
pub const DEFAULT_BAND_ATTACK_MS: [f32; 5] = [40.0, 20.0, 10.0, 5.0, 3.0];
pub const DEFAULT_BAND_RELEASE_MS: [f32; 5] = [400.0, 250.0, 150.0, 100.0, 80.0];
pub const DEFAULT_BAND_MAKEUP_DB: [f32; 5] = [0.0; 5];
/// Per-band downward expander: threshold (dB, band level), ratio (1 = off)
/// and floor (largest attenuation, dB). The element's `ebu-r128` values.
pub const DEFAULT_BAND_EXPANDER_THRESHOLD_DB: [f32; 5] = [-47.0; 5];
pub const DEFAULT_BAND_EXPANDER_RATIO: [f32; 5] = [1.5; 5];
pub const DEFAULT_BAND_EXPANDER_FLOOR_DB: [f32; 5] = [4.0; 5];

/// Meter value the element reports for silence (LUFS, dBTP, dB).
pub const METER_SILENCE: f64 = -200.0;

/// What the clip budget governor is doing, from the element's
/// `governor-state` meter field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "kebab-case")]
pub enum AudioProcessorGovernorState {
    /// The governor is turned off.
    Off,
    /// Frozen: a pause, below the AGC gate, or a stage bypassed.
    Frozen,
    /// Within the budget, nothing to do.
    Free,
    /// Near the budget, or holding density.
    Hold,
    /// Adding density: the target is out of reach otherwise.
    Density,
    /// On target, the trim held, and the clipper and limiter over budget.
    OverBudget,
    /// Giving density back.
    Release,
    /// A state this version of Strom does not know.
    #[serde(other)]
    Unknown,
}

impl AudioProcessorGovernorState {
    /// The state for the element's `governor-state` string.
    pub fn from_nick(nick: &str) -> Self {
        match nick {
            "off" => Self::Off,
            "frozen" => Self::Frozen,
            "free" => Self::Free,
            "hold" => Self::Hold,
            "density" => Self::Density,
            "over-budget" => Self::OverBudget,
            "release" => Self::Release,
            _ => Self::Unknown,
        }
    }

    /// Short label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Frozen => "frozen",
            Self::Free => "free",
            Self::Hold => "hold",
            Self::Density => "density",
            Self::OverBudget => "over budget",
            Self::Release => "release",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossovers_are_strictly_ascending() {
        for pair in DEFAULT_CROSSOVER_FREQUENCIES_HZ.windows(2) {
            assert!(pair[0] < pair[1], "{pair:?}");
        }
    }

    #[test]
    fn custom_is_the_only_preset_that_uses_block_settings_and_comes_last() {
        assert_eq!(PRESETS.last().map(|p| p.0), Some(PRESET_CUSTOM));
        assert!(PRESETS.iter().any(|p| p.0 == DEFAULT_PRESET));
    }

    /// Every state round-trips through its element nick and its JSON form,
    /// and a state Strom does not know reads as `Unknown`.
    #[test]
    fn governor_states_follow_the_element_nicks() {
        for nick in [
            "off",
            "frozen",
            "free",
            "hold",
            "density",
            "over-budget",
            "release",
        ] {
            let state = AudioProcessorGovernorState::from_nick(nick);
            assert_ne!(state, AudioProcessorGovernorState::Unknown, "{nick}");
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::Value::String(nick.to_string())
            );
        }
        assert_eq!(
            AudioProcessorGovernorState::from_nick("sideways"),
            AudioProcessorGovernorState::Unknown
        );
        let parsed: AudioProcessorGovernorState =
            serde_json::from_value(serde_json::json!("sideways")).unwrap();
        assert_eq!(parsed, AudioProcessorGovernorState::Unknown);
    }

    /// The element's ranges (strom-dsp `docs/elements/broadcastprocessor.md`). A default outside
    /// them would fail every flow that uses the custom preset.
    #[test]
    fn defaults_are_inside_the_element_ranges() {
        let within = |v: f32, lo: f32, hi: f32| (lo..=hi).contains(&v);
        assert!(within(DEFAULT_INPUT_GAIN_DB, -24.0, 24.0));
        assert!(within(DEFAULT_TARGET_LOUDNESS_LUFS, -40.0, -5.0));
        assert!(within(DEFAULT_TRUE_PEAK_CEILING_DBTP, -12.0, 0.0));
        assert!(within(DEFAULT_AGC_MAX_GAIN_DB, 0.0, 30.0));
        assert!(within(DEFAULT_AGC_MAX_CUT_DB, 0.0, 30.0));
        assert!(within(DEFAULT_AGC_RATE_UP_DB_S, 0.1, 20.0));
        assert!(within(DEFAULT_AGC_RATE_DOWN_DB_S, 0.1, 40.0));
        assert!(within(DEFAULT_AGC_GATE_LUFS, -80.0, -20.0));
        assert!(within(DEFAULT_MULTIBAND_DRIVE_DB, -12.0, 12.0));
        assert!(within(DEFAULT_LIMITER_RELEASE_MS, 1.0, 2000.0));
        assert!(within(DEFAULT_LIMITER_LOOKAHEAD_MS, 0.5, 20.0));
        assert!(within(DEFAULT_SPIKE_THRESHOLD_DB, 6.0, 30.0));
        assert!(within(DEFAULT_EXPANDER_HOLD_MS, 0.0, 1000.0));
        assert!(within(DEFAULT_BASS_CLIP_THRESHOLD_DB, -40.0, 0.0));
        assert!(within(DEFAULT_CLIPPER_BANDWIDTH_HZ, 8000.0, 22000.0));
        assert!(within(DEFAULT_CLIP_BUDGET_DB, 0.0, 6.0));
        assert!(LATENCY_MODES.iter().any(|m| m.0 == DEFAULT_LATENCY_MODE));
        assert!((MIN_NUM_BANDS..=MAX_NUM_BANDS).contains(&DEFAULT_NUM_BANDS));
        const { assert!(DEFAULT_METER_INTERVAL_MS <= 10_000) };
        for f in DEFAULT_CROSSOVER_FREQUENCIES_HZ {
            assert!(within(f, 20.0, 20000.0));
        }
        for n in 0..5 {
            assert!(within(DEFAULT_BAND_THRESHOLD_DB[n], -60.0, 0.0));
            assert!(within(DEFAULT_BAND_RATIO[n], 1.0, 20.0));
            assert!(within(DEFAULT_BAND_ATTACK_MS[n], 0.1, 500.0));
            assert!(within(DEFAULT_BAND_RELEASE_MS[n], 5.0, 5000.0));
            assert!(within(DEFAULT_BAND_MAKEUP_DB[n], -12.0, 24.0));
            assert!(within(DEFAULT_BAND_EXPANDER_THRESHOLD_DB[n], -80.0, 0.0));
            assert!(within(DEFAULT_BAND_EXPANDER_RATIO[n], 1.0, 10.0));
            assert!(within(DEFAULT_BAND_EXPANDER_FLOOR_DB[n], 0.0, 40.0));
        }
    }
}
