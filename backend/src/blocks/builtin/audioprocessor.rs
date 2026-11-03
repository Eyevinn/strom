//! Audio Processor block: a broadcast/streaming final processor.
//!
//! Wraps strom-dsp's `stromdsp-broadcastprocessor` (input gain, spike guard,
//! loudness AGC, multiband compressor with a downward expander per band and a
//! bass clipper, final clipper, true-peak limiter, and a clip budget governor
//! over the clipper and limiter). An operator puts it on the programme output,
//! in front of an encoder or an output block.
//!
//! Chain: audioconvert -> stromdsp-broadcastprocessor
//!
//! The element takes F32LE interleaved audio at any rate and channel count, so
//! the block converts the sample format and layout on its own input and keeps
//! the rate. It emits what the element produces (F32LE).
//!
//! Presets: with a named preset the block sets the element's `preset` and the
//! settings the presets leave alone (`enabled`, `num-bands`, `latency-mode`,
//! `meter-interval`, and `limiter-lookahead` when the flow overrides it).
//! With `custom` it sets every element property itself, so the sound never
//! depends on the element's defaults. The settings that apply only to `custom`
//! are listed in `strom_types::audioprocessor::CUSTOM_ONLY_PROPERTIES`.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use crate::gst::pipeline::properties::set_property_checked;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::audioprocessor::*;
use strom_types::{block::*, element::ElementPadRef, EnumValue, PropertyValue, StromEvent, *};
use tracing::{debug, info, trace};

/// The GStreamer element the block wraps.
pub const ELEMENT_FACTORY: &str = "stromdsp-broadcastprocessor";

/// Internal id of the processor element (relative to the block instance).
pub const PROCESSOR_ELEMENT: &str = "processor";

/// Internal id of the input format converter.
const CONVERT_ELEMENT: &str = "convert";

/// Structure name of the element's meter message.
const METER_MESSAGE: &str = "stromdsp-broadcastprocessor";

/// Default of one exposed setting.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Float(f32),
    Bool(bool),
    UInt(u32),
    /// An element enum: the default nick and every (nick, label).
    Enum(&'static str, &'static [(&'static str, &'static str)]),
}

/// One exposed setting and the element property it drives.
struct Setting {
    name: &'static str,
    element_property: &'static str,
    label: &'static str,
    description: &'static str,
    default: Kind,
    /// False for properties the element accepts only in NULL/READY.
    live: bool,
}

impl Setting {
    fn custom_only(&self) -> bool {
        CUSTOM_ONLY_PROPERTIES.contains(&self.name)
    }

    fn default_value(&self) -> PropertyValue {
        match self.default {
            Kind::Float(v) => PropertyValue::Float(v as f64),
            Kind::Bool(v) => PropertyValue::Bool(v),
            Kind::UInt(v) => PropertyValue::UInt(v as u64),
            Kind::Enum(v, _) => PropertyValue::String(v.to_string()),
        }
    }

    fn property_type(&self) -> PropertyType {
        match self.default {
            Kind::Float(_) => PropertyType::Float,
            Kind::Bool(_) => PropertyType::Bool,
            Kind::UInt(_) => PropertyType::UInt,
            Kind::Enum(_, values) => PropertyType::Enum {
                values: values
                    .iter()
                    .map(|(nick, label)| EnumValue {
                        value: nick.to_string(),
                        label: Some(label.to_string()),
                    })
                    .collect(),
            },
        }
    }

    /// The value to apply: the stored one, or the default. A value of the
    /// wrong type is an error naming the property, not a silent default.
    fn value(&self, properties: &HashMap<String, PropertyValue>) -> Result<PropertyValue, String> {
        let Some(v) = properties.get(self.name) else {
            return Ok(self.default_value());
        };
        let ok = match (self.default, v) {
            (Kind::Float(_), PropertyValue::Float(_))
            | (Kind::Float(_), PropertyValue::Int(_))
            | (Kind::Float(_), PropertyValue::UInt(_))
            | (Kind::Bool(_), PropertyValue::Bool(_))
            | (Kind::UInt(_), PropertyValue::UInt(_)) => true,
            (Kind::UInt(_), PropertyValue::Int(i)) => *i >= 0,
            (Kind::Enum(_, values), PropertyValue::String(s)) => {
                values.iter().any(|(nick, _)| nick == s)
            }
            _ => false,
        };
        if !ok {
            return Err(format!(
                "Audio Processor: {} must be a {}, got {:?}",
                self.name,
                match self.default {
                    Kind::Float(_) => "number",
                    Kind::Bool(_) => "boolean",
                    Kind::UInt(_) => "non-negative integer",
                    Kind::Enum(..) => "known value",
                },
                v
            ));
        }
        // Normalise numbers to the type the element property has.
        Ok(match (self.default, v) {
            (Kind::Float(_), PropertyValue::Int(i)) => PropertyValue::Float(*i as f64),
            (Kind::Float(_), PropertyValue::UInt(u)) => PropertyValue::Float(*u as f64),
            (Kind::UInt(_), PropertyValue::Int(i)) => PropertyValue::UInt(*i as u64),
            _ => v.clone(),
        })
    }
}

/// Every exposed setting except `preset`, in display order.
const SETTINGS: &[Setting] = &[
    Setting {
        name: "enabled",
        element_property: "enabled",
        label: "Enabled",
        description: "Off passes the audio through at unity gain with the same delay, so the latency does not change.",
        default: Kind::Bool(DEFAULT_ENABLED),
        live: true,
    },
    Setting {
        name: "input_gain",
        element_property: "input-gain",
        label: "Input Gain (dB)",
        description: "Gain before processing (-24 to 24 dB).",
        default: Kind::Float(DEFAULT_INPUT_GAIN_DB),
        live: true,
    },
    Setting {
        name: "target_loudness",
        element_property: "target-loudness",
        label: "Target Loudness (LUFS)",
        description: "Programme loudness the AGC aims for (-40 to -5 LUFS).",
        default: Kind::Float(DEFAULT_TARGET_LOUDNESS_LUFS),
        live: true,
    },
    Setting {
        name: "true_peak_ceiling",
        element_property: "true-peak-ceiling",
        label: "True-Peak Ceiling (dBTP)",
        description: "The limiter keeps the output true peak at or below this (-12 to 0 dBTP).",
        default: Kind::Float(DEFAULT_TRUE_PEAK_CEILING_DBTP),
        live: true,
    },
    Setting {
        name: "spike_guard_enabled",
        element_property: "spike-guard-enabled",
        label: "Spike Guard",
        description: "Removes clicks and other short, isolated peaks before the AGC, so they neither pump it nor count toward the loudness it measures.",
        default: Kind::Bool(DEFAULT_SPIKE_GUARD_ENABLED),
        live: true,
    },
    Setting {
        name: "spike_threshold",
        element_property: "spike-threshold",
        label: "Spike Threshold (dB)",
        description: "How far over the programme's own recent peaks a short peak must be to be removed; it comes down to this level (6 to 30 dB).",
        default: Kind::Float(DEFAULT_SPIKE_THRESHOLD_DB),
        live: true,
    },
    Setting {
        name: "agc_enabled",
        element_property: "agc-enabled",
        label: "AGC",
        description: "Loudness AGC: rides the gain slowly towards the target loudness.",
        default: Kind::Bool(DEFAULT_AGC_ENABLED),
        live: true,
    },
    Setting {
        name: "agc_max_gain",
        element_property: "agc-max-gain",
        label: "AGC Max Gain (dB)",
        description: "Most the AGC may raise the level (0 to 30 dB).",
        default: Kind::Float(DEFAULT_AGC_MAX_GAIN_DB),
        live: true,
    },
    Setting {
        name: "agc_max_cut",
        element_property: "agc-max-cut",
        label: "AGC Max Cut (dB)",
        description: "Most the AGC may lower the level (0 to 30 dB).",
        default: Kind::Float(DEFAULT_AGC_MAX_CUT_DB),
        live: true,
    },
    Setting {
        name: "agc_rate_up",
        element_property: "agc-rate-up",
        label: "AGC Rate Up (dB/s)",
        description: "How fast the AGC raises the gain (0.1 to 20 dB/s).",
        default: Kind::Float(DEFAULT_AGC_RATE_UP_DB_S),
        live: true,
    },
    Setting {
        name: "agc_rate_down",
        element_property: "agc-rate-down",
        label: "AGC Rate Down (dB/s)",
        description: "How fast the AGC lowers the gain (0.1 to 40 dB/s).",
        default: Kind::Float(DEFAULT_AGC_RATE_DOWN_DB_S),
        live: true,
    },
    Setting {
        name: "agc_gate",
        element_property: "agc-gate",
        label: "AGC Gate (LUFS)",
        description: "Below this loudness the AGC holds its gain, so pauses and silence are not pulled up (-80 to -20 LUFS).",
        default: Kind::Float(DEFAULT_AGC_GATE_LUFS),
        live: true,
    },
    Setting {
        name: "multiband_enabled",
        element_property: "multiband-enabled",
        label: "Multiband Compressor",
        description: "Multiband compressor between the AGC and the limiter.",
        default: Kind::Bool(DEFAULT_MULTIBAND_ENABLED),
        live: true,
    },
    Setting {
        name: "multiband_drive",
        element_property: "multiband-drive",
        label: "Multiband Drive (dB)",
        description: "Gain into the multiband compressor: more drive, more compression (-12 to 12 dB).",
        default: Kind::Float(DEFAULT_MULTIBAND_DRIVE_DB),
        live: true,
    },
    Setting {
        name: "expander_hold",
        element_property: "expander-hold",
        label: "Expander Hold (ms)",
        description: "How long the per-band downward expander waits before it lowers room tone and noise between words (0 to 1000 ms).",
        default: Kind::Float(DEFAULT_EXPANDER_HOLD_MS),
        live: true,
    },
    Setting {
        name: "bass_clipper_enabled",
        element_property: "bass-clipper-enabled",
        label: "Bass Clipper",
        description: "Holds loud bass on the lowest band at the bass clip threshold, without harmonics. Needs at least two bands.",
        default: Kind::Bool(DEFAULT_BASS_CLIPPER_ENABLED),
        live: true,
    },
    Setting {
        name: "bass_clip_threshold",
        element_property: "bass-clip-threshold",
        label: "Bass Clip Threshold (dB)",
        description: "Level of the lowest band the bass clipper holds (-40 to 0 dB).",
        default: Kind::Float(DEFAULT_BASS_CLIP_THRESHOLD_DB),
        live: true,
    },
    Setting {
        name: "num_bands",
        element_property: "num-bands",
        label: "Bands",
        description: "Number of multiband compressor bands (1 to 5). Construction-time only.",
        default: Kind::UInt(DEFAULT_NUM_BANDS),
        live: false,
    },
    Setting {
        name: "clipper_enabled",
        element_property: "clipper-enabled",
        label: "Final Clipper",
        description: "Soft, band-limited clipper just under the true-peak ceiling: takes the peaks so the limiter only catches residue.",
        default: Kind::Bool(DEFAULT_CLIPPER_ENABLED),
        live: true,
    },
    Setting {
        name: "clipper_bandwidth",
        element_property: "clipper-bandwidth",
        label: "Clipper Bandwidth (Hz)",
        description: "The final clipper adds nothing above this frequency (8000 to 22000 Hz).",
        default: Kind::Float(DEFAULT_CLIPPER_BANDWIDTH_HZ),
        live: true,
    },
    Setting {
        name: "governor_enabled",
        element_property: "governor-enabled",
        label: "Clip Budget Governor",
        description: "Slow loop that watches the clipper and limiter against the clip budget. Loudness comes first: it never lowers the output below the target to stay within the budget, it reports the excess instead.",
        default: Kind::Bool(DEFAULT_GOVERNOR_ENABLED),
        live: true,
    },
    Setting {
        name: "clip_budget",
        element_property: "clip-budget",
        label: "Clip Budget (dB)",
        description: "How much clipper and limiter work the governor allows before it holds back gain and reports an excess (0 to 6 dB).",
        default: Kind::Float(DEFAULT_CLIP_BUDGET_DB),
        live: true,
    },
    Setting {
        name: "limiter_enabled",
        element_property: "limiter-enabled",
        label: "Limiter",
        description: "True-peak limiter at the end of the chain.",
        default: Kind::Bool(DEFAULT_LIMITER_ENABLED),
        live: true,
    },
    Setting {
        name: "limiter_release",
        element_property: "limiter-release",
        label: "Limiter Release (ms)",
        description: "Limiter release time (1 to 2000 ms).",
        default: Kind::Float(DEFAULT_LIMITER_RELEASE_MS),
        live: true,
    },
    Setting {
        name: "limiter_lookahead",
        element_property: "limiter-lookahead",
        label: "Limiter Lookahead (ms)",
        description: "Overrides the latency mode's limiter lookahead (1.5 ms standard, 0.6 ms low; 0.5 to 20 ms). Adds to the block's latency. Leave it unset to follow the latency mode. Applies with every preset. Construction-time only.",
        default: Kind::Float(DEFAULT_LIMITER_LOOKAHEAD_MS),
        live: false,
    },
    Setting {
        name: "latency_mode",
        element_property: "latency-mode",
        label: "Latency Mode",
        description: "Standard: about 5.6 ms at 48 kHz. Low: about 2.8 ms, for two-way use (commentary, talkback, calls); it trades a little peak control (shorter lookaheads, a lighter final clipper) for the lower latency. Applies with every preset. Construction-time only.",
        default: Kind::Enum(DEFAULT_LATENCY_MODE, LATENCY_MODES),
        live: false,
    },
    Setting {
        name: "meter_interval",
        element_property: "meter-interval",
        label: "Meter Interval (ms)",
        description: "How often meter values are sent (0 turns the meters off).",
        default: Kind::UInt(DEFAULT_METER_INTERVAL_MS),
        live: true,
    },
];

fn setting(name: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.name == name)
}

/// The preset the block runs with: the stored one, or the default. An unknown
/// preset is an error naming the ones there are.
pub fn preset(properties: &HashMap<String, PropertyValue>) -> Result<String, String> {
    match properties.get(PRESET_PROPERTY) {
        None => Ok(DEFAULT_PRESET.to_string()),
        Some(PropertyValue::String(s)) if PRESETS.iter().any(|(nick, _)| nick == s) => {
            Ok(s.clone())
        }
        Some(other) => Err(format!(
            "Audio Processor: unknown preset {:?}, expected one of {}",
            other,
            PRESETS
                .iter()
                .map(|(nick, _)| *nick)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Whether `name` is a block property that applies only with the custom preset.
pub fn is_custom_only(name: &str) -> bool {
    CUSTOM_ONLY_PROPERTIES.contains(&name)
}

/// The element property writes that make the custom preset, in order: every
/// custom-only setting (stored value or Strom default), then the band and
/// crossover settings the block does not expose.
///
/// `running` leaves out the properties the element only takes in NULL/READY,
/// for switching a running block to custom.
pub fn custom_writes(
    properties: &HashMap<String, PropertyValue>,
    running: bool,
) -> Result<Vec<(String, PropertyValue)>, String> {
    let mut writes = Vec::new();
    for s in SETTINGS.iter().filter(|s| s.custom_only()) {
        if running && !s.live {
            continue;
        }
        writes.push((s.element_property.to_string(), s.value(properties)?));
    }
    // All crossovers in one write: one at a time, a write can land on the
    // wrong side of a neighbour that still has its old value.
    writes.push((
        CROSSOVERS_PROPERTY.to_string(),
        PropertyValue::String(crossover_array(&DEFAULT_CROSSOVER_FREQUENCIES_HZ)),
    ));
    for n in 0..MAX_NUM_BANDS as usize {
        for (suffix, value) in [
            ("threshold", DEFAULT_BAND_THRESHOLD_DB[n]),
            ("ratio", DEFAULT_BAND_RATIO[n]),
            ("attack", DEFAULT_BAND_ATTACK_MS[n]),
            ("release", DEFAULT_BAND_RELEASE_MS[n]),
            ("makeup", DEFAULT_BAND_MAKEUP_DB[n]),
            ("expander-threshold", DEFAULT_BAND_EXPANDER_THRESHOLD_DB[n]),
            ("expander-ratio", DEFAULT_BAND_EXPANDER_RATIO[n]),
            ("expander-floor", DEFAULT_BAND_EXPANDER_FLOOR_DB[n]),
        ] {
            writes.push((
                format!("band{n}-{suffix}"),
                PropertyValue::Float(value as f64),
            ));
        }
    }
    Ok(writes)
}

/// Element property that sets every crossover at once (a GstValueArray of
/// gfloat Hz).
const CROSSOVERS_PROPERTY: &str = "crossover-frequencies";

/// `frequencies` in GStreamer's serialized array form, which the checked
/// setter deserializes into the element's GstValueArray.
fn crossover_array(frequencies: &[f32]) -> String {
    let items: Vec<String> = frequencies.iter().map(|f| format!("(float){f}")).collect();
    format!("< {} >", items.join(", "))
}

/// Every element property write the block makes at build time, in order.
pub fn build_writes(
    properties: &HashMap<String, PropertyValue>,
) -> Result<Vec<(String, PropertyValue)>, String> {
    let preset = preset(properties)?;
    let mut writes = Vec::new();

    // The settings no preset touches come first, `num-bands` before anything
    // that addresses a band. The preset comes after them, so they do not
    // turn it into custom. `latency-mode` sets the limiter lookahead along
    // with the other lookaheads, so a stored `limiter_lookahead` is written
    // after it, as an override, and only when the flow has one.
    for name in [
        "num_bands",
        "latency_mode",
        "limiter_lookahead",
        "enabled",
        "meter_interval",
    ] {
        let s = setting(name).expect("always-applied setting exists");
        if name == "limiter_lookahead" && !properties.contains_key(name) {
            continue;
        }
        writes.push((s.element_property.to_string(), s.value(properties)?));
    }

    if preset == PRESET_CUSTOM {
        // Writing `custom` to the element changes nothing; the writes do.
        writes.extend(custom_writes(properties, false)?);
    } else {
        writes.push(("preset".to_string(), PropertyValue::String(preset)));
    }
    Ok(writes)
}

/// Audio Processor block builder.
pub struct AudioProcessorBuilder;

impl BlockBuilder for AudioProcessorBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        build_with_factory(instance_id, properties, ELEMENT_FACTORY)
    }
}

/// Build the block around `factory`. Separate from `build` so a test can name
/// a factory that does not exist and check that the flow fails.
fn build_with_factory(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    factory: &str,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building Audio Processor block instance: {}", instance_id);

    if gst::ElementFactory::find(factory).is_none() {
        return Err(BlockBuildError::MissingPlugin(missing_element_message()));
    }

    let writes = build_writes(properties).map_err(BlockBuildError::InvalidProperty)?;

    let convert_id = format!("{instance_id}:{CONVERT_ELEMENT}");
    let convert = gst::ElementFactory::make("audioconvert")
        .name(&convert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {e}")))?;

    let processor_id = format!("{instance_id}:{PROCESSOR_ELEMENT}");
    let processor = gst::ElementFactory::make(factory)
        .name(&processor_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{factory}: {e}")))?;

    for (name, value) in &writes {
        set_property_checked(&processor, name, value).map_err(|reason| {
            BlockBuildError::InvalidProperty(format!(
                "Audio Processor: {ELEMENT_FACTORY} {name} = {value:?}: {reason}"
            ))
        })?;
    }
    debug!(
        "Audio Processor {}: applied {} element properties (preset {:?})",
        instance_id,
        writes.len(),
        properties.get(PRESET_PROPERTY)
    );

    let internal_links = vec![(
        ElementPadRef::pad(&convert_id, "src"),
        ElementPadRef::pad(&processor_id, "sink"),
    )];

    let expected_source = processor_id.clone();
    let block_id = instance_id.to_string();
    let bus_message_handler = Some(Box::new(
        move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
            connect_meter_handler(bus, flow_id, events, expected_source, block_id)
        },
    ) as crate::blocks::BusMessageConnectFn);

    Ok(BlockBuildResult {
        elements: vec![(convert_id, convert), (processor_id, processor)],
        internal_links,
        bus_message_handler,
        pad_properties: HashMap::new(),
    })
}

/// The build error when this GStreamer has no broadcast processor.
pub fn missing_element_message() -> String {
    format!(
        "Audio Processor needs the GStreamer element {ELEMENT_FACTORY} from strom-dsp \
         (gst-plugin-stromdsp), and this build does not have it. The flow does not pass \
         the audio through unprocessed."
    )
}

/// Read an element meter message into an event. `None` when the structure is
/// not the processor's meter message or a field is missing.
pub fn meter_event(s: &gst::StructureRef, flow_id: FlowId, element_id: &str) -> Option<StromEvent> {
    if s.name() != METER_MESSAGE {
        return None;
    }
    let f = |name: &str| s.get::<f64>(name).ok().map(finite_or_silence);
    let band_gain_reduction = s
        .get::<gst::Array>("band-gain-reduction")
        .ok()?
        .iter()
        .filter_map(|v| v.get::<f64>().ok())
        .map(finite_or_silence)
        .collect();
    Some(StromEvent::AudioProcessorData {
        flow_id,
        element_id: element_id.to_string(),
        input_momentary: f("input-momentary")?,
        input_short_term: f("input-short-term")?,
        output_momentary: f("output-momentary")?,
        output_short_term: f("output-short-term")?,
        output_integrated: f("output-integrated")?,
        input_true_peak: f("input-true-peak")?,
        output_true_peak: f("output-true-peak")?,
        agc_gain: f("agc-gain")?,
        band_gain_reduction,
        limiter_gain_reduction: f("limiter-gain-reduction")?,
    })
}

/// The element reports silence as -200, never NaN or infinity. JSON has no
/// NaN, so anything that is not finite is mapped to silence rather than
/// breaking the WebSocket message.
fn finite_or_silence(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        METER_SILENCE
    }
}

/// Read the stage meters of an element meter message into an event. `None`
/// when the structure is not the processor's meter message or a field is
/// missing (strom-dsp before v0.2.0 does not report these stages).
pub fn stage_meter_event(
    s: &gst::StructureRef,
    flow_id: FlowId,
    element_id: &str,
) -> Option<StromEvent> {
    if s.name() != METER_MESSAGE {
        return None;
    }
    let f = |name: &str| s.get::<f64>(name).ok().map(finite_or_silence);
    let band_expander_gain_reduction = s
        .get::<gst::Array>("band-expander-gain-reduction")
        .ok()?
        .iter()
        .filter_map(|v| v.get::<f64>().ok())
        .map(finite_or_silence)
        .collect();
    Some(StromEvent::AudioProcessorStageData {
        flow_id,
        element_id: element_id.to_string(),
        spike_events: s.get::<u64>("spike-events").ok()?,
        band_expander_gain_reduction,
        bass_clip_gain_reduction: f("bass-clip-gr")?,
        clip_depth: f("clip-depth")?,
        loudness_deficit: f("loudness-deficit")?,
        budget_excess: f("budget-excess")?,
        governor_state: AudioProcessorGovernorState::from_nick(
            s.get::<&str>("governor-state").ok()?,
        ),
    })
}

fn connect_meter_handler(
    bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
    expected_source: String,
    block_id: String,
) -> gst::glib::SignalHandlerId {
    debug!(
        "Connecting Audio Processor meter handler for flow {} element {}",
        flow_id, expected_source
    );

    // No add_signal_watch() here: setup_bus_watch takes the single watch this
    // bus needs, and matches it with exactly one remove on teardown.
    bus.connect_message(None, move |_bus, msg| {
        let gst::MessageView::Element(element_msg) = msg.view() else {
            return;
        };
        let Some(s) = element_msg.structure() else {
            return;
        };
        if s.name() != METER_MESSAGE {
            return;
        }
        if msg.src().map(|src| src.name()) != Some(expected_source.as_str().into()) {
            return;
        }
        match meter_event(s, flow_id, &block_id) {
            Some(event) => events.broadcast(event),
            None => trace!(
                "Audio Processor {}: meter message missing fields: {}",
                block_id,
                s
            ),
        }
        match stage_meter_event(s, flow_id, &block_id) {
            Some(event) => events.broadcast(event),
            None => trace!(
                "Audio Processor {}: meter message missing stage fields: {}",
                block_id,
                s
            ),
        }
    })
}

/// Get metadata for the Audio Processor block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![audioprocessor_definition()]
}

fn audioprocessor_definition() -> BlockDefinition {
    let mut exposed_properties = vec![ExposedProperty {
        name: PRESET_PROPERTY.to_string(),
        label: "Preset".to_string(),
        description: "Processing preset. EBU R 128: -23 LUFS / -1 dBTP. ATSC A/85: -24 LKFS / \
                      -2 dBTP. Streaming: -16 LUFS / -1 dBTP. Speech: -18 LUFS / -1 dBTP. A named \
                      preset sets loudness, spike guard, AGC, multiband, expander, bass clipper, \
                      final clipper, clip budget and limiter from its own tuning; choose Custom to \
                      set them yourself. Latency mode, bands and limiter lookahead apply with \
                      every preset."
            .to_string(),
        property_type: PropertyType::Enum {
            values: PRESETS
                .iter()
                .map(|(nick, label)| EnumValue {
                    value: nick.to_string(),
                    label: Some(label.to_string()),
                })
                .collect(),
        },
        default_value: Some(PropertyValue::String(DEFAULT_PRESET.to_string())),
        mapping: PropertyMapping {
            element_id: PROCESSOR_ELEMENT.to_string(),
            property_name: "preset".to_string(),
            transform: None,
        },
        live: true,
        persist: None,
    }];

    for s in SETTINGS {
        let description = if s.custom_only() {
            format!("{} Used with the Custom preset.", s.description)
        } else {
            s.description.to_string()
        };
        exposed_properties.push(ExposedProperty {
            name: s.name.to_string(),
            label: s.label.to_string(),
            description,
            property_type: s.property_type(),
            default_value: Some(s.default_value()),
            mapping: PropertyMapping {
                element_id: PROCESSOR_ELEMENT.to_string(),
                property_name: s.element_property.to_string(),
                transform: None,
            },
            live: s.live,
            persist: None,
        });
    }

    BlockDefinition {
        id: BLOCK_ID.to_string(),
        name: "Audio Processor".to_string(),
        description: "Work in progress. Broadcast/streaming final processor: input gain, \
                      spike guard, loudness AGC, multiband compressor with a downward expander \
                      per band and a bass clipper, final clipper and true-peak limiter, with a \
                      clip budget governor that never trades loudness for less clipping. \
                      Presets for EBU R 128, ATSC A/85, streaming and speech. Put it on the \
                      programme output, before the encoder. Latency about 5.6 ms at 48 kHz, or \
                      about 2.8 ms in the low latency mode."
            .to_string(),
        category: "Audio".to_string(),
        exposed_properties,
        external_pads: ExternalPads {
            inputs: vec![ExternalPad::new(
                "audio_in",
                MediaType::Audio,
                CONVERT_ELEMENT,
                "sink",
            )],
            outputs: vec![ExternalPad::new(
                "audio_out",
                MediaType::Audio,
                PROCESSOR_ELEMENT,
                "src",
            )],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("\u{E228}".to_string()),
            width: Some(2.6),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// `PropertyValue` has no `PartialEq`; its Debug form names the variant
    /// and the value.
    macro_rules! assert_pv {
        ($a:expr, $b:expr) => {
            assert_eq!(format!("{:?}", $a), format!("{:?}", $b))
        };
    }

    fn names(writes: &[(String, PropertyValue)]) -> Vec<&str> {
        writes.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn value<'a>(writes: &'a [(String, PropertyValue)], name: &str) -> &'a PropertyValue {
        &writes
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no write of {name}"))
            .1
    }

    #[test]
    fn definition_has_one_audio_input_and_output_and_every_setting() {
        let def = audioprocessor_definition();
        assert_eq!(def.id, "builtin.audioprocessor");
        assert_eq!(def.category, "Audio");
        assert_eq!(def.name, "Audio Processor");
        // No framework-wide flag exists for this; the description says it.
        assert!(
            def.description.starts_with("Work in progress."),
            "{}",
            def.description
        );
        assert_eq!(def.external_pads.inputs.len(), 1);
        assert_eq!(def.external_pads.outputs.len(), 1);
        assert_eq!(def.external_pads.inputs[0].media_type, MediaType::Audio);
        assert_eq!(def.external_pads.outputs[0].media_type, MediaType::Audio);
        assert_eq!(def.external_pads.inputs[0].internal_element_id, "convert");
        assert_eq!(
            def.external_pads.outputs[0].internal_element_id,
            "processor"
        );

        let exposed: Vec<&str> = def
            .exposed_properties
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        for required in [
            "preset",
            "target_loudness",
            "true_peak_ceiling",
            "input_gain",
            "agc_enabled",
            "agc_max_gain",
            "agc_max_cut",
            "agc_gate",
            "multiband_enabled",
            "limiter_enabled",
            "enabled",
            "spike_guard_enabled",
            "spike_threshold",
            "expander_hold",
            "bass_clipper_enabled",
            "bass_clip_threshold",
            "clipper_enabled",
            "clipper_bandwidth",
            "governor_enabled",
            "clip_budget",
            "latency_mode",
            "limiter_lookahead",
        ] {
            assert!(exposed.contains(&required), "{required} not exposed");
        }
        // Every custom-only name is a real setting, so the frontend's list
        // and the backend's agree.
        for name in CUSTOM_ONLY_PROPERTIES {
            assert!(
                exposed.contains(name),
                "{name} is custom-only but not exposed"
            );
        }
    }

    #[test]
    fn definition_is_registered_with_a_builder() {
        assert!(super::super::get_all_builtin_blocks()
            .iter()
            .any(|b| b.id == BLOCK_ID));
        assert!(super::super::get_builder(BLOCK_ID).is_some());
    }

    /// The element accepts `num-bands`, `latency-mode` and `limiter-lookahead`
    /// only in NULL/READY. Marking them live would make every live write fail.
    #[test]
    fn ready_only_element_properties_are_not_live() {
        let def = audioprocessor_definition();
        for p in &def.exposed_properties {
            let ready_only = matches!(
                p.mapping.property_name.as_str(),
                "num-bands" | "latency-mode" | "limiter-lookahead"
            );
            assert_eq!(p.live, !ready_only, "{}", p.name);
            assert_eq!(p.mapping.element_id, PROCESSOR_ELEMENT, "{}", p.name);
        }
    }

    #[test]
    fn defaults_come_from_strom_types() {
        let def = audioprocessor_definition();
        let default = |name: &str| {
            def.exposed_properties
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| p.default_value.clone())
                .unwrap()
        };
        assert_pv!(
            default("preset"),
            PropertyValue::String(DEFAULT_PRESET.to_string())
        );
        assert_pv!(
            default("target_loudness"),
            PropertyValue::Float(DEFAULT_TARGET_LOUDNESS_LUFS as f64)
        );
        assert_pv!(
            default("true_peak_ceiling"),
            PropertyValue::Float(DEFAULT_TRUE_PEAK_CEILING_DBTP as f64)
        );
        assert_pv!(
            default("limiter_lookahead"),
            PropertyValue::Float(DEFAULT_LIMITER_LOOKAHEAD_MS as f64)
        );
        assert_pv!(
            default("num_bands"),
            PropertyValue::UInt(DEFAULT_NUM_BANDS as u64)
        );
        assert_pv!(
            default("latency_mode"),
            PropertyValue::String(DEFAULT_LATENCY_MODE.to_string())
        );
        assert_pv!(
            default("clip_budget"),
            PropertyValue::Float(DEFAULT_CLIP_BUDGET_DB as f64)
        );
        assert_pv!(
            default("spike_threshold"),
            PropertyValue::Float(DEFAULT_SPIKE_THRESHOLD_DB as f64)
        );
    }

    /// The latency mode is a dropdown of the element's nicks, not a preset
    /// setting, and its description says what the low mode costs.
    #[test]
    fn latency_mode_is_an_enum_that_applies_with_every_preset() {
        let def = audioprocessor_definition();
        let p = def
            .exposed_properties
            .iter()
            .find(|p| p.name == "latency_mode")
            .unwrap();
        assert!(!is_custom_only("latency_mode"));
        assert!(!p.live);
        assert_eq!(p.mapping.property_name, "latency-mode");
        match &p.property_type {
            PropertyType::Enum { values } => {
                let nicks: Vec<&str> = values.iter().map(|v| v.value.as_str()).collect();
                assert_eq!(nicks, ["standard", "low"]);
            }
            other => panic!("latency_mode is {other:?}"),
        }
        assert!(p.description.contains("peak control"), "{}", p.description);
    }

    /// Every stage setting the presets set is custom-only, so a named preset
    /// hides it and a live write to it is refused.
    #[test]
    fn stage_settings_are_custom_only() {
        for name in [
            "spike_guard_enabled",
            "spike_threshold",
            "expander_hold",
            "bass_clipper_enabled",
            "bass_clip_threshold",
            "clipper_enabled",
            "clipper_bandwidth",
            "governor_enabled",
            "clip_budget",
        ] {
            assert!(is_custom_only(name), "{name}");
            assert!(setting(name).unwrap().live, "{name}");
        }
    }

    #[test]
    fn named_preset_sets_the_preset_and_only_what_presets_leave_alone() {
        let writes = build_writes(&props(&[
            ("preset", PropertyValue::String("streaming".to_string())),
            // Ignored: the preset owns it.
            ("target_loudness", PropertyValue::Float(-14.0)),
        ]))
        .unwrap();
        assert_eq!(
            names(&writes),
            [
                "num-bands",
                "latency-mode",
                "enabled",
                "meter-interval",
                "preset"
            ]
        );
        assert_pv!(
            value(&writes, "preset"),
            &PropertyValue::String("streaming".to_string())
        );
    }

    #[test]
    fn default_preset_is_written_explicitly() {
        let writes = build_writes(&HashMap::new()).unwrap();
        assert_pv!(
            value(&writes, "preset"),
            &PropertyValue::String(DEFAULT_PRESET.to_string())
        );
        assert_pv!(
            value(&writes, "num-bands"),
            &PropertyValue::UInt(DEFAULT_NUM_BANDS as u64)
        );
        assert_pv!(
            value(&writes, "meter-interval"),
            &PropertyValue::UInt(DEFAULT_METER_INTERVAL_MS as u64)
        );
        assert_pv!(value(&writes, "enabled"), &PropertyValue::Bool(true));
        assert_pv!(
            value(&writes, "latency-mode"),
            &PropertyValue::String(DEFAULT_LATENCY_MODE.to_string())
        );
        // The latency mode sets the limiter lookahead unless the flow
        // overrides it.
        assert!(!names(&writes).contains(&"limiter-lookahead"));
    }

    /// A stored lookahead overrides the latency mode's, so it is written after
    /// it; writing it first would be undone by the mode.
    #[test]
    fn a_stored_limiter_lookahead_is_written_after_the_latency_mode() {
        let writes = build_writes(&props(&[
            ("preset", PropertyValue::String("speech".to_string())),
            ("latency_mode", PropertyValue::String("low".to_string())),
            ("limiter_lookahead", PropertyValue::Float(3.0)),
        ]))
        .unwrap();
        let written = names(&writes);
        assert_eq!(
            written,
            [
                "num-bands",
                "latency-mode",
                "limiter-lookahead",
                "enabled",
                "meter-interval",
                "preset"
            ]
        );
        assert_pv!(
            value(&writes, "latency-mode"),
            &PropertyValue::String("low".to_string())
        );
        assert_pv!(
            value(&writes, "limiter-lookahead"),
            &PropertyValue::Float(3.0)
        );
    }

    #[test]
    fn an_unknown_latency_mode_is_an_error() {
        let err = build_writes(&props(&[(
            "latency_mode",
            PropertyValue::String("instant".to_string()),
        )]))
        .unwrap_err();
        assert!(err.contains("latency_mode"), "{err}");
    }

    /// With Custom the block owns the sound: every element property the
    /// chain uses is written, none is left at the element's default.
    #[test]
    fn custom_preset_sets_every_element_property() {
        let writes = build_writes(&props(&[
            ("preset", PropertyValue::String("custom".to_string())),
            ("target_loudness", PropertyValue::Float(-14.0)),
            ("agc_enabled", PropertyValue::Bool(false)),
            ("clip_budget", PropertyValue::Float(3.5)),
            ("spike_guard_enabled", PropertyValue::Bool(false)),
            ("limiter_lookahead", PropertyValue::Float(2.0)),
        ]))
        .unwrap();
        let written = names(&writes);
        assert!(!written.contains(&"preset"), "custom is never written");

        let mut expected: Vec<String> = SETTINGS
            .iter()
            .map(|s| s.element_property.to_string())
            .collect();
        expected.push("crossover-frequencies".to_string());
        for n in 0..5 {
            for p in [
                "threshold",
                "ratio",
                "attack",
                "release",
                "makeup",
                "expander-threshold",
                "expander-ratio",
                "expander-floor",
            ] {
                expected.push(format!("band{n}-{p}"));
            }
        }
        for name in &expected {
            assert!(written.contains(&name.as_str()), "{name} not written");
        }
        assert_eq!(written.len(), expected.len(), "{written:?}");
        // num-bands before any band property.
        assert_eq!(written[0], "num-bands");

        assert_pv!(
            value(&writes, "target-loudness"),
            &PropertyValue::Float(-14.0)
        );
        assert_pv!(value(&writes, "agc-enabled"), &PropertyValue::Bool(false));
        assert_pv!(
            value(&writes, "true-peak-ceiling"),
            &PropertyValue::Float(DEFAULT_TRUE_PEAK_CEILING_DBTP as f64)
        );
        assert_pv!(
            value(&writes, "band0-ratio"),
            &PropertyValue::Float(DEFAULT_BAND_RATIO[0] as f64)
        );
        assert_pv!(value(&writes, "clip-budget"), &PropertyValue::Float(3.5));
        assert_pv!(
            value(&writes, "spike-guard-enabled"),
            &PropertyValue::Bool(false)
        );
        assert_pv!(
            value(&writes, "bass-clip-threshold"),
            &PropertyValue::Float(DEFAULT_BASS_CLIP_THRESHOLD_DB as f64)
        );
        assert_pv!(
            value(&writes, "band3-expander-floor"),
            &PropertyValue::Float(DEFAULT_BAND_EXPANDER_FLOOR_DB[3] as f64)
        );
    }

    #[test]
    fn switching_a_running_block_to_custom_skips_ready_only_properties() {
        let writes = custom_writes(&HashMap::new(), true).unwrap();
        let written = names(&writes);
        assert!(!written.contains(&"limiter-lookahead"));
        assert!(!written.contains(&"latency-mode"));
        assert!(!written.contains(&"num-bands"));
        assert!(written.contains(&"clip-budget"));
        assert!(written.contains(&"band4-expander-ratio"));
        assert!(written.contains(&"crossover-frequencies"));
        assert!(written.contains(&"target-loudness"));
        assert!(written.contains(&"band4-makeup"));
    }

    #[test]
    fn integers_are_accepted_for_float_settings() {
        let writes = build_writes(&props(&[
            ("preset", PropertyValue::String("custom".to_string())),
            ("target_loudness", PropertyValue::Int(-16)),
            ("num_bands", PropertyValue::Int(3)),
        ]))
        .unwrap();
        assert_pv!(
            value(&writes, "target-loudness"),
            &PropertyValue::Float(-16.0)
        );
        assert_pv!(value(&writes, "num-bands"), &PropertyValue::UInt(3));
    }

    #[test]
    fn a_wrong_type_or_unknown_preset_is_an_error() {
        let err = build_writes(&props(&[
            ("preset", PropertyValue::String("custom".to_string())),
            ("agc_enabled", PropertyValue::String("yes".to_string())),
        ]))
        .unwrap_err();
        assert!(err.contains("agc_enabled"), "{err}");

        let err = build_writes(&props(&[(
            "preset",
            PropertyValue::String("loud".to_string()),
        )]))
        .unwrap_err();
        assert!(err.contains("ebu-r128"), "{err}");
    }

    #[test]
    fn meter_message_becomes_an_event() {
        gst::init().unwrap();
        let s = gst::Structure::builder(METER_MESSAGE)
            .field("running-time", 1_000_000_000u64)
            .field("stream-time", 1_000_000_000u64)
            .field("input-momentary", -30.0f64)
            .field("input-short-term", -31.0f64)
            .field("output-momentary", -23.5f64)
            .field("output-short-term", -23.1f64)
            .field("output-integrated", -23.2f64)
            .field("input-true-peak", -0.5f64)
            .field("output-true-peak", -1.2f64)
            .field("agc-gain", 7.0f64)
            .field(
                "band-gain-reduction",
                gst::Array::new([0.5f64, 1.0, 1.5, 2.0]),
            )
            .field("limiter-gain-reduction", 0.3f64)
            .build();
        let flow_id = FlowId::new_v4();
        match meter_event(&s, flow_id, "proc") {
            Some(StromEvent::AudioProcessorData {
                flow_id: f,
                element_id,
                input_momentary,
                input_short_term,
                output_momentary,
                output_short_term,
                output_integrated,
                input_true_peak,
                output_true_peak,
                agc_gain,
                band_gain_reduction,
                limiter_gain_reduction,
            }) => {
                assert_eq!(output_integrated, -23.2);
                assert_eq!(input_true_peak, -0.5);
                assert_eq!(f, flow_id);
                assert_eq!(element_id, "proc");
                assert_eq!(input_momentary, -30.0);
                assert_eq!(input_short_term, -31.0);
                assert_eq!(output_momentary, -23.5);
                assert_eq!(output_short_term, -23.1);
                assert_eq!(output_true_peak, -1.2);
                assert_eq!(agc_gain, 7.0);
                assert_eq!(band_gain_reduction, vec![0.5, 1.0, 1.5, 2.0]);
                assert_eq!(limiter_gain_reduction, 0.3);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The full v0.2.0 meter message: the old fields and the stage fields.
    fn stage_meter_message() -> gst::Structure {
        gst::Structure::builder(METER_MESSAGE)
            .field("running-time", 1_000_000_000u64)
            .field("stream-time", 1_000_000_000u64)
            .field("input-momentary", -30.0f64)
            .field("input-short-term", -31.0f64)
            .field("output-momentary", -23.5f64)
            .field("output-short-term", -23.1f64)
            .field("output-integrated", -23.2f64)
            .field("input-true-peak", -0.5f64)
            .field("output-true-peak", -1.2f64)
            .field("spike-events", 3u64)
            .field("agc-gain", 7.0f64)
            .field("band-gain-reduction", gst::Array::new([0.5f64, 1.0, 1.5]))
            .field("limiter-gain-reduction", 0.3f64)
            .field(
                "band-expander-gain-reduction",
                gst::Array::new([2.0f64, 0.0, 4.0]),
            )
            .field("bass-clip-gr", 1.25f64)
            .field("clip-depth", -42.0f64)
            .field("loudness-deficit", 0.0f64)
            .field("budget-excess", 0.65f64)
            .field("governor-state", "over-budget")
            .build()
    }

    #[test]
    fn stage_meters_become_their_own_event() {
        gst::init().unwrap();
        let s = stage_meter_message();
        let flow_id = FlowId::new_v4();
        // The same message still makes the unchanged AudioProcessorData.
        assert!(matches!(
            meter_event(&s, flow_id, "proc"),
            Some(StromEvent::AudioProcessorData { .. })
        ));
        match stage_meter_event(&s, flow_id, "proc") {
            Some(StromEvent::AudioProcessorStageData {
                flow_id: f,
                element_id,
                spike_events,
                band_expander_gain_reduction,
                bass_clip_gain_reduction,
                clip_depth,
                loudness_deficit,
                budget_excess,
                governor_state,
            }) => {
                assert_eq!(f, flow_id);
                assert_eq!(element_id, "proc");
                assert_eq!(spike_events, 3);
                assert_eq!(band_expander_gain_reduction, vec![2.0, 0.0, 4.0]);
                assert_eq!(bass_clip_gain_reduction, 1.25);
                assert_eq!(clip_depth, -42.0);
                assert_eq!(loudness_deficit, 0.0);
                assert_eq!(budget_excess, 0.65);
                assert_eq!(governor_state, AudioProcessorGovernorState::OverBudget);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// strom-dsp before v0.2.0 sends no stage fields: the old event still
    /// comes, the stage event does not. A stage field missing from a newer
    /// message drops only the stage event, and an unknown governor state
    /// reads as `Unknown`, not as a dropped message.
    #[test]
    fn stage_meters_with_a_missing_field_are_ignored() {
        gst::init().unwrap();
        let flow_id = FlowId::new_v4();
        for field in [
            "spike-events",
            "band-expander-gain-reduction",
            "bass-clip-gr",
            "clip-depth",
            "loudness-deficit",
            "budget-excess",
            "governor-state",
        ] {
            let mut s = stage_meter_message();
            s.remove_field(field);
            assert!(
                stage_meter_event(&s, flow_id, "proc").is_none(),
                "{field} missing"
            );
            assert!(
                meter_event(&s, flow_id, "proc").is_some(),
                "{field} missing"
            );
        }

        let mut s = stage_meter_message();
        s.set("governor-state", "sideways");
        assert!(matches!(
            stage_meter_event(&s, flow_id, "proc"),
            Some(StromEvent::AudioProcessorStageData {
                governor_state: AudioProcessorGovernorState::Unknown,
                ..
            })
        ));

        let s = gst::Structure::builder("ebur128-level").build();
        assert!(stage_meter_event(&s, flow_id, "proc").is_none());
    }

    #[test]
    fn meter_message_with_a_missing_field_or_other_name_is_ignored() {
        gst::init().unwrap();
        let s = gst::Structure::builder(METER_MESSAGE)
            .field("input-momentary", -30.0f64)
            .build();
        assert!(meter_event(&s, FlowId::new_v4(), "proc").is_none());
        let s = gst::Structure::builder("ebur128-level").build();
        assert!(meter_event(&s, FlowId::new_v4(), "proc").is_none());
    }

    /// Without the element the flow must fail with a message naming it, never
    /// build a block that passes audio through unprocessed.
    #[test]
    fn a_missing_element_fails_the_build_and_names_it() {
        gst::init().unwrap();
        let err = match build_with_factory("proc", &HashMap::new(), "stromdsp-no-such-element") {
            Err(e) => e,
            Ok(_) => panic!("built without the element"),
        };
        assert!(matches!(err, BlockBuildError::MissingPlugin(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("stromdsp-broadcastprocessor"), "{msg}");
        assert!(msg.contains("strom-dsp"), "{msg}");
    }

    #[test]
    fn crossovers_are_serialized_as_a_float_array() {
        assert_eq!(
            crossover_array(&[120.0, 800.5]),
            "< (float)120, (float)800.5 >"
        );
    }

    #[test]
    fn non_finite_meter_values_read_as_silence() {
        assert_eq!(finite_or_silence(f64::NEG_INFINITY), METER_SILENCE);
        assert_eq!(finite_or_silence(f64::NAN), METER_SILENCE);
        assert_eq!(finite_or_silence(-12.0), -12.0);
    }
}
