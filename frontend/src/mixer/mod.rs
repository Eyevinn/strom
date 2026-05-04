//! Mixer editor - fullscreen audio mixer view.
//!
//! Provides an interactive mixer console similar to digital mixers like Behringer X32:
//! - Per-channel faders, pan controls, mute buttons
//! - Real-time metering
//! - Keyboard shortcuts for quick mixing
//!
//! Per-channel gate, compressor, 4-band EQ, aux sends, groups, PFL

mod api;
mod detail;
mod keyboard;
mod rendering;
mod state;
mod util;
mod widgets;

use egui::{Color32, Context, CornerRadius, Rect, Response, Sense, Stroke, Ui, Vec2};
use std::collections::HashMap;
use strom_types::mixer::*;
use strom_types::{FlowId, PropertyValue};

use crate::api::ApiClient;
use crate::meter::{MeterData, MeterDataStore};

use util::*;

use strom_types::mixer::MIN_KNEE_LINEAR;

// ── Layout constants ─────────────────────────────────────────────────
/// Gap between strips
const STRIP_GAP: f32 = 2.0;
/// Inner margin inside each strip frame
const STRIP_MARGIN: f32 = 3.0;
/// Preferred knob diameter
const KNOB_SIZE: f32 = 22.0;
/// Small button height (G/C/E, routing)
const SMALL_BTN_H: f32 = 18.0;
/// Standard button height (mute, PFL)
const BTN_H: f32 = 20.0;
/// LCD display height
const LCD_H: f32 = 16.0;
/// Pan knob diameter
const PAN_KNOB_SIZE: f32 = 24.0;
/// Height of the fader + meter area
const FADER_HEIGHT: f32 = 220.0;
/// Minimum usable inner width (for 0 aux sends)
const MIN_STRIP_INNER: f32 = 42.0;
/// Height of bus master faders (shorter than channel faders)
const BUS_FADER_HEIGHT: f32 = 120.0;
/// Fixed inner width for bus master strips
const BUS_STRIP_INNER: f32 = 52.0;
/// Minimum height for the bus master row
const BUS_ROW_MIN_HEIGHT: f32 = 200.0;

/// A single channel strip in the mixer.
#[derive(Debug, Clone)]
struct ChannelStrip {
    /// Channel number (1-indexed)
    channel_num: usize,
    /// Channel label
    label: String,
    /// Input gain (dB)
    gain: f32,
    /// Pan position (-1.0 to 1.0)
    pan: f32,
    /// Fader level (0.0 to 2.0)
    fader: f32,
    /// Mute state
    mute: bool,
    /// PFL (Pre-Fader Listen) state
    pfl: bool,
    /// Route to main mix
    to_main: bool,
    /// Route to groups (up to 4)
    to_grp: [bool; MAX_GROUPS],
    /// Aux send levels (up to 4 aux buses)
    aux_sends: [f32; MAX_AUX_BUSES],
    /// Aux send pre-fader mode (true=pre, false=post)
    aux_pre: [bool; MAX_AUX_BUSES],
    /// HPF enabled
    hpf_enabled: bool,
    /// HPF cutoff frequency (Hz)
    hpf_freq: f32,
    /// Gate enabled
    gate_enabled: bool,
    /// Gate threshold (dB)
    gate_threshold: f32,
    /// Gate attack (ms)
    gate_attack: f32,
    /// Gate release (ms)
    gate_release: f32,
    /// Compressor enabled
    comp_enabled: bool,
    /// Compressor threshold (dB)
    comp_threshold: f32,
    /// Compressor ratio
    comp_ratio: f32,
    /// Compressor attack (ms)
    comp_attack: f32,
    /// Compressor release (ms)
    comp_release: f32,
    /// Compressor makeup gain (dB)
    comp_makeup: f32,
    /// Compressor knee (dB)
    comp_knee: f32,
    /// EQ enabled
    eq_enabled: bool,
    /// EQ bands: (freq, gain_db, q) for 4 bands
    eq_bands: [(f32, f32, f32); 4],
}

/// Group strip state.
#[derive(Debug, Clone)]
struct GroupStrip {
    /// Group index (0-based)
    index: usize,
    /// Fader level (0.0 to 2.0)
    fader: f32,
    /// Mute state
    mute: bool,
}

/// Aux bus master state.
#[derive(Debug, Clone)]
struct AuxMaster {
    /// Aux index (0-based)
    index: usize,
    /// Master fader level
    fader: f32,
    /// Mute state
    mute: bool,
}

impl ChannelStrip {
    fn new(channel_num: usize) -> Self {
        Self {
            channel_num,
            label: format!("Ch {}", channel_num),
            gain: DEFAULT_GAIN,
            pan: DEFAULT_PAN,
            fader: DEFAULT_FADER,
            mute: false,
            pfl: false,
            to_main: true,
            to_grp: [false; MAX_GROUPS],
            aux_sends: [0.0; MAX_AUX_BUSES],
            aux_pre: DEFAULT_AUX_PRE,
            hpf_enabled: false,
            hpf_freq: DEFAULT_HPF_FREQ,
            gate_enabled: false,
            gate_threshold: DEFAULT_GATE_THRESHOLD,
            gate_attack: DEFAULT_GATE_ATTACK,
            gate_release: DEFAULT_GATE_RELEASE,
            comp_enabled: false,
            comp_threshold: DEFAULT_COMP_THRESHOLD,
            comp_ratio: DEFAULT_COMP_RATIO,
            comp_attack: DEFAULT_COMP_ATTACK,
            comp_release: DEFAULT_COMP_RELEASE,
            comp_makeup: DEFAULT_COMP_MAKEUP,
            comp_knee: DEFAULT_COMP_KNEE,
            eq_enabled: false,
            eq_bands: DEFAULT_EQ_BANDS,
        }
    }
}

impl GroupStrip {
    fn new(index: usize) -> Self {
        Self {
            index,
            fader: DEFAULT_FADER,
            mute: false,
        }
    }
}

impl AuxMaster {
    fn new(index: usize) -> Self {
        Self {
            index,
            fader: DEFAULT_FADER,
            mute: false,
        }
    }
}

/// What control is currently being adjusted (for value display).
#[derive(Debug, Clone, PartialEq)]
enum ActiveControl {
    None,
    Pan(usize),            // Channel index
    Fader(usize),          // Channel index
    AuxSend(usize, usize), // (Channel index, Aux index)
    GroupFader(usize),     // Group index
    AuxMasterFader(usize), // Aux master index
    MainFader,
}

/// What is currently selected in the mixer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Selection {
    Channel(usize),
    Main,
}

/// Mixer editor state.
pub struct MixerEditor {
    /// Flow ID
    flow_id: FlowId,
    /// Block ID (e.g., "b0")
    block_id: String,

    /// Number of channels
    num_channels: usize,
    /// Number of aux buses
    num_aux_buses: usize,
    /// Number of groups
    num_groups: usize,

    /// Channel strips
    channels: Vec<ChannelStrip>,
    /// Group strips
    groups: Vec<GroupStrip>,
    /// Aux masters
    aux_masters: Vec<AuxMaster>,

    /// Currently selected strip (channel or bus)
    selection: Option<Selection>,
    /// Currently active control (for value display)
    active_control: ActiveControl,

    /// Main fader level
    main_fader: f32,
    /// Main mute
    main_mute: bool,
    /// Main bus compressor enabled
    main_comp_enabled: bool,
    /// Main bus compressor threshold (dB)
    main_comp_threshold: f32,
    /// Main bus compressor ratio
    main_comp_ratio: f32,
    /// Main bus compressor attack (ms)
    main_comp_attack: f32,
    /// Main bus compressor release (ms)
    main_comp_release: f32,
    /// Main bus compressor makeup gain (dB)
    main_comp_makeup: f32,
    /// Main bus compressor knee (dB)
    main_comp_knee: f32,
    /// Main bus EQ enabled
    main_eq_enabled: bool,
    /// Main bus EQ bands: (freq, gain_db, q) for 4 bands
    main_eq_bands: [(f32, f32, f32); 4],
    /// Main bus limiter enabled
    main_limiter_enabled: bool,
    /// Main bus limiter threshold (dB)
    main_limiter_threshold: f32,

    /// API client
    api: ApiClient,

    /// Status message
    status: String,
    /// Error message
    error: Option<String>,

    /// Live updates enabled
    live_updates: bool,
    /// Fade duration in milliseconds applied to volume/mute updates.
    /// Sent as `ramp_ms` in PATCH requests; backend ignores it for
    /// non-volume properties. Higher values produce slower fades —
    /// useful when matching an auto-transition duration.
    fade_ms: u32,
    /// Last update time (for throttling)
    last_update: instant::Instant,
    /// Save requested (checked by app to persist properties)
    save_requested: bool,
    /// True after reset — next save writes only structural properties
    is_reset: bool,
    /// Channel index currently being label-edited (None = not editing)
    editing_label: Option<usize>,
    /// Transient: true when a strip or panel was clicked this frame
    strip_interacted: bool,
    /// Whether the pipeline is currently running (set by the app)
    pipeline_running: bool,
}

impl MixerEditor {
    /// Get the block ID.
    pub fn block_id(&self) -> &str {
        &self.block_id
    }

    /// Get the flow ID.
    pub fn flow_id(&self) -> FlowId {
        self.flow_id
    }

    /// Update the pipeline running state. Called by the app before rendering.
    pub fn set_pipeline_running(&mut self, running: bool) {
        self.pipeline_running = running;
    }

    /// Check if a save was requested (Ctrl+S or Save button).
    pub fn needs_save(&self) -> bool {
        self.save_requested
    }

    /// Clear the save-requested flag.
    pub fn clear_save(&mut self) {
        self.save_requested = false;
        self.is_reset = false;
    }

    /// True if this save is a reset (only structural properties should be saved).
    pub fn is_reset(&self) -> bool {
        self.is_reset
    }

    /// Compute the usable inner width of a strip based on number of aux buses.
    /// The aux knob row is typically the widest element.
    fn strip_inner(&self) -> f32 {
        if self.num_aux_buses == 0 {
            return MIN_STRIP_INNER;
        }
        let knob_row =
            self.num_aux_buses as f32 * KNOB_SIZE + (self.num_aux_buses as f32 - 1.0) * 2.0;
        knob_row.max(MIN_STRIP_INNER)
    }
}
