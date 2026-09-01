//! Live Audio Router block — the same routing model as `builtin.audiorouter`,
//! but the crosspoints can be changed on a running flow, and each crosspoint
//! carries a gain rather than an on/off flag.
//!
//! Built on the pattern the audio mixer block already uses for its aux sends
//! (`builtin.mixer`: `tee → volume → queue → audiomixer`). That matters for
//! three reasons that a matrix element cannot cover:
//!
//! * **Synchronisation.** `audiomixer` is a `GstAudioAggregator`: it aligns
//!   inputs on timestamps, fills gaps with silence and normalises differing
//!   buffer sizes. `interleave` is a `GstCollectPads` element that simply
//!   waits for one buffer on every pad, so a source that stops — or an input
//!   pad that is never connected — stalls the whole router.
//! * **Fades.** Only the standalone `volume` element samples its `volume`
//!   property per sample (`volume_transform_ip` in `gstvolume.c`), which is
//!   what `crate::gst::volume_ramp` relies on. `audiomixmatrix`' `matrix` is
//!   not controllable at all, and an `audiomixer` sink pad's `volume` is
//!   sampled once per output block — both step, and a step is a click.
//! * **Gain per crosspoint.** The coefficient is a `gdouble` on a real
//!   element, so a crosspoint is a level, not a checkbox.
//!
//! ```text
//! audio_in_I → identity_in_I → deinterleave_in_I → tee_iIcC ─┐
//!                                                            │  (one branch
//!   tee_iIcC → xp_iIcC_oOcD (volume) → xpq_iIcC_oOcD (queue) ─┤   per crosspoint)
//!                                                            │
//!   → mixer_O (audiomixer, sink pad places the mono on channel D)
//!   → caps_out_O → capssetter_out_O → queue_out_O → audio_out_O
//! ```
//!
//! Fan-out is one `tee` feeding several crosspoints; fan-in is several
//! crosspoints landing on the same mixer. An unrouted output channel simply
//! has no crosspoint at unity, so it is silent without needing a silence
//! source. Every crosspoint of the configured crossbar is built up front and
//! stays linked, so changing the routing never relinks anything — it only
//! writes gains.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::routing::{self, Crosspoint, RoutingGains};
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, info, warn};

/// Maximum number of input/output streams.
const MAX_STREAMS: usize = 8;
/// Maximum channels per stream.
const MAX_CHANNELS: usize = 64;

/// Upper bound on the crossbar. Every crosspoint is a `volume` + a `queue`
/// that exist for the lifetime of the flow, so the product of the total input
/// and output channel counts decides the element count. Configurations above
/// this are refused at build time rather than silently producing a pipeline
/// that will not start.
const MAX_CROSSPOINTS: usize = 1024;

/// Block definition id.
pub const BLOCK_ID: &str = "builtin.liveaudiorouter";

/// Name of the live routing property.
pub const ROUTING_MATRIX_PROPERTY: &str = "routing_matrix";

/// Name of the crosspoint fade-time property.
pub const FADE_MS_PROPERTY: &str = "crosspoint_fade_ms";

/// Default crosspoint fade. The same short anti-click ramp the mixer block
/// uses for its faders — long enough to mask the discontinuity, short enough
/// that a routing change still feels instant.
pub const DEFAULT_CROSSPOINT_FADE_MS: u32 = strom_types::mixer::DEFAULT_VOLUME_RAMP_MS;

/// Upper bound on the configurable fade. Beyond a second a "routing change"
/// stops being one.
pub const MAX_CROSSPOINT_FADE_MS: u32 = 5_000;

/// Read the crosspoint fade from a block's properties, falling back to the
/// default. Accepts any numeric `PropertyValue` since clients differ on
/// whether they send an integer or an unsigned.
pub fn fade_ms(properties: &HashMap<String, PropertyValue>) -> u32 {
    properties
        .get(FADE_MS_PROPERTY)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => u32::try_from(*u).ok(),
            PropertyValue::Int(i) if *i >= 0 => u32::try_from(*i).ok(),
            PropertyValue::Float(f) if *f >= 0.0 => Some(*f as u32),
            _ => None,
        })
        .unwrap_or(DEFAULT_CROSSPOINT_FADE_MS)
        .min(MAX_CROSSPOINT_FADE_MS)
}

/// Element id suffix that `routing_matrix` is mapped to in the block
/// definition. The routing spans every crosspoint element, but a live block
/// property has to name one real element to be routable at all (a `_block`
/// mapping is rejected as non-live), so the first output mixer acts as the
/// anchor and the live path fans the write out from there.
const ROUTING_ANCHOR_SUFFIX: &str = ":mixer_0";

/// Prefix of a crosspoint `volume` element id, after the instance id.
const CROSSPOINT_PREFIX: &str = ":xp_";

/// The `GstAudioConverter` option that maps a mixer sink pad's channels onto
/// the mixer's output channels.
const MIX_MATRIX_KEY: &str = "GstAudioConverter.mix-matrix";

/// Element naming for a crosspoint. The wire format itself lives in
/// `strom_types::routing`, shared with the graph editor.
trait CrosspointElement {
    fn volume_id(&self, instance_id: &str) -> String;
}

impl CrosspointElement for Crosspoint {
    /// Element id of the `volume` element carrying this crosspoint's gain.
    fn volume_id(&self, instance_id: &str) -> String {
        format!(
            "{}{}i{}c{}_o{}c{}",
            instance_id,
            CROSSPOINT_PREFIX,
            self.in_stream,
            self.in_channel,
            self.out_stream,
            self.out_channel
        )
    }
}

// ============================================================================
// Routing model
// ============================================================================

/// Parse a routing matrix, warning about anything unusable.
///
/// The format lives in `strom_types::routing` because the graph editor reads
/// and writes the same JSON; this wrapper only adds the logging.
fn parse_routing_gains(json: &str) -> RoutingGains {
    let (gains, skipped) = routing::parse_routing_gains(json);
    for key in skipped {
        warn!("Live Audio Router: unusable routing entry: {key}");
    }
    gains
}

/// The crosspoint an element id names, if it is a crosspoint of `instance_id`.
///
/// This is how the live path enumerates an instance's crosspoints: from the
/// running pipeline's element map rather than from a build-time registry, so
/// there is no global state to keep in step or leak.
pub fn crosspoint_of(instance_id: &str, element_id: &str) -> Option<Crosspoint> {
    let rest = element_id
        .strip_prefix(instance_id)?
        .strip_prefix(CROSSPOINT_PREFIX)?;
    let (input, output) = rest.split_once("_o")?;
    let (in_stream, in_channel) = routing::parse_routing_key(input, 'i')?;
    let (out_stream, out_channel) = routing::parse_routing_key(&format!("o{output}"), 'o')?;
    Some(Crosspoint::new(
        in_stream,
        in_channel,
        out_stream,
        out_channel,
    ))
}

/// The gain to write to each crosspoint element of `instance_id`.
///
/// This is the whole decision a live routing change makes, with no GStreamer
/// in it: the live path and the tests go through the same function, so they
/// cannot drift. Element ids that are not crosspoints of this instance are
/// ignored, and a crosspoint the routing does not mention resolves to 0.0 —
/// closing a crosspoint is as much part of a routing change as opening one.
pub fn crosspoint_targets<'a, I>(
    instance_id: &str,
    json: &str,
    element_ids: I,
) -> Vec<(&'a str, f64)>
where
    I: IntoIterator<Item = &'a str>,
{
    let gains = parse_routing_gains(json);
    element_ids
        .into_iter()
        .filter_map(|id| {
            let crosspoint = crosspoint_of(instance_id, id)?;
            Some((id, gains.get(&crosspoint).copied().unwrap_or(0.0)))
        })
        .collect()
}

/// The block instance a routing anchor element id belongs to.
pub fn instance_from_anchor(element_id: &str) -> Option<&str> {
    element_id.strip_suffix(ROUTING_ANCHOR_SUFFIX)
}

// ============================================================================
// Builder
// ============================================================================

/// Live Audio Router block builder.
pub struct LiveAudioRouterBuilder;

/// `audiomixer`'s own default output block size, in milliseconds. This is the
/// router's contribution to output latency and its aggregate cadence. It has
/// no bearing on how smooth a crosspoint fade is — that happens per sample in
/// the `volume` element — so it is purely a latency-versus-CPU knob.
const DEFAULT_OUTPUT_BUFFER_MS: u64 = 10;

impl BlockBuilder for LiveAudioRouterBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_inputs = parse_num_streams(properties, "num_inputs", 2);
        let num_outputs = parse_num_streams(properties, "num_outputs", 2);

        let inputs = (0..num_inputs)
            .map(|i| ExternalPad {
                label: Some(format!("A{i}")),
                name: format!("audio_in_{i}"),
                media_type: MediaType::Audio,
                internal_element_id: format!("identity_in_{i}"),
                internal_pad_name: "sink".to_string(),
            })
            .collect();

        let outputs = (0..num_outputs)
            .map(|i| ExternalPad {
                label: Some(format!("A{i}")),
                name: format!("audio_out_{i}"),
                media_type: MediaType::Audio,
                internal_element_id: format!("queue_out_{i}"),
                internal_pad_name: "src".to_string(),
            })
            .collect();

        Some(ExternalPads { inputs, outputs })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        let num_inputs = parse_num_streams(properties, "num_inputs", 2);
        let num_outputs = parse_num_streams(properties, "num_outputs", 2);
        let input_channels: Vec<usize> = (0..num_inputs)
            .map(|i| parse_channels(properties, &format!("input_{i}_channels"), 2))
            .collect();
        let output_channels: Vec<usize> = (0..num_outputs)
            .map(|i| parse_channels(properties, &format!("output_{i}_channels"), 2))
            .collect();

        let total_in: usize = input_channels.iter().sum();
        let total_out: usize = output_channels.iter().sum();
        let crosspoints = total_in * total_out;
        if crosspoints > MAX_CROSSPOINTS {
            return Err(BlockBuildError::InvalidConfiguration(format!(
                "Live Audio Router: {total_in} input channels x {total_out} output channels is \
                 {crosspoints} crosspoints, above the limit of {MAX_CROSSPOINTS}. Every \
                 crosspoint is a live gain stage, so reduce the channel counts."
            )));
        }

        info!(
            "Building LiveAudioRouter '{}': {} inputs ({} ch) -> {} outputs ({} ch), {} crosspoints",
            instance_id, num_inputs, total_in, num_outputs, total_out, crosspoints
        );

        let force_live = parse_bool(properties, "force_live", true);
        let latency_ms = parse_millis(
            properties,
            "latency",
            strom_types::mixer::DEFAULT_LATENCY_MS,
        );
        let min_upstream_latency_ms = parse_millis(
            properties,
            "min_upstream_latency",
            strom_types::mixer::DEFAULT_MIN_UPSTREAM_LATENCY_MS,
        );
        let output_buffer_ms = parse_millis(
            properties,
            "output_buffer_duration",
            DEFAULT_OUTPUT_BUFFER_MS,
        )
        .max(1);

        // A matrix that has never been set gets the straight-through default,
        // so a router that has just been dropped in passes audio. An empty
        // matrix is a decision and is honoured as written — closing every
        // crosspoint must survive a restart.
        let gains = match properties.get(ROUTING_MATRIX_PROPERTY) {
            Some(PropertyValue::String(json)) => parse_routing_gains(json),
            Some(other) => {
                warn!("Live Audio Router: routing_matrix is not a string: {other:?}");
                RoutingGains::new()
            }
            None => {
                let default = routing::default_routing(&input_channels, &output_channels);
                info!(
                    "LiveAudioRouter '{}': no routing configured, defaulting to {} straight-through crosspoints",
                    instance_id,
                    default.len()
                );
                default
            }
        };

        let mut elements: Vec<(String, gst::Element)> = Vec::new();
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

        // ------------------------------------------------------------------
        // Output buses: one aggregator per output stream. Built first so the
        // crosspoint loop below can request its sink pads.
        // ------------------------------------------------------------------
        let mut mixers: Vec<gst::Element> = Vec::with_capacity(num_outputs);
        for (out_idx, &channels) in output_channels.iter().enumerate() {
            let mixer_id = format!("{instance_id}:mixer_{out_idx}");
            let mixer = crate::blocks::builtin::mixer::make_audiomixer(
                &mixer_id,
                force_live,
                latency_ms,
                min_upstream_latency_ms,
            )?;
            mixer.set_property(
                "output-buffer-duration",
                output_buffer_ms * gst::ClockTime::MSECOND,
            );
            mixers.push(mixer.clone());
            elements.push((mixer_id.clone(), mixer));

            // The mixer's own output must be unpositioned: a sink pad's
            // mix-matrix is only applied when the converter is not asked to do
            // positional mapping as well.
            let caps_id = format!("{instance_id}:caps_out_{out_idx}");
            let caps = gst::ElementFactory::make("capsfilter")
                .name(&caps_id)
                .property(
                    "caps",
                    gst::Caps::builder("audio/x-raw")
                        .field("channels", channels as i32)
                        .field("channel-mask", gst::Bitmask::new(0))
                        .build(),
                )
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("caps_out: {e}")))?;
            elements.push((caps_id.clone(), caps));

            // Stamp the conventional mask back on so downstream sees
            // well-formed caps, the way `builtin.audiorouter` does.
            let setter_id = format!("{instance_id}:capssetter_out_{out_idx}");
            let setter = make_capssetter(&setter_id, channels)?;
            elements.push((setter_id.clone(), setter));

            let queue_id = format!("{instance_id}:queue_out_{out_idx}");
            let queue = gst::ElementFactory::make("queue")
                .name(&queue_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("queue_out: {e}")))?;
            elements.push((queue_id.clone(), queue));

            internal_links.push((
                ElementPadRef::pad(&mixer_id, "src"),
                ElementPadRef::pad(&caps_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&caps_id, "src"),
                ElementPadRef::pad(&setter_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&setter_id, "src"),
                ElementPadRef::pad(&queue_id, "sink"),
            ));
        }

        // ------------------------------------------------------------------
        // Inputs: split each stream into mono and tee every channel out to
        // the crosspoints that will consume it.
        // ------------------------------------------------------------------
        for (in_idx, &channels) in input_channels.iter().enumerate() {
            let identity_id = format!("{instance_id}:identity_in_{in_idx}");
            let identity = gst::ElementFactory::make("identity")
                .name(&identity_id)
                .property("silent", true)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {e}")))?;
            elements.push((identity_id.clone(), identity));

            let deint_id = format!("{instance_id}:deinterleave_in_{in_idx}");
            let deint = gst::ElementFactory::make("deinterleave")
                .name(&deint_id)
                .property("keep-positions", false)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("deinterleave: {e}")))?;

            internal_links.push((
                ElementPadRef::pad(&identity_id, "src"),
                ElementPadRef::pad(&deint_id, "sink"),
            ));

            // One tee per input channel. `allow-not-linked` keeps a tee from
            // erroring while its branches are still being set up, and covers
            // an output stream whose external pad nobody connected.
            for ch in 0..channels {
                let tee_id = format!("{instance_id}:tee_i{in_idx}c{ch}");
                let tee = gst::ElementFactory::make("tee")
                    .name(&tee_id)
                    .property("allow-not-linked", true)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("tee: {e}")))?;
                elements.push((tee_id, tee));
            }

            // `deinterleave` src pads appear once caps are known, so the
            // channel → tee link is made on pad-added. Only the tee names are
            // captured — a strong reference to the bin or its elements here
            // would keep the pipeline alive forever (see CLAUDE.md).
            let instance_for_cb = instance_id.to_string();
            deint.connect_pad_added(move |element, pad| {
                let Some(channel) = parse_channel_from_pad_name(&pad.name()) else {
                    warn!(
                        "Live Audio Router: unparsable deinterleave pad {}",
                        pad.name()
                    );
                    return;
                };
                if channel >= channels {
                    warn!(
                        "Live Audio Router: input {} produced channel {} but is configured for {}",
                        in_idx, channel, channels
                    );
                    return;
                }
                let Some(bin) = element.parent().and_then(|p| p.downcast::<gst::Bin>().ok()) else {
                    // Pipeline is being torn down.
                    return;
                };
                let tee_id = format!("{instance_for_cb}:tee_i{in_idx}c{channel}");
                let Some(tee) = bin.by_name(&tee_id) else {
                    warn!("Live Audio Router: {tee_id} is not in the pipeline");
                    return;
                };
                let Some(sink) = tee.static_pad("sink") else {
                    return;
                };
                if let Err(e) = pad.link(&sink) {
                    warn!(
                        "Live Audio Router: failed to link {} to {tee_id}: {e:?}",
                        pad.name()
                    );
                } else {
                    debug!("Live Audio Router: linked {} to {tee_id}", pad.name());
                }
            });

            elements.push((deint_id, deint));
        }

        // ------------------------------------------------------------------
        // The crossbar: every input channel gets a gain stage into every
        // output channel. Building the full crossbar up front is what makes
        // the routing live — a change only writes gains, never relinks.
        // ------------------------------------------------------------------
        for (in_idx, &in_ch_count) in input_channels.iter().enumerate() {
            for in_ch in 0..in_ch_count {
                let tee_id = format!("{instance_id}:tee_i{in_idx}c{in_ch}");

                for (out_idx, &out_ch_count) in output_channels.iter().enumerate() {
                    for out_ch in 0..out_ch_count {
                        let xp = Crosspoint {
                            in_stream: in_idx,
                            in_channel: in_ch,
                            out_stream: out_idx,
                            out_channel: out_ch,
                        };
                        let gain = gains.get(&xp).copied().unwrap_or(0.0);

                        let volume_id = xp.volume_id(instance_id);
                        let volume = gst::ElementFactory::make("volume")
                            .name(&volume_id)
                            .property("volume", gain)
                            .build()
                            .map_err(|e| {
                                BlockBuildError::ElementCreation(format!("crosspoint volume: {e}"))
                            })?;
                        elements.push((volume_id.clone(), volume));

                        // Request the mixer sink pad here so its mix-matrix can
                        // be set before the pipeline runs: the pad places this
                        // mono branch on output channel `out_ch` and nothing
                        // else. The gain lives in the volume element, so the
                        // matrix is pure placement.
                        let mixer = &mixers[out_idx];
                        let pad = mixer.request_pad_simple("sink_%u").ok_or_else(|| {
                            BlockBuildError::ElementCreation(format!(
                                "Live Audio Router: mixer_{out_idx} refused a sink pad"
                            ))
                        })?;
                        set_pad_placement(&pad, out_ch, out_ch_count);

                        internal_links.push((
                            ElementPadRef::element(&tee_id), // request a tee src pad
                            ElementPadRef::pad(&volume_id, "sink"),
                        ));
                        // Straight onto the mixer pad — no queue. A tee branch
                        // normally needs one so it cannot block its siblings,
                        // but every branch here ends on a `GstAggregatorPad`,
                        // which queues the buffer and returns rather than
                        // waiting for the mix. The bus is built with
                        // `force-live`, a `latency` timeout and
                        // `ignore-inactive-pads`, so it never waits
                        // indefinitely on a pad and always drains. A queue per
                        // crosspoint would be a thread per crosspoint for no
                        // decoupling that isn't already there; the per-bus
                        // `queue_out_O` below is where downstream is decoupled.
                        internal_links.push((
                            ElementPadRef::pad(&volume_id, "src"),
                            ElementPadRef::pad(
                                format!("{instance_id}:mixer_{out_idx}"),
                                pad.name().as_str(),
                            ),
                        ));
                    }
                }
            }
        }

        info!(
            "LiveAudioRouter '{}' built: {} crosspoints, {} open at build time",
            instance_id,
            crosspoints,
            gains.values().filter(|g| **g > 0.0).count()
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Point a mixer sink pad's mono input at one output channel.
///
/// `GstAudioConverter`'s mix-matrix is indexed rows = output channels,
/// columns = input channels — the transpose of `audiomixmatrix`' `matrix`.
fn set_pad_placement(pad: &gst::Pad, out_channel: usize, out_channels: usize) {
    let rows = (0..out_channels).map(|row| {
        let coeff: f32 = if row == out_channel { 1.0 } else { 0.0 };
        gst::Array::new([coeff.to_send_value()]).to_send_value()
    });
    let mut config = gst::Structure::new_empty("GstAudioConverter");
    config.set(MIX_MATRIX_KEY, gst::Array::new(rows));
    pad.set_property("converter-config", &config);
}

/// capssetter fixing the channel-mask the way `builtin.audiorouter` does:
/// 1ch = 0x1, 2ch = 0x3, 3+ch = 0x0 (unpositioned).
fn make_capssetter(id: &str, channels: usize) -> Result<gst::Element, BlockBuildError> {
    let channel_mask: u64 = match channels {
        1 => 0x1,
        2 => 0x3,
        _ => 0x0,
    };
    let caps = gst::Caps::builder("audio/x-raw")
        .field("channel-mask", gst::Bitmask::new(channel_mask))
        .build();
    gst::ElementFactory::make("capssetter")
        .name(id)
        .property("caps", &caps)
        .property("join", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capssetter: {e}")))
}

// ============================================================================
// Property parsing helpers
// ============================================================================

fn parse_num_streams(
    properties: &HashMap<String, PropertyValue>,
    key: &str,
    default: usize,
) -> usize {
    properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
            _ => None,
        })
        .unwrap_or(default)
        .clamp(1, MAX_STREAMS)
}

fn parse_channels(properties: &HashMap<String, PropertyValue>, key: &str, default: usize) -> usize {
    properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
            _ => None,
        })
        .unwrap_or(default)
        .clamp(1, MAX_CHANNELS)
}

fn parse_millis(properties: &HashMap<String, PropertyValue>, key: &str, default: u64) -> u64 {
    properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u),
            PropertyValue::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        })
        .unwrap_or(default)
}

fn parse_bool(properties: &HashMap<String, PropertyValue>, key: &str, default: bool) -> bool {
    match properties.get(key) {
        Some(PropertyValue::Bool(b)) => *b,
        _ => default,
    }
}

/// Parse channel index from a deinterleave pad name (e.g. `src_0` -> 0).
fn parse_channel_from_pad_name(pad_name: &str) -> Option<usize> {
    pad_name.strip_prefix("src_").and_then(|s| s.parse().ok())
}
/// Get Live Audio Router block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![liveaudiorouter_definition()]
}

fn liveaudiorouter_definition() -> BlockDefinition {
    let mut exposed_properties = vec![
        ExposedProperty {
            name: "num_inputs".to_string(),
            label: "Number of Inputs".to_string(),
            description: "Number of input audio streams (1-8)".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_inputs".to_string(),
                transform: None,
            },
            live: false,
            persist: None,
        },
        ExposedProperty {
            name: "num_outputs".to_string(),
            label: "Number of Outputs".to_string(),
            description: "Number of output audio streams (1-8)".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_outputs".to_string(),
                transform: None,
            },
            live: false,
            persist: None,
        },
    ];

    for i in 0..MAX_STREAMS {
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_channels", i),
            label: format!("Input {} Channels", i),
            description: format!("Number of channels for input stream {} (1-64)", i),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_channels", i),
                transform: None,
            },
            live: false,
            persist: None,
        });
    }

    for i in 0..MAX_STREAMS {
        exposed_properties.push(ExposedProperty {
            name: format!("output_{}_channels", i),
            label: format!("Output {} Channels", i),
            description: format!("Number of channels for output stream {} (1-64)", i),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("output_{}_channels", i),
                transform: None,
            },
            live: false,
            persist: None,
        });
    }

    // ------------------------------------------------------------------
    // Output bus settings. Same names, types and defaults as the audio mixer
    // block: both sum onto an `audiomixer`, so an operator who has tuned one
    // should not have to learn a second vocabulary for the other.
    // ------------------------------------------------------------------
    exposed_properties.push(ExposedProperty {
        name: "force_live".to_string(),
        label: "Force Live".to_string(),
        description: "Always operate in live mode. Keeps the output buses producing when not every input is connected. Construction-time only.".to_string(),
        property_type: PropertyType::Bool,
        default_value: Some(PropertyValue::Bool(true)),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "force_live".to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    });
    exposed_properties.push(ExposedProperty {
        name: "latency".to_string(),
        label: "Latency".to_string(),
        description: "How long an output bus waits for a slower input before producing output, in milliseconds. This is also how long a source that stops can hold up the bus. Construction-time only.".to_string(),
        property_type: PropertyType::UInt,
        default_value: Some(PropertyValue::UInt(strom_types::mixer::DEFAULT_LATENCY_MS)),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "latency".to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    });
    exposed_properties.push(ExposedProperty {
        name: "min_upstream_latency".to_string(),
        label: "Min Upstream Latency".to_string(),
        description: "Minimum upstream latency reported to upstream elements in milliseconds. Construction-time only.".to_string(),
        property_type: PropertyType::UInt,
        default_value: Some(PropertyValue::UInt(
            strom_types::mixer::DEFAULT_MIN_UPSTREAM_LATENCY_MS,
        )),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "min_upstream_latency".to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    });
    exposed_properties.push(ExposedProperty {
        name: "output_buffer_duration".to_string(),
        label: "Output Block Size (ms)".to_string(),
        description: "How much audio each output bus emits per buffer. This is the router's own contribution to output latency; lower costs more CPU. It does not affect how smooth a crosspoint fade is — that is done per sample. Construction-time only.".to_string(),
        property_type: PropertyType::UInt,
        default_value: Some(PropertyValue::UInt(DEFAULT_OUTPUT_BUFFER_MS)),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "output_buffer_duration".to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    });

    exposed_properties.push(ExposedProperty {
        name: FADE_MS_PROPERTY.to_string(),
        label: "Crosspoint Fade (ms)".to_string(),
        description: "How long a crosspoint takes to fade when the routing changes. The fade is applied per sample, so it removes the click a hard switch makes. 0 switches instantly.".to_string(),
        property_type: PropertyType::UInt,
        default_value: Some(PropertyValue::UInt(DEFAULT_CROSSPOINT_FADE_MS as u64)),
        // No element of its own — it is read when a routing change is applied.
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: FADE_MS_PROPERTY.to_string(),
            transform: None,
        },
        live: true,
        persist: None,
    });

    exposed_properties.push(ExposedProperty {
        name: ROUTING_MATRIX_PROPERTY.to_string(),
        label: "Routing Matrix".to_string(),
        description: "JSON routing matrix, applied to a running flow with a short per-sample fade on every crosspoint that changes. iXcY = input X channel Y, oXcY = output X channel Y. Either {\"i0c0\": [\"o0c0\", \"o1c0\"]} to open crosspoints at unity, or {\"i0c0\": {\"o0c0\": 1.0, \"o1c0\": 0.35}} to set a gain per crosspoint. A crosspoint that is not listed is closed.".to_string(),
        property_type: PropertyType::Multiline,
        default_value: Some(PropertyValue::String("{}".to_string())),
        // The routing spans every crosspoint element; the first output mixer
        // is the anchor the live path fans out from (see ROUTING_ANCHOR_SUFFIX).
        mapping: PropertyMapping {
            element_id: "mixer_0".to_string(),
            property_name: ROUTING_MATRIX_PROPERTY.to_string(),
            transform: None,
        },
        live: true,
        persist: None,
    });

    BlockDefinition {
        id: "builtin.liveaudiorouter".to_string(),
        name: "Live Audio Router".to_string(),
        description: "Route audio channels between multiple input and output streams. Every crosspoint is a gain, not just on/off, and both the routing and the gains can be changed on a running flow — each change fades per sample rather than stepping. Inputs are summed on synchronising audio buses, so streams with different buffering stay aligned and a source that drops out does not stall the router.".to_string(),
        category: "Audio".to_string(),
        exposed_properties,
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_in_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "identity_in_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A1".to_string()),
                    name: "audio_in_1".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "identity_in_1".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_out_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "queue_out_0".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    label: Some("A1".to_string()),
                    name: "audio_out_1".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "queue_out_1".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🔀".to_string()),
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}

// ============================================================================
// Unit tests for the routing model
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn xp(i: usize, c: usize, o: usize, d: usize) -> Crosspoint {
        Crosspoint::new(i, c, o, d)
    }

    #[test]
    fn a_crosspoint_element_id_round_trips() {
        let point = xp(2, 3, 1, 7);
        let id = point.volume_id("router");
        assert_eq!(id, "router:xp_i2c3_o1c7");
        assert_eq!(crosspoint_of("router", &id), Some(point));
    }

    #[test]
    fn crosspoint_of_ignores_other_elements_and_other_instances() {
        assert_eq!(crosspoint_of("router", "router:mixer_0"), None);
        assert_eq!(crosspoint_of("router", "other:xp_i0c0_o0c0"), None);
    }

    #[test]
    fn the_routing_anchor_names_its_instance() {
        assert_eq!(instance_from_anchor("router:mixer_0"), Some("router"));
        assert_eq!(instance_from_anchor("router:mixer_1"), None);
        assert_eq!(instance_from_anchor("router:xp_i0c0_o0c0"), None);
    }

    #[test]
    fn an_unlisted_crosspoint_resolves_to_closed() {
        let ids = [
            "router:xp_i0c0_o0c0",
            "router:xp_i0c0_o0c1",
            "router:mixer_0",
        ];
        let targets = crosspoint_targets("router", r#"{"i0c0":["o0c0"]}"#, ids);
        assert_eq!(targets.len(), 2, "only the crosspoints: {targets:?}");
        assert_eq!(
            targets
                .iter()
                .find(|(id, _)| id.ends_with("o0c0"))
                .unwrap()
                .1,
            1.0
        );
        assert_eq!(
            targets
                .iter()
                .find(|(id, _)| id.ends_with("o0c1"))
                .unwrap()
                .1,
            0.0,
            "a crosspoint the routing does not mention must be closed"
        );
    }
}
