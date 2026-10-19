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
//! The target takes raw audio only. Raw audio in any format, rate or channel
//! count is adapted by `audioconvert ! audioresample` before the sink. Encoded
//! audio is refused with an error that says what arrived.

use super::refusal::refuse_input;
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, MediaType, PropertyValue};
use tracing::info;

const BLOCK_NAME: &str = "Local Output";

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

        let input_id = format!("{}:input", instance_id);
        let convert_id = format!("{}:audioconvert", instance_id);
        let resample_id = format!("{}:audioresample", instance_id);
        let sink_id = format!("{}:sink", instance_id);

        let input = make("identity", &input_id)?;
        let convert = make("audioconvert", &convert_id)?;
        adapt_input(instance_id, &input, &convert)?;
        let resample = make("audioresample", &resample_id)?;
        let sink = make_sink(ctx, &device_id, &sink_id)?;

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
                ElementPadRef::pad(&sink_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (input_id, input),
                (convert_id, convert),
                (resample_id, resample),
                (sink_id, sink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
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

/// Decide, from the Caps event at the block's input `identity`, how the
/// stream reaches the device.
///
/// Encoded audio is refused here, before the identity checks the caps against
/// `audioconvert`, which only takes raw audio. Without this, encoded audio
/// fails with `not-negotiated` and the flow shows only `Internal data stream
/// error`.
///
/// Three or more unpositioned channels (Strom's form for N independent
/// channels) have no layout `audioconvert` can mix down by itself, so they
/// would not negotiate with a stereo device either. They play channel 1 left
/// and channel 2 right; the rest are left out.
///
/// The input answers upstream's caps queries itself, accepting anything. Left
/// to the identity, they would reach `audioconvert`, which answers from the
/// device's caps before it has a mix-matrix: unpositioned channels then fail
/// the query, upstream never sends caps, and neither decision above is made.
fn adapt_input(
    instance_id: &str,
    input: &gst::Element,
    convert: &gst::Element,
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
        let Some(convert) = convert_weak.upgrade() else {
            return gst::PadProbeReturn::Ok;
        };
        let channels = s.get::<i32>("channels").unwrap_or(0).max(0) as usize;
        let unpositioned = s
            .get::<gst::Bitmask>("channel-mask")
            .map(|m| m.0 == 0)
            .unwrap_or(true);
        if channels > 2 && unpositioned {
            info!(
                "Local Output {}: {} unpositioned channels arrive; playing channel 1 left \
                 and channel 2 right",
                instance_id, channels
            );
            convert.set_property("mix-matrix", first_two_channels_to_stereo(channels));
        } else {
            convert.set_property("mix-matrix", gst::Array::new(Vec::<glib::SendValue>::new()));
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

/// A `mix-matrix` (rows = output channels, columns = input channels) that
/// takes input channel 1 to the left output and channel 2 to the right.
fn first_two_channels_to_stereo(in_channels: usize) -> gst::Array {
    let row = |out: usize| {
        gst::Array::new((0..in_channels).map(|i| {
            let coeff: f32 = if i == out { 1.0 } else { 0.0 };
            coeff.to_send_value()
        }))
        .to_send_value()
    };
    gst::Array::new([row(0), row(1)])
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
