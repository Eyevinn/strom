//! Tests for the Live Recorder block, built through the real
//! `LiveRecorderBuilder` and fed by live test sources.
//!
//! A track "stalls" when a probe on its encoder drops every buffer for a while:
//! no data and no EOS, as when a WHIP publisher's video freezes.

pub mod common;
#[path = "common/recorder.rs"]
pub mod recorder;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::live_recorder::LiveRecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const REQUIRED: &[&str] = &[
    "isofmp4mux",
    "qtdemux",
    "matroskamux",
    "matroskademux",
    "mpegtsmux",
    "tsdemux",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "videotestsrc",
    "audiotestsrc",
    "appsrc",
    "identity",
    "queue",
    "filesrc",
    "fakesink",
];

fn init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().expect("GStreamer initialises");
        gstisobmff::plugin_register_static().expect("register isobmff plugins");
    });
    common::require_elements(REQUIRED);
}

struct LiveRecorder {
    ctx: BlockBuildContext,
    elements: HashMap<String, gst::Element>,
    instance_id: String,
}

impl LiveRecorder {
    fn input(&self, name: &str) -> gst::Element {
        self.elements[&format!("{}:{}", self.instance_id, name)].clone()
    }

    fn mux(&self) -> gst::Element {
        self.input("mux")
    }

    fn run_setups(&self) {
        for setup in self.ctx.take_element_setups() {
            setup(uuid::Uuid::new_v4(), EventBroadcaster::with_capacity(16));
        }
    }
}

fn add_live_recorder(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
    props: &[(&str, PropertyValue)],
) -> LiveRecorder {
    let mut properties: HashMap<String, PropertyValue> = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    properties.insert(
        "output_dir".into(),
        PropertyValue::String("recordings".into()),
    );
    properties.insert(
        "filename_prefix".into(),
        PropertyValue::String(instance_id.into()),
    );
    properties.insert(
        "_media_path".into(),
        PropertyValue::String(media_root.to_string_lossy().to_string()),
    );
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = LiveRecorderBuilder
        .build(instance_id, &properties, &ctx)
        .expect("live recorder builds");
    let mut elements = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        elements.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = pipeline.by_name(&from.element_id).unwrap();
        let dst = pipeline.by_name(&to.element_id).unwrap();
        src.link_pads(from.pad_name.as_deref(), &dst, to.pad_name.as_deref())
            .expect("internal link");
    }
    LiveRecorder {
        ctx,
        elements,
        instance_id: instance_id.to_string(),
    }
}

/// Drop every buffer leaving `element` between `from` and `to` after `start`.
fn stall(element: &gst::Element, start: Instant, from: Duration, to: Duration) {
    element
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            let t = start.elapsed();
            if t >= from && t < to {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Ok
            }
        });
}

/// Count the buffers the muxer takes on each of its sink pads.
fn count_mux_intake(mux: &gst::Element) -> Vec<Arc<AtomicU64>> {
    mux.sink_pads()
        .iter()
        .map(|pad| {
            let n = Arc::new(AtomicU64::new(0));
            let counter = Arc::clone(&n);
            pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
            n
        })
        .collect()
}

fn no_errors(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
        if let gst::MessageView::Error(err) = msg.view() {
            panic!(
                "pipeline error from {:?}: {} ({:?})",
                err.src().map(|s| s.path_string()),
                err.error(),
                err.debug()
            );
        }
    }
}

fn finish(pipeline: &gst::Pipeline) {
    pipeline.send_event(gst::event::Eos::new());
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(10),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    if let Some(gst::MessageView::Error(err)) = msg.as_ref().map(|m| m.view()) {
        panic!("error at EOS: {} ({:?})", err.error(), err.debug());
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// PTS of every sample per stream, by caps name, from demuxing `path`.
fn demux(path: &Path) -> HashMap<String, Vec<gst::ClockTime>> {
    let demuxer = match path.extension().and_then(|e| e.to_str()) {
        Some("mkv") => "matroskademux",
        Some("ts") => "tsdemux",
        _ => "qtdemux",
    };
    let pipeline = gst::parse::launch(&format!("filesrc name=src ! {demuxer} name=d"))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    // `location` as a property, not in the launch string: the parser reads a
    // Windows path's backslashes as escapes.
    pipeline
        .by_name("src")
        .unwrap()
        .set_property("location", path.to_str().unwrap());
    let samples: Arc<Mutex<HashMap<String, Vec<gst::ClockTime>>>> = Arc::default();
    let demux = pipeline.by_name("d").unwrap();
    let pipeline_weak = pipeline.downgrade();
    let samples_for_pads = Arc::clone(&samples);
    demux.connect_pad_added(move |_demux, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        // Not async: a demuxer with one streaming thread would otherwise park
        // on the first sink's preroll while the second waits for data.
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();
        let samples = Arc::clone(&samples_for_pads);
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            // Some demuxers set caps after pad-added, so read them here.
            let kind = pad
                .current_caps()
                .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
                .unwrap_or_default();
            if let Some(gst::PadProbeData::Buffer(b)) = info.data.as_ref() {
                // matroskademux fills a gap with empty GAP buffers of its own.
                if b.flags().contains(gst::BufferFlags::GAP) || b.size() == 0 {
                    return gst::PadProbeReturn::Ok;
                }
                if let Some(pts) = b.pts() {
                    samples
                        .lock()
                        .unwrap()
                        .entry(kind.clone())
                        .or_default()
                        .push(pts);
                }
            }
            gst::PadProbeReturn::Ok
        });
    });
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(10),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    if let Some(gst::MessageView::Error(err)) = msg.as_ref().map(|m| m.view()) {
        panic!("{} does not demux: {}", path.display(), err.error());
    }
    pipeline.set_state(gst::State::Null).unwrap();
    let mut result = samples.lock().unwrap().clone();
    for v in result.values_mut() {
        v.sort();
    }
    result
}

/// Largest gap between consecutive samples, and the span they cover.
fn largest_gap(pts: &[gst::ClockTime]) -> (gst::ClockTime, gst::ClockTime) {
    let gap = pts
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .max()
        .unwrap_or(gst::ClockTime::ZERO);
    let span = match (pts.first(), pts.last()) {
        (Some(a), Some(b)) => b.saturating_sub(*a),
        _ => gst::ClockTime::ZERO,
    };
    (gap, span)
}

fn file_size(files: &[PathBuf]) -> u64 {
    recorder::total_bytes(files)
}

fn av_recorder(
    pipeline: &gst::Pipeline,
    id: &str,
    root: &Path,
    extra: &[(&str, PropertyValue)],
) -> (LiveRecorder, gst::Element, gst::Element) {
    let mut props = vec![
        ("num_video_tracks", PropertyValue::UInt(1)),
        ("num_audio_tracks", PropertyValue::UInt(1)),
    ];
    props.extend(extra.iter().cloned());
    let rec = add_live_recorder(pipeline, id, root, &props);
    let venc = recorder::video_source(pipeline, -1, true);
    let aenc = recorder::audio_source(pipeline, -1, true);
    venc.link(&rec.input("video_input_0")).unwrap();
    aenc.link(&rec.input("audio_input_0")).unwrap();
    rec.run_setups();
    (rec, venc, aenc)
}

/// A stalled audio track must not stop the video reaching the muxer, and must
/// come back in the same file.
#[test]
fn an_audio_stall_holds_nothing_up_and_audio_returns_in_the_same_file() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let (rec, _venc, aenc) = av_recorder(&pipeline, "rec", dir.path(), &[]);
    let start = Instant::now();
    stall(&aenc, start, Duration::from_secs(2), Duration::from_secs(7));
    let intake = count_mux_intake(&rec.mux());

    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let video_at_3s = intake[0].load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(3));
    let video_at_6s = intake[0].load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(4));
    no_errors(&pipeline);
    finish(&pipeline);

    // 30 fps: three seconds of a stalled audio track is ~90 video frames.
    assert!(
        video_at_6s - video_at_3s >= 60,
        "video stopped reaching the muxer while audio was stalled: {} frames in 3s",
        video_at_6s - video_at_3s
    );
    let files = recorder::recordings(dir.path(), "rec");
    assert_eq!(
        files.len(),
        1,
        "a stall must not start a new file: {:?}",
        files
    );
    let samples = demux(&files[0]);
    let (audio_gap, audio_span) = largest_gap(&samples["audio/mpeg"]);
    let (video_gap, _) = largest_gap(&samples["video/x-h264"]);
    assert!(
        audio_gap >= gst::ClockTime::from_seconds(4),
        "the stall should show as a gap in the audio, largest gap {}",
        audio_gap
    );
    assert!(
        audio_span >= gst::ClockTime::from_seconds(8),
        "audio did not come back: it spans {}",
        audio_span
    );
    assert!(
        video_gap < gst::ClockTime::from_mseconds(500),
        "video has a gap of {}",
        video_gap
    );
}

/// While the video is stalled the muxer cannot end a fragment on a keyframe.
/// The keepalive's GAPs let it write the audio anyway, so the file keeps growing.
#[test]
fn a_video_stall_keeps_the_audio_written_to_disk() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let (_rec, venc, _aenc) = av_recorder(&pipeline, "rec", dir.path(), &[]);
    let start = Instant::now();
    stall(
        &venc,
        start,
        Duration::from_secs(2),
        Duration::from_secs(10),
    );

    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let at_4s = file_size(&recorder::recordings(dir.path(), "rec"));
    std::thread::sleep(Duration::from_secs(4));
    let at_8s = file_size(&recorder::recordings(dir.path(), "rec"));
    std::thread::sleep(Duration::from_secs(4));
    no_errors(&pipeline);
    finish(&pipeline);

    // 128 kbit/s AAC is ~16 KB/s: four seconds of audio is ~64 KB.
    assert!(
        at_8s >= at_4s + 30_000,
        "nothing reached the file while the video was stalled: {} bytes at 4s, {} at 8s",
        at_4s,
        at_8s
    );
    let files = recorder::recordings(dir.path(), "rec");
    assert_eq!(files.len(), 1, "{:?}", files);
    let samples = demux(&files[0]);
    let (audio_gap, audio_span) = largest_gap(&samples["audio/mpeg"]);
    let (video_gap, video_span) = largest_gap(&samples["video/x-h264"]);
    assert!(
        audio_gap < gst::ClockTime::from_mseconds(500),
        "audio has a gap of {}",
        audio_gap
    );
    assert!(
        audio_span >= gst::ClockTime::from_seconds(10),
        "audio spans only {}",
        audio_span
    );
    assert!(
        video_gap >= gst::ClockTime::from_seconds(6),
        "the stall should show as a video gap, largest {}",
        video_gap
    );
    assert!(
        video_span >= gst::ClockTime::from_seconds(10),
        "video did not come back: spans {}",
        video_span
    );
}

/// A connected track that never carries anything must not keep the rest from
/// being recorded, nor the pipeline from PLAYING.
#[test]
fn a_connected_track_that_never_carries_data_does_not_block_the_recording() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let rec = add_live_recorder(
        &pipeline,
        "rec",
        dir.path(),
        &[
            ("num_video_tracks", PropertyValue::UInt(1)),
            ("num_audio_tracks", PropertyValue::UInt(1)),
        ],
    );
    let venc = recorder::video_source(&pipeline, -1, true);
    venc.link(&rec.input("video_input_0")).unwrap();
    let silent = gst::ElementFactory::make("appsrc")
        .property("is-live", true)
        .property_from_str("format", "time")
        .build()
        .unwrap();
    pipeline.add(&silent).unwrap();
    silent.link(&rec.input("audio_input_0")).unwrap();
    rec.run_setups();
    // What the encoder gets out: if the recorder held it up, a tee would hold up
    // every other user of the source.
    let encoded = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&encoded);
    venc.static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

    pipeline.set_state(gst::State::Playing).unwrap();
    let started = Instant::now();
    let (result, state, _) = pipeline.state(gst::ClockTime::from_seconds(3));
    assert!(
        result.is_ok() && state == gst::State::Playing,
        "pipeline stuck at {:?} ({:?})",
        state,
        result
    );
    std::thread::sleep(Duration::from_secs(5));
    let at_8s = encoded.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(5));
    let at_13s = encoded.load(Ordering::Relaxed);
    let release = strom::blocks::builtin::live_recorder::NO_DATA_TIMEOUT;
    std::thread::sleep(release.saturating_sub(Duration::from_secs(13)) + Duration::from_secs(5));
    no_errors(&pipeline);
    let ran = started.elapsed();
    finish(&pipeline);

    // 30 fps: five seconds is ~150 frames.
    assert!(
        at_13s - at_8s >= 100,
        "the video source was held up while the muxer waited for audio: {} frames in 5s",
        at_13s - at_8s
    );
    let files = recorder::recordings(dir.path(), "rec");
    assert_eq!(files.len(), 1, "{:?}", files);
    let samples = demux(&files[0]);
    let (video_gap, video_span) = largest_gap(&samples["video/x-h264"]);
    // Nothing is lost while the muxer waited for the audio: the file covers the
    // whole run, from before the release.
    assert!(
        video_span.nseconds() as u128 + 1_500_000_000 >= ran.as_nanos(),
        "video spans only {} of a {:?} run",
        video_span,
        ran
    );
    assert!(
        video_gap < gst::ClockTime::from_mseconds(500),
        "video has a gap of {}",
        video_gap
    );
}

/// With a split length set, every file starts with its own init segment and
/// plays on its own.
#[test]
fn split_files_each_play_on_their_own() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let _ = av_recorder(
        &pipeline,
        "rec",
        dir.path(),
        &[("max_size_time_secs", PropertyValue::UInt(2))],
    );
    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(7));
    no_errors(&pipeline);
    finish(&pipeline);

    let files = recorder::recordings(dir.path(), "rec");
    assert!(files.len() >= 3, "expected a file per 2s, got {:?}", files);
    for file in &files {
        let samples = demux(file);
        assert!(
            samples.get("video/x-h264").is_some_and(|v| !v.is_empty())
                && samples.get("audio/mpeg").is_some_and(|a| !a.is_empty()),
            "{} does not play on its own: {:?}",
            file.display(),
            samples
                .iter()
                .map(|(k, v)| (k, v.len()))
                .collect::<Vec<_>>()
        );
        // Each file starts at zero, not where it sat in the whole recording.
        let first = samples
            .values()
            .filter_map(|v| v.first())
            .min()
            .copied()
            .unwrap();
        assert!(
            first < gst::ClockTime::from_mseconds(500),
            "{} starts at {}",
            file.display(),
            first
        );
    }
}

/// Nothing the block leaves behind may keep a stopped pipeline alive: not its
/// probes, not the keepalive thread, not the sink's file callback.
#[test]
fn a_stopped_pipeline_is_freed() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let (rec, venc, aenc) = av_recorder(&pipeline, "rec", dir.path(), &[]);
    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    finish(&pipeline);

    let weak = pipeline.downgrade();
    drop((rec, venc, aenc));
    drop(pipeline);
    // The keepalive thread polls every 100 ms and holds only weak references.
    let deadline = Instant::now() + Duration::from_secs(2);
    while weak.upgrade().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        weak.upgrade().is_none(),
        "the pipeline survived its own drop"
    );
}

mod fragment_sink {
    use super::*;
    use strom::blocks::builtin::live_recorder::fragment_sink::{
        Format, FragmentFileSink, SplitPolicy,
    };

    /// What `isofmp4mux` emits, reduced to the flags the sink reads.
    enum Piece {
        Init,
        Fragment(u64),
        Data,
        /// The operator presses split now.
        SplitNow,
    }

    /// Push `pieces` through a sink that splits every 2 s, end with EOS, and
    /// return the files it wrote with their contents.
    fn write(pieces: &[Piece]) -> Vec<(PathBuf, Vec<u8>)> {
        init();
        let dir = tempfile::tempdir().unwrap();
        let location = dir.path().join("f_%05d.mp4");
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("appsrc")
            .property(
                "caps",
                gst::Caps::builder("video/quicktime")
                    .field("variant", "iso-fragmented")
                    .build(),
            )
            .property_from_str("format", "time")
            .build()
            .unwrap();
        let sink = FragmentFileSink::new(
            "sink",
            location.to_str().unwrap(),
            Format::FragmentedMp4,
            SplitPolicy {
                max_duration: Some(gst::ClockTime::from_seconds(2)),
                max_bytes: None,
            },
        );
        pipeline.add_many([&src, sink.upcast_ref()]).unwrap();
        src.link(&sink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let appsrc = src.downcast_ref::<gstreamer_app::AppSrc>().unwrap();
        let mut last_pts = 0;
        for piece in pieces {
            if let Piece::SplitNow = piece {
                // Let what was pushed so far reach the sink first.
                std::thread::sleep(Duration::from_millis(100));
                sink.split_now();
                continue;
            }
            let (bytes, flags, pts): (&[u8], _, u64) = match piece {
                Piece::Init => (
                    b"INIT;",
                    gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER,
                    0,
                ),
                Piece::Fragment(s) => {
                    last_pts = *s;
                    (b"FRAG;", gst::BufferFlags::HEADER, *s)
                }
                Piece::Data => (b"data;", gst::BufferFlags::DELTA_UNIT, last_pts),
                Piece::SplitNow => unreachable!(),
            };
            let mut buffer = gst::Buffer::from_slice(bytes.to_vec());
            {
                let b = buffer.get_mut().unwrap();
                b.set_flags(flags);
                b.set_pts(gst::ClockTime::from_seconds(pts));
            }
            appsrc.push_buffer(buffer).unwrap();
        }
        appsrc.end_of_stream().unwrap();
        let bus = pipeline.bus().unwrap();
        bus.timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Eos])
            .expect("EOS");
        pipeline.set_state(gst::State::Null).unwrap();

        let mut files: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .map(|p| {
                let c = std::fs::read(&p).unwrap();
                (p, c)
            })
            .collect();
        files.sort();
        files
    }

    fn text(files: &[(PathBuf, Vec<u8>)]) -> Vec<String> {
        files
            .iter()
            .map(|(_, c)| String::from_utf8_lossy(c).to_string())
            .collect()
    }

    #[test]
    fn every_file_starts_with_the_init_segment_and_a_fragment() {
        use Piece::*;
        let files = write(&[
            Init,
            Fragment(0),
            Data,
            Fragment(1),
            Data,
            Fragment(2),
            Data,
            Fragment(3),
            Data,
        ]);
        assert_eq!(
            text(&files),
            vec!["INIT;FRAG;data;FRAG;data;", "INIT;FRAG;data;FRAG;data;",]
        );
    }

    /// The muxer's tail at EOS must not end up alone in a file of its own.
    #[test]
    fn a_split_due_at_the_last_fragment_keeps_it_in_the_current_file() {
        use Piece::*;
        let files = write(&[
            Init,
            Fragment(0),
            Data,
            Fragment(1),
            Data,
            Fragment(2),
            Data,
        ]);
        assert_eq!(text(&files), vec!["INIT;FRAG;data;FRAG;data;FRAG;data;"]);
    }

    /// Split now starts the next file at the next fragment, not one later,
    /// so the new file name shows as soon as the muxer allows.
    #[test]
    fn split_now_starts_the_next_file_at_the_next_fragment() {
        use Piece::*;
        let files = write(&[Init, Fragment(0), Data, SplitNow, Fragment(1), Data]);
        assert_eq!(text(&files), vec!["INIT;FRAG;data;", "INIT;FRAG;data;"]);
    }
}

/// A flow stop goes straight to NULL with no EOS. The muxer still holds the
/// fragment in progress then; the stop must have it written, not drop it.
#[test]
fn a_stop_without_eos_still_writes_the_last_fragment() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let _rec = av_recorder(&pipeline, "rec", dir.path(), &[]);
    pipeline.set_state(gst::State::Playing).unwrap();
    let started = Instant::now();
    std::thread::sleep(Duration::from_millis(5500));
    no_errors(&pipeline);
    let ran = started.elapsed();
    let stop = Instant::now();
    pipeline.set_state(gst::State::Null).unwrap();
    let stop_took = stop.elapsed();

    assert!(
        stop_took < Duration::from_secs(2),
        "the stop took {:?}",
        stop_took
    );
    let files = recorder::recordings(dir.path(), "rec");
    assert_eq!(files.len(), 1, "{:?}", files);
    let samples = demux(&files[0]);
    for (kind, pts) in &samples {
        let (_, span) = largest_gap(pts);
        // Fragments are 1 s: without the drain the file ends a fragment short.
        assert!(
            span.nseconds() as u128 + 400_000_000 >= ran.as_nanos(),
            "{} spans {} of a {:?} recording",
            kind,
            span,
            ran
        );
    }
}

/// The stop drain must not fire on a pause: the file goes on after PLAYING.
#[test]
fn a_pause_does_not_end_the_file() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let _rec = av_recorder(&pipeline, "rec", dir.path(), &[]);
    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    pipeline.set_state(gst::State::Paused).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    no_errors(&pipeline);
    finish(&pipeline);

    let files = recorder::recordings(dir.path(), "rec");
    assert_eq!(files.len(), 1, "{:?}", files);
    let samples = demux(&files[0]);
    let video = &samples["video/x-h264"];
    let last = *video.last().unwrap();
    assert!(
        last >= gst::ClockTime::from_seconds(4),
        "the file ended at {} — the pause finished it",
        last
    );
}

/// The same guarantees in Matroska and MPEG-TS, whose muxers are live
/// aggregators too but whose files the sink cuts differently.
mod containers {
    use super::*;

    /// Before GStreamer 1.26 matroskamux is not a live muxer, so the block must
    /// refuse mkv there rather than freeze like the Recorder. Returns whether it
    /// did, having checked the refusal; the behaviour tests run on 1.26 and later.
    fn mkv_refused() -> bool {
        if gst::version() >= (1, 26, 0, 0) {
            return false;
        }
        let props: HashMap<String, PropertyValue> =
            [("container".to_string(), PropertyValue::String("mkv".into()))].into();
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let err = match LiveRecorderBuilder.build("rec", &props, &ctx) {
            Ok(_) => panic!(
                "mkv built on GStreamer {:?}, where matroskamux is not live",
                gst::version()
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("needs GStreamer 1.26") && err.contains("mp4 or mpegts"),
            "the refusal should say what is needed and what to use: {}",
            err
        );
        true
    }

    /// Audio stalls, then video stalls, then the flow stops with no EOS.
    fn stalls_and_stop(container: &str) {
        init();
        if container == "mkv" && mkv_refused() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pipeline = gst::Pipeline::new();
        let (_rec, venc, aenc) = av_recorder(
            &pipeline,
            "rec",
            dir.path(),
            &[("container", PropertyValue::String(container.into()))],
        );
        let start = Instant::now();
        stall(
            &aenc,
            start,
            Duration::from_secs(2),
            Duration::from_millis(4500),
        );
        stall(
            &venc,
            start,
            Duration::from_secs(6),
            Duration::from_secs(10),
        );

        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_millis(6800));
        let at_7s = file_size(&recorder::recordings(dir.path(), "rec"));
        std::thread::sleep(Duration::from_secs(3));
        let at_10s = file_size(&recorder::recordings(dir.path(), "rec"));
        std::thread::sleep(Duration::from_secs(3));
        no_errors(&pipeline);
        let ran = start.elapsed();
        pipeline.set_state(gst::State::Null).unwrap();

        assert!(
            at_10s >= at_7s + 20_000,
            "{}: nothing reached the file while the video was stalled: {} then {} bytes",
            container,
            at_7s,
            at_10s
        );
        let files = recorder::recordings(dir.path(), "rec");
        assert_eq!(files.len(), 1, "{}: {:?}", container, files);
        let samples = demux(&files[0]);
        let audio = &samples["audio/mpeg"];
        let video = &samples["video/x-h264"];
        let (audio_gap, _) = largest_gap(audio);
        let (video_gap, _) = largest_gap(video);
        assert!(
            audio_gap >= gst::ClockTime::from_seconds(2)
                && audio_gap < gst::ClockTime::from_seconds(4),
            "{}: the audio stall should be the audio's only gap, largest {}",
            container,
            audio_gap
        );
        assert!(
            video_gap >= gst::ClockTime::from_mseconds(3500),
            "{}: the video stall should show as a gap, largest {}",
            container,
            video_gap
        );
        // The stop wrote what the muxer still held: the file runs to the end.
        let first = *audio.first().unwrap();
        let last = *audio.last().unwrap();
        assert!(
            (last - first).nseconds() as u128 + 1_000_000_000 >= ran.as_nanos(),
            "{}: audio covers {} of a {:?} run",
            container,
            last - first,
            ran
        );
    }

    /// Each split file plays on its own; Matroska files start at zero.
    fn split(container: &str) {
        init();
        if container == "mkv" && mkv_refused() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pipeline = gst::Pipeline::new();
        let _ = av_recorder(
            &pipeline,
            "rec",
            dir.path(),
            &[
                ("container", PropertyValue::String(container.into())),
                ("max_size_time_secs", PropertyValue::UInt(2)),
            ],
        );
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(7));
        no_errors(&pipeline);
        finish(&pipeline);

        let files = recorder::recordings(dir.path(), "rec");
        assert!(
            files.len() >= 3,
            "{}: expected a file per 2s, got {:?}",
            container,
            files
        );
        for file in &files {
            let samples = demux(file);
            assert!(
                samples.get("video/x-h264").is_some_and(|v| !v.is_empty())
                    && samples.get("audio/mpeg").is_some_and(|a| !a.is_empty()),
                "{} does not play on its own: {:?}",
                file.display(),
                samples
                    .iter()
                    .map(|(k, v)| (k, v.len()))
                    .collect::<Vec<_>>()
            );
            if container == "mkv" {
                let first = samples
                    .values()
                    .filter_map(|v| v.first())
                    .min()
                    .copied()
                    .unwrap();
                assert!(
                    first < gst::ClockTime::from_mseconds(500),
                    "{} starts at {}",
                    file.display(),
                    first
                );
            }
        }
    }

    /// matroskamux labels a recording without video `audio/x-matroska`.
    #[test]
    fn mkv_audio_only_records() {
        init();
        if mkv_refused() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pipeline = gst::Pipeline::new();
        let rec = add_live_recorder(
            &pipeline,
            "rec",
            dir.path(),
            &[
                ("container", PropertyValue::String("mkv".into())),
                ("num_video_tracks", PropertyValue::UInt(0)),
                ("num_audio_tracks", PropertyValue::UInt(1)),
            ],
        );
        recorder::audio_source(&pipeline, -1, true)
            .link(&rec.input("audio_input_0"))
            .unwrap();
        rec.run_setups();
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        no_errors(&pipeline);
        finish(&pipeline);
        let files = recorder::recordings(dir.path(), "rec");
        assert_eq!(files.len(), 1, "{:?}", files);
        assert!(
            demux(&files[0])
                .get("audio/mpeg")
                .is_some_and(|a| !a.is_empty()),
            "{} has no audio",
            files[0].display()
        );
    }

    #[test]
    fn mkv_stalls_and_stop() {
        stalls_and_stop("mkv");
    }

    #[test]
    fn mpegts_stalls_and_stop() {
        stalls_and_stop("mpegts");
    }

    #[test]
    fn mkv_split() {
        split("mkv");
    }

    #[test]
    fn mpegts_split() {
        split("mpegts");
    }
}

/// `ts_passthrough` writes the incoming MPEG-TS as it is, through one `ts_in`
/// pad, the way the Recorder does.
#[test]
fn ts_passthrough_takes_one_ts_input() {
    init();
    let props: HashMap<String, PropertyValue> = [(
        "container".to_string(),
        PropertyValue::String("ts_passthrough".into()),
    )]
    .into();
    let pads = LiveRecorderBuilder.get_external_pads(&props).unwrap();
    assert_eq!(
        pads.inputs
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        vec!["ts_in"]
    );
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let rec = add_live_recorder(
        &pipeline,
        "rec",
        dir.path(),
        &[("container", PropertyValue::String("ts_passthrough".into()))],
    );
    assert!(rec.elements.contains_key("rec:ts_input"));
    assert!(rec.elements.contains_key("rec:multifilesink"));
}

/// 320x240@30 H.264 with a keyframe only every 10 s, unless asked for one.
fn long_gop_video_source(pipeline: &gst::Pipeline) -> gst::Element {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .build()
        .unwrap();
    let enc = gst::ElementFactory::make("x264enc")
        .property("key-int-max", 300u32)
        .property_from_str("tune", "zerolatency")
        .build()
        .unwrap();
    pipeline.add_many([&src, &caps, &enc]).unwrap();
    gst::Element::link_many([&src, &caps, &enc]).unwrap();
    enc
}

/// Each video sample's PTS and whether it is a keyframe, in file order.
fn video_keyframes(path: &Path) -> Vec<(gst::ClockTime, bool)> {
    let pipeline = gst::parse::launch(
        "filesrc name=src ! qtdemux ! video/x-h264 ! fakesink name=s sync=false async=false",
    )
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    // `location` as a property, not in the launch string: the parser reads a
    // Windows path's backslashes as escapes.
    pipeline
        .by_name("src")
        .unwrap()
        .set_property("location", path.to_str().unwrap());
    let frames: Arc<Mutex<Vec<(gst::ClockTime, bool)>>> = Arc::default();
    let sink_frames = Arc::clone(&frames);
    pipeline
        .by_name("s")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            if let Some(gst::PadProbeData::Buffer(b)) = info.data.as_ref() {
                if let Some(pts) = b.pts() {
                    sink_frames
                        .lock()
                        .unwrap()
                        .push((pts, !b.flags().contains(gst::BufferFlags::DELTA_UNIT)));
                }
            }
            gst::PadProbeReturn::Ok
        });
    pipeline.set_state(gst::State::Playing).unwrap();
    pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(10),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).unwrap();
    let mut v = frames.lock().unwrap().clone();
    v.sort();
    v
}

/// Frames lost after the encoder leave deltas whose references never reached
/// the file. After a stall the recording must resume on a keyframe, and soon:
/// the recorder asks the encoder for one rather than wait out a 10 s GOP.
#[test]
fn a_video_that_comes_back_resumes_on_a_keyframe_without_waiting_a_gop() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let rec = add_live_recorder(
        &pipeline,
        "rec",
        dir.path(),
        &[
            ("num_video_tracks", PropertyValue::UInt(1)),
            ("num_audio_tracks", PropertyValue::UInt(1)),
        ],
    );
    let venc = long_gop_video_source(&pipeline);
    let aenc = recorder::audio_source(&pipeline, -1, true);
    venc.link(&rec.input("video_input_0")).unwrap();
    aenc.link(&rec.input("audio_input_0")).unwrap();
    rec.run_setups();
    let start = Instant::now();
    // The encoder goes on encoding; its frames are lost from 2 s to 5 s.
    stall(&venc, start, Duration::from_secs(2), Duration::from_secs(5));

    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(9));
    no_errors(&pipeline);
    finish(&pipeline);

    let files = recorder::recordings(dir.path(), "rec");
    assert_eq!(files.len(), 1, "{:?}", files);
    let frames = video_keyframes(&files[0]);
    // The stall ends at 5 s. The first frame kept after it must be a keyframe,
    // and it must come within a second, not at the encoder's next scheduled
    // keyframe at 10 s.
    let back = frames
        .iter()
        .find(|(pts, _)| *pts > gst::ClockTime::from_mseconds(4500));
    let Some(&(pts, keyframe)) = back else {
        panic!("no video after the stall: the recorder waited for the encoder's own keyframe");
    };
    assert!(
        keyframe,
        "the first video frame after the stall is a delta frame, at {}",
        pts
    );
    assert!(
        pts < gst::ClockTime::from_seconds(6),
        "the video came back only at {}: the recorder waited for the encoder's own keyframe",
        pts
    );
}

/// A stop while a connected track has never carried data, before the
/// keepalive would release it, still writes what the other track recorded.
#[test]
fn a_stop_before_a_dataless_track_is_released_still_writes_the_file() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let pipeline = gst::Pipeline::new();
    let rec = add_live_recorder(
        &pipeline,
        "rec",
        dir.path(),
        &[
            ("num_video_tracks", PropertyValue::UInt(1)),
            ("num_audio_tracks", PropertyValue::UInt(1)),
        ],
    );
    recorder::video_source(&pipeline, -1, true)
        .link(&rec.input("video_input_0"))
        .unwrap();
    let silent = gst::ElementFactory::make("appsrc")
        .property("is-live", true)
        .property_from_str("format", "time")
        .build()
        .unwrap();
    pipeline.add(&silent).unwrap();
    silent.link(&rec.input("audio_input_0")).unwrap();
    rec.run_setups();

    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(4));
    no_errors(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();

    let files = recorder::recordings(dir.path(), "rec");
    assert_eq!(files.len(), 1, "the stop wrote no file: {:?}", files);
    let samples = demux(&files[0]);
    let (_, span) = largest_gap(&samples["video/x-h264"]);
    assert!(
        span >= gst::ClockTime::from_seconds(3),
        "video spans only {}",
        span
    );
}
