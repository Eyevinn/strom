//! Live Recorder block: records live tracks to fragmented MP4 without letting one
//! track hold up the others.
//!
//! ```text
//! video_input_N (identity) --[caps probe]--> parser --> queue --> isofmp4mux --> fragment sink
//! audio_input_N (identity) --[caps probe]--> parser --> queue -->     ^
//! ```
//!
//! `isofmp4mux` is a live aggregator: a track that goes quiet does not stop the
//! others, and when it comes back it continues in the same file, with a gap
//! where it was missing. Two things keep that true for every track:
//!
//! - A track that is quiet gets GAP events (see [`keepalive`]). The muxer cuts
//!   fragments on the video's keyframes, so without them a stalled video would
//!   keep the audio in memory until it returns.
//! - A connected track that has carried nothing by the time the others have been
//!   running for a while is released from the muxer, which cannot write a header
//!   until every one of its pads has caps.
//!
//! [`fragment_sink`] writes the muxer's output and starts a new file at a
//! fragment boundary when a split is due.
//!
//! Only pre-encoded video is accepted. Raw audio is encoded to AAC here; encoded
//! audio is passed through.
//!
//! Output files: {media_path}/{output_dir}/{filename_prefix}_{timestamp}_%05d.mp4

pub mod fragment_sink;
mod keepalive;
mod mp4_boxes;

use super::refusal::{audio_refusal, refuse_input, video_refusal};
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use fragment_sink::{FragmentFileSink, SplitPolicy};
use gstreamer as gst;
use gstreamer::prelude::*;
pub use keepalive::NO_DATA_TIMEOUT;
use keepalive::{Track, TrackKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::{block::*, PropertyValue, *};
use tracing::{debug, error, info, warn};

pub struct LiveRecorderBuilder;

const BLOCK_NAME: &str = "Live Recorder";
const DEFAULT_OUTPUT_DIR: &str = "recordings";
const DEFAULT_FILENAME_PREFIX: &str = "recording";

/// Element id suffix of the fragment sink, used by the split-now API.
pub const FRAGMENT_SINK_SUFFIX: &str = "fragmentsink";

/// Fragment length handed to the muxer. A fragment also ends only on a video
/// keyframe, so with video this is a lower bound. It is also roughly what a
/// crash can lose.
const FRAGMENT_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(1);

/// How long each track's queue may hold data while the muxer waits for a late
/// track. Above the keepalive's `NO_DATA_TIMEOUT`, so the wait never blocks
/// upstream.
const TRACK_QUEUE_TIME: gst::ClockTime = gst::ClockTime::from_seconds(30);

/// Memory cap on that wait: 30 s of a 50 Mbit/s video.
const TRACK_QUEUE_BYTES: u32 = 200 * 1024 * 1024;

fn uint_property(properties: &HashMap<String, PropertyValue>, name: &str, default: u64) -> u64 {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u),
            PropertyValue::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        })
        .unwrap_or(default)
}

fn string_property(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: &str,
) -> String {
    match properties.get(name) {
        Some(PropertyValue::String(s)) => s.clone(),
        _ => default.to_string(),
    }
}

impl BlockBuilder for LiveRecorderBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_video = uint_property(properties, "num_video_tracks", 1);
        let num_audio = uint_property(properties, "num_audio_tracks", 1);
        Some(external_pads(num_video as usize, num_audio as usize))
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        let media_path = string_property(properties, "_media_path", "./media");
        let output_dir = string_property(properties, "output_dir", DEFAULT_OUTPUT_DIR);
        let filename_prefix =
            string_property(properties, "filename_prefix", DEFAULT_FILENAME_PREFIX);
        let max_size_time_secs = uint_property(properties, "max_size_time_secs", 0);
        let max_size_mb = uint_property(properties, "max_size_mb", 0);
        let max_duration_mins = uint_property(properties, "max_duration_mins", 0);
        let num_video = uint_property(properties, "num_video_tracks", 1) as usize;
        let num_audio = uint_property(properties, "num_audio_tracks", 1) as usize;

        if num_video == 0 && num_audio == 0 {
            return Err(BlockBuildError::InvalidProperty(
                "Live Recorder: num_video_tracks and num_audio_tracks are both 0 — at least one track is required".to_string(),
            ));
        }

        let output_path = std::path::Path::new(&media_path).join(&output_dir);
        if let Err(e) = std::fs::create_dir_all(&output_path) {
            warn!(
                "Live Recorder {}: could not create output directory {}: {}",
                instance_id,
                output_path.display(),
                e
            );
        }
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let location = format!(
            "{}/{}_{}_%05d.mp4",
            output_path.to_string_lossy(),
            filename_prefix,
            timestamp
        );
        let relative_location =
            format!("{}/{}_{}_%05d.mp4", output_dir, filename_prefix, timestamp);

        let mux_id = format!("{}:mux", instance_id);
        let mux = gst::ElementFactory::make("isofmp4mux")
            .name(&mux_id)
            .property("fragment-duration", FRAGMENT_DURATION)
            // The encoder's GOP is the operator's choice; the muxer must not
            // ask for a keyframe at every fragment.
            .property("send-force-keyunit", false)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("isofmp4mux: {}", e)))?;

        let sink_id = format!("{}:{}", instance_id, FRAGMENT_SINK_SUFFIX);
        let policy = SplitPolicy {
            max_duration: (max_size_time_secs > 0)
                .then(|| gst::ClockTime::from_seconds(max_size_time_secs)),
            max_bytes: (max_size_mb > 0).then(|| max_size_mb * 1024 * 1024),
        };
        let sink = FragmentFileSink::new(&sink_id, &location, policy);

        let mut elements: Vec<(String, gst::Element)> = vec![
            (mux_id.clone(), mux.clone()),
            (sink_id.clone(), sink.clone().upcast()),
        ];

        let mut tracks: Vec<Arc<Track>> = Vec::new();
        for (kind, count) in [(TrackKind::Video, num_video), (TrackKind::Audio, num_audio)] {
            for index in 0..count {
                let (input, queue, track) = build_track(instance_id, kind, index, &mux)?;
                elements.push((input.name().to_string(), input));
                elements.push((queue.name().to_string(), queue));
                tracks.push(track);
            }
        }

        info!(
            "Live Recorder {}: built with {} video and {} audio track(s), writing {}",
            instance_id, num_video, num_audio, location
        );

        {
            let mux_weak = mux.downgrade();
            let sink_weak = sink.downgrade();
            let block_id = instance_id.to_string();
            ctx.register_element_setup(Box::new(move |flow_id, events| {
                let (Some(mux), Some(sink)) = (mux_weak.upgrade(), sink_weak.upgrade()) else {
                    return;
                };

                // Runs after the flow is linked and before it leaves NULL: the muxer
                // takes no new pads once it has started, so every connected track
                // gets its pad here.
                for track in &tracks {
                    track.connect_to_muxer(&block_id, &mux);
                }
                sink.set_drain_pads(tracks.iter().filter_map(|t| t.queue_sink_pad()).collect());

                let events_for_files = events.clone();
                let block_for_files = block_id.clone();
                let relative = relative_location.clone();
                sink.set_file_opened_callback(Arc::new(move |index, path| {
                    debug!(
                        "Live Recorder {}: writing file {}",
                        block_for_files,
                        path.display()
                    );
                    events_for_files.broadcast(StromEvent::RecorderFileChanged {
                        flow_id,
                        block_id: block_for_files.clone(),
                        filename: relative.replace("%05d", &format!("{:05}", index)),
                    });
                }));

                keepalive::spawn(&block_id, &mux, tracks);

                if max_duration_mins > 0 {
                    let block_for_timer = block_id.clone();
                    info!(
                        "Live Recorder {}: auto-stop scheduled after {} minute(s)",
                        block_id, max_duration_mins
                    );
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(max_duration_mins * 60))
                            .await;
                        info!(
                            "Live Recorder {}: max duration reached, requesting flow stop",
                            block_for_timer
                        );
                        events.broadcast(StromEvent::RecorderAutoStop {
                            flow_id,
                            block_id: block_for_timer,
                        });
                    });
                }
            }));
        }

        Ok(BlockBuildResult {
            elements,
            internal_links: vec![(
                strom_types::element::ElementPadRef::element(mux_id),
                strom_types::element::ElementPadRef::element(sink_id),
            )],
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Build one track's input `identity` and the `queue` in front of the muxer, and
/// the caps probe that puts the right parser between them.
fn build_track(
    instance_id: &str,
    kind: TrackKind,
    index: usize,
    mux: &gst::Element,
) -> Result<(gst::Element, gst::Element, Arc<Track>), BlockBuildError> {
    let label = format!("{} {}", kind.name(), index);
    let input = gst::ElementFactory::make("identity")
        .name(format!("{}:{}_input_{}", instance_id, kind.name(), index))
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;
    // Each muxer pad needs its own streaming thread, so a quiet track never holds
    // the thread another track needs.
    //
    // Not default sizes: the muxer writes nothing until every connected track has
    // caps, and a WHIP guest's video can arrive seconds after their audio. Until
    // then the early tracks wait here. A default queue fills in a second and
    // blocks upstream, and through a tee that freezes every other user of the
    // source. Holding the wait here, by time, keeps upstream moving; the keepalive
    // releases a track that has not arrived well before this fills.
    let queue = gst::ElementFactory::make("queue")
        .name(format!("{}:{}_{}_queue", instance_id, kind.name(), index))
        .property("max-size-time", TRACK_QUEUE_TIME.nseconds())
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", TRACK_QUEUE_BYTES)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("queue: {}", e)))?;

    let track = Track::new(label, kind, &input, &queue);
    track.install_probes();

    let src_pad = input
        .static_pad("src")
        .ok_or_else(|| BlockBuildError::ElementCreation("identity has no src pad".to_string()))?;
    let parser_inserted = AtomicBool::new(false);
    let probe_track = Arc::clone(&track);
    let queue_weak = queue.downgrade();
    let mux_weak = mux.downgrade();
    let block_id = instance_id.to_string();
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        if parser_inserted.swap(true, Ordering::SeqCst) {
            return gst::PadProbeReturn::Ok;
        }
        let caps = caps_event.caps_owned();
        let (Some(queue), Some(input)) = (queue_weak.upgrade(), pad.parent_element()) else {
            return gst::PadProbeReturn::Ok;
        };
        if !probe_track.is_connected() {
            warn!(
                "Live Recorder {}: {} carries data but was not connected when the flow started, so it is not recorded",
                block_id, probe_track.label
            );
            probe_track.retire();
            return gst::PadProbeReturn::Ok;
        }
        if probe_track.is_retired() {
            warn!(
                "Live Recorder {}: {} carried nothing until after the recording started without it, so it is not recorded",
                block_id, probe_track.label
            );
            return gst::PadProbeReturn::Ok;
        }
        match chain_for(&caps, probe_track.kind) {
            Ok(factories) => {
                if let Err(e) = insert_chain(pad, &queue, &block_id, &probe_track.label, &factories) {
                    error!("Live Recorder {}: {} could not be linked: {}", block_id, probe_track.label, e);
                    if let Some(mux) = mux_weak.upgrade() {
                        probe_track.release_from_muxer(&mux);
                    }
                    return gst::PadProbeReturn::Ok;
                }
                probe_track.mark_caps();
                info!(
                    "Live Recorder {}: {} linked through {}",
                    block_id,
                    probe_track.label,
                    factories.join(" -> ")
                );
            }
            Err(reason) => {
                if let Some(mux) = mux_weak.upgrade() {
                    probe_track.release_from_muxer(&mux);
                }
                refuse_input(&input, &reason);
            }
        }
        gst::PadProbeReturn::Ok
    });

    Ok((input, queue, track))
}

/// The elements a track's caps need before the muxer, or the reason it cannot be
/// recorded.
fn chain_for(caps: &gst::Caps, kind: TrackKind) -> Result<Vec<&'static str>, String> {
    let Some(s) = caps.structure(0) else {
        return Err(format!("{} got caps with no structure", BLOCK_NAME));
    };
    let name = s.name().as_str();
    match kind {
        TrackKind::Video => match name {
            "video/x-h264" => Ok(vec!["h264parse"]),
            "video/x-h265" => Ok(vec!["h265parse"]),
            other => Err(video_refusal(BLOCK_NAME, "H.264 or H.265", other)),
        },
        TrackKind::Audio => match name {
            "audio/mpeg" if s.get::<i32>("mpegversion").unwrap_or(0) != 1 => Ok(vec!["aacparse"]),
            "audio/x-ac3" => Ok(vec!["ac3parse"]),
            "audio/x-eac3" => Ok(vec!["ac3parse"]),
            "audio/x-opus" => Ok(vec!["opusparse"]),
            "audio/x-flac" => Ok(vec!["flacparse"]),
            "audio/x-raw" => Ok(vec![
                "audioconvert",
                "audioresample",
                "avenc_aac",
                "aacparse",
            ]),
            other => Err(audio_refusal(
                BLOCK_NAME,
                "AAC, AC-3, E-AC-3, Opus or FLAC",
                other,
            )),
        },
    }
}

/// Put `factories` between the input's src pad and the queue.
fn insert_chain(
    src: &gst::Pad,
    queue: &gst::Element,
    block_id: &str,
    label: &str,
    factories: &[&str],
) -> Result<(), String> {
    let bin = queue
        .parent()
        .and_then(|p| p.downcast::<gst::Bin>().ok())
        .ok_or("queue has no bin")?;
    let mut chain = Vec::with_capacity(factories.len());
    for factory in factories {
        let element = gst::ElementFactory::make(factory)
            .name(format!(
                "{}:{}_{}",
                block_id,
                label.replace(' ', "_"),
                factory
            ))
            .build()
            .map_err(|e| format!("{}: {}", factory, e))?;
        if *factory == "h264parse" || *factory == "h265parse" {
            // Parameter sets before every keyframe, so any fragment can start a file.
            element.set_property("config-interval", -1i32);
        }
        bin.add(&element).map_err(|e| e.to_string())?;
        element
            .sync_state_with_parent()
            .map_err(|e| e.to_string())?;
        chain.push(element);
    }
    gst::Element::link_many(chain.iter()).map_err(|e| e.to_string())?;
    let first_sink = chain[0].static_pad("sink").ok_or("chain has no sink pad")?;
    src.link(&first_sink).map_err(|e| format!("{:?}", e))?;
    chain
        .last()
        .unwrap()
        .link(queue)
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn external_pads(num_video: usize, num_audio: usize) -> ExternalPads {
    let mut inputs = Vec::new();
    for i in 0..num_video {
        inputs.push(ExternalPad {
            label: Some(format!("V{}", i)),
            name: format!("video_in_{}", i),
            media_type: MediaType::Video,
            internal_element_id: format!("video_input_{}", i),
            internal_pad_name: "sink".to_string(),
        });
    }
    for i in 0..num_audio {
        inputs.push(ExternalPad {
            label: Some(format!("A{}", i)),
            name: format!("audio_in_{}", i),
            media_type: MediaType::Audio,
            internal_element_id: format!("audio_input_{}", i),
            internal_pad_name: "sink".to_string(),
        });
    }
    ExternalPads {
        inputs,
        outputs: vec![],
    }
}

/// Get Live Recorder block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![definition()]
}

fn int_property(name: &str, label: &str, description: &str, default: i64) -> ExposedProperty {
    ExposedProperty {
        name: name.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        property_type: PropertyType::Int,
        default_value: Some(PropertyValue::Int(default)),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: name.to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    }
}

fn string_prop(name: &str, label: &str, description: &str, default: &str) -> ExposedProperty {
    ExposedProperty {
        name: name.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        property_type: PropertyType::String,
        default_value: Some(PropertyValue::String(default.to_string())),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: name.to_string(),
            transform: None,
        },
        live: false,
        persist: None,
    }
}

fn definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.liverecorder".to_string(),
        name: BLOCK_NAME.to_string(),
        description: "Records live audio/video to fragmented MP4. A track that stops does not hold up the others, and continues in the same file when it returns. Experimental.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            int_property("num_video_tracks", "Video Tracks", "Number of video input tracks (0 = audio only)", 1),
            int_property("num_audio_tracks", "Audio Tracks", "Number of audio input tracks (0 = video only)", 1),
            string_prop("output_dir", "Output Directory", "Subdirectory within the media folder where recordings are saved", DEFAULT_OUTPUT_DIR),
            string_prop("filename_prefix", "Filename Prefix", "Prefix for output filenames", DEFAULT_FILENAME_PREFIX),
            int_property("max_size_time_secs", "Max Segment Duration (s)", "Start a new file after this many seconds, at the next fragment. 0 = no splitting.", 0),
            int_property("max_size_mb", "Max Segment Size (MB)", "Start a new file when the current one reaches this size, at the next fragment. 0 = no limit.", 0),
            int_property("max_duration_mins", "Auto-stop After (min)", "Stop the flow automatically after this many minutes of recording. 0 = disabled.", 0),
        ],
        external_pads: external_pads(1, 1),
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: None,
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}
