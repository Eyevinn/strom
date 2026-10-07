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
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! qtdemux name=d",
        path.display()
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let samples: Arc<Mutex<HashMap<String, Vec<gst::ClockTime>>>> = Arc::default();
    let demux = pipeline.by_name("d").unwrap();
    let pipeline_weak = pipeline.downgrade();
    let samples_for_pads = Arc::clone(&samples);
    demux.connect_pad_added(move |_demux, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let kind = pad
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            .unwrap_or_default();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();
        let samples = Arc::clone(&samples_for_pads);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            if let Some(gst::PadProbeData::Buffer(b)) = info.data.as_ref() {
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
    use strom::blocks::builtin::live_recorder::fragment_sink::{FragmentFileSink, SplitPolicy};

    /// What `isofmp4mux` emits, reduced to the flags the sink reads.
    enum Piece {
        Init,
        Fragment(u64),
        Data,
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
}
