//! Recorder block for writing audio/video streams to file.
//!
//! Uses splitmuxsink with mp4mux (default), matroskamux, or mpegtsmux for container format.
//! Supports automatic file splitting by time or size.
//!
//! Only pre-encoded material is accepted — the recorder does not encode.
//! Use encoder blocks upstream if you have raw video/audio (e.g. after WHIP ingest).
//!
//! Input handling (dynamic parser insertion via pad probe):
//! - Video: H.264 -> h264parse (config-interval=-1), H.265 -> h265parse (config-interval=-1)
//! - Audio: AAC -> aacparse, MP3 -> mpegaudioparse, AC3 -> ac3parse, Opus -> opusparse, DTS -> dcaparse
//! - Raw video/audio: rejected with a clear error message
//!
//! Pipeline structure:
//! ```text
//! video_in (identity) --[pad probe]--> [parser] --> splitmuxsink:video_0
//! audio_in_N (identity) --[pad probe]--> [parser chain] --> splitmuxsink:audio_0..N
//! ```
//!
//! splitmuxsink stalls forever on a pad that is never fed, so sink pads are requested at
//! pipeline start for connected tracks only — not for every configured track, and not
//! lazily from the pad probes.
//!
//! The sink itself stays locked until a track's caps arrive, so a recorder whose input
//! never carries data cannot hold the pipeline out of PLAYING (see
//! `prepare_idle_recording_sink`).
//!
//! A track that carried data and then stopped is a different matter: splitmuxsink goes on
//! waiting for it and the whole recording freezes. `spawn_track_stall_watchdog` ends such
//! a track so the others keep recording.
//!
//! Output files are written to: {media_path}/{output_dir}/{filename_prefix}_%05d.{ext}

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use chrono;
use gst::glib::prelude::ToValue;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use strom_types::{
    block::{EnumValue, *},
    PropertyValue, *,
};
use tracing::{debug, error, info, warn};

pub struct RecorderBuilder;

// Default values
const DEFAULT_OUTPUT_DIR: &str = "recordings";
const DEFAULT_FILENAME_PREFIX: &str = "recording";
const DEFAULT_CONTAINER: &str = "mp4";
const DEFAULT_MAX_SIZE_TIME_SECS: u64 = 0; // 0 = unlimited
const DEFAULT_MAX_SIZE_BYTES: u64 = 0; // 0 = unlimited
const DEFAULT_MAX_DURATION_MINS: u64 = 0; // 0 = disabled
const DEFAULT_NUM_VIDEO_TRACKS: usize = 1;
const DEFAULT_NUM_AUDIO_TRACKS: usize = 1;

/// Element ID suffix for splitmuxsink, used by the API to look it up via PipelineManager.
pub const SPLITMUXSINK_SUFFIX: &str = "splitmuxsink";

/// Keep a recorder with nothing to record out of the pipeline's state changes.
///
/// A sink only completes READY->PAUSED once it has prerolled a buffer, so a
/// recorder whose input never carries data holds the whole pipeline short of
/// PLAYING — and a flow where only some inputs are live has recorders in
/// exactly that position. Locked, the sink sits in NULL and writes no file
/// until `activate_recording_sink` brings it in on a track's first caps.
fn prepare_idle_recording_sink(splitmuxsink: &gst::Element) {
    splitmuxsink.set_locked_state(true);
}

/// Bring the recording sink into the running pipeline, from the caps probe of a
/// track that is about to be linked to it. Idempotent across tracks.
fn activate_recording_sink(splitmuxsink: &gst::Element, instance_id: &str) {
    splitmuxsink.set_locked_state(false);
    if let Err(e) = splitmuxsink.sync_state_with_parent() {
        error!(
            "Recorder {}: failed to sync splitmuxsink with pipeline state: {}",
            instance_id, e
        );
    }
}

/// How long the muxer may accept nothing at all before the recorder ends the
/// track it is waiting for.
///
/// The trigger is the whole recording being frozen, not one quiet input, so this
/// is already well past anything a live source does normally — a WHIP seat's
/// jitter buffer runs at 400 ms. Ending a track is not reversible: the recording
/// keeps the tracks that are still running, but the one that ended does not come
/// back.
const TRACK_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the stall watchdog looks. Also how quickly it notices the pipeline
/// is gone and stops.
const TRACK_STALL_POLL: Duration = Duration::from_millis(500);

/// What the stall watchdog knows about one track, written by that track's probes.
struct TrackActivity {
    /// Milliseconds since the recorder's epoch when the muxer last took a buffer
    /// from this track. 0 = it never has.
    last_muxed_ms: AtomicU64,
    /// Running time of that buffer, in milliseconds: how far this track has carried
    /// the recording, in the form splitmuxsink compares between its pads.
    last_muxed_running_ms: AtomicU64,
    /// `base - start` of this track's segment, added to a PTS to reach running time.
    running_time_offset_ns: AtomicI64,
    /// Set once this track has left the recording — the watchdog ended it, or the
    /// caps probe handed its sink pad back. The input probe then drops buffers, so
    /// upstream never sees the flow error a dead branch would return: a WHIP seat's
    /// encoder feeds the vision mixer through the same tee, and must not be stopped
    /// along with the recording.
    retired: AtomicBool,
}

/// A track the stall watchdog is responsible for.
struct WatchedTrack {
    /// "video 0" / "audio 1" — for the log line, built once at setup.
    label: String,
    input: gst::glib::WeakRef<gst::Element>,
    /// The splitmuxsink sink pad this track feeds — the second place an EOS can be
    /// delivered when the block's own input pad will not take one. See
    /// `end_stalled_track`.
    muxer_pad: gst::glib::WeakRef<gst::Pad>,
    activity: Arc<TrackActivity>,
    /// Whether a refused EOS has already been reported for this track. The watchdog
    /// retries every poll, and one line per attempt would be 120 a minute.
    refusal_logged: AtomicBool,
}

/// Drop this track's buffers at the block boundary once it has left the recording.
///
/// Without it the branch answers upstream with a flow error, and a WHIP seat's
/// encoder — which feeds the vision mixer through the same tee — stops with it.
///
/// A BUFFER probe is the hottest path in the pipeline, so this is one relaxed
/// atomic load and nothing else.
fn add_retired_input_probe(pad: &gst::Pad, activity: &Arc<TrackActivity>) {
    let activity = Arc::clone(activity);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
        if activity.retired.load(Ordering::Relaxed) {
            gst::PadProbeReturn::Drop
        } else {
            gst::PadProbeReturn::Ok
        }
    });
}

/// Record what the muxer takes from this track: when, and how far it carried the
/// recording. Measured at the splitmuxsink sink pad rather than at the block's
/// input, because the input goes quiet whatever the cause — once the muxer stops,
/// backpressure reaches every input within a second and they all look equally
/// dead.
///
/// Position is kept as running time, not as the raw PTS. The two are far apart
/// here — a WHIP seat's video arrives with a timestamp offset its audio does not
/// have — and running time is what splitmuxsink itself compares between pads, so
/// it is the only form in which two tracks can be ranked against each other.
///
/// A buffer with no PTS is dropped: `mp4mux` answers one with "Buffer has no PTS"
/// and errors the whole pipeline, which takes the seat's video with it. `aacparse`
/// emits one when it drains a partial frame at EOS, so ending a track produces
/// exactly this buffer.
///
/// The segment arrives as an event, and events on a muxer sink pad are rare, so
/// the conversion is folded into an offset there and the buffer path stays at two
/// relaxed atomic stores plus `Instant::elapsed` on the vDSO fast path.
fn add_muxer_intake_probe(pad: &gst::Pad, activity: &Arc<TrackActivity>, epoch: Instant) {
    let segment_activity = Arc::clone(activity);
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Segment(segment) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        if let Some(segment) = segment.segment().downcast_ref::<gst::ClockTime>() {
            let base = segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as i64;
            let start = segment.start().unwrap_or(gst::ClockTime::ZERO).nseconds() as i64;
            segment_activity
                .running_time_offset_ns
                .store(base - start, Ordering::Relaxed);
        }
        gst::PadProbeReturn::Ok
    });

    let activity = Arc::clone(activity);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(pts) = buffer.pts() else {
            return gst::PadProbeReturn::Drop;
        };
        let offset_ns = activity.running_time_offset_ns.load(Ordering::Relaxed);
        let running_ns = (pts.nseconds() as i64).saturating_add(offset_ns).max(0);
        activity
            .last_muxed_running_ms
            .store(running_ns as u64 / 1_000_000, Ordering::Relaxed);
        activity
            .last_muxed_ms
            .store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
}

/// End a track, so the muxer stops waiting for it. Returns whether the EOS was
/// actually delivered.
///
/// splitmuxsink holds a GOP until every one of its sink pads has advanced past it,
/// so a single track that stops freezes the whole recording — and, through the tee
/// that feeds the recorder, every other branch of that source with it. EOS is what
/// takes a pad out of that wait. A GAP event does not: splitmuxsink ignores it on a
/// non-reference stream, so the track has to end rather than idle.
///
/// The EOS is pushed from the watchdog thread rather than from an IDLE probe. By
/// the time a track has stalled, the thread that fed it is usually parked inside
/// `gst_pad_push` on this very pad, so the pad never goes idle and an IDLE probe
/// would never run. An identity src pad has no task of its own, so its stream lock
/// is free.
///
/// That push can still be refused, and `gst_pad_push_event` answers a refusal with
/// one bare `false` whatever the cause. For a sticky, serialized event on a src pad
/// the causes are: the pad is flushing or is no longer activated, the pad already
/// carries an EOS, the pad is unlinked, or the peer is flushing, already at EOS, or
/// out of its parent. Every one of them means this branch is being taken down or is
/// already dead — a busy downstream does not refuse, it blocks — and none of them
/// can be told apart from the return value. A refusal is also usually terminal for
/// this route: a failed push still leaves the sticky EOS on the pad, and the pad
/// then refuses every later one on sight.
///
/// So there is a second place to deliver it: the splitmuxsink sink pad itself. That
/// pad is the one splitmuxsink is actually waiting on, no upstream teardown can
/// invalidate it, and the stalled track is by definition the one nothing is pushing
/// into, so its stream lock is free. The block's input stays the first choice
/// because it lets the parser drain its last frame on the way past.
///
/// The track is marked retired only once the EOS has landed. Marking it before the
/// attempt takes it off the watchdog whether or not it worked, and one refusal then
/// freezes the recording for good.
fn end_stalled_track(
    input: &gst::Element,
    muxer_pad: Option<&gst::Pad>,
    activity: &TrackActivity,
) -> bool {
    let delivered = input
        .static_pad("src")
        .is_some_and(|pad| pad.push_event(gst::event::Eos::new()))
        || muxer_pad.is_some_and(|pad| pad.send_event(gst::event::Eos::new()));

    if delivered {
        activity.retired.store(true, Ordering::SeqCst);
    }
    delivered
}

/// Watch the recording and end a track that has stopped the muxer.
///
/// Only tracks that hold a splitmuxsink pad are watched, and only from the muxer's
/// first buffer on that pad. A connected track that never carries one is not
/// covered: the muxer does wait for it just the same, but the timeout would then be
/// counting against a publisher that has not connected yet, whose track is meant to
/// start late.
///
/// The thread holds weak references only and stops within a poll interval of the
/// pipeline being torn down, so it cannot outlive its flow.
fn spawn_track_stall_watchdog(
    instance_id: &str,
    splitmuxsink: &gst::Element,
    tracks: Vec<WatchedTrack>,
    epoch: Instant,
) {
    if tracks.len() < 2 {
        // One track cannot be held up by another, and ending it would end the
        // recording rather than rescue it.
        return;
    }
    let splitmuxsink_weak = splitmuxsink.downgrade();
    let block_id = instance_id.to_string();

    let spawned = std::thread::Builder::new()
        .name(format!("rec-stall-{}", instance_id))
        .spawn(move || watch_tracks(&block_id, splitmuxsink_weak, &tracks, epoch));

    if let Err(e) = spawned {
        error!(
            "Recorder {}: failed to start the track stall watchdog: {} — a track that stops will freeze this recording",
            instance_id, e
        );
    }
}

/// The watchdog loop. Returns once the pipeline is gone or every track has ended.
fn watch_tracks(
    block_id: &str,
    splitmuxsink: gst::glib::WeakRef<gst::Element>,
    tracks: &[WatchedTrack],
    epoch: Instant,
) {
    let timeout_ms = TRACK_STALL_TIMEOUT.as_millis() as u64;

    // When the last track was ended. Ending one frees the others, but not within a
    // poll interval: without a pause here the whole recording is ended track by
    // track before the first one has taken effect.
    let mut last_end_ms: Option<u64> = None;

    loop {
        std::thread::sleep(TRACK_STALL_POLL);

        // The pipeline is gone: nothing left to watch.
        if splitmuxsink.upgrade().is_none() {
            return;
        }

        let now_ms = epoch.elapsed().as_millis() as u64;
        let mut live = Vec::with_capacity(tracks.len());
        let mut still_watching = 0usize;

        for track in tracks {
            if track.activity.retired.load(Ordering::Relaxed) {
                continue;
            }
            let Some(input) = track.input.upgrade() else {
                continue;
            };
            still_watching += 1;

            let last_ms = track.activity.last_muxed_ms.load(Ordering::Relaxed);
            if last_ms == 0 {
                continue;
            }
            live.push((
                track,
                input,
                now_ms.saturating_sub(last_ms),
                track.activity.last_muxed_running_ms.load(Ordering::Relaxed),
            ));
        }

        if still_watching == 0 {
            return;
        }

        // Ending one track frees the others, but not within a poll interval: without
        // a pause here the whole recording is ended track by track before the first
        // one has taken effect.
        if last_end_ms.is_some_and(|t| now_ms.saturating_sub(t) < timeout_ms) {
            continue;
        }

        let readings: Vec<TrackReading> = live
            .iter()
            .map(|(_, _, quiet_ms, running_ms)| TrackReading {
                quiet_ms: *quiet_ms,
                running_ms: *running_ms,
            })
            .collect();
        let Some(index) = track_holding_the_recording(&readings, timeout_ms) else {
            continue;
        };
        let (track, input, quiet_ms, running_ms) = &live[index];

        let muxer_pad = track.muxer_pad.upgrade();
        if end_stalled_track(input, muxer_pad.as_ref(), &track.activity) {
            warn!(
                "Recorder {}: {} has muxed nothing for {}s at {}ms — ended that track so the rest of the recording continues",
                block_id,
                track.label,
                quiet_ms / 1000,
                running_ms
            );
            last_end_ms = Some(now_ms);
        } else if !track.refusal_logged.swap(true, Ordering::Relaxed) {
            warn!(
                "Recorder {}: {} would not take the EOS that ends its track — retrying every {}ms until it does",
                block_id,
                track.label,
                TRACK_STALL_POLL.as_millis()
            );
        }
    }
}

/// One track's state as the watchdog reads it on a poll.
struct TrackReading {
    /// How long since the muxer last took a buffer from this track.
    quiet_ms: u64,
    /// How far this track has carried the recording, in running time.
    running_ms: u64,
}

/// Which track, if any, is holding the recording up — an index into `readings`.
///
/// splitmuxsink waits on whichever pad has carried the recording least far, so that
/// is the one to end: the same choice it is making internally. Two things have to be
/// true of it before it is ended, and they answer different questions.
///
/// It has to have gone quiet, which says the muxer is not simply slow. And the rest
/// of the recording has to have moved on without it by at least as much, which says
/// the muxer is waiting for this track rather than for something outside it: a box
/// that is merely overloaded stalls every track at the same running time, so the
/// spread stays flat and nothing here fires.
///
/// The spread is what makes this quick. It opens as soon as the track stops, because
/// splitmuxsink goes on taking the other tracks into its internal queues for as long
/// as those have room — which for audio is tens of seconds. Waiting instead for every
/// input to fall silent is waiting for that backpressure to travel all the way up,
/// and the program output is frozen for the whole of it.
fn track_holding_the_recording(readings: &[TrackReading], timeout_ms: u64) -> Option<usize> {
    if readings.len() < 2 {
        // One track cannot be held up by another, and ending it would end the
        // recording rather than rescue it.
        return None;
    }
    let furthest = readings.iter().map(|r| r.running_ms).max()?;
    let (index, behind) = readings
        .iter()
        .enumerate()
        .min_by_key(|(_, r)| r.running_ms)?;

    let quiet = behind.quiet_ms >= timeout_ms;
    let lagging = furthest.saturating_sub(behind.running_ms) >= timeout_ms;
    // Every track quiet is the same stall seen once the backpressure has arrived
    // everywhere, and the spread never opens when the tracks stop together.
    let all_quiet = readings.iter().all(|r| r.quiet_ms >= timeout_ms);

    (quiet && (lagging || all_quiet)).then_some(index)
}

impl BlockBuilder for RecorderBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let container = properties
            .get("container")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or(DEFAULT_CONTAINER);

        // TS passthrough mode: single ts_in pad, no demux/remux
        if container == "ts_passthrough" {
            return Some(ExternalPads {
                inputs: vec![ExternalPad {
                    label: Some("TS".to_string()),
                    name: "ts_in".to_string(),
                    media_type: MediaType::Video, // video/mpegts caps
                    internal_element_id: "ts_input".to_string(),
                    internal_pad_name: "sink".to_string(),
                }],
                outputs: vec![],
            });
        }

        let num_video_tracks = properties
            .get("num_video_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(DEFAULT_NUM_VIDEO_TRACKS);

        let num_audio_tracks = properties
            .get("num_audio_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(DEFAULT_NUM_AUDIO_TRACKS);

        let mut inputs = Vec::new();

        for i in 0..num_video_tracks {
            inputs.push(ExternalPad {
                label: Some(format!("V{}", i)),
                name: format!("video_in_{}", i),
                media_type: MediaType::Video,
                internal_element_id: format!("video_input_{}", i),
                internal_pad_name: "sink".to_string(),
            });
        }

        for i in 0..num_audio_tracks {
            inputs.push(ExternalPad {
                label: Some(format!("A{}", i)),
                name: format!("audio_in_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("audio_input_{}", i),
                internal_pad_name: "sink".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building Recorder block instance: {}", instance_id);

        // --- Read properties ---
        let media_path = properties
            .get("_media_path")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "./media".to_string());

        let output_dir = properties
            .get("output_dir")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| DEFAULT_OUTPUT_DIR.to_string());

        let filename_prefix = properties
            .get("filename_prefix")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| DEFAULT_FILENAME_PREFIX.to_string());

        let container = properties
            .get("container")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| DEFAULT_CONTAINER.to_string());

        let max_size_time_secs = properties
            .get("max_size_time_secs")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as u64),
                _ => None,
            })
            .unwrap_or(DEFAULT_MAX_SIZE_TIME_SECS);

        let max_size_bytes = properties
            .get("max_size_mb")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u * 1024 * 1024),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as u64 * 1024 * 1024),
                _ => None,
            })
            .unwrap_or(DEFAULT_MAX_SIZE_BYTES);

        let max_duration_mins = properties
            .get("max_duration_mins")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as u64),
                _ => None,
            })
            .unwrap_or(DEFAULT_MAX_DURATION_MINS);

        let num_video_tracks = properties
            .get("num_video_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(DEFAULT_NUM_VIDEO_TRACKS);

        let num_audio_tracks = properties
            .get("num_audio_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(DEFAULT_NUM_AUDIO_TRACKS);

        // --- Validate track counts ---
        if num_video_tracks == 0 && num_audio_tracks == 0 {
            return Err(BlockBuildError::InvalidProperty(
                "Recorder: num_video_tracks and num_audio_tracks are both 0 — at least one track is required".to_string(),
            ));
        }

        // --- Validate and build output path ---
        let file_ext = match container.as_str() {
            "mpegts" | "ts" | "ts_passthrough" => "ts",
            "mkv" => "mkv",
            _ => "mp4",
        };

        let output_path = std::path::Path::new(&media_path).join(&output_dir);
        if let Err(e) = std::fs::create_dir_all(&output_path) {
            warn!(
                "Recorder {}: could not create output directory {}: {}",
                instance_id,
                output_path.display(),
                e
            );
        }

        // Include a timestamp in the filename to avoid collisions across recording sessions.
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let location = format!(
            "{}/{}_{}_%05d.{}",
            output_path.to_string_lossy(),
            filename_prefix,
            timestamp,
            file_ext
        );
        // Relative path template (relative to media root) — used in download URLs.
        let relative_location = format!(
            "{}/{}_{}_%05d.{}",
            output_dir, filename_prefix, timestamp, file_ext
        );

        info!(
            "Recorder {}: output location template: {}, container: {}, max_size_time: {}s",
            instance_id, location, container, max_size_time_secs
        );

        // --- TS passthrough mode: raw MPEG-TS bytes directly to file ---
        if container == "ts_passthrough" {
            return build_ts_passthrough(
                instance_id,
                &location,
                max_size_time_secs,
                max_size_bytes,
            );
        }

        // --- Create muxer ---
        let mux_id = format!("{}:mux", instance_id);
        let mux = match container.as_str() {
            "mkv" => {
                gst::ElementFactory::make("matroskamux")
                    .name(&mux_id)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("matroskamux: {}", e)))?
                // MKV is inherently streamable — no moov atom problem, no special setup needed.
            }
            "mpegts" | "ts" => {
                let m = gst::ElementFactory::make("mpegtsmux")
                    .name(&mux_id)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("mpegtsmux: {}", e)))?;
                m.set_property("alignment", 7i32);
                m
            }
            _ => {
                // MP4 (default): use robust muxing so the file is playable even if killed.
                // reserved-max-duration: upper bound on recording duration (12 hours).
                // reserved-moov-update-period: rewrite moov header every 2 seconds.
                let m = gst::ElementFactory::make("mp4mux")
                    .name(&mux_id)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("mp4mux: {}", e)))?;
                let twelve_hours_ns: u64 = 12 * 3600 * 1_000_000_000;
                let two_seconds_ns: u64 = 2 * 1_000_000_000;
                if m.has_property("reserved-max-duration") {
                    m.set_property("reserved-max-duration", twelve_hours_ns);
                }
                if m.has_property("reserved-moov-update-period") {
                    m.set_property("reserved-moov-update-period", two_seconds_ns);
                }
                m
            }
        };

        // --- Create splitmuxsink ---
        let sink_id = format!("{}:splitmuxsink", instance_id);
        let splitmuxsink = gst::ElementFactory::make("splitmuxsink")
            .name(&sink_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("splitmuxsink: {}", e)))?;

        prepare_idle_recording_sink(&splitmuxsink);

        splitmuxsink.set_property("location", &location);
        splitmuxsink.set_property("muxer", &mux);

        if max_size_time_secs > 0 {
            let max_ns = max_size_time_secs * 1_000_000_000u64;
            splitmuxsink.set_property("max-size-time", max_ns);
        }

        if max_size_bytes > 0 {
            splitmuxsink.set_property("max-size-bytes", max_size_bytes);
        }

        // Enable robust muxing for MP4: splitmuxsink periodically updates the muxer's
        // reserved moov header, keeping the file playable if the pipeline is killed.
        // Not needed for MKV or MPEG-TS (inherently robust).
        // Note: use-robust-muxing and async-finalize are mutually exclusive.
        if container == "mp4" && splitmuxsink.has_property("use-robust-muxing") {
            splitmuxsink.set_property("use-robust-muxing", true);
        }

        let mut elements: Vec<(String, gst::Element)> =
            vec![(sink_id.clone(), splitmuxsink.clone())];

        // --- splitmuxsink sink pads: requested at pipeline start, connected tracks only ---
        //
        // splitmuxsink releases a GOP only once every requested pad has reached the next
        // GOP, so a pad that is never fed stalls the recording. An unconnected track must
        // not get one.
        //
        // The element-setup hook at the end of this function requests them: it runs after
        // the flow has linked every block but before the pipeline leaves NULL, so
        // connectivity is known and the muxer has not started. Requesting from the caps
        // probes instead loses a race — the muxer starts on the first track's data, after
        // which mp4mux refuses the second pad and matroskamux drops its data. Requesting
        // up front also keeps the reference stream on the primary video pad.
        //
        // Templates: video (first connected video), video_aux_%u (the rest), audio_%u.

        // Pad name per track, filled in by that hook. Empty means unconnected, no pad.
        let mut video_pad_cells: Vec<Arc<OnceLock<String>>> = Vec::new();
        let mut audio_pad_cells: Vec<Arc<OnceLock<String>>> = Vec::new();
        let mut video_input_weaks: Vec<gst::glib::WeakRef<gst::Element>> = Vec::new();
        let mut audio_input_weaks: Vec<gst::glib::WeakRef<gst::Element>> = Vec::new();

        // Track liveness, for the stall watchdog the setup hook starts.
        let stall_epoch = Instant::now();
        let mut video_activities: Vec<Arc<TrackActivity>> = Vec::new();
        let mut audio_activities: Vec<Arc<TrackActivity>> = Vec::new();

        // --- Create video input chains ---
        for vi in 0..num_video_tracks {
            let video_input_id = format!("{}:video_input_{}", instance_id, vi);
            let video_input = gst::ElementFactory::make("identity")
                .name(&video_input_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("video identity {}: {}", vi, e))
                })?;

            let parser_inserted = Arc::new(AtomicBool::new(false));
            let splitmuxsink_weak = splitmuxsink.downgrade();
            let instance_id_clone = instance_id.to_string();

            let pad_name_cell: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
            video_pad_cells.push(Arc::clone(&pad_name_cell));
            video_input_weaks.push(video_input.downgrade());

            // Use a pad probe on the identity src pad to detect caps and insert parser
            let src_pad = video_input.static_pad("src").ok_or_else(|| {
                BlockBuildError::ElementCreation("video identity has no src pad".to_string())
            })?;

            let activity = Arc::new(TrackActivity {
                last_muxed_ms: AtomicU64::new(0),
                last_muxed_running_ms: AtomicU64::new(0),
                running_time_offset_ns: AtomicI64::new(0),
                retired: AtomicBool::new(false),
            });
            add_retired_input_probe(&src_pad, &activity);
            let probe_activity = Arc::clone(&activity);
            video_activities.push(activity);

            src_pad.add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM,
                move |pad, probe_info| {
                    let event = match probe_info.data.as_ref() {
                        Some(gst::PadProbeData::Event(e)) => e,
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    if event.type_() != gst::EventType::Caps {
                        return gst::PadProbeReturn::Ok;
                    }

                    if parser_inserted.swap(true, Ordering::SeqCst) {
                        return gst::PadProbeReturn::Ok;
                    }

                    let caps = match event.view() {
                        gst::EventView::Caps(c) => c.caps().to_owned(),
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    let structure = match caps.structure(0) {
                        Some(s) => s,
                        None => {
                            error!("Recorder {}: no structure in video caps", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let caps_name = structure.name().to_string();
                    debug!("Recorder {}: video caps detected: {}", instance_id_clone, caps_name);

                    let splitmuxsink = match splitmuxsink_weak.upgrade() {
                        Some(e) => e,
                        None => {
                            error!("Recorder {}: splitmuxsink element no longer exists", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // Empty cell: this track was unconnected at build time, so it has no pad.
                    let sink_pad = match pad_name_cell.get().and_then(|n| splitmuxsink.static_pad(n)) {
                        Some(p) => p,
                        None => {
                            warn!(
                                "Recorder {}: video track {} is carrying data but was not connected when the pipeline was built, so it has no splitmuxsink pad and will not be recorded",
                                instance_id_clone, vi
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // Every path that gives up below must hand the pad back, or splitmuxsink
                    // waits on it forever and the recording stalls. Retiring the track with
                    // it takes it off the stall watchdog, which has nothing left to end, and
                    // drops the buffers that would otherwise be pushed into the dead branch.
                    let give_pad_back = || {
                        probe_activity.retired.store(true, Ordering::SeqCst);
                        splitmuxsink.release_request_pad(&sink_pad)
                    };

                    let (parser_factory, config_interval) = if caps_name == "video/x-h264" {
                        ("h264parse", -1i32)
                    } else if caps_name == "video/x-h265" {
                        ("h265parse", -1i32)
                    } else if caps_name == "video/x-raw" {
                        warn!(
                            "Recorder {}: received raw video — recorder only accepts pre-encoded video. Add an encoder block before the recorder.",
                            instance_id_clone
                        );
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    } else {
                        warn!(
                            "Recorder {}: unsupported video codec: {} (supported: H.264, H.265)",
                            instance_id_clone, caps_name
                        );
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    };

                    let bin = match splitmuxsink.parent().and_then(|p| p.downcast::<gst::Bin>().ok()) {
                        Some(b) => b,
                        None => {
                            error!("Recorder {}: splitmuxsink has no Bin parent", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let parser_name = format!("{}:video_{}_parser", instance_id_clone, vi);
                    let parser = match gst::ElementFactory::make(parser_factory)
                        .name(&parser_name)
                        .build()
                    {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Recorder {}: failed to create {}: {}", instance_id_clone, parser_factory, e);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // config-interval=-1 inserts SPS/PPS before every keyframe
                    if parser.has_property("config-interval") {
                        parser.set_property("config-interval", config_interval);
                    }

                    if let Err(e) = bin.add(&parser) {
                        error!("Recorder {}: failed to add parser to bin: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    if let Err(e) = parser.sync_state_with_parent() {
                        error!("Recorder {}: failed to sync parser state: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }

                    let parser_sink = match parser.static_pad("sink") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: parser has no sink pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };
                    let parser_src = match parser.static_pad("src") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: parser has no src pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // Each splitmuxsink sink pad needs its own streaming thread. splitmuxsink
                    // blocks one input pad while waiting for the others to reach the next GOP
                    // boundary, so if two pads are fed by a single upstream streaming task (for
                    // example tsdemux in passthrough mode) the blocked pad holds the only thread
                    // that could unblock it and the pipeline deadlocks. Default properties: the
                    // only requirement here is thread decoupling.
                    let queue_name = format!("{}:video_{}_queue", instance_id_clone, vi);
                    let queue = match gst::ElementFactory::make("queue").name(&queue_name).build() {
                        Ok(q) => q,
                        Err(e) => {
                            error!("Recorder {}: failed to create video queue: {}", instance_id_clone, e);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };
                    if let Err(e) = bin.add(&queue) {
                        error!("Recorder {}: failed to add video queue to bin: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    // Linked from a pad probe while the pipeline is already running, so the new
                    // element's state has to be synced explicitly.
                    if let Err(e) = queue.sync_state_with_parent() {
                        error!("Recorder {}: failed to sync video queue state: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    let queue_sink = match queue.static_pad("sink") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: video queue has no sink pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };
                    let queue_src = match queue.static_pad("src") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: video queue has no src pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    if let Err(e) = pad.link(&parser_sink) {
                        error!("Recorder {}: failed to link identity to parser: {:?}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    if let Err(e) = parser_src.link(&queue_sink) {
                        error!("Recorder {}: failed to link parser to video queue: {:?}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    activate_recording_sink(&splitmuxsink, &instance_id_clone);

                    if let Err(e) = queue_src.link(&sink_pad) {
                        error!("Recorder {}: failed to link video queue to splitmuxsink: {:?}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }

                    info!("Recorder {}: video chain linked: identity -> {} -> queue -> splitmuxsink", instance_id_clone, parser_factory);
                    gst::PadProbeReturn::Ok
                },
            );

            elements.push((video_input_id, video_input));
        }

        // --- Create audio input chains ---
        for i in 0..num_audio_tracks {
            let audio_input_id = format!("{}:audio_input_{}", instance_id, i);
            let audio_input = gst::ElementFactory::make("identity")
                .name(&audio_input_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audio identity {}: {}", i, e))
                })?;

            let parser_inserted = Arc::new(AtomicBool::new(false));
            let splitmuxsink_weak = splitmuxsink.downgrade();
            let instance_id_clone = instance_id.to_string();

            let pad_name_cell: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
            audio_pad_cells.push(Arc::clone(&pad_name_cell));
            audio_input_weaks.push(audio_input.downgrade());

            let src_pad = audio_input.static_pad("src").ok_or_else(|| {
                BlockBuildError::ElementCreation(format!("audio_{} identity has no src pad", i))
            })?;

            let activity = Arc::new(TrackActivity {
                last_muxed_ms: AtomicU64::new(0),
                last_muxed_running_ms: AtomicU64::new(0),
                running_time_offset_ns: AtomicI64::new(0),
                retired: AtomicBool::new(false),
            });
            add_retired_input_probe(&src_pad, &activity);
            let probe_activity = Arc::clone(&activity);
            audio_activities.push(activity);

            src_pad.add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM,
                move |pad, probe_info| {
                    let event = match probe_info.data.as_ref() {
                        Some(gst::PadProbeData::Event(e)) => e,
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    if event.type_() != gst::EventType::Caps {
                        return gst::PadProbeReturn::Ok;
                    }

                    if parser_inserted.swap(true, Ordering::SeqCst) {
                        return gst::PadProbeReturn::Ok;
                    }

                    let caps = match event.view() {
                        gst::EventView::Caps(c) => c.caps().to_owned(),
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    let structure = match caps.structure(0) {
                        Some(s) => s,
                        None => {
                            error!("Recorder {}: no structure in audio_{} caps", instance_id_clone, i);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let caps_name = structure.name().to_string();
                    let mpegversion = structure.get::<i32>("mpegversion").unwrap_or(0);
                    debug!("Recorder {}: audio_{} caps detected: {}", instance_id_clone, i, caps_name);

                    let splitmuxsink = match splitmuxsink_weak.upgrade() {
                        Some(e) => e,
                        None => {
                            error!("Recorder {}: splitmuxsink element no longer exists", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // Empty cell: this track was unconnected at build time, so it has no pad.
                    let sink_pad = match pad_name_cell.get().and_then(|n| splitmuxsink.static_pad(n)) {
                        Some(p) => p,
                        None => {
                            warn!(
                                "Recorder {}: audio track {} is carrying data but was not connected when the pipeline was built, so it has no splitmuxsink pad and will not be recorded",
                                instance_id_clone, i
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // Every path that gives up below must hand the pad back, or splitmuxsink
                    // waits on it forever and the recording stalls. Retiring the track with
                    // it takes it off the stall watchdog, which has nothing left to end, and
                    // drops the buffers that would otherwise be pushed into the dead branch.
                    let give_pad_back = || {
                        probe_activity.retired.store(true, Ordering::SeqCst);
                        splitmuxsink.release_request_pad(&sink_pad)
                    };

                    // Only accept pre-encoded audio. Raw audio requires an encoder before the recorder.
                    if caps_name == "audio/x-raw" {
                        warn!(
                            "Recorder {}: received raw audio — recorder only accepts pre-encoded audio. Add an encoder block before the recorder.",
                            instance_id_clone
                        );
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }

                    // Insert the appropriate parser for the encoded format, or link directly.
                    let parser_factory = match caps_name.as_str() {
                        "audio/mpeg" if mpegversion == 1 => Some("mpegaudioparse"),
                        "audio/mpeg" => Some("aacparse"), // mpegversion 2 or 4
                        "audio/x-ac3" => Some("ac3parse"),
                        "audio/x-dts" => Some("dcaparse"),
                        "audio/x-opus" => Some("opusparse"),
                        other => {
                            warn!(
                                "Recorder {}: unsupported audio codec: {} (supported: AAC, MP3, AC3, DTS, Opus)",
                                instance_id_clone, other
                            );
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let bin = match splitmuxsink.parent().and_then(|p| p.downcast::<gst::Bin>().ok()) {
                        Some(b) => b,
                        None => {
                            error!("Recorder {}: splitmuxsink has no Bin parent", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // parser_factory is always Some here (unknown codecs returned early above)
                    let factory = parser_factory.unwrap();
                    let parser_name = format!("{}:audio_{}_parser", instance_id_clone, i);
                    let parser = match gst::ElementFactory::make(factory)
                        .name(&parser_name)
                        .build()
                    {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Recorder {}: failed to create {}: {}", instance_id_clone, factory, e);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    if let Err(e) = bin.add(&parser) {
                        error!("Recorder {}: failed to add audio parser: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    if let Err(e) = parser.sync_state_with_parent() {
                        error!("Recorder {}: failed to sync audio parser state: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    let parser_sink = match parser.static_pad("sink") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: audio parser has no sink pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };
                    let parser_src = match parser.static_pad("src") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: audio parser has no src pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // See the video leg: one queue per splitmuxsink sink pad, so the pads do not
                    // block each other on a shared upstream streaming thread. Default properties.
                    let queue_name = format!("{}:audio_{}_queue", instance_id_clone, i);
                    let queue = match gst::ElementFactory::make("queue").name(&queue_name).build() {
                        Ok(q) => q,
                        Err(e) => {
                            error!("Recorder {}: failed to create audio queue: {}", instance_id_clone, e);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };
                    if let Err(e) = bin.add(&queue) {
                        error!("Recorder {}: failed to add audio queue to bin: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    // Linked from a pad probe while the pipeline is already running, so the new
                    // element's state has to be synced explicitly.
                    if let Err(e) = queue.sync_state_with_parent() {
                        error!("Recorder {}: failed to sync audio queue state: {}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    let queue_sink = match queue.static_pad("sink") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: audio queue has no sink pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };
                    let queue_src = match queue.static_pad("src") {
                        Some(p) => p,
                        None => {
                            error!("Recorder {}: audio queue has no src pad", instance_id_clone);
                            give_pad_back();
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    if let Err(e) = pad.link(&parser_sink) {
                        error!("Recorder {}: failed to link identity to audio parser: {:?}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    if let Err(e) = parser_src.link(&queue_sink) {
                        error!("Recorder {}: failed to link audio parser to audio queue: {:?}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }
                    activate_recording_sink(&splitmuxsink, &instance_id_clone);

                    if let Err(e) = queue_src.link(&sink_pad) {
                        error!("Recorder {}: failed to link audio queue to splitmuxsink: {:?}", instance_id_clone, e);
                        give_pad_back();
                        return gst::PadProbeReturn::Ok;
                    }

                    info!("Recorder {}: audio_{} chain linked: identity -> parser -> queue -> splitmuxsink", instance_id_clone, i);

                    gst::PadProbeReturn::Ok
                },
            );

            elements.push((audio_input_id, audio_input));
        }

        info!(
            "Recorder {}: built with {} video track(s), {} audio track(s), container: {}",
            instance_id, num_video_tracks, num_audio_tracks, container
        );

        // Request the sink pads — see the note above the input chains.
        {
            let splitmuxsink_weak = splitmuxsink.downgrade();
            let block_id = instance_id.to_string();
            ctx.register_element_setup(Box::new(move |_flow_id, _events| {
                let Some(splitmuxsink) = splitmuxsink_weak.upgrade() else {
                    return;
                };

                // First connected video track takes the primary pad, so splitmuxsink splits
                // on its keyframes; the rest take video_aux. splitmuxsink names video pads
                // itself, so record what it handed back, not what we asked for.
                let mut have_primary_video = false;
                let mut watched: Vec<WatchedTrack> = Vec::new();
                for (vi, ((input, cell), activity)) in video_input_weaks
                    .iter()
                    .zip(video_pad_cells.iter())
                    .zip(video_activities.iter())
                    .enumerate()
                {
                    if !input_is_connected(input) {
                        info!(
                            "Recorder {}: video track {} is not connected — requesting no splitmuxsink pad for it",
                            block_id, vi
                        );
                        continue;
                    }
                    let requested = if !have_primary_video {
                        splitmuxsink.request_pad_simple("video")
                    } else {
                        splitmuxsink
                            .pad_template("video_aux_%u")
                            .and_then(|tmpl| splitmuxsink.request_pad(&tmpl, None, None))
                    };
                    match requested {
                        Some(pad) => {
                            have_primary_video = true;
                            debug!(
                                "Recorder {}: requested splitmuxsink pad {} for video track {}",
                                block_id,
                                pad.name(),
                                vi
                            );
                            let _ = cell.set(pad.name().to_string());
                            add_muxer_intake_probe(&pad, activity, stall_epoch);
                            watched.push(WatchedTrack {
                                label: format!("video {}", vi),
                                input: input.clone(),
                                muxer_pad: pad.downgrade(),
                                activity: Arc::clone(activity),
                                refusal_logged: AtomicBool::new(false),
                            });
                        }
                        None => error!(
                            "Recorder {}: splitmuxsink refused a sink pad for video track {} — that track will not be recorded",
                            block_id, vi
                        ),
                    }
                }

                for (i, ((input, cell), activity)) in audio_input_weaks
                    .iter()
                    .zip(audio_pad_cells.iter())
                    .zip(audio_activities.iter())
                    .enumerate()
                {
                    if !input_is_connected(input) {
                        info!(
                            "Recorder {}: audio track {} is not connected — requesting no splitmuxsink pad for it",
                            block_id, i
                        );
                        continue;
                    }
                    // audio_N keeps track order on the input index. mp4mux and matroskamux
                    // honour the name; mpegtsmux falls back to sink_%d.
                    let requested_name = format!("audio_{}", i);
                    let requested = splitmuxsink.pad_template("audio_%u").and_then(|tmpl| {
                        splitmuxsink.request_pad(&tmpl, Some(requested_name.as_str()), None)
                    });
                    match requested {
                        Some(pad) => {
                            debug!(
                                "Recorder {}: requested splitmuxsink pad {} for audio track {}",
                                block_id,
                                pad.name(),
                                i
                            );
                            let _ = cell.set(pad.name().to_string());
                            add_muxer_intake_probe(&pad, activity, stall_epoch);
                            watched.push(WatchedTrack {
                                label: format!("audio {}", i),
                                input: input.clone(),
                                muxer_pad: pad.downgrade(),
                                activity: Arc::clone(activity),
                                refusal_logged: AtomicBool::new(false),
                            });
                        }
                        None => error!(
                            "Recorder {}: splitmuxsink refused a sink pad for audio track {} — that track will not be recorded",
                            block_id, i
                        ),
                    }
                }

                // Every pad requested above is one the muxer will wait for. Watch
                // exactly those.
                spawn_track_stall_watchdog(&block_id, &splitmuxsink, watched, stall_epoch);
            }));
        }

        // Register element setup to connect format-location signal at pipeline start.
        // The signal fires each time splitmuxsink opens a new file, giving us the actual filename.
        // Also starts the auto-stop timer if max_duration_mins > 0.
        let splitmuxsink_for_signal = splitmuxsink.clone();
        let location_template = location.clone();
        let relative_location_template = relative_location.clone();
        let block_id_for_signal = instance_id.to_string();
        ctx.register_element_setup(Box::new(move |flow_id, events| {
            let events_clone = events.clone();
            let block_id_clone = block_id_for_signal.clone();
            let location_clone = location_template.clone();
            let relative_location_clone = relative_location_template.clone();
            splitmuxsink_for_signal.connect("format-location", false, move |args| {
                let index = args[1].get::<u32>().unwrap_or(0);
                // Reproduce the filename that splitmuxsink uses (same %05d format)
                let filename = location_clone.replace("%05d", &format!("{:05}", index));
                let relative_path =
                    relative_location_clone.replace("%05d", &format!("{:05}", index));
                debug!(
                    "Recorder {}: writing file index {}: {}",
                    block_id_clone, index, filename
                );
                events_clone.broadcast(strom_types::StromEvent::RecorderFileChanged {
                    flow_id,
                    block_id: block_id_clone.clone(),
                    filename: relative_path,
                });
                // Return the filename — the signal requires a gchararray return value
                Some(filename.to_value())
            });

            if max_duration_mins > 0 {
                let events_for_timer = events.clone();
                let block_id_for_timer = block_id_for_signal.clone();
                info!(
                    "Recorder {}: auto-stop scheduled after {} minute(s)",
                    block_id_for_signal, max_duration_mins
                );
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(max_duration_mins * 60))
                        .await;
                    info!(
                        "Recorder {}: max duration reached, requesting flow stop",
                        block_id_for_timer
                    );
                    events_for_timer.broadcast(strom_types::StromEvent::RecorderAutoStop {
                        flow_id,
                        block_id: block_id_for_timer,
                    });
                });
            }
        }));

        Ok(BlockBuildResult {
            elements,
            internal_links: vec![],
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Whether a recorder input identity has something linked to its sink pad.
///
/// Runs after construction's linking pass, so this is exact for links resolved there —
/// every link into a recorder today, since recorder inputs take encoded media and so are
/// fed by encoder src pads. A link deferred to `pending_links` resolves after this and
/// would read as unconnected, losing that track. Nothing enforces that none can be.
fn input_is_connected(input: &gst::glib::WeakRef<gst::Element>) -> bool {
    input
        .upgrade()
        .and_then(|e| e.static_pad("sink"))
        .map(|p| p.is_linked())
        .unwrap_or(false)
}

/// Build a TS passthrough pipeline: identity -> multifilesink.
///
/// The raw MPEG-TS bitstream is written directly to file without any demux/remux.
/// Uses multifilesink for optional size/time-based file rotation.
fn build_ts_passthrough(
    instance_id: &str,
    location: &str,
    max_size_time_secs: u64,
    max_size_bytes: u64,
) -> Result<BlockBuildResult, BlockBuildError> {
    let input_id = format!("{}:ts_input", instance_id);
    let sink_id = format!("{}:multifilesink", instance_id);

    let ts_input = gst::ElementFactory::make("identity")
        .name(&input_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;

    let multifilesink = gst::ElementFactory::make("multifilesink")
        .name(&sink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("multifilesink: {}", e)))?;

    multifilesink.set_property("location", location);
    // next-file=4 means split on each buffer that has the DISCONT flag, which
    // aligns well with TS packet boundaries when used with tsparse upstream.
    // We override to time/size-based splitting when limits are configured.
    if max_size_bytes > 0 {
        multifilesink.set_property("next-file", 3i32); // max-size
        multifilesink.set_property("max-file-size", max_size_bytes);
    } else if max_size_time_secs > 0 {
        let max_ns = max_size_time_secs * 1_000_000_000u64;
        multifilesink.set_property("next-file", 2i32); // max-duration
        multifilesink.set_property("max-file-duration", max_ns);
    }
    // sync=false: don't block on clock, write as fast as data arrives
    multifilesink.set_property("sync", false);

    let elements = vec![
        (input_id.clone(), ts_input.clone()),
        (sink_id.clone(), multifilesink.clone()),
    ];

    // Static link: ts_input -> multifilesink
    use strom_types::Link;
    let internal_links = vec![Link {
        from: format!("{}:src", input_id),
        to: format!("{}:sink", sink_id),
    }
    .to_pad_refs()];

    info!(
        "Recorder {}: TS passthrough mode, writing to: {}",
        instance_id, location
    );

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Get Recorder block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![recorder_definition()]
}

fn recorder_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.recorder".to_string(),
        name: "Recorder".to_string(),
        description: "Records audio/video streams to file. Supports MP4, MKV, and MPEG-TS containers with optional time/size-based file splitting.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "num_video_tracks".to_string(),
                label: "Video Tracks".to_string(),
                description: "Number of video input tracks (0 = audio only, 1 = normal, 2+ = multi-video)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_video_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "num_audio_tracks".to_string(),
                label: "Audio Tracks".to_string(),
                description: "Number of audio input tracks (0 = video only, 1 = normal, 2+ = multi-audio)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_audio_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "container".to_string(),
                label: "Container Format".to_string(),
                description: "Output container format".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "mp4".to_string(), label: Some("MP4".to_string()) },
                        EnumValue { value: "mkv".to_string(), label: Some("MKV (Matroska)".to_string()) },
                        EnumValue { value: "mpegts".to_string(), label: Some("MPEG-TS (remux)".to_string()) },
                        EnumValue { value: "ts_passthrough".to_string(), label: Some("MPEG-TS (passthrough)".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("mp4".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "container".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "output_dir".to_string(),
                label: "Output Directory".to_string(),
                description: "Subdirectory within the media folder where recordings are saved".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(DEFAULT_OUTPUT_DIR.to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "output_dir".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "filename_prefix".to_string(),
                label: "Filename Prefix".to_string(),
                description: "Prefix for output filenames (e.g. \"recording\" -> recording_00001.mp4)".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(DEFAULT_FILENAME_PREFIX.to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "filename_prefix".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "max_size_time_secs".to_string(),
                label: "Max Segment Duration (s)".to_string(),
                description: "Split recording into segments of this many seconds. 0 = no splitting.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_size_time_secs".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "max_size_mb".to_string(),
                label: "Max Segment Size (MB)".to_string(),
                description: "Split recording when file reaches this size in megabytes. 0 = no limit.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_size_mb".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "max_duration_mins".to_string(),
                label: "Auto-stop After (min)".to_string(),
                description: "Stop the flow automatically after this many minutes of recording. 0 = disabled.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_duration_mins".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V0".to_string()),
                    name: "video_in_0".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_input_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_in_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_input_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: None,
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(quiet_ms: u64, running_ms: u64) -> TrackReading {
        TrackReading {
            quiet_ms,
            running_ms,
        }
    }

    /// The case the watchdog exists for, seen the moment it happens rather than
    /// once the backpressure has reached every input: one track stopped, the
    /// others are still being taken in and have moved on without it.
    #[test]
    fn a_track_the_recording_has_moved_on_without_is_the_one_ended() {
        let readings = [reading(6_000, 10_000), reading(0, 16_000)];
        assert_eq!(track_holding_the_recording(&readings, 5_000), Some(0));
    }

    /// The same shape a second too early: the track is quiet but the recording
    /// has not yet moved far enough past it to say the muxer is waiting on it.
    #[test]
    fn a_quiet_track_the_others_have_barely_passed_is_left_alone() {
        let readings = [reading(6_000, 10_000), reading(0, 13_000)];
        assert_eq!(track_holding_the_recording(&readings, 5_000), None);
    }

    /// An overloaded box, which from any single input looks exactly like a track
    /// that stopped. Nothing advances, so the spread stays flat — but every track
    /// is quiet, and the recording is frozen either way, so the furthest behind
    /// still goes.
    #[test]
    fn tracks_that_stopped_together_still_end_the_furthest_behind() {
        let readings = [reading(6_000, 12_000), reading(7_000, 11_000)];
        assert_eq!(track_holding_the_recording(&readings, 5_000), Some(1));
    }

    #[test]
    fn tracks_that_are_all_delivering_are_left_alone() {
        let readings = [reading(30, 12_000), reading(20, 12_010)];
        assert_eq!(track_holding_the_recording(&readings, 5_000), None);
    }

    /// A lone track is the recording; ending it would not rescue anything.
    #[test]
    fn a_single_track_is_never_ended() {
        let readings = [reading(60_000, 10_000)];
        assert_eq!(track_holding_the_recording(&readings, 5_000), None);
    }

    fn idle_activity() -> TrackActivity {
        TrackActivity {
            last_muxed_ms: AtomicU64::new(0),
            last_muxed_running_ms: AtomicU64::new(0),
            running_time_offset_ns: AtomicI64::new(0),
            retired: AtomicBool::new(false),
        }
    }

    /// A pad outside a running pipeline has never been activated, and
    /// `gst_pad_push_event` refuses a sticky event on one — the same bare `false`
    /// a pad mid-teardown gives. A track retired on that refusal is one the
    /// watchdog never looks at again, so the recording stays frozen for good.
    #[test]
    fn a_track_that_refused_the_eos_is_not_retired() {
        gst::init().expect("gstreamer init");
        let input = gst::ElementFactory::make("identity")
            .build()
            .expect("identity is part of gstreamer core");
        let activity = idle_activity();

        assert!(
            !end_stalled_track(&input, None, &activity),
            "an inactive pad cannot have taken the EOS"
        );
        assert!(
            !activity.retired.load(Ordering::SeqCst),
            "the track was retired on an EOS that was never delivered, so the watchdog will not try again"
        );
    }

    /// The counterpart: on a live branch the EOS lands, and only then does the
    /// track leave the recording.
    #[test]
    fn a_track_that_took_the_eos_is_retired() {
        gst::init().expect("gstreamer init");
        let pipeline = gst::Pipeline::new();
        let input = gst::ElementFactory::make("identity")
            .build()
            .expect("identity is part of gstreamer core");
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .build()
            .expect("fakesink is part of gstreamer core");
        pipeline.add_many([&input, &sink]).expect("add elements");
        input.link(&sink).expect("link identity to fakesink");
        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        let activity = idle_activity();

        assert!(
            end_stalled_track(&input, None, &activity),
            "a linked, playing branch takes the EOS"
        );
        assert!(activity.retired.load(Ordering::SeqCst));

        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
    }

    /// The block's own input is not the only way out of the muxer's wait. When it
    /// will not take the EOS — unlinked here, one of the states a torn-down branch
    /// leaves behind — the splitmuxsink pad the track feeds still will.
    #[test]
    fn an_unlinked_input_falls_back_to_the_muxer_pad() {
        gst::init().expect("gstreamer init");
        let pipeline = gst::Pipeline::new();
        let input = gst::ElementFactory::make("identity")
            .build()
            .expect("identity is part of gstreamer core");
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .build()
            .expect("fakesink is part of gstreamer core");
        pipeline.add_many([&input, &sink]).expect("add elements");
        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        let muxer_pad = sink.static_pad("sink").expect("fakesink sink pad");
        let activity = idle_activity();

        assert!(
            !input
                .static_pad("src")
                .expect("identity src pad")
                .push_event(gst::event::Eos::new()),
            "an unlinked src pad must refuse the EOS, or this test proves nothing"
        );
        assert!(
            end_stalled_track(&input, Some(&muxer_pad), &activity),
            "the muxer pad is the second route out of the wait"
        );
        assert!(activity.retired.load(Ordering::SeqCst));

        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
    }
}
