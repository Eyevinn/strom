//! A flow stop must finish every recorder's current file.
//!
//! `PipelineManager::stop()` takes the pipeline to NULL, which discards whatever
//! has not reached the muxer, and the muxer never writes its final index. An mp4
//! was left with an `mdat` of size 0 and the moov from its last periodic update,
//! seconds short of what was recorded, with its tracks disagreeing. The recorder
//! now sends EOS into its tracks before NULL and `stop()` waits for the file to be
//! finished, for a bounded time.
//!
//! Each test drives a real flow through `PipelineManager`, because the defect is
//! in the stop path and not in the recorder's elements:
//! - a live recording stopped mid-stream is finished, at its full length;
//! - a recording that cannot finish (its file write never returns) does not hold
//!   the stop past the drain timeout;
//! - a recorder that never had data does not delay the stop at all.

pub mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuilder, BlockRegistry};
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const REQUIRED: &[&str] = &[
    "splitmuxsink",
    "mp4mux",
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

/// `stop()`'s drain timeout is 5 s. A stop that waits for it and then goes to
/// NULL finishes well inside this; one that waits for the file forever does not.
const BOUNDED_STOP: Duration = Duration::from_secs(15);

fn element(id: &str, element_type: &str, props: &[(&str, PropertyValue)]) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: element_type.to_string(),
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn link(from: &str, to: &str) -> strom_types::Link {
    strom_types::Link {
        from: from.to_string(),
        to: to.to_string(),
    }
}

/// A flow with an mp4 recorder `rec` (one video, one audio track) writing to
/// `<media_root>/recordings`. `fed` connects live H.264 and AAC sources to it;
/// otherwise both inputs are connected to live sources that never send data.
fn recorder_flow(name: &str, fed: bool) -> strom_types::Flow {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("container".into(), PropertyValue::String("mp4".into()));
    props.insert("num_video_tracks".into(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".into(), PropertyValue::UInt(1));
    props.insert(
        "output_dir".into(),
        PropertyValue::String("recordings".into()),
    );
    props.insert("filename_prefix".into(), PropertyValue::String(name.into()));

    let mut flow = strom_types::Flow::new(name);
    flow.blocks.push(strom_types::BlockInstance {
        id: "rec".to_string(),
        block_definition_id: "builtin.recorder".to_string(),
        name: None,
        properties: props.clone(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: RecorderBuilder.get_external_pads(&props),
    });

    if fed {
        flow.elements.extend([
            element(
                "vsrc",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            // zerolatency: x264's default lookahead holds about a second of
            // video in the encoder, where no recorder EOS can reach it.
            element(
                "venc",
                "x264enc",
                &[
                    ("key-int-max", PropertyValue::UInt(10)),
                    ("tune", PropertyValue::String("zerolatency".into())),
                ],
            ),
            element(
                "asrc",
                "audiotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element("aenc", "avenc_aac", &[]),
        ]);
        flow.links.extend([
            link("vsrc:src", "venc:sink"),
            link("venc:src", "rec:video_in_0"),
            link("asrc:src", "aenc:sink"),
            link("aenc:src", "rec:audio_in_0"),
        ]);
    } else {
        let silent = [
            ("is-live", PropertyValue::Bool(true)),
            ("format", PropertyValue::String("time".into())),
        ];
        flow.elements.extend([
            element("vsrc", "appsrc", &silent),
            element("asrc", "appsrc", &silent),
        ]);
        flow.links.extend([
            link("vsrc:src", "rec:video_in_0"),
            link("asrc:src", "rec:audio_in_0"),
        ]);
    }
    flow
}

fn start(flow: &strom_types::Flow, media_root: &Path, registry: &BlockRegistry) -> PipelineManager {
    let mut manager = PipelineManager::new(
        flow,
        EventBroadcaster::with_capacity(16),
        registry,
        vec![],
        "all".to_string(),
        None,
        media_root.to_path_buf(),
        Arc::new(Mutex::new(HashMap::new())),
    )
    .expect("PipelineManager builds");
    manager.start().expect("pipeline starts");
    manager
}

/// Wait up to `timeout` for the first buffer on the recorder's video input, and
/// return when it arrived. Only once data flows is there a file to finish.
fn first_video_buffer(manager: &PipelineManager, timeout: Duration) -> Instant {
    let input = manager
        .find_gst_element("rec:video_input_0")
        .expect("recorder video input in the pipeline");
    let pad = input.static_pad("src").expect("identity src pad");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        let _ = tx.try_send(Instant::now());
        gst::PadProbeReturn::Remove
    });
    rx.recv_timeout(timeout)
        .expect("no video reached the recorder")
}

fn recordings(media_root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(media_root.join("recordings"))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    files
}

/// The declared size of the top-level `mdat` box, if the file has one. 0 means
/// "to end of file": mp4mux writes the real size only when it finishes the file.
fn mdat_size(path: &Path) -> Option<u64> {
    let data = std::fs::read(path).expect("read recording");
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as u64;
        let kind = &data[offset + 4..offset + 8];
        let (size, header) = if size == 1 {
            let large = u64::from_be_bytes(data[offset + 8..offset + 16].try_into().unwrap());
            (large, 16)
        } else {
            (size, 8)
        };
        if kind == b"mdat" {
            return Some(size);
        }
        if size < header as u64 {
            break;
        }
        offset += size as usize;
    }
    None
}

/// How far each track of an mp4 runs, as qtdemux reads it: the end of its last
/// sample. Returns (video, audio). The queues matter: qtdemux feeds both sinks
/// from one thread, which a prerolling sink would otherwise park.
fn track_ends(path: &Path) -> (Duration, Duration) {
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! qtdemux name=d \
         d.video_0 ! queue ! fakesink name=v sync=false \
         d.audio_0 ! queue ! fakesink name=a sync=false",
        path.display()
    ))
    .expect("demux pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();

    let end_of = |name: &str| {
        let end = Arc::new(Mutex::new(gst::ClockTime::ZERO));
        let end_in_probe = Arc::clone(&end);
        pipeline
            .by_name(name)
            .and_then(|sink| sink.static_pad("sink"))
            .expect("fakesink pad")
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer() {
                    let stop =
                        buffer.pts().unwrap_or_default() + buffer.duration().unwrap_or_default();
                    let mut end = end_in_probe.lock().unwrap();
                    *end = (*end).max(stop);
                }
                gst::PadProbeReturn::Ok
            });
        end
    };
    let video = end_of("v");
    let audio = end_of("a");

    pipeline
        .set_state(gst::State::Playing)
        .expect("play recording");
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("recording read to the end");
    pipeline.set_state(gst::State::Null).unwrap();
    if let gst::MessageView::Error(err) = msg.view() {
        panic!("reading {} back failed: {}", path.display(), err.error());
    }

    let to_duration =
        |t: &Arc<Mutex<gst::ClockTime>>| Duration::from_nanos(t.lock().unwrap().nseconds());
    (to_duration(&video), to_duration(&audio))
}

/// Run `stop()` off the test thread and return how long it took, failing the
/// test rather than hanging it if the stop never comes back.
fn timed_stop(mut manager: PipelineManager) -> Duration {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let result = manager.stop();
        let _ = tx.send((result, started.elapsed()));
        // Keep the manager's Drop on this thread too: it sets NULL again.
        drop(manager);
    });
    let (result, took) = rx
        .recv_timeout(BOUNDED_STOP)
        .unwrap_or_else(|_| panic!("stop() did not return within {:?}", BOUNDED_STOP));
    result.expect("pipeline stops");
    took
}

/// Recording stopped mid-stream: the file must be finished (a real `mdat`
/// size) and run as long as data was recorded, on both tracks.
///
/// Without the drain the `mdat` size is 0 and both tracks end at the last
/// periodic moov update, 2 s into a recording of 3.5 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_recording_is_finished_at_its_full_length() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let media_root = tempfile::tempdir().expect("tempdir");
    let registry_file = tempfile::NamedTempFile::new().expect("registry file");
    let registry = BlockRegistry::new(registry_file.path());

    let manager = start(
        &recorder_flow("finished", true),
        media_root.path(),
        &registry,
    );
    let first_buffer = first_video_buffer(&manager, Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(3500));
    let recorded = first_buffer.elapsed();
    timed_stop(manager);

    let files = recordings(media_root.path());
    assert_eq!(files.len(), 1, "expected one recording, got {:?}", files);
    let file = &files[0];

    assert!(
        mdat_size(file).is_some_and(|size| size > 0),
        "the stop did not finish the file: mdat size {:?}",
        mdat_size(file)
    );

    let (video, audio) = track_ends(file);
    let shortest = recorded - Duration::from_millis(500);
    assert!(
        video >= shortest && audio >= shortest,
        "recorded {:?} but the file holds video to {:?} and audio to {:?}",
        recorded,
        video,
        audio
    );
    let spread = video.abs_diff(audio);
    assert!(
        spread < Duration::from_millis(300),
        "tracks disagree: video ends at {:?}, audio at {:?}",
        video,
        audio
    );
}

/// A recording whose file write never returns cannot finish, and the stop must
/// not wait for it: it gives up after the drain timeout and goes to NULL.
///
/// The file sink's input is blocked, as a stuck disk would block it, so the EOS
/// the stop sends can never reach the end of the recording.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recording_that_cannot_finish_does_not_hold_the_stop() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let media_root = tempfile::tempdir().expect("tempdir");
    let registry_file = tempfile::NamedTempFile::new().expect("registry file");
    let registry = BlockRegistry::new(registry_file.path());

    let manager = start(&recorder_flow("stuck", true), media_root.path(), &registry);
    first_video_buffer(&manager, Duration::from_secs(10));
    std::thread::sleep(Duration::from_secs(1));

    let file_sink_pad = manager
        .find_gst_element("rec:splitmuxsink")
        .expect("splitmuxsink in the pipeline")
        .clone()
        .downcast::<gst::Bin>()
        .expect("splitmuxsink is a bin")
        .iterate_sinks()
        .into_iter()
        .filter_map(Result::ok)
        .find_map(|sink| sink.static_pad("sink"))
        .expect("splitmuxsink has created its file sink");
    file_sink_pad
        .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
            gst::PadProbeReturn::Ok
        })
        .expect("block the file sink");

    let took = timed_stop(manager);
    // A stop this quick did not wait for the drain at all, so the block above
    // was never in its way and the test proved nothing.
    assert!(
        took >= Duration::from_secs(4),
        "stop took only {:?}: the recording was not stuck",
        took
    );
}

/// A recorder whose inputs never carried data has no file, and the stop must
/// not wait for one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorder_that_never_had_data_does_not_delay_the_stop() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let media_root = tempfile::tempdir().expect("tempdir");
    let registry_file = tempfile::NamedTempFile::new().expect("registry file");
    let registry = BlockRegistry::new(registry_file.path());

    let manager = start(&recorder_flow("idle", false), media_root.path(), &registry);
    std::thread::sleep(Duration::from_secs(1));

    let took = timed_stop(manager);
    assert!(
        took < Duration::from_secs(2),
        "stopping a recorder with nothing recorded took {:?}",
        took
    );
    assert!(
        recordings(media_root.path()).is_empty(),
        "a recorder with no data wrote a file"
    );
}

/// A split still closing the previous file when the stop comes must not be taken
/// for the end of the recording. splitmuxsink closes a file at a split with an
/// EOS through the muxer too; a stop that stopped waiting at that one would take
/// the pipeline to NULL before the new file is finished.
///
/// The split's EOS is held at the muxer input until the stop has started, so it
/// reaches the muxer output while the stop is waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_split_in_progress_is_not_taken_for_the_end() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let media_root = tempfile::tempdir().expect("tempdir");
    let registry_file = tempfile::NamedTempFile::new().expect("registry file");
    let registry = BlockRegistry::new(registry_file.path());

    let manager = start(&recorder_flow("split", true), media_root.path(), &registry);
    first_video_buffer(&manager, Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(1500));

    let splitmuxsink = manager
        .find_gst_element("rec:splitmuxsink")
        .expect("splitmuxsink in the pipeline")
        .clone();
    let mux = splitmuxsink.property::<gst::Element>("muxer");
    let (held_tx, held) = std::sync::mpsc::sync_channel(1);
    let holding = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for pad in mux.sink_pads() {
        let held_tx = held_tx.clone();
        let holding = Arc::clone(&holding);
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            let is_eos = matches!(&info.data, Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::Eos);
            if is_eos && !holding.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = held_tx.try_send(());
                std::thread::sleep(Duration::from_millis(500));
            }
            gst::PadProbeReturn::Ok
        });
    }

    splitmuxsink.emit_by_name::<()>("split-now", &[]);
    held.recv_timeout(Duration::from_secs(5))
        .expect("the split never sent EOS to the muxer");
    timed_stop(manager);

    let files = recordings(media_root.path());
    assert_eq!(
        files.len(),
        2,
        "expected the split and the stop to leave two files, got {:?}",
        files
    );
    for file in &files {
        assert!(
            mdat_size(file).is_some_and(|size| size > 0),
            "{} was not finished: mdat size {:?}",
            file.display(),
            mdat_size(file)
        );
    }
}
