//! Audio analyzer block providing waveform (oscilloscope) and vectorscope (Lissajous) data.
//!
//! Uses a tee to split audio: one branch passes through unchanged, the other feeds
//! an appsink for sample-level analysis. The appsink receives S16LE stereo at 48kHz,
//! accumulates samples, and periodically computes min/max waveform columns and
//! decimated vectorscope pairs, broadcasting them as StromEvent::AudioAnalyzerData.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use base64::{engine::general_purpose::STANDARD, Engine};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom_types::element::ElementPadRef;
use strom_types::{block::*, EnumValue, FlowId, MediaType, PropertyValue, StromEvent};
use tracing::{debug, trace};

/// Audio Analyzer block builder.
pub struct AudioAnalyzerBuilder;

/// Internal state for accumulating audio samples between frames.
struct AudioAnalyzerState {
    samples_l: Vec<i16>,
    samples_r: Vec<i16>,
    last_frame_time: Instant,
    num_columns: usize,
    num_pairs: usize,
    update_interval: Duration,
}

impl BlockBuilder for AudioAnalyzerBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building AudioAnalyzer block instance: {}", instance_id);

        // Parse properties
        let update_rate: u32 = properties
            .get("update_rate")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u32),
                PropertyValue::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(30);

        let num_columns: usize = properties
            .get("waveform_columns")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as usize),
                PropertyValue::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(400);

        let num_pairs: usize = properties
            .get("vector_pairs")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as usize),
                PropertyValue::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(512);

        let update_interval = Duration::from_micros(1_000_000 / update_rate as u64);

        debug!(
            "AudioAnalyzer properties: update_rate={} Hz, columns={}, pairs={}",
            update_rate, num_columns, num_pairs
        );

        // Create elements
        let tee_id = format!("{}:tee", instance_id);
        let queue_id = format!("{}:queue", instance_id);
        let resample_id = format!("{}:audioresample", instance_id);
        let convert_id = format!("{}:audioconvert", instance_id);
        let appsink_id = format!("{}:appsink", instance_id);

        let tee = gst::ElementFactory::make("tee")
            .name(&tee_id)
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("tee: {}", e)))?;

        let queue = gst::ElementFactory::make("queue")
            .name(&queue_id)
            .property("max-size-buffers", 1u32)
            .property("max-size-time", 0u64)
            .property("max-size-bytes", 0u32)
            .property_from_str("leaky", "downstream")
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("queue: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&resample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&convert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("channels", 2i32)
            .field("layout", "interleaved")
            .field("rate", 48000i32)
            .build();

        let appsink = gst_app::AppSink::builder()
            .name(&appsink_id)
            .caps(&caps)
            .max_buffers(1)
            .drop(true)
            .sync(false)
            .build();

        // Set up appsink callback
        let state = Arc::new(Mutex::new(AudioAnalyzerState {
            samples_l: Vec::with_capacity(48000),
            samples_r: Vec::with_capacity(48000),
            last_frame_time: Instant::now(),
            num_columns,
            num_pairs,
            update_interval,
        }));

        let callback_state = Arc::clone(&state);
        let callback_instance_id = instance_id.to_string();
        // EventBroadcaster will be injected via bus_message_handler;
        // we use an Arc<Mutex<Option<...>>> to pass it to the appsink callback.
        let broadcaster: Arc<Mutex<Option<(FlowId, EventBroadcaster)>>> =
            Arc::new(Mutex::new(None));
        let callback_broadcaster = Arc::clone(&broadcaster);

        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let data = map.as_slice();

                    // S16LE stereo: 4 bytes per frame (2 bytes L + 2 bytes R)
                    let frame_count = data.len() / 4;
                    if frame_count == 0 {
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    let mut state = callback_state.lock().unwrap();

                    // Deinterleave S16LE stereo into L/R
                    for i in 0..frame_count {
                        let offset = i * 4;
                        let l = i16::from_le_bytes([data[offset], data[offset + 1]]);
                        let r = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
                        state.samples_l.push(l);
                        state.samples_r.push(r);
                    }

                    // Check if it's time to emit a frame
                    if state.last_frame_time.elapsed() < state.update_interval {
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    if state.samples_l.is_empty() {
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    // Compute frame
                    let (waveform_l_min, waveform_l_max) =
                        compute_waveform(&state.samples_l, state.num_columns);
                    let (waveform_r_min, waveform_r_max) =
                        compute_waveform(&state.samples_r, state.num_columns);
                    let (vectorscope_l, vectorscope_r) =
                        compute_vectorscope(&state.samples_l, &state.samples_r, state.num_pairs);

                    state.samples_l.clear();
                    state.samples_r.clear();
                    state.last_frame_time = Instant::now();

                    // Broadcast event
                    if let Some((flow_id, ref events)) = *callback_broadcaster.lock().unwrap() {
                        let element_id = callback_instance_id.clone();
                        trace!(
                            "Broadcasting AudioAnalyzerData for flow {} element {}",
                            flow_id,
                            element_id
                        );
                        events.broadcast(StromEvent::AudioAnalyzerData {
                            flow_id,
                            element_id,
                            waveform_l_min: encode_i8(&waveform_l_min),
                            waveform_l_max: encode_i8(&waveform_l_max),
                            waveform_r_min: encode_i8(&waveform_r_min),
                            waveform_r_max: encode_i8(&waveform_r_max),
                            vectorscope_l: encode_i8(&vectorscope_l),
                            vectorscope_r: encode_i8(&vectorscope_r),
                        });
                    }

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let appsink_element = appsink.upcast::<gst::Element>();

        let elements = vec![
            (tee_id.clone(), tee),
            (queue_id.clone(), queue),
            (resample_id.clone(), audioresample),
            (convert_id.clone(), audioconvert),
            (appsink_id.clone(), appsink_element),
        ];

        // Internal links: tee:src_1 -> queue -> audioresample -> audioconvert -> appsink
        // (tee:src_0 is for passthrough, handled by external pad mapping)
        let internal_links = vec![
            (
                ElementPadRef::pad(&tee_id, "src_1"),
                ElementPadRef::element(&queue_id),
            ),
            (
                ElementPadRef::element(&queue_id),
                ElementPadRef::element(&resample_id),
            ),
            (
                ElementPadRef::element(&resample_id),
                ElementPadRef::element(&convert_id),
            ),
            (
                ElementPadRef::element(&convert_id),
                ElementPadRef::element(&appsink_id),
            ),
        ];

        // Bus message handler to inject the EventBroadcaster into the appsink callback
        let bus_message_handler = Some(Box::new(
            move |_bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                debug!("AudioAnalyzer: injecting broadcaster for flow {}", flow_id);
                *broadcaster.lock().unwrap() = Some((flow_id, events));
                // Return a dummy signal handler ID - we use the bus handler only for injection
                // Connect a no-op handler to get a valid SignalHandlerId
                _bus.connect_message(None, |_bus, _msg| {})
            },
        ) as crate::blocks::BusMessageConnectFn);

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler,
            pad_properties: HashMap::new(),
        })
    }
}

/// Compute waveform min/max per column from samples, quantized to i8.
/// Base64-encode a slice of i8 values (reinterpreted as u8).
fn encode_i8(data: &[i8]) -> String {
    // SAFETY: i8 and u8 have identical layout; this is a no-op reinterpret.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len()) };
    STANDARD.encode(bytes)
}

fn compute_waveform(samples: &[i16], num_columns: usize) -> (Vec<i8>, Vec<i8>) {
    if samples.is_empty() || num_columns == 0 {
        return (vec![0i8; num_columns], vec![0i8; num_columns]);
    }

    let mut mins = Vec::with_capacity(num_columns);
    let mut maxs = Vec::with_capacity(num_columns);

    let samples_per_col = samples.len() as f64 / num_columns as f64;

    for col in 0..num_columns {
        let start = (col as f64 * samples_per_col) as usize;
        let end = (((col + 1) as f64) * samples_per_col) as usize;
        let end = end.min(samples.len());

        if start >= end {
            mins.push(0);
            maxs.push(0);
            continue;
        }

        let mut min_val = i16::MAX;
        let mut max_val = i16::MIN;
        for &s in &samples[start..end] {
            if s < min_val {
                min_val = s;
            }
            if s > max_val {
                max_val = s;
            }
        }

        // Quantize i16 -> i8: round min down, max up, so even quiet
        // signal is visible (min/max won't both be zero unless truly silent).
        // Cast to i32 to avoid overflow at i16 extremes.
        let min_i32 = min_val as i32;
        let max_i32 = max_val as i32;
        let min_q = if min_i32 < 0 {
            ((min_i32 - 255) / 256).max(-128) as i8
        } else {
            (min_i32 / 256) as i8
        };
        let max_q = if max_i32 > 0 {
            ((max_i32 + 255) / 256).min(127) as i8
        } else {
            (max_i32 / 256) as i8
        };
        mins.push(min_q);
        maxs.push(max_q);
    }

    (mins, maxs)
}

/// Compute vectorscope pairs by uniformly sampling from accumulated buffers, quantized to i8.
fn compute_vectorscope(
    samples_l: &[i16],
    samples_r: &[i16],
    num_pairs: usize,
) -> (Vec<i8>, Vec<i8>) {
    let len = samples_l.len().min(samples_r.len());
    if len == 0 || num_pairs == 0 {
        return (Vec::new(), Vec::new());
    }

    let mut vl = Vec::with_capacity(num_pairs);
    let mut vr = Vec::with_capacity(num_pairs);

    let step = if len > num_pairs {
        len as f64 / num_pairs as f64
    } else {
        1.0
    };
    let actual_pairs = num_pairs.min(len);

    for i in 0..actual_pairs {
        let idx = (i as f64 * step) as usize;
        let idx = idx.min(len - 1);
        vl.push((samples_l[idx] / 256) as i8);
        vr.push((samples_r[idx] / 256) as i8);
    }

    (vl, vr)
}

/// Get metadata for AudioAnalyzer block.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![audioanalyzer_definition()]
}

fn audioanalyzer_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.audioanalyzer".to_string(),
        name: "Audio Analyzer".to_string(),
        description:
            "Waveform (oscilloscope) and vectorscope (Lissajous) visualization for audio signals."
                .to_string(),
        category: "Analysis".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "update_rate".to_string(),
                label: "Update Rate".to_string(),
                description: "How often visualization frames are sent (Hz)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "20".to_string(),
                            label: Some("20 Hz".to_string()),
                        },
                        EnumValue {
                            value: "30".to_string(),
                            label: Some("30 Hz".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("30".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "update_rate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "waveform_columns".to_string(),
                label: "Waveform Columns".to_string(),
                description: "Number of horizontal columns in the waveform display".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "200".to_string(),
                            label: Some("200".to_string()),
                        },
                        EnumValue {
                            value: "400".to_string(),
                            label: Some("400".to_string()),
                        },
                        EnumValue {
                            value: "800".to_string(),
                            label: Some("800".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("400".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "waveform_columns".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "vector_pairs".to_string(),
                label: "Vectorscope Points".to_string(),
                description: "Number of sample pairs for the vectorscope display".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "256".to_string(),
                            label: Some("256".to_string()),
                        },
                        EnumValue {
                            value: "512".to_string(),
                            label: Some("512".to_string()),
                        },
                        EnumValue {
                            value: "1024".to_string(),
                            label: Some("1024".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("512".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "vector_pairs".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "tee".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "tee".to_string(),
                internal_pad_name: "src_0".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📊".to_string()),
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}
