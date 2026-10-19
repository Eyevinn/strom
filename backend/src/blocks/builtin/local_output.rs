//! Local Output block: plays audio on a local output device through the OS's
//! native audio API.
//!
//! The device is picked from the `Audio/Sink` devices the app-lifetime
//! `DeviceMonitor` lists (PulseAudio/PipeWire on Linux, CoreAudio on macOS,
//! WASAPI and ASIO on Windows), and the sink is created from that `GstDevice`
//! with `Device::create_element()`. That fills in whatever the platform plugin
//! needs to address the device, such as the ASIO driver's `device-clsid`. With
//! no device picked the block plays through `autoaudiosink`, the OS default.
//!
//! The target takes raw audio only. Raw audio in any format or rate is adapted
//! by `audioconvert ! audioresample` before the sink. Encoded audio is refused
//! with an error that says what arrived.
//!
//! `audio_channels` sets how many channels the device gets, 2 by default. A
//! plain count of 3 or more is that many independent outputs: input channel N
//! goes to device output N, and outputs with no input channel stay silent. A
//! surround choice (5.1, 7.1, ...) is a speaker layout instead. See
//! [`plan_output`] for how the two meet what arrives and what the device takes.

use super::refusal::refuse_input;
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    block::*, element::ElementPadRef, parse_audio_channel_config, AudioChannelConfig, MediaType,
    PropertyValue,
};
use tracing::info;

const BLOCK_NAME: &str = "Local Output";

/// Channels the device gets unless `audio_channels` says otherwise.
const DEFAULT_CHANNELS: &str = "2";

/// Widest channel count GStreamer's channel mixer takes (see Local Input).
const MAX_AUDIO_CHANNELS: u32 = 64;

/// Local Output block builder.
pub struct LocalOutputBuilder;

impl BlockBuilder for LocalOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building Local Output block: {}", instance_id);

        let device_id = match properties.get("audio_device") {
            Some(PropertyValue::String(s)) => s.trim().to_string(),
            _ => String::new(),
        };
        let want = read_channels(properties)?;

        let input_id = format!("{}:input", instance_id);
        let convert_id = format!("{}:audioconvert", instance_id);
        let resample_id = format!("{}:audioresample", instance_id);
        let outcaps_id = format!("{}:outcaps", instance_id);
        let sink_id = format!("{}:sink", instance_id);

        let input = make("identity", &input_id)?;
        let convert = make("audioconvert", &convert_id)?;
        let resample = make("audioresample", &resample_id)?;
        // Open until the input's caps arrive: the channels the device gets
        // are decided then, from what arrives and what the device takes.
        let outcaps = make("capsfilter", &outcaps_id)?;
        outcaps.set_property("caps", gst::Caps::new_empty_simple("audio/x-raw"));
        let sink = make_sink(ctx, &device_id, &sink_id)?;
        // An ASIO driver plays only the outputs it is told to open.
        if sink.find_property("output-channels").is_some() {
            sink.set_property("output-channels", first_channels(want.channels));
        }
        adapt_input(instance_id, want, &input, &convert, &outcaps)?;

        let internal_links = vec![
            (
                ElementPadRef::pad(&input_id, "src"),
                ElementPadRef::pad(&convert_id, "sink"),
            ),
            (
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&resample_id, "sink"),
            ),
            (
                ElementPadRef::pad(&resample_id, "src"),
                ElementPadRef::pad(&outcaps_id, "sink"),
            ),
            (
                ElementPadRef::pad(&outcaps_id, "src"),
                ElementPadRef::pad(&sink_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (input_id, input),
                (convert_id, convert),
                (resample_id, resample),
                (outcaps_id, outcaps),
                (sink_id, sink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// `audio_channels`, 2 when unset.
fn read_channels(
    properties: &HashMap<String, PropertyValue>,
) -> Result<AudioChannelConfig, BlockBuildError> {
    let value = match properties.get("audio_channels") {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(PropertyValue::UInt(n)) => n.to_string(),
        Some(PropertyValue::Int(n)) => n.to_string(),
        _ => DEFAULT_CHANNELS.to_string(),
    };
    let config = parse_audio_channel_config(&value).ok_or_else(|| {
        BlockBuildError::InvalidProperty(format!(
            "audio_channels '{}' is not a channel count or a count with a layout (6:0x3f)",
            value
        ))
    })?;
    if config.channels > MAX_AUDIO_CHANNELS {
        return Err(BlockBuildError::InvalidConfiguration(format!(
            "audio_channels {} is more than the {} channels GStreamer can convert",
            config.channels, MAX_AUDIO_CHANNELS
        )));
    }
    Ok(config)
}

/// `"0,1,...,n-1"`: an ASIO driver's first `n` outputs.
fn first_channels(n: u32) -> String {
    (0..n).map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

fn make(factory: &str, name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))
}

/// The sink for `device_id`, or `autoaudiosink` when it is empty.
///
/// The device comes from the app-lifetime `DeviceDiscovery`, as Local Input's
/// sources do, not from a transient `DeviceMonitor`: tearing one of those down
/// right after enumeration crashed inside the macOS providers.
fn make_sink(
    ctx: &BlockBuildContext,
    device_id: &str,
    element_id: &str,
) -> Result<gst::Element, BlockBuildError> {
    if device_id.is_empty() {
        return make("autoaudiosink", element_id);
    }

    let device = ctx.local_device(device_id).ok_or_else(|| {
        BlockBuildError::InvalidConfiguration(format!(
            "Audio/Sink device '{}' not found. Refresh /api/discovery/devices and pick a current id",
            device_id
        ))
    })?;
    if device.device_class() != "Audio/Sink" {
        return Err(BlockBuildError::InvalidConfiguration(format!(
            "Device '{}' ({}) is a {} device, not an audio output",
            device.display_name(),
            device_id,
            device.device_class()
        )));
    }

    let element = device.create_element(Some(element_id)).map_err(|e| {
        BlockBuildError::ElementCreation(format!(
            "create_element for Audio/Sink device '{}' ({}): {}",
            device.display_name(),
            device_id,
            e
        ))
    })?;

    info!(
        "Local Output: bound to device '{}' (id={}, factory={})",
        device.display_name(),
        device_id,
        element
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    );

    Ok(element)
}

/// Raw audio caps with `channels`, and `mask` as the channel-mask when given.
fn output_caps(channels: u32, mask: Option<u64>) -> gst::Caps {
    let mut caps = gst::Caps::builder("audio/x-raw").field("channels", channels as i32);
    if let Some(mask) = mask {
        caps = caps.field("channel-mask", gst::Bitmask::new(mask));
    }
    caps.build()
}

/// What the block hands the device.
#[derive(Debug, Clone, PartialEq)]
struct OutputPlan {
    channels: u32,
    /// The output's channel-mask: 0 for independent outputs, `None` for mono.
    mask: Option<u64>,
    /// `audioconvert`'s mix-matrix (rows = outputs, columns = input channels),
    /// or `None` to let it mix by speaker position.
    matrix: Option<Vec<Vec<f32>>>,
}

/// Decide how `in_channels` arriving with `in_mask` reach a device that
/// accepts `device`, given the `want`ed output.
///
/// The output is `want.channels` channels:
/// - 1 is mono and 2 is stereo, and a surround choice is its speaker layout.
///   Audio that has positions (mono, stereo, a surround layout) is mixed into
///   them by position, so mono plays on both sides and 5.1 folds down to
///   stereo.
/// - A plain count of 3 or more is that many independent outputs. They are
///   unpositioned when the device takes that (ASIO, and ALSA from 9 channels
///   on); otherwise they carry the layout the device offers for that count.
///
/// Anything else (unpositioned input, or independent outputs) goes channel for
/// channel: input N to output N, extra inputs dropped, missing ones silent.
fn plan_output(
    in_channels: u32,
    in_mask: Option<u64>,
    want: AudioChannelConfig,
    device: &gst::Caps,
) -> Result<OutputPlan, String> {
    let n = want.channels;
    let positioned_in = in_channels <= 2 || in_mask.is_some_and(|m| m != 0);
    let (mask, positioned_out) = match (n, want.channel_mask) {
        (1, _) => (None, true),
        (2, _) => (Some(0x3), true),
        (_, Some(m)) if m != 0 => (Some(m), true),
        _ => {
            let mask = if device.can_intersect(&output_caps(n, Some(0))) {
                0
            } else {
                device_layout(device, n).ok_or_else(|| device_refusal(n, device))?
            };
            (Some(mask), false)
        }
    };
    if !device.can_intersect(&output_caps(n, mask)) {
        return Err(device_refusal(n, device));
    }
    let matrix = (!(positioned_in && positioned_out)).then(|| {
        (0..n as usize)
            .map(|out| {
                (0..in_channels as usize)
                    .map(|i| if i == out { 1.0 } else { 0.0 })
                    .collect()
            })
            .collect()
    });
    Ok(OutputPlan {
        channels: n,
        mask,
        matrix,
    })
}

/// The fixed channel-mask `device` offers for `channels` channels, if any.
fn device_layout(device: &gst::Caps, channels: u32) -> Option<u64> {
    device.iter().find_map(|s| {
        let mask = s.get::<gst::Bitmask>("channel-mask").ok()?.0;
        (mask.count_ones() == channels).then_some(mask)
    })
}

fn device_refusal(channels: u32, device: &gst::Caps) -> String {
    let offers: Vec<String> = device
        .iter()
        .filter_map(|s| {
            let count = s.value("channels").ok()?.serialize().ok()?.to_string();
            Some(match s.get::<gst::Bitmask>("channel-mask") {
                Ok(m) => format!("{} (channel-mask 0x{:x})", count, m.0),
                Err(_) => count,
            })
        })
        .collect();
    format!(
        "{} is set to {} channels, but its output device does not take that many. It \
         takes: {}. Change audio_channels",
        BLOCK_NAME,
        channels,
        if offers.is_empty() {
            "nothing it reports".to_string()
        } else {
            offers.join(", ")
        }
    )
}

fn matrix_value(matrix: &[Vec<f32>]) -> gst::Array {
    gst::Array::new(
        matrix
            .iter()
            .map(|row| gst::Array::new(row.iter().map(|c| c.to_send_value())).to_send_value()),
    )
}

/// Decide, from the Caps event at the block's input `identity`, how the
/// stream reaches the device ([`plan_output`]), and set `audioconvert`'s
/// mix-matrix and the `outcaps` filter to match.
///
/// Encoded audio is refused here, before the identity checks the caps against
/// `audioconvert`, which only takes raw audio. A device that cannot take the
/// wanted channels refuses the stream the same way. Without this, both fail
/// with `not-negotiated` and the flow shows only `Internal data stream error`.
///
/// The input answers upstream's caps queries itself, accepting anything. Left
/// to the identity, they would reach `audioconvert`, which answers from the
/// device's caps before it has a mix-matrix: unpositioned channels can then
/// fail the query, upstream never sends caps, and nothing above is decided.
fn adapt_input(
    instance_id: &str,
    want: AudioChannelConfig,
    input: &gst::Element,
    convert: &gst::Element,
    outcaps: &gst::Element,
) -> Result<(), BlockBuildError> {
    let sink_pad = input
        .static_pad("sink")
        .ok_or_else(|| BlockBuildError::ElementCreation("identity has no sink pad".into()))?;
    sink_pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_pad, info| {
        let Some(gst::PadProbeData::Query(query)) = info.data.as_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let caps = q
                    .filter()
                    .map(|f| f.to_owned())
                    .unwrap_or_else(gst::Caps::new_any);
                q.set_result(&caps);
                gst::PadProbeReturn::Handled
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                q.set_result(true);
                gst::PadProbeReturn::Handled
            }
            _ => gst::PadProbeReturn::Ok,
        }
    });

    let convert_weak = convert.downgrade();
    let outcaps_weak = outcaps.downgrade();
    let instance_id = instance_id.to_string();
    sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(s) = caps.caps().structure(0) else {
            return gst::PadProbeReturn::Ok;
        };
        if s.name() != "audio/x-raw" {
            if let Some(input) = pad.parent_element() {
                refuse_input(&input, &encoded_audio_refusal(s.name()));
            }
            // The identity never sees caps it cannot pass on, so it raises no
            // not-negotiated error of its own after ours.
            return gst::PadProbeReturn::Drop;
        }
        let (Some(convert), Some(outcaps)) = (convert_weak.upgrade(), outcaps_weak.upgrade())
        else {
            return gst::PadProbeReturn::Ok;
        };
        let in_channels = s.get::<i32>("channels").unwrap_or(0).max(0) as u32;
        let in_mask = s.get::<gst::Bitmask>("channel-mask").ok().map(|m| m.0);
        // What the device takes: asked of whatever the filter feeds.
        let device = outcaps
            .static_pad("src")
            .map(|p| p.peer_query_caps(None))
            .unwrap_or_else(gst::Caps::new_any);
        match plan_output(in_channels, in_mask, want, &device) {
            Ok(plan) => {
                info!(
                    "Local Output {}: {} input channels play as {} output channels \
                     (channel-mask {:?}, {})",
                    instance_id,
                    in_channels,
                    plan.channels,
                    plan.mask,
                    if plan.matrix.is_some() {
                        "channel for channel"
                    } else {
                        "mixed by speaker position"
                    }
                );
                let matrix = plan.matrix.as_deref().unwrap_or(&[]);
                convert.set_property("mix-matrix", matrix_value(matrix));
                outcaps.set_property("caps", output_caps(plan.channels, plan.mask));
                gst::PadProbeReturn::Ok
            }
            Err(reason) => {
                if let Some(input) = pad.parent_element() {
                    refuse_input(&input, &reason);
                }
                gst::PadProbeReturn::Drop
            }
        }
    });
    Ok(())
}

fn encoded_audio_refusal(caps_name: &str) -> String {
    format!(
        "{} plays raw audio, but its audio input carries {}. Link a raw audio pad to it, \
         not an encoder's encoded_out",
        BLOCK_NAME, caps_name
    )
}

pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![local_output_definition()]
}

fn local_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.local_output".to_string(),
        name: BLOCK_NAME.to_string(),
        description: "Plays audio on a local output device through the OS's native audio API (PulseAudio/PipeWire on Linux, CoreAudio on macOS, WASAPI or ASIO on Windows). Pick the device from the live list, or leave it empty for the OS default (autoaudiosink). Takes raw audio in any format, rate or channel count and converts it for the device.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![ExposedProperty {
            name: "audio_device".to_string(),
            label: "Audio Output".to_string(),
            description: "Local Audio/Sink device to play to (built-in outputs, headphones, professional audio interfaces, ASIO drivers, virtual devices, ...). Picked from the live device list. Leave empty to use the OS default (autoaudiosink).".to_string(),
            property_type: PropertyType::Device {
                category: strom_types::discovery::DeviceCategory::AudioSink,
            },
            default_value: Some(PropertyValue::String(String::new())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "audio_device".to_string(),
                transform: None,
            },
            live: false,
            persist: None,
        },
        ExposedProperty {
            name: "audio_channels".to_string(),
            label: "Audio Channels".to_string(),
            description: "How many channels the device gets. A plain count of 3 or more is that many independent outputs: input channel N plays on output N, and outputs with no input channel stay silent. On an ASIO device this opens outputs 1..N. The Surround choices are a speaker layout instead: positioned input is mixed into it. Mono and stereo input fill 1 and 2 the same way. The flow fails with what the device takes if it cannot take this many.".to_string(),
            property_type: PropertyType::Enum {
                values: audio_channel_enum_values(false),
            },
            default_value: Some(PropertyValue::String(DEFAULT_CHANNELS.to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "audio_channels".to_string(),
                transform: None,
            },
            live: false,
            persist: None,
        }],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "input".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🔊".to_string()),
            width: Some(2.0),
            height: Some(1.0),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        gst::init().unwrap();
        gst::Caps::from_str(s).expect("caps")
    }

    /// What `osxaudiosink` offers for built-in speakers.
    fn speakers() -> gst::Caps {
        caps("audio/x-raw, channels=2, channel-mask=(bitmask)0x3; audio/x-raw, channels=1")
    }

    /// An ASIO driver: any count up to its outputs, no layout.
    fn asio(outputs: i32) -> gst::Caps {
        caps(&format!("audio/x-raw, channels=(int)[ 1, {} ]", outputs))
    }

    /// What `alsasink` offers up to 8 channels: fixed layouts, 5 and 7 absent.
    fn alsa() -> gst::Caps {
        caps(
            "audio/x-raw, channels=2, channel-mask=(bitmask)0x3; audio/x-raw, channels=1; \
             audio/x-raw, channels=3, channel-mask=(bitmask)0xb; \
             audio/x-raw, channels=4, channel-mask=(bitmask)0x33; \
             audio/x-raw, channels=6, channel-mask=(bitmask)0x3f; \
             audio/x-raw, channels=8, channel-mask=(bitmask)0xc3f",
        )
    }

    fn want(value: &str) -> AudioChannelConfig {
        parse_audio_channel_config(value).expect("channel config")
    }

    fn one_to_one(outputs: usize, inputs: usize) -> Option<Vec<Vec<f32>>> {
        Some(
            (0..outputs)
                .map(|o| {
                    (0..inputs)
                        .map(|i| if i == o { 1.0 } else { 0.0 })
                        .collect()
                })
                .collect(),
        )
    }

    #[test]
    fn by_default_unpositioned_channels_play_1_and_2_in_stereo() {
        let plan = plan_output(6, Some(0), want("2"), &speakers()).unwrap();
        assert_eq!(plan.channels, 2);
        assert_eq!(plan.mask, Some(0x3));
        assert_eq!(plan.matrix, one_to_one(2, 6));
    }

    #[test]
    fn positioned_audio_is_mixed_by_position() {
        let stereo = plan_output(2, Some(0x3), want("2"), &speakers()).unwrap();
        assert_eq!(stereo.matrix, None);
        let mono = plan_output(1, None, want("2"), &speakers()).unwrap();
        assert_eq!(mono.matrix, None, "mono plays on both sides");
        let surround = plan_output(6, Some(0x3f), want("2"), &speakers()).unwrap();
        assert_eq!(surround.matrix, None, "5.1 folds down to stereo");
    }

    #[test]
    fn eight_outputs_on_asio_are_unpositioned_and_channel_for_channel() {
        let plan = plan_output(6, Some(0), want("8"), &asio(16)).unwrap();
        assert_eq!(plan.channels, 8);
        assert_eq!(plan.mask, Some(0), "independent outputs");
        assert_eq!(plan.matrix, one_to_one(8, 6), "outputs 7 and 8 stay silent");
    }

    #[test]
    fn stereo_into_eight_outputs_fills_outputs_1_and_2() {
        let plan = plan_output(2, Some(0x3), want("8"), &asio(8)).unwrap();
        assert_eq!(plan.matrix, one_to_one(8, 2));
    }

    #[test]
    fn eight_outputs_on_a_device_with_only_a_layout_take_that_layout() {
        let plan = plan_output(8, Some(0), want("8"), &alsa()).unwrap();
        assert_eq!(plan.mask, Some(0xc3f));
        assert_eq!(plan.matrix, one_to_one(8, 8));
    }

    #[test]
    fn a_surround_choice_mixes_positioned_audio_into_it() {
        let plan = plan_output(2, Some(0x3), want("6:0x3f"), &alsa()).unwrap();
        assert_eq!((plan.channels, plan.mask), (6, Some(0x3f)));
        assert_eq!(plan.matrix, None);
    }

    #[test]
    fn a_device_that_cannot_take_the_count_refuses_and_says_what_it_takes() {
        let reason = plan_output(6, Some(0), want("8"), &speakers()).unwrap_err();
        assert!(reason.contains("8 channels"), "{}", reason);
        assert!(reason.contains("audio_channels"), "{}", reason);
        assert!(reason.contains("2 (channel-mask 0x3)"), "{}", reason);
    }

    #[test]
    fn audio_channels_defaults_to_stereo() {
        let config = read_channels(&HashMap::new()).unwrap();
        assert_eq!((config.channels, config.channel_mask), (2, None));
        let def = local_output_definition();
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == "audio_channels")
            .expect("audio_channels property");
        assert!(matches!(
            prop.default_value,
            Some(PropertyValue::String(ref v)) if v == DEFAULT_CHANNELS
        ));
    }

    #[test]
    fn asio_opens_the_first_outputs() {
        assert_eq!(first_channels(1), "0");
        assert_eq!(first_channels(4), "0,1,2,3");
    }

    #[test]
    fn the_refusal_names_what_arrived() {
        let reason = encoded_audio_refusal("audio/x-opus");
        assert!(reason.contains("audio/x-opus"), "{}", reason);
        assert!(reason.contains("raw audio"), "{}", reason);
    }

    #[test]
    fn the_device_picker_lists_audio_outputs() {
        let def = local_output_definition();
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == "audio_device")
            .expect("audio_device property");
        assert!(matches!(
            prop.property_type,
            PropertyType::Device {
                category: strom_types::discovery::DeviceCategory::AudioSink
            }
        ));
    }
}
