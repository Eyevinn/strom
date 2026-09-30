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
//! `prepare_idle_recording_sink`). The `ts_passthrough` multifilesink is locked the same
//! way, from the single caps probe on its static input.
//!
//! A track that carried data and then stopped is a different matter: splitmuxsink goes on
//! waiting for it and the whole recording freezes. `spawn_track_stall_watchdog` ends such
//! a track so the others keep recording, and starts a new file with it once it carries
//! data again.
//!
//! Output files are written to: {media_path}/{output_dir}/{filename_prefix}_%05d.{ext}

use super::refusal::{audio_refusal, refuse_input, video_refusal};
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use chrono;
use gst::glib::prelude::ToValue;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use strom_types::{
    block::{EnumValue, *},
    PropertyValue, *,
};
use tracing::{debug, error, info, warn};

pub struct RecorderBuilder;

// Default values
const BLOCK_NAME: &str = "Recorder";
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
fn prepare_idle_recording_sink(sink: &gst::Element) {
    sink.set_locked_state(true);
}

/// Bring the recording sink into the running pipeline, from the caps probe of a
/// track that is about to be linked to it. Idempotent across tracks.
fn activate_recording_sink(sink: &gst::Element, instance_id: &str) {
    sink.set_locked_state(false);
    if let Err(e) = sink.sync_state_with_parent() {
        error!(
            "Recorder {}: failed to sync recording sink with pipeline state: {}",
            instance_id, e
        );
    }
}

/// How long the muxer may accept nothing at all before the recorder ends the
/// track it is waiting for.
///
/// The trigger is the whole recording being frozen, not one quiet input, so this
/// is already well past anything a live source does normally — a WHIP seat's
/// jitter buffer runs at 400 ms. A frozen recording is not enough on its own:
/// the queues feeding the muxer also have to show one track dry while another is
/// backed up (see `watch_tracks`). Ending a track is not final. Once its input
/// carries data again, the recorder starts a new file with it (see
/// `start_next_file`).
const TRACK_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the stall watchdog looks. Also how quickly it notices the pipeline
/// is gone and stops, and how soon a track that carries data again is back in
/// the recording.
const TRACK_STALL_POLL: Duration = Duration::from_millis(500);

/// How long `start_next_file` waits for the current file to be written out
/// before it restarts the muxer anyway. mp4 is written with robust muxing, so a
/// file cut short still plays; it only misses its last header update.
const FILE_FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `start_next_file` waits for a video track still being recorded to
/// reach a keyframe before it leaves the track out of the next file. Covers the
/// 10 s GOP of a remote encoder that ignores key-unit requests.
const KEYFRAME_CUT_TIMEOUT: Duration = Duration::from_secs(15);

/// The most a parked input keeps for replay, in bytes: a GOP of high-bitrate video
/// with room to spare. Past it the stash starts over from the next keyframe.
const STASH_MAX_BYTES: usize = 32 << 20;

/// What a parked input has kept for the next file: everything from the first
/// keyframe that reached it, in order.
#[derive(Default)]
struct Stash {
    buffers: Vec<gst::Buffer>,
    bytes: usize,
}

impl Stash {
    /// Keep `buffer` if it continues the stash or can start one.
    fn keep(&mut self, buffer: &gst::Buffer) {
        let delta = buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        if self.bytes + buffer.size() > STASH_MAX_BYTES {
            self.buffers.clear();
            self.bytes = 0;
        }
        if self.buffers.is_empty() && delta {
            return;
        }
        self.bytes += buffer.size();
        self.buffers.push(buffer.clone());
    }
}

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
    /// Set once this track has left the recording for good: the caps probe handed
    /// its sink pad back. The input probe then drops buffers, so upstream never
    /// sees the flow error a dead branch would return: a WHIP seat's encoder feeds
    /// the vision mixer through the same tee, and must not be stopped along with
    /// the recording.
    retired: AtomicBool,
    /// Set when the stall watchdog takes this track out of the recording. Unlike
    /// `retired` this is not final: the track is recorded again, in a new file,
    /// once its input carries data.
    ended: AtomicBool,
    /// Set once the input is cut off from its chain and the park probe is in
    /// place. See `park_input`.
    parked: AtomicBool,
    /// Set by the park probe when data reaches a parked input: a keyframe for
    /// video, any buffer for audio.
    resumed: AtomicBool,
    /// What the parked input has kept for the next file. See `add_park_probe`.
    stash: Mutex<Stash>,
}

impl TrackActivity {
    fn new() -> Self {
        Self {
            last_muxed_ms: AtomicU64::new(0),
            last_muxed_running_ms: AtomicU64::new(0),
            running_time_offset_ns: AtomicI64::new(0),
            retired: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            resumed: AtomicBool::new(false),
            stash: Mutex::new(Stash::default()),
        }
    }
}

/// A track the stall watchdog is responsible for.
struct WatchedTrack {
    /// "video 0" / "audio 1" — for the log line, built once at setup.
    label: String,
    /// "video_0" / "audio_1": the stem of this track's parser and queue names,
    /// and the name of an audio track's splitmuxsink pad.
    key: String,
    is_video: bool,
    input: gst::glib::WeakRef<gst::Element>,
    /// The splitmuxsink sink pad this track holds, if any. Its peer is the
    /// track's queue. A track that stayed ended across a new file holds none.
    muxer_pad: Option<gst::glib::WeakRef<gst::Pad>>,
    activity: Arc<TrackActivity>,
}

/// The element name of one part of a track's chain: `role` is "parser" or "queue".
fn chain_element_name(instance_id: &str, key: &str, role: &str) -> String {
    format!("{}:{}_{}", instance_id, key, role)
}

/// The parser for a track's caps, or `None` for a format the recorder does not
/// take on that kind of track. Raw media is refused: the recorder does not encode.
fn parser_for(is_video: bool, caps: &gst::StructureRef) -> Option<&'static str> {
    match (is_video, caps.name().as_str()) {
        (true, "video/x-h264") => Some("h264parse"),
        (true, "video/x-h265") => Some("h265parse"),
        (false, "audio/mpeg") if caps.get::<i32>("mpegversion").unwrap_or(0) == 1 => {
            Some("mpegaudioparse")
        }
        (false, "audio/mpeg") => Some("aacparse"), // mpegversion 2 or 4
        (false, "audio/x-ac3") => Some("ac3parse"),
        (false, "audio/x-dts") => Some("dcaparse"),
        (false, "audio/x-opus") => Some("opusparse"),
        _ => None,
    }
}

/// The refusal for caps `parser_for` has no parser for.
fn refusal_for(is_video: bool, caps_name: &str) -> String {
    if is_video {
        video_refusal(BLOCK_NAME, "H.264 or H.265", caps_name)
    } else {
        audio_refusal(BLOCK_NAME, "AAC, MP3, AC-3, DTS or Opus", caps_name)
    }
}

/// Put a parser and a queue between a track's input and its splitmuxsink pad.
///
/// Each splitmuxsink sink pad needs its own streaming thread. splitmuxsink
/// blocks one input pad while waiting for the others to reach the next GOP
/// boundary, so if two pads are fed by a single upstream streaming task (for
/// example tsdemux in passthrough mode) the blocked pad holds the only thread
/// that could unblock it and the pipeline deadlocks. Default queue properties:
/// the only requirement here is thread decoupling.
///
/// `before_muxer_link` runs just before the queue is linked to the muxer; the
/// first caps probe brings the idle recording sink in there.
fn link_track_chain(
    bin: &gst::Bin,
    instance_id: &str,
    key: &str,
    parser_factory: &str,
    input_src: &gst::Pad,
    muxer_pad: &gst::Pad,
    before_muxer_link: impl FnOnce(),
) -> Result<(), String> {
    let parser = gst::ElementFactory::make(parser_factory)
        .name(chain_element_name(instance_id, key, "parser"))
        .build()
        .map_err(|e| format!("failed to create {}: {}", parser_factory, e))?;
    // config-interval=-1 inserts SPS/PPS before every keyframe
    if parser.has_property("config-interval") {
        parser.set_property("config-interval", -1i32);
    }
    let queue = gst::ElementFactory::make("queue")
        .name(chain_element_name(instance_id, key, "queue"))
        .build()
        .map_err(|e| format!("failed to create queue: {}", e))?;

    bin.add_many([&parser, &queue])
        .map_err(|e| format!("failed to add parser and queue to bin: {}", e))?;
    // Linked while the pipeline is already running, so the new elements' state
    // has to be synced explicitly.
    for element in [&parser, &queue] {
        element
            .sync_state_with_parent()
            .map_err(|e| format!("failed to sync {} state: {}", element.name(), e))?;
    }

    let parser_sink = parser.static_pad("sink").ok_or("parser has no sink pad")?;
    input_src
        .link(&parser_sink)
        .map_err(|e| format!("failed to link input to parser: {:?}", e))?;
    parser
        .link(&queue)
        .map_err(|e| format!("failed to link parser to queue: {}", e))?;
    before_muxer_link();
    queue
        .static_pad("src")
        .ok_or("queue has no src pad")?
        .link(muxer_pad)
        .map_err(|e| format!("failed to link queue to splitmuxsink: {:?}", e))?;
    Ok(())
}

/// Request this track's splitmuxsink sink pad.
///
/// The first video track takes the primary pad, so splitmuxsink splits on its
/// keyframes; the rest take video_aux, named by splitmuxsink. audio_N keeps
/// track order on the input index: mp4mux and matroskamux honour the name,
/// mpegtsmux falls back to sink_%d.
fn request_muxer_pad(splitmuxsink: &gst::Element, is_video: bool, key: &str) -> Option<gst::Pad> {
    if is_video {
        if splitmuxsink.static_pad("video").is_none() {
            splitmuxsink.request_pad_simple("video")
        } else {
            splitmuxsink
                .pad_template("video_aux_%u")
                .and_then(|tmpl| splitmuxsink.request_pad(&tmpl, None, None))
        }
    } else {
        splitmuxsink
            .pad_template("audio_%u")
            .and_then(|tmpl| splitmuxsink.request_pad(&tmpl, Some(key), None))
    }
}

/// Drop this track's buffers at the block boundary once it has left the recording.
///
/// Without it the branch answers upstream with a flow error, and a WHIP seat's
/// encoder — which feeds the vision mixer through the same tee — stops with it.
///
/// It has to be the identity's **sink** pad. `gst_pad_push` answers from the src
/// pad's own flags, such as EOS, before it dispatches any probe, so a drop probe
/// there depends on what state the pad was left in.
///
/// Events have to be dropped as well as buffers, and that is what makes this a
/// correctness matter rather than a tidiness one. An encoder upstream of the block
/// emits a sticky TAG event every so often. Forwarding one into a pad that already
/// carries EOS fails, `push_sticky` turns that refusal into `GST_FLOW_ERROR`, and
/// the error travels back to the seat's source, which pauses its streaming task and
/// never restarts. A buffer probe alone lets those events straight through.
///
/// Flush events are deliberately not covered: they carry pad state that has to
/// reach the branch even after it has left the recording.
///
/// These are the hottest paths in the pipeline, so this is one relaxed atomic load
/// and nothing else.
fn add_retired_input_probe(pad: &gst::Pad, activity: &Arc<TrackActivity>) {
    let activity = Arc::clone(activity);
    pad.add_probe(
        gst::PadProbeType::BUFFER
            | gst::PadProbeType::BUFFER_LIST
            | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_pad, _info| {
            if activity.retired.load(Ordering::Relaxed) {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Ok
            }
        },
    );
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

/// Take a track out of the recording, so the muxer stops waiting for it.
///
/// splitmuxsink holds a GOP until every one of its sink pads has advanced past it,
/// so a single track that stops freezes the whole recording — and, through the tee
/// that feeds the recorder, every other branch of that source with it. EOS is what
/// takes a pad out of that wait. A GAP event does not: splitmuxsink ignores it on a
/// non-reference stream, so the track has to end rather than idle.
///
/// The EOS goes into the track's parser, not out of the block's input: the input
/// is parked first (see `park_input`), so it never carries EOS itself and can be
/// linked into a new chain when data returns.
fn end_stalled_track(input: &gst::Element, activity: &Arc<TrackActivity>) {
    if activity.ended.swap(true, Ordering::SeqCst) {
        return;
    }
    park_input(input, activity);
}

/// Cut a track's input off from its chain and end the chain, as soon as the
/// input's src pad is idle.
///
/// The chain behind it — parser, queue, splitmuxsink pad — gets EOS, so the
/// muxer stops waiting for that pad and can finish the file. The input itself
/// stays clean: unlinked, with the park probe keeping it quiet.
///
/// The watchdog only picks a track whose queue has drained, so nothing is being
/// pushed through its input and the IDLE probe normally runs at once, on the
/// calling thread. Otherwise it runs when the push in flight returns.
fn park_input(input: &gst::Element, activity: &Arc<TrackActivity>) {
    let Some(src) = input.static_pad("src") else {
        return;
    };
    let activity = Arc::clone(activity);
    src.add_probe(gst::PadProbeType::IDLE, move |pad, _info| {
        // A cut at a keyframe and the fallback cut can both get here.
        if activity.parked.swap(true, Ordering::SeqCst) {
            return gst::PadProbeReturn::Remove;
        }
        add_park_probe(pad, &activity);
        if let Some(peer) = pad.peer() {
            let _ = pad.unlink(&peer);
            // The parser drains its last frame and passes the EOS on, through
            // the queue, to the splitmuxsink pad.
            peer.send_event(gst::event::Eos::new());
        }
        gst::PadProbeReturn::Remove
    });
}

/// Where a cut at a keyframe stands. See `park_at_next_keyframe`.
const CUT_WAITING: u8 = 0;
const CUT_AT_KEYFRAME: u8 = 1;
const CUT_DONE: u8 = 2;

/// Park a video track that is still being recorded at its next keyframe, so the
/// current file keeps its frames up to there and the next file starts on that
/// keyframe.
///
/// A key-unit request goes upstream first. A remote encoder may ignore it, and
/// the wait is then up to one GOP. From the keyframe on, buffers are kept for the
/// next file until the input is parked. A keyframe inside a buffer list is not
/// split out: the list goes to the current file, and the cut is at the next one.
///
/// `cut` ends at `CUT_DONE`, set here once the input is parked or by the caller
/// when it stops waiting; the probe then removes itself on its next buffer.
fn park_at_next_keyframe(input: &gst::Element, activity: &Arc<TrackActivity>, cut: &Arc<AtomicU8>) {
    let (Some(src), Some(sink)) = (input.static_pad("src"), input.static_pad("sink")) else {
        return;
    };
    let input_weak = input.downgrade();
    let activity = Arc::clone(activity);
    let probe_cut = Arc::clone(cut);
    src.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, info| {
            if activity.parked.load(Ordering::SeqCst) {
                probe_cut.store(CUT_DONE, Ordering::SeqCst);
                return gst::PadProbeReturn::Remove;
            }
            let state = probe_cut.load(Ordering::SeqCst);
            if state == CUT_DONE {
                return gst::PadProbeReturn::Remove;
            }
            let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() else {
                return gst::PadProbeReturn::Ok;
            };
            if state == CUT_WAITING && buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                return gst::PadProbeReturn::Ok;
            }
            activity
                .stash
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keep(buffer);
            if state == CUT_WAITING {
                probe_cut.store(CUT_AT_KEYFRAME, Ordering::SeqCst);
                if let Some(input) = input_weak.upgrade() {
                    park_input(&input, &activity);
                }
            }
            gst::PadProbeReturn::Drop
        },
    );
    sink.push_event(
        gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build(),
    );
}

/// Keep a parked input quiet, keep what reaches it for the next file, and notice
/// when data comes back to it.
///
/// Buffers are taken off the stream rather than refused: answering upstream with
/// a flow error would stop whatever feeds the input, and a WHIP seat's encoder
/// feeds the vision mixer through the same tee. Sticky events pass: an unlinked
/// pad stores them and reports success, so the input still holds the current caps
/// and segment when it is linked into a new chain. Other serialized events, such
/// as GAP, are dropped, since an unlinked pad would refuse them.
///
/// The RECONFIGURE that linking the new chain sends upstream is dropped too. The
/// caps have not changed, and an encoder upstream answers it with an ALLOCATION
/// query that waits in the new queue behind whatever splitmuxsink is holding. A
/// queue holding only that query reads as drained, so the watchdog cannot see the
/// track is backed up, and the recording stays frozen.
///
/// Each buffer from the first keyframe on is kept, and `start_next_file` replays
/// them into the new chain. So the keyframe that shows a track is back is the
/// first frame of the new file, and the file has the track even if nothing
/// follows it, or if the next keyframe is further off than `TRACK_STALL_TIMEOUT`.
/// Holding the buffer in the probe instead would block the tee the input hangs off
/// for as long as the switch takes.
///
/// It runs on every buffer while the input is parked, so it takes a lock and can
/// grow the stash — the exception to the rule for buffer probes, and only for an
/// input that is out of the recording. Once `start_next_file` has replayed the
/// stash and cleared `parked`, the next item removes it.
fn add_park_probe(src: &gst::Pad, activity: &Arc<TrackActivity>) {
    let activity = Arc::clone(activity);
    src.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST | gst::PadProbeType::EVENT_BOTH,
        move |_pad, info| {
            if !activity.parked.load(Ordering::Relaxed) {
                return gst::PadProbeReturn::Remove;
            }
            match info.data.as_ref() {
                Some(gst::PadProbeData::Buffer(_)) | Some(gst::PadProbeData::BufferList(_)) => {}
                Some(gst::PadProbeData::Event(event)) if event.is_upstream() => {
                    return if event.type_() == gst::EventType::Reconfigure {
                        gst::PadProbeReturn::Drop
                    } else {
                        gst::PadProbeReturn::Ok
                    };
                }
                Some(gst::PadProbeData::Event(event)) if event.is_sticky() => {
                    return gst::PadProbeReturn::Ok;
                }
                _ => return gst::PadProbeReturn::Drop,
            }
            let mut stash = activity.stash.lock().unwrap_or_else(|e| e.into_inner());
            // Checked again under the lock: once the stash is replayed, what
            // arrives goes straight to the new chain, after it.
            if !activity.parked.load(Ordering::SeqCst) {
                return gst::PadProbeReturn::Remove;
            }
            match info.data.as_ref() {
                Some(gst::PadProbeData::Buffer(buffer)) => stash.keep(buffer),
                Some(gst::PadProbeData::BufferList(list)) => {
                    list.iter_owned().for_each(|buffer| stash.keep(&buffer))
                }
                _ => {}
            }
            if !stash.buffers.is_empty() {
                activity.resumed.store(true, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Drop
        },
    );
}

/// Replay a parked input's stash into its new chain, then let live data through.
///
/// The buffers go to the chain directly, since pushing them through the input
/// would park them again. The chain needs the stream's caps and segment before
/// them. The input holds those and sends them ahead of its next buffer, which may
/// never come, so they are pushed now: pushing one sticky event sends every one
/// still pending, in order.
///
/// Whatever arrives during the replay is added to the stash and replayed after
/// it. `parked` is cleared under the stash lock once it is empty, so live data
/// follows the last replayed buffer. A video track with nothing to replay gets
/// the keyframe gate first, so its part of the file still starts on a keyframe.
///
/// Every track replays on its own thread: splitmuxsink holds one track's queue
/// until the others reach the same point, so one replay can wait on another.
fn replay_stash(src: &gst::Pad, activity: &TrackActivity, is_video: bool) {
    if let Some(segment) = src.sticky_event::<gst::event::Segment>(0) {
        let _ = src.push_event(segment);
    }
    let peer = src.peer();
    let mut replayed = false;
    let mut flowing = true;
    loop {
        let batch = {
            let mut stash = activity.stash.lock().unwrap_or_else(|e| e.into_inner());
            if stash.buffers.is_empty() {
                if is_video && !replayed {
                    add_keyframe_gate(src);
                }
                activity.parked.store(false, Ordering::SeqCst);
                return;
            }
            stash.bytes = 0;
            std::mem::take(&mut stash.buffers)
        };
        replayed = true;
        for buffer in batch {
            if flowing {
                flowing = peer.as_ref().is_some_and(|p| p.chain(buffer).is_ok());
            }
        }
    }
}

/// Drop a video track's frames until its next keyframe, so a new file starts on
/// one. Removes itself there; it costs nothing once the file is under way.
fn add_keyframe_gate(src: &gst::Pad) {
    src.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        |_pad, info| {
            let delta = match info.data.as_ref() {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
                }
                Some(gst::PadProbeData::BufferList(list)) => list
                    .get(0)
                    .is_some_and(|b| b.flags().contains(gst::BufferFlags::DELTA_UNIT)),
                _ => false,
            };
            if delta {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Remove
            }
        },
    );
}

/// Where a track stands when the watchdog decides whether to start a new file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackState {
    /// Out of the recording for good: refused, or its input is gone.
    Gone,
    /// Holding a pad but not linked yet: no data has arrived on it.
    Waiting,
    /// Linked to the muxer and being recorded.
    Recording,
    /// Linked, but its source sent EOS: it has ended for real and sends nothing
    /// more.
    Finished,
    /// Ended by the watchdog; its input is still quiet.
    Ended,
    /// Ended by the watchdog, and data has come back to its input.
    Resumed,
}

/// Which tracks go into the next file.
#[derive(Debug, Default, PartialEq, Eq)]
struct NextFile {
    /// Each gets a fresh chain and a splitmuxsink pad.
    record: Vec<usize>,
    /// Ended or finished tracks, which stay out. The pad they hold is handed
    /// back, or the new file would wait for them from its first GOP.
    release: Vec<usize>,
}

/// Whether to start a new file, and with which tracks. `None` leaves the current
/// file alone.
///
/// A new file is the only way back in for an ended track. splitmuxsink keeps a
/// pad that has seen EOS ended in every later fragment too, and mp4mux takes no
/// new pad once a file has started, so the current file cannot take that track
/// again whatever happens to its input. The new file starts once data has come
/// back, with every track that is being recorded. An ended track still quiet
/// stays out, and so does one whose source has finished. A track still waiting for its first data keeps its pad and goes on
/// waiting, as it did in the first file.
fn plan_next_file(tracks: &[TrackState]) -> Option<NextFile> {
    if !tracks.contains(&TrackState::Resumed) {
        return None;
    }
    let mut next = NextFile::default();
    for (index, state) in tracks.iter().enumerate() {
        match state {
            TrackState::Recording | TrackState::Resumed => next.record.push(index),
            TrackState::Ended | TrackState::Finished => next.release.push(index),
            TrackState::Gone | TrackState::Waiting => {}
        }
    }
    Some(next)
}

fn track_state(track: &WatchedTrack) -> TrackState {
    let activity = &track.activity;
    let Some(input) = track.input.upgrade() else {
        return TrackState::Gone;
    };
    if activity.retired.load(Ordering::SeqCst) {
        TrackState::Gone
    } else if activity.ended.load(Ordering::SeqCst) {
        if activity.parked.load(Ordering::SeqCst) && activity.resumed.load(Ordering::SeqCst) {
            TrackState::Resumed
        } else {
            TrackState::Ended
        }
    } else if let Some(src) = input.static_pad("src").filter(|p| p.is_linked()) {
        if src.pad_flags().contains(gst::PadFlags::EOS) {
            TrackState::Finished
        } else {
            TrackState::Recording
        }
    } else {
        TrackState::Waiting
    }
}

/// Poll `done` until it holds, the recorder's pipeline stops, or `timeout`
/// passes (`None`: no limit). Returns whether `done` held.
fn wait_for(
    splitmuxsink: &gst::Element,
    timeout: Option<Duration>,
    done: impl Fn() -> bool,
) -> bool {
    let started = Instant::now();
    loop {
        if done() {
            return true;
        }
        let pipeline_running = splitmuxsink
            .parent()
            .and_then(|p| p.downcast::<gst::Element>().ok())
            .is_some_and(|p| p.current_state() >= gst::State::Paused);
        if !pipeline_running || timeout.is_some_and(|t| started.elapsed() >= t) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Close the current file and start the next one with the tracks in `next`.
///
/// The same splitmuxsink is reused, taken back to NULL and set up as it was at
/// the start: pads requested before it leaves NULL, which mp4mux requires.
/// `start-index` carries the fragment count over, so the new file is the next
/// `%05d` and `format-location` reports it as usual.
///
/// 1. Every track going over is parked, which ends its chain. The current file
///    then has EOS on every pad, and the muxer writes it out. A video track
///    still being recorded is parked at its next keyframe, so neither file loses
///    the frames up to it. One with no keyframe in time is ended instead.
/// 2. That EOS is caught before the file sink, once the file is written. Let
///    through, it would reach the pipeline as the recorder's end of stream:
///    splitmuxsink forwards it once the primary video pad has ended, which is
///    exactly the case this recovers from.
/// 3. With the muxer in NULL, the old chains and their splitmuxsink pads are
///    removed, and every track going over gets a new pad and a new chain from its
///    input, which still holds its current caps and segment. Tracks that stay out
///    get no pad, so the new file does not wait for them.
/// 4. The muxer is started again. `async-handling` keeps its preroll inside the
///    recorder. A live pipeline ignores a child's ASYNC_START anyway, but one
///    that is not live would otherwise go back to PAUSED until the new file
///    prerolls.
/// 5. Each track replays what its input kept while parked, from the first
///    keyframe on, and goes live (see `replay_stash`). The stall check keeps
///    running meanwhile: a replay can wait on a track that died in the switch.
fn start_next_file(
    block_id: &str,
    splitmuxsink: &gst::Element,
    tracks: &mut [WatchedTrack],
    next: &NextFile,
    epoch: Instant,
    next_file_index: &AtomicU32,
    stall: &mut StallCheck,
) {
    let Some(bin) = splitmuxsink
        .parent()
        .and_then(|p| p.downcast::<gst::Bin>().ok())
    else {
        return;
    };
    let file_sink_pad = splitmuxsink.downcast_ref::<gst::Bin>().and_then(|b| {
        b.iterate_sinks()
            .into_iter()
            .filter_map(Result::ok)
            .find_map(|sink| sink.static_pad("sink"))
    });

    // 2, armed before 1 so the EOS cannot slip past.
    let file_written = Arc::new(AtomicBool::new(false));
    let catch_eos = file_sink_pad.as_ref().and_then(|pad| {
        let file_written = Arc::clone(&file_written);
        pad.add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| match info.data.as_ref() {
                Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::Eos => {
                    file_written.store(true, Ordering::SeqCst);
                    gst::PadProbeReturn::Drop
                }
                _ => gst::PadProbeReturn::Ok,
            },
        )
    });

    // When a buffer last went into the file sink, so the wait below can tell a
    // muxer still writing from one that is idle. Only while the switch runs.
    let last_write_ms = Arc::new(AtomicU64::new(epoch.elapsed().as_millis() as u64));
    let track_writes = file_sink_pad.as_ref().and_then(|pad| {
        let last_write_ms = Arc::clone(&last_write_ms);
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_pad, _info| {
                last_write_ms.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            },
        )
    });

    // 1
    let mut record = next.record.clone();
    let mut release = next.release.clone();
    let mut cuts = Vec::new();
    let mut cut_out = Vec::new();
    for &index in &record {
        let track = &tracks[index];
        if track.activity.parked.load(Ordering::SeqCst) {
            continue;
        }
        let Some(input) = track.input.upgrade() else {
            continue;
        };
        if track.is_video {
            let cut = Arc::new(AtomicU8::new(CUT_WAITING));
            park_at_next_keyframe(&input, &track.activity, &cut);
            cuts.push((index, cut));
        } else {
            park_input(&input, &track.activity);
        }
    }
    let all_parked = |record: &[usize]| {
        record
            .iter()
            .all(|&i| tracks[i].activity.parked.load(Ordering::SeqCst))
    };
    if !cuts.is_empty()
        && !wait_for(splitmuxsink, Some(KEYFRAME_CUT_TIMEOUT), || {
            all_parked(&record)
        })
    {
        // No keyframe to start the next file on. The muxer opens a file only once
        // its primary video has one, so such a track would hold the new file
        // empty. It is ended instead, and a new file takes it back once its input
        // sees a keyframe, as for any ended track.
        for (index, cut) in &cuts {
            if cut.load(Ordering::SeqCst) != CUT_WAITING {
                continue;
            }
            let track = &tracks[*index];
            warn!(
                "Recorder {}: {} reached no keyframe within {}s — it stays out of the next file until it does",
                block_id,
                track.label,
                KEYFRAME_CUT_TIMEOUT.as_secs()
            );
            track.activity.ended.store(true, Ordering::SeqCst);
            if let Some(input) = track.input.upgrade() {
                park_input(&input, &track.activity);
            }
            record.retain(|i| i != index);
            release.push(*index);
            cut_out.push(*index);
        }
    }
    for (_, cut) in &cuts {
        cut.store(CUT_DONE, Ordering::SeqCst);
    }
    let all_parked = || all_parked(&record) && all_parked(&cut_out);
    if !wait_for(splitmuxsink, Some(FILE_FINISH_TIMEOUT), all_parked) {
        warn!(
            "Recorder {}: a track is still pushing into the current file after {}s — waiting for it before starting the next file",
            block_id,
            FILE_FINISH_TIMEOUT.as_secs()
        );
        // Its old chain cannot be taken down while a push is running through it.
        if !wait_for(splitmuxsink, None, all_parked) {
            return;
        }
    }

    let written = || file_written.load(Ordering::SeqCst);
    if !wait_for(splitmuxsink, Some(FILE_FINISH_TIMEOUT), written) {
        // Stopping the muxer while its storage is still taking a write fails the
        // whole flow: the write comes back flushing and mp4mux posts an error. An
        // idle muxer is waiting on a track that will not end, so that file is
        // closed as it is.
        warn!(
            "Recorder {}: the current file was not written out within {}s — waiting for its storage to go idle",
            block_id,
            FILE_FINISH_TIMEOUT.as_secs()
        );
        let idle = || {
            file_sink_pad
                .as_ref()
                .is_none_or(|pad| file_sink_idle(pad, &last_write_ms, epoch))
        };
        wait_for(splitmuxsink, None, || written() || idle());
        if !written() {
            warn!(
                "Recorder {}: the muxer is idle without finishing the current file — closing it as it is",
                block_id
            );
        }
    }

    // 3
    splitmuxsink.set_locked_state(true);
    if let Err(e) = splitmuxsink.set_state(gst::State::Null) {
        error!(
            "Recorder {}: splitmuxsink refused NULL while starting the next file: {}",
            block_id, e
        );
    }
    splitmuxsink.set_property(
        "start-index",
        next_file_index.load(Ordering::SeqCst).min(i32::MAX as u32) as i32,
    );
    if let Some(pad) = file_sink_pad.as_ref() {
        for id in [catch_eos, track_writes].into_iter().flatten() {
            pad.remove_probe(id);
        }
    }

    for &index in record.iter().chain(release.iter()) {
        for role in ["parser", "queue"] {
            if let Some(old) = bin.by_name(&chain_element_name(block_id, &tracks[index].key, role))
            {
                let _ = old.set_state(gst::State::Null);
                let _ = bin.remove(&old);
            }
        }
    }
    // A state change resets a pad's data flow but not splitmuxsink's record of
    // it: a pad that has seen EOS stays ended, and the new file would neither
    // wait for it nor split on it. Only a fresh pad starts clean.
    for &index in record.iter().chain(release.iter()) {
        if let Some(pad) = tracks[index].muxer_pad.take().and_then(|p| p.upgrade()) {
            splitmuxsink.release_request_pad(&pad);
        }
    }

    let mut recorded = Vec::with_capacity(record.len());
    for &index in &record {
        let track = &mut tracks[index];
        let Some(input) = track.input.upgrade() else {
            continue;
        };
        let Some(src) = input.static_pad("src") else {
            continue;
        };
        let Some(pad) = request_muxer_pad(splitmuxsink, track.is_video, &track.key) else {
            error!(
                "Recorder {}: splitmuxsink refused a sink pad for {} — it is out of the recording",
                block_id, track.label
            );
            track.activity.retired.store(true, Ordering::SeqCst);
            continue;
        };
        add_muxer_intake_probe(&pad, &track.activity, epoch);
        track.muxer_pad = Some(pad.downgrade());

        let caps = src.current_caps();
        let Some(structure) = caps.as_ref().and_then(|c| c.structure(0)) else {
            error!(
                "Recorder {}: {} has no caps to build its chain from — it is out of the recording",
                block_id, track.label
            );
            track.activity.retired.store(true, Ordering::SeqCst);
            splitmuxsink.release_request_pad(&pad);
            track.muxer_pad = None;
            continue;
        };
        // The format can change while a track is out, e.g. when a WHIP seat
        // rejoins with another codec. Refused here as it would be at the start.
        let Some(parser_factory) = parser_for(track.is_video, structure) else {
            track.activity.retired.store(true, Ordering::SeqCst);
            splitmuxsink.release_request_pad(&pad);
            track.muxer_pad = None;
            refuse_input(&input, &refusal_for(track.is_video, structure.name()));
            continue;
        };
        if let Err(e) = link_track_chain(
            &bin,
            block_id,
            &track.key,
            parser_factory,
            &src,
            &pad,
            || {},
        ) {
            error!(
                "Recorder {}: {}: {} — it is out of the recording",
                block_id, track.label, e
            );
            track.activity.retired.store(true, Ordering::SeqCst);
            splitmuxsink.release_request_pad(&pad);
            track.muxer_pad = None;
            continue;
        }
        if track.is_video {
            if let Some(sink) = input.static_pad("sink") {
                sink.push_event(
                    gst_video::UpstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build(),
                );
            }
        }
        recorded.push(index);
    }

    // 4
    splitmuxsink.set_property("async-handling", true);
    activate_recording_sink(splitmuxsink, block_id);

    // The stall clock restarts now: a track that never reaches the new file is
    // ended again rather than freezing it.
    let now_ms = (epoch.elapsed().as_millis() as u64).max(1);
    for &index in &recorded {
        let activity = &tracks[index].activity;
        activity.last_muxed_ms.store(now_ms, Ordering::SeqCst);
        activity.last_muxed_running_ms.store(0, Ordering::SeqCst);
        activity.ended.store(false, Ordering::SeqCst);
    }

    stall.last_end_ms = Some(now_ms);

    // 5
    let tracks = &*tracks;
    std::thread::scope(|scope| {
        let mut replays = Vec::with_capacity(recorded.len());
        for &index in &recorded {
            let track = &tracks[index];
            let Some(src) = track.input.upgrade().and_then(|i| i.static_pad("src")) else {
                continue;
            };
            replays.push(scope.spawn(move || {
                replay_stash(&src, &track.activity, track.is_video);
                track.activity.resumed.store(false, Ordering::SeqCst);
                // An EOS that reached the input while it was parked is stored on
                // it, but a stored event only goes out ahead of the next buffer,
                // and none follows an EOS.
                if src.sticky_event::<gst::event::Eos>(0).is_some() {
                    if let Some(parser_sink) = src.peer() {
                        parser_sink.send_event(gst::event::Eos::new());
                    }
                }
            }));
        }
        while !replays.iter().all(|replay| replay.is_finished()) {
            std::thread::sleep(TRACK_STALL_POLL);
            let states: Vec<TrackState> = tracks.iter().map(track_state).collect();
            stall.poll(block_id, tracks, &states, epoch);
        }
    });
}

/// Whether the file sink is between writes: none started in the last second,
/// and none in progress.
fn file_sink_idle(pad: &gst::Pad, last_write_ms: &AtomicU64, epoch: Instant) -> bool {
    let now_ms = epoch.elapsed().as_millis() as u64;
    if now_ms.saturating_sub(last_write_ms.load(Ordering::Relaxed)) < 1000 {
        return false;
    }
    // An IDLE probe on an idle pad runs at once and is gone; on a busy one it
    // waits for the write in progress. It goes on the muxer's src pad: a sink
    // pad reports idle even in the middle of a write.
    let Some(muxer_src) = pad.peer() else {
        return true;
    };
    match muxer_src.add_probe(gst::PadProbeType::IDLE, |_pad, _info| {
        gst::PadProbeReturn::Remove
    }) {
        None => true,
        Some(id) => {
            muxer_src.remove_probe(id);
            false
        }
    }
}

/// Watch the recording: end a track that has stopped the muxer, and start a new
/// file once an ended track carries data again.
///
/// Only tracks that hold a splitmuxsink pad are watched, and only from the muxer's
/// first buffer on that pad. A connected track that never carries one is not
/// covered: the muxer does wait for it just the same, but the timeout would then be
/// counting against a publisher that has not connected yet, whose track is meant to
/// start late.
///
/// The thread holds weak references only, outside the moments it is acting on
/// the pipeline, and stops within a poll interval of the pipeline being torn
/// down, so it cannot outlive its flow.
fn spawn_track_stall_watchdog(
    instance_id: &str,
    splitmuxsink: &gst::Element,
    mut tracks: Vec<WatchedTrack>,
    epoch: Instant,
    next_file_index: Arc<AtomicU32>,
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
        .spawn(move || {
            watch_tracks(
                &block_id,
                splitmuxsink_weak,
                &mut tracks,
                epoch,
                &next_file_index,
            )
        });

    if let Err(e) = spawned {
        error!(
            "Recorder {}: failed to start the track stall watchdog: {} — a track that stops will freeze this recording",
            instance_id, e
        );
    }
}

/// Buffers waiting in the queue that feeds this splitmuxsink pad, or `None` while
/// the track's chain is not linked yet.
///
/// Reading the level takes the queue's lock. Its streaming threads release that
/// lock while they wait on a full queue or push downstream, so a stalled muxer
/// cannot block the watchdog here.
fn queued_buffers(muxer_pad: &gst::Pad) -> Option<u32> {
    let queue = muxer_pad.peer()?.parent_element()?;
    queue
        .has_property("current-level-buffers")
        .then(|| queue.property::<u32>("current-level-buffers"))
}

/// Which frozen track, if any, is holding the recording up. Each entry is a track's
/// running time and the level of its queue.
///
/// Every track being quiet at the muxer does not mean one of them died: a muxer
/// that stops writing — a stalled disk, slow network storage — freezes every track
/// at once. The queue in front of each pad tells the two apart. A track whose
/// source stopped has handed its last buffers to the muxer, so its queue is empty;
/// a track the muxer is refusing fills its queue. A track is held responsible only
/// when its queue is empty and another track's queue is backed up behind it. All
/// backed up is the muxer itself; all empty is every source stopping together,
/// where no track is waiting on another.
///
/// Among the empty ones, splitmuxsink waits on whichever has carried the recording
/// least far, so that is the one to end — the same choice it is making internally.
fn track_holding_the_recording(tracks: &[(u64, Option<u32>)]) -> Option<usize> {
    let backed_up = tracks.iter().any(|(_, level)| level.is_some_and(|n| n > 0));
    if !backed_up {
        return None;
    }
    tracks
        .iter()
        .enumerate()
        .filter(|(_, (_, level))| *level == Some(0))
        .min_by_key(|(_, (running_ms, _))| *running_ms)
        .map(|(index, _)| index)
}

/// The watchdog loop. Returns once the pipeline is gone or every track has left
/// the recording for good.
fn watch_tracks(
    block_id: &str,
    splitmuxsink: gst::glib::WeakRef<gst::Element>,
    tracks: &mut [WatchedTrack],
    epoch: Instant,
    next_file_index: &AtomicU32,
) {
    let mut stall = StallCheck::default();

    loop {
        std::thread::sleep(TRACK_STALL_POLL);

        // The pipeline is gone: nothing left to watch.
        let Some(sink) = splitmuxsink.upgrade() else {
            return;
        };

        let states: Vec<TrackState> = tracks.iter().map(track_state).collect();
        if states.iter().all(|s| *s == TrackState::Gone) {
            return;
        }
        if let Some(next) = plan_next_file(&states) {
            let back: Vec<&str> = states
                .iter()
                .zip(tracks.iter())
                .filter(|(s, _)| **s == TrackState::Resumed)
                .map(|(_, t)| t.label.as_str())
                .collect();
            info!(
                "Recorder {}: {} carrying data again — starting a new file so it is recorded",
                block_id,
                back.join(", ")
            );
            start_next_file(
                block_id,
                &sink,
                tracks,
                &next,
                epoch,
                next_file_index,
                &mut stall,
            );
            stall.last_end_ms = Some(epoch.elapsed().as_millis() as u64);
            continue;
        }
        drop(sink);

        stall.poll(block_id, tracks, &states, epoch);
    }
}

/// What the stall check carries from one poll to the next.
#[derive(Default)]
struct StallCheck {
    /// When the last track was ended or the last file started. Either frees the
    /// others, but not within a poll interval: without a pause here the whole
    /// recording is ended track by track before the first one has taken effect.
    last_end_ms: Option<u64>,
    /// Whether the current freeze has already been reported as the muxer's own, so
    /// a long storage stall logs once rather than on every poll.
    reported_muxer_stall: bool,
}

impl StallCheck {
    /// End the track holding the recording up, if the recording is frozen.
    fn poll(
        &mut self,
        block_id: &str,
        tracks: &[WatchedTrack],
        states: &[TrackState],
        epoch: Instant,
    ) {
        let timeout_ms = TRACK_STALL_TIMEOUT.as_millis() as u64;
        let now_ms = epoch.elapsed().as_millis() as u64;
        let mut live = Vec::with_capacity(tracks.len());

        for (track, state) in tracks.iter().zip(states) {
            if *state != TrackState::Recording {
                continue;
            }
            let Some(input) = track.input.upgrade() else {
                continue;
            };
            let last_ms = track.activity.last_muxed_ms.load(Ordering::Relaxed);
            if last_ms == 0 {
                continue;
            }
            let queued = track
                .muxer_pad
                .as_ref()
                .and_then(|pad| pad.upgrade())
                .and_then(|pad| queued_buffers(&pad));
            live.push((
                track,
                input,
                now_ms.saturating_sub(last_ms),
                track.activity.last_muxed_running_ms.load(Ordering::Relaxed),
                queued,
            ));
        }

        // The whole recording has to be frozen before anything is ended. One quiet
        // input proves nothing: whichever track stopped, the muxer blocks and every
        // other input backs up behind it within a second, so a machine that is
        // merely overloaded looks exactly the same from any single input.
        let frozen = live.len() > 1
            && !self
                .last_end_ms
                .is_some_and(|t| now_ms.saturating_sub(t) < timeout_ms)
            && live
                .iter()
                .all(|(_, _, quiet_ms, _, _)| *quiet_ms >= timeout_ms);
        if !frozen {
            self.reported_muxer_stall = false;
            return;
        }

        let positions: Vec<(u64, Option<u32>)> = live
            .iter()
            .map(|(_, _, _, running_ms, queued)| (*running_ms, *queued))
            .collect();
        let Some(index) = track_holding_the_recording(&positions) else {
            if !self.reported_muxer_stall && positions.iter().all(|(_, q)| q.is_some_and(|n| n > 0))
            {
                warn!(
                    "Recorder {}: nothing muxed for {}s with every track backed up — the muxer or its storage is stalled, so no track is ended",
                    block_id,
                    live.iter().map(|(_, _, quiet_ms, _, _)| *quiet_ms).min().unwrap_or(0) / 1000
                );
                self.reported_muxer_stall = true;
            }
            return;
        };
        let (track, input, quiet_ms, running_ms, _) = live.swap_remove(index);
        // Still replaying into a new file: that replay is what is running.
        if track.activity.parked.load(Ordering::SeqCst) {
            return;
        }
        warn!(
            "Recorder {}: nothing muxed for {}s and {} has run dry at {}ms while another track is backed up — ending that track so the rest of the recording continues; it is recorded again, in a new file, once it carries data",
            block_id,
            quiet_ms / 1000,
            track.label,
            running_ms
        );
        end_stalled_track(&input, &track.activity);
        self.last_end_ms = Some(now_ms);
    }
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
        // The index of the file splitmuxsink opens next, kept by the
        // format-location handler. Starting a new file after a track comes back
        // resets splitmuxsink's own count, so the recorder carries it over.
        let next_file_index = Arc::new(AtomicU32::new(0));
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
            let sink_pad_for_drops = video_input.static_pad("sink").ok_or_else(|| {
                BlockBuildError::ElementCreation("video identity has no sink pad".to_string())
            })?;

            let activity = Arc::new(TrackActivity::new());
            add_retired_input_probe(&sink_pad_for_drops, &activity);
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

                    let Some(parser_factory) = parser_for(true, structure) else {
                        give_pad_back();
                        if let Some(input) = pad.parent_element() {
                            refuse_input(&input, &refusal_for(true, &caps_name));
                        }
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

                    if let Err(e) = link_track_chain(
                        &bin,
                        &instance_id_clone,
                        &format!("video_{}", vi),
                        parser_factory,
                        pad,
                        &sink_pad,
                        || activate_recording_sink(&splitmuxsink, &instance_id_clone),
                    ) {
                        error!("Recorder {}: video track {}: {}", instance_id_clone, vi, e);
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
            let sink_pad_for_drops = audio_input.static_pad("sink").ok_or_else(|| {
                BlockBuildError::ElementCreation(format!("audio_{} identity has no sink pad", i))
            })?;

            let activity = Arc::new(TrackActivity::new());
            add_retired_input_probe(&sink_pad_for_drops, &activity);
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

                    // Only accept pre-encoded audio. Raw audio requires an encoder before
                    // the recorder, so raw falls through to the refusal below.
                    let Some(parser_factory) = parser_for(false, structure) else {
                        give_pad_back();
                        if let Some(input) = pad.parent_element() {
                            refuse_input(&input, &refusal_for(false, &caps_name));
                        }
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

                    if let Err(e) = link_track_chain(
                        &bin,
                        &instance_id_clone,
                        &format!("audio_{}", i),
                        parser_factory,
                        pad,
                        &sink_pad,
                        || activate_recording_sink(&splitmuxsink, &instance_id_clone),
                    ) {
                        error!("Recorder {}: audio track {}: {}", instance_id_clone, i, e);
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
            let next_file_index_for_watchdog = Arc::clone(&next_file_index);
            let splitmuxsink_weak = splitmuxsink.downgrade();
            let block_id = instance_id.to_string();
            ctx.register_element_setup(Box::new(move |_flow_id, _events| {
                let Some(splitmuxsink) = splitmuxsink_weak.upgrade() else {
                    return;
                };

                // splitmuxsink names video pads itself, so record what it handed
                // back, not what was asked for.
                let mut watched: Vec<WatchedTrack> = Vec::new();
                let video = video_input_weaks
                    .iter()
                    .zip(video_pad_cells.iter())
                    .zip(video_activities.iter())
                    .enumerate()
                    .map(|(n, t)| (true, n, t));
                let audio = audio_input_weaks
                    .iter()
                    .zip(audio_pad_cells.iter())
                    .zip(audio_activities.iter())
                    .enumerate()
                    .map(|(n, t)| (false, n, t));
                for (is_video, n, ((input, cell), activity)) in video.chain(audio) {
                    let kind = if is_video { "video" } else { "audio" };
                    if !input_is_connected(input) {
                        info!(
                            "Recorder {}: {} track {} is not connected — requesting no splitmuxsink pad for it",
                            block_id, kind, n
                        );
                        continue;
                    }
                    let key = format!("{}_{}", kind, n);
                    let Some(pad) = request_muxer_pad(&splitmuxsink, is_video, &key) else {
                        error!(
                            "Recorder {}: splitmuxsink refused a sink pad for {} track {} — that track will not be recorded",
                            block_id, kind, n
                        );
                        continue;
                    };
                    debug!(
                        "Recorder {}: requested splitmuxsink pad {} for {} track {}",
                        block_id,
                        pad.name(),
                        kind,
                        n
                    );
                    let _ = cell.set(pad.name().to_string());
                    add_muxer_intake_probe(&pad, activity, stall_epoch);
                    watched.push(WatchedTrack {
                        label: format!("{} {}", kind, n),
                        key,
                        is_video,
                        input: input.clone(),
                        muxer_pad: Some(pad.downgrade()),
                        activity: Arc::clone(activity),
                    });
                }

                // Every pad requested above is one the muxer will wait for. Watch
                // exactly those.
                spawn_track_stall_watchdog(
                    &block_id,
                    &splitmuxsink,
                    watched,
                    stall_epoch,
                    next_file_index_for_watchdog,
                );
            }));
        }

        // Register element setup to connect format-location signal at pipeline start.
        // The signal fires each time splitmuxsink opens a new file, giving us the actual filename.
        // Also starts the auto-stop timer if max_duration_mins > 0.
        let splitmuxsink_for_signal = splitmuxsink.clone();
        let next_file_index_for_signal = next_file_index;
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
                next_file_index_for_signal.store(index.saturating_add(1), Ordering::SeqCst);
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

    prepare_idle_recording_sink(&multifilesink);

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

    // One caps probe, not one per track: this path has a single static input, so
    // the first caps to reach it is the only signal that data is coming. Until
    // then the sink stays locked in NULL and cannot stall the pipeline's preroll.
    //
    // The link is made here rather than declared, as the splitmuxsink path does.
    // A pad linked to a sink sitting in NULL loses stream-start and segment, which
    // are pushed before caps and dropped by the inactive peer; GStreamer replays
    // sticky events only on link, so linking after activation is what delivers them.
    let src_pad = ts_input.static_pad("src").ok_or_else(|| {
        BlockBuildError::ElementCreation("ts_input identity has no src pad".to_string())
    })?;
    let sink_weak = multifilesink.downgrade();
    let activated = AtomicBool::new(false);
    let activate_instance_id = instance_id.to_string();
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        if event.type_() != gst::EventType::Caps {
            return gst::PadProbeReturn::Ok;
        }
        if activated.swap(true, Ordering::SeqCst) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(sink) = sink_weak.upgrade() else {
            return gst::PadProbeReturn::Ok;
        };
        activate_recording_sink(&sink, &activate_instance_id);

        let sink_pad = match sink.static_pad("sink") {
            Some(p) => p,
            None => {
                error!(
                    "Recorder {}: multifilesink has no sink pad",
                    activate_instance_id
                );
                return gst::PadProbeReturn::Ok;
            }
        };
        if let Err(e) = pad.link(&sink_pad) {
            error!(
                "Recorder {}: failed to link ts_input to multifilesink: {:?}",
                activate_instance_id, e
            );
        }
        gst::PadProbeReturn::Ok
    });

    let elements = vec![
        (input_id.clone(), ts_input.clone()),
        (sink_id.clone(), multifilesink.clone()),
    ];

    info!(
        "Recorder {}: TS passthrough mode, writing to: {}",
        instance_id, location
    );

    Ok(BlockBuildResult {
        elements,
        internal_links: vec![],
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
        name: BLOCK_NAME.to_string(),
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

    use TrackState::{Ended, Finished, Gone, Recording, Resumed, Waiting};

    /// The production case: video ran dry and was ended, audio kept recording,
    /// and now video is back. Both go into a new file.
    #[test]
    fn a_resumed_track_starts_a_new_file_with_the_recording_ones() {
        assert_eq!(
            plan_next_file(&[Resumed, Recording]),
            Some(NextFile {
                record: vec![0, 1],
                release: vec![],
            })
        );
    }

    /// Ended and still quiet: the current file goes on without it. Starting a new
    /// file here would only cut the recording up.
    #[test]
    fn an_ended_track_that_stays_quiet_changes_nothing() {
        assert_eq!(plan_next_file(&[Ended, Recording]), None);
        assert_eq!(plan_next_file(&[Recording, Recording]), None);
    }

    /// Two tracks ended, one came back. The one still quiet must not hold a pad
    /// in the new file, or that file waits for it from its first GOP; nor may one
    /// whose source sent EOS. A track still waiting for its first data keeps its
    /// pad, as it would in any file, and a refused track stays out.
    #[test]
    fn a_track_still_quiet_stays_out_of_the_new_file() {
        assert_eq!(
            plan_next_file(&[Ended, Resumed, Recording, Waiting, Gone, Finished]),
            Some(NextFile {
                record: vec![1, 2],
                release: vec![0, 5],
            })
        );
    }

    /// A track that stopped: its queue has drained while the one the muxer is
    /// refusing has filled up.
    #[test]
    fn a_drained_track_behind_a_backed_up_one_is_ended() {
        assert_eq!(
            track_holding_the_recording(&[(9_000, Some(40)), (7_000, Some(0))]),
            Some(1)
        );
    }

    /// The muxer or its storage stopped, so every queue is backed up. Ending the
    /// track furthest behind would lose it for the rest of the recording and
    /// rescue nothing.
    #[test]
    fn a_muxer_that_stalls_for_every_track_ends_none() {
        assert_eq!(
            track_holding_the_recording(&[(9_000, Some(40)), (7_000, Some(12))]),
            None
        );
    }

    /// Every source stopped together: nothing is waiting on anything.
    #[test]
    fn tracks_that_stop_together_are_left_alone() {
        assert_eq!(
            track_holding_the_recording(&[(9_000, Some(0)), (7_000, Some(0))]),
            None
        );
    }

    /// Of two drained tracks, the one splitmuxsink is waiting on is the one that
    /// has carried the recording least far. A track whose queue could not be read
    /// is never the one ended.
    #[test]
    fn of_several_drained_tracks_the_furthest_behind_is_ended() {
        assert_eq!(
            track_holding_the_recording(&[
                (9_000, Some(40)),
                (8_000, Some(0)),
                (6_000, None),
                (7_000, Some(0)),
            ]),
            Some(3)
        );
    }

    /// Ending a track must not take the rest of the seat with it.
    ///
    /// A WHIP seat's media leaves one tee for the recorder, the vision mixer, the
    /// audio mixer and the return router. An ended branch that answers upstream
    /// with a flow error stops the encoder feeding it, and the seat then goes
    /// silent for the whole flow until it is restarted. It has to swallow what
    /// arrives and answer OK instead: buffers, which an unlinked pad would answer
    /// with NOT_LINKED, and events, which is the half that bites in a real flow:
    /// an encoder upstream emits a sticky TAG event every so often, and
    /// `push_sticky` reports a refused one to the caller as `GST_FLOW_ERROR`.
    ///
    /// It also has to notice data coming back — but for video only on a
    /// keyframe, since a new file cannot start on anything else.
    #[test]
    fn a_stash_starts_on_a_keyframe_and_starts_over_when_full() {
        gst::init().expect("GStreamer initialises");
        let buffer = |size: usize, delta: bool| {
            let mut buffer = gst::Buffer::with_size(size).expect("allocate buffer");
            if delta {
                buffer
                    .get_mut()
                    .expect("fresh buffer is writable")
                    .set_flags(gst::BufferFlags::DELTA_UNIT);
            }
            buffer
        };
        let mut stash = Stash::default();
        stash.keep(&buffer(16, true));
        assert!(
            stash.buffers.is_empty(),
            "a delta frame cannot start a stash"
        );
        stash.keep(&buffer(16, false));
        stash.keep(&buffer(16, true));
        assert_eq!(
            stash.buffers.len(),
            2,
            "a stash keeps what follows its keyframe"
        );

        stash.keep(&buffer(STASH_MAX_BYTES, true));
        assert!(
            stash.buffers.is_empty(),
            "a full stash starts over, and only on a keyframe"
        );
        stash.keep(&buffer(16, false));
        assert_eq!((stash.buffers.len(), stash.bytes), (1, 16));
    }

    #[test]
    fn an_ended_track_answers_ok_upstream_and_waits_for_a_keyframe() {
        gst::init().expect("gstreamer init");
        let pipeline = gst::Pipeline::new();
        let input = gst::ElementFactory::make("identity")
            .build()
            .expect("identity is part of gstreamer core");
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .property("sync", false)
            .build()
            .expect("fakesink is part of gstreamer core");
        pipeline.add_many([&input, &sink]).expect("add elements");
        input.link(&sink).expect("link identity to fakesink");

        let sink_pad = input.static_pad("sink").expect("identity has a sink pad");
        let activity = Arc::new(TrackActivity::new());
        add_retired_input_probe(&sink_pad, &activity);

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        sink_pad.send_event(gst::event::StreamStart::new("ended-track"));
        sink_pad.send_event(gst::event::Caps::new(&gst::Caps::new_empty_simple(
            "video/x-h264",
        )));
        sink_pad.send_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()));

        let push = |delta: bool| {
            let mut buffer = gst::Buffer::with_size(16).expect("allocate buffer");
            if delta {
                buffer
                    .get_mut()
                    .expect("fresh buffer is writable")
                    .set_flags(gst::BufferFlags::DELTA_UNIT);
            }
            sink_pad.chain(buffer)
        };
        assert_eq!(
            push(false),
            Ok(gst::FlowSuccess::Ok),
            "the branch takes buffers while the track is live"
        );

        end_stalled_track(&input, &activity);
        assert!(
            activity.parked.load(Ordering::SeqCst),
            "an idle input is parked at once"
        );
        assert!(
            !input.static_pad("src").unwrap().is_linked(),
            "a parked input is cut off from its chain"
        );
        assert!(
            sink.static_pad("sink")
                .unwrap()
                .pad_flags()
                .contains(gst::PadFlags::EOS),
            "the chain behind a parked input is ended, so the muxer stops waiting for it"
        );

        assert_eq!(
            push(true),
            Ok(gst::FlowSuccess::Ok),
            "an ended branch has to keep answering OK, or it stops whatever feeds the tee it hangs off"
        );
        assert!(
            !activity.resumed.load(Ordering::SeqCst),
            "a delta frame cannot start a new file"
        );

        let tags = gst::TagList::new();
        assert!(
            sink_pad.send_event(gst::event::Tag::new(tags)),
            "an ended branch has to take events too, not just buffers"
        );
        assert_eq!(
            push(false),
            Ok(gst::FlowSuccess::Ok),
            "a sticky event refused by the ended branch turns the next push into a flow error"
        );
        assert!(
            activity.resumed.load(Ordering::SeqCst),
            "a keyframe on an ended track means it can be recorded again"
        );
        assert_eq!(
            activity.stash.lock().unwrap().buffers.len(),
            1,
            "the keyframe that shows the track is back is kept for the new file"
        );

        let _ = pipeline.set_state(gst::State::Null);
    }
}
