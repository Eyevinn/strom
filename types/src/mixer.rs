//! Default values for the audio mixer block.
//!
//! Single source of truth shared by both backend and frontend.

// ── Channel / Main bus processing defaults ──────────────────────────
pub const DEFAULT_FADER: f32 = 1.0;
pub const DEFAULT_GAIN: f32 = 0.0;
pub const DEFAULT_PAN: f32 = 0.0;

// HPF
pub const DEFAULT_HPF_FREQ: f32 = 80.0;

// Gate
pub const DEFAULT_GATE_THRESHOLD: f32 = -40.0;
pub const DEFAULT_GATE_ATTACK: f32 = 5.0;
pub const DEFAULT_GATE_RELEASE: f32 = 100.0;

// Compressor (shared between channel and main bus)
pub const DEFAULT_COMP_THRESHOLD: f32 = -20.0;
pub const DEFAULT_COMP_RATIO: f32 = 4.0;
pub const DEFAULT_COMP_ATTACK: f32 = 10.0;
pub const DEFAULT_COMP_RELEASE: f32 = 100.0;
pub const DEFAULT_COMP_MAKEUP: f32 = 0.0;
pub const DEFAULT_COMP_KNEE: f32 = -6.0;

// EQ bands: (freq Hz, gain dB, Q)
pub const DEFAULT_EQ_BANDS: [(f32, f32, f32); 4] = [
    (80.0, 0.0, 1.0),   // Low
    (400.0, 0.0, 1.0),  // Low-mid
    (2000.0, 0.0, 1.0), // High-mid
    (8000.0, 0.0, 1.0), // High
];

// Limiter
pub const DEFAULT_LIMITER_THRESHOLD: f32 = -3.0;

// ── Structural defaults ─────────────────────────────────────────────
pub const DEFAULT_CHANNELS: usize = 8;
pub const MAX_CHANNELS: usize = 128;
pub const MAX_AUX_BUSES: usize = 32;
pub const MAX_GROUPS: usize = 32;

// ── Routing defaults ──────────────────────────────────────────────
/// Default aux send pre/post-fader mode per bus.
/// All aux buses default to post-fader (FX/headphone sends, affected by
/// the channel fader). Flip individual buses to pre-fader in the UI when
/// using them for monitor/IEM sends.
pub const DEFAULT_AUX_PRE: [bool; MAX_AUX_BUSES] = [false; MAX_AUX_BUSES];

/// Minimum compressor knee value in linear scale (corresponds to -24 dB)
pub const MIN_KNEE_LINEAR: f64 = 0.0631;

// ── Latency / live defaults ─────────────────────────────────────────
pub const DEFAULT_LATENCY_MS: u64 = 30;
pub const DEFAULT_MIN_UPSTREAM_LATENCY_MS: u64 = 30;

// ── Volume ramp / anti-zipper defaults ──────────────────────────────
/// Default ramp duration applied to `volume`-element `volume` updates
/// when no explicit `ramp_ms` is provided. Eliminates zipper noise from
/// fader-drag updates without making the fader feel laggy.
pub const DEFAULT_VOLUME_RAMP_MS: u32 = 20;

/// Anti-click ramp applied automatically when `mute` is toggled. Not
/// user-configurable — short enough to feel instant, long enough to
/// prevent the discontinuity click of a hard mute.
pub const MUTE_ANTICLICK_RAMP_MS: u32 = 30;
