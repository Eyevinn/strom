//! A WHIP Input slot must notice when a running session's video loses data that
//! only a keyframe can repair, and notice when a keyframe has repaired it.
//!
//! The session asks the publisher for a keyframe while the slot's damage flag
//! is up, so the flag is the whole contract between the two halves: it has to
//! rise on a loss the jitterbuffer passed through, stay down on healthy video
//! (every raised flag is a forced keyframe, a bitrate spike), and fall again on
//! the next keyframe.
//!
//! The test builds the real slot chain and feeds it H.264 RTP the way the
//! session bridge does, with one packet missing mid-stream.

pub mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use strom::blocks::builtin::whip::build_whipserversrc;
use strom::blocks::BlockBuildContext;
use strom::gst::keyframe_request::{VideoDamage, SILENT_DECODER_FRAMES};
use strom_types::PropertyValue;

/// `whipserversrc` is not needed: the slot chain is core GStreamer plus the
/// RTP depayloader and an H.264 decoder that `decodebin` finds.
const REQUIRED: &[&str] = &[
    "appsrc",
    "appsink",
    "decodebin",
    "videoconvert",
    "tee",
    "videotestsrc",
    "x264enc",
    "rtph264pay",
    "rtph264depay",
    "h264parse",
];

/// Frames encoded for the test stream.
const FRAMES: i32 = 90;

/// One RTP packet, with the index of the frame it carries.
struct Packet {
    buffer: gst::Buffer,
    frame: usize,
}

struct Stream {
    caps: gst::Caps,
    packets: Vec<Packet>,
    /// Frame indices x264enc emitted as keyframes.
    keyframes: Vec<usize>,
}

/// Encode `FRAMES` frames to H.264 RTP, a keyframe every `gop` frames. A small
/// MTU spreads each frame over several packets, as a browser's 1200-byte
/// packets do at real resolutions, so one missing packet leaves a frame
/// incomplete rather than absent. `x264_extra` adds encoder properties.
fn encode(gop: u32, x264_extra: &str) -> Stream {
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc num-buffers={FRAMES} pattern=ball \
         ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 \
         ! x264enc name=enc key-int-max={gop} bframes=0 tune=zerolatency speed-preset=ultrafast {x264_extra} \
         ! rtph264pay config-interval=-1 mtu=300 \
         ! appsink name=sink sync=false"
    ))
    .expect("encode pipeline parses")
    .downcast::<gst::Pipeline>()
    .unwrap();

    let keyframe_pts: Arc<Mutex<Vec<gst::ClockTime>>> = Arc::default();
    {
        let keyframe_pts = keyframe_pts.clone();
        pipeline
            .by_name("enc")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                    if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                        keyframe_pts.lock().unwrap().push(buffer.pts().unwrap());
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }

    let sink: gst_app::AppSink = pipeline.by_name("sink").unwrap().downcast().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let mut caps = None;
    let mut raw = Vec::new();
    while let Ok(sample) = sink.pull_sample() {
        caps.get_or_insert_with(|| sample.caps().unwrap().to_owned());
        raw.push(sample.buffer().unwrap().to_owned());
    }
    pipeline.set_state(gst::State::Null).unwrap();

    // x264enc offsets its timestamps (by 1000 hours), so index frames from
    // the first one.
    let first_pts = raw
        .iter()
        .filter_map(|b| b.pts())
        .min()
        .expect("timestamped RTP");
    let frame_duration = gst::ClockTime::SECOND / 30;
    let frame_of = |pts: gst::ClockTime| {
        ((pts - first_pts).nseconds() as f64 / frame_duration.nseconds() as f64).round() as usize
    };
    let keyframes = keyframe_pts
        .lock()
        .unwrap()
        .iter()
        .map(|p| frame_of(*p))
        .collect();
    let packets = raw
        .into_iter()
        .map(|buffer| Packet {
            frame: frame_of(buffer.pts().unwrap()),
            buffer,
        })
        .collect();
    Stream {
        caps: caps.expect("the encoder produced RTP"),
        packets,
        keyframes,
    }
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The slot's video chain, running, with its damage flag and a count of the
/// frames it decoded.
struct Slot {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    damage: Arc<Vec<VideoDamage>>,
    decoded: Arc<AtomicUsize>,
}

impl Slot {
    fn build() -> Slot {
        let instance_id = "whip_in";
        let mut props: HashMap<String, PropertyValue> = HashMap::new();
        props.insert(
            "mode".to_string(),
            PropertyValue::String("video".to_string()),
        );
        props.insert("max_sessions".to_string(), PropertyValue::Int(1));
        props.insert("decode".to_string(), PropertyValue::Bool(true));
        props.insert(
            "endpoint_id".to_string(),
            PropertyValue::String("video-damage".to_string()),
        );

        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let built =
            build_whipserversrc(instance_id, &props, &ctx).expect("WHIP Input block builds");
        let (_, config) = ctx
            .take_whip_endpoint_configs()
            .into_iter()
            .next()
            .expect("the block registers its endpoint config");

        let pipeline = gst::Pipeline::new();
        let mut by_id: HashMap<String, gst::Element> = HashMap::new();
        for (id, element) in &built.elements {
            pipeline.add(element).expect("add block element");
            by_id.insert(id.clone(), element.clone());
        }
        for (from, to) in &built.internal_links {
            let src = &by_id[&from.element_id];
            let sink = &by_id[&to.element_id];
            match (&from.pad_name, &to.pad_name) {
                (Some(src_pad), Some(sink_pad)) => {
                    src.static_pad(src_pad)
                        .unwrap()
                        .link(&sink.static_pad(sink_pad).unwrap())
                        .expect("internal pad link");
                }
                _ => src.link(sink).expect("internal element link"),
            }
        }

        let appsrc: gst_app::AppSrc = by_id[&format!("{}:appsrc_video_0", instance_id)]
            .clone()
            .downcast()
            .expect("appsrc_video_0 is an appsrc");
        let tee = &by_id[&format!("{}:video_out_tee_0", instance_id)];
        let fakesink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add(&fakesink).unwrap();
        tee.link(&fakesink).unwrap();

        let decoded = Arc::new(AtomicUsize::new(0));
        {
            let decoded = decoded.clone();
            fakesink.static_pad("sink").unwrap().add_probe(
                gst::PadProbeType::BUFFER,
                move |_pad, _info| {
                    decoded.fetch_add(1, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                },
            );
        }

        pipeline.set_state(gst::State::Playing).unwrap();
        // What `WhipEndpointConfig::allocate_slot` does when a session claims
        // the slot: the decodebin is built locked so an idle slot cannot hold
        // the pipeline out of PLAYING.
        let decodebin = &by_id[&format!("{}:decodebin_video_0", instance_id)];
        decodebin.set_locked_state(false);
        decodebin.sync_state_with_parent().unwrap();

        Slot {
            pipeline,
            appsrc,
            damage: config.video_damage.clone(),
            decoded,
        }
    }

    fn push(&self, stream: &Stream, packets: impl Iterator<Item = usize>) {
        for i in packets {
            let sample = gst::Sample::builder()
                .buffer(&stream.packets[i].buffer)
                .caps(&stream.caps)
                .build();
            self.appsrc
                .push_sample(&sample)
                .expect("slot appsrc takes RTP");
        }
    }

    fn damaged(&self) -> bool {
        self.damage[0].is_damaged()
    }

    fn check_bus(&self) {
        let bus = self.pipeline.bus().unwrap();
        while let Some(msg) = bus.pop() {
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
}

impl Drop for Slot {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[test]
fn a_lost_packet_damages_the_slot_until_the_next_keyframe() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let stream = encode(45, "");
    assert_eq!(
        stream.keyframes.first(),
        Some(&0),
        "the stream opens on a keyframe"
    );
    let repair = *stream
        .keyframes
        .iter()
        .find(|&&k| k > 20)
        .expect("a second keyframe to repair the loss");
    // A delta frame well inside the first GOP, spread over several packets.
    let lossy_frame = 15;
    let lossy: Vec<usize> = (0..stream.packets.len())
        .filter(|&i| stream.packets[i].frame == lossy_frame)
        .collect();
    assert!(
        lossy.len() >= 3,
        "frame {lossy_frame} spans {} packet(s)",
        lossy.len()
    );
    let lost = lossy[lossy.len() / 2];
    let first_of = |frame: usize| {
        (0..stream.packets.len())
            .find(|&i| stream.packets[i].frame == frame)
            .unwrap()
    };

    let slot = Slot::build();

    // Healthy video: decodes, and is not damaged.
    slot.push(&stream, 0..lost);
    wait_for(
        "the first frames to decode",
        Duration::from_secs(10),
        || {
            slot.check_bus();
            slot.decoded.load(Ordering::Relaxed) >= lossy_frame - 2
        },
    );
    assert!(
        !slot.damaged(),
        "a session that opened on a keyframe and lost nothing must not be damaged"
    );

    // One packet missing: damaged within a few frames. Few enough that a
    // decoder rejecting everything after the loss could not account for it
    // (that is the silent-decoder signal, which needs SILENT_DECODER_FRAMES):
    // this is the gap itself.
    let soon_after = lossy_frame + 5;
    assert!(soon_after - lossy_frame < SILENT_DECODER_FRAMES as usize);
    slot.push(&stream, lost + 1..first_of(soon_after));
    wait_for(
        "the loss to damage the slot",
        Duration::from_secs(10),
        || {
            slot.check_bus();
            slot.damaged()
        },
    );

    // It stays damaged through the rest of the GOP, because every frame after
    // the loss references what was lost.
    slot.push(&stream, first_of(soon_after)..first_of(repair));
    std::thread::sleep(Duration::from_millis(200));
    assert!(slot.damaged(), "only a keyframe repairs a gap");

    // The next keyframe repairs it.
    slot.push(&stream, first_of(repair)..stream.packets.len());
    wait_for(
        "the keyframe to repair the slot",
        Duration::from_secs(10),
        || {
            slot.check_bus();
            !slot.damaged()
        },
    );
}

#[test]
fn healthy_video_never_damages_the_slot() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let stream = encode(45, "");
    let slot = Slot::build();

    // Watch the flag for the whole stream, not just at the end: one raised
    // flag would have been a forced keyframe.
    let ever_damaged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let watcher = {
        let damage = slot.damage.clone();
        let ever_damaged = ever_damaged.clone();
        let watching = watching.clone();
        std::thread::spawn(move || {
            while watching.load(Ordering::Relaxed) {
                if damage[0].is_damaged() {
                    ever_damaged.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };

    slot.push(&stream, 0..stream.packets.len());
    wait_for("the stream to decode", Duration::from_secs(10), || {
        slot.check_bus();
        slot.decoded.load(Ordering::Relaxed) >= FRAMES as usize - 5
    });
    watching.store(false, Ordering::Relaxed);
    watcher.join().unwrap();
    assert!(!ever_damaged.load(Ordering::Relaxed));
}

/// The keyframe sent to repair a loss is several times the size of a delta
/// frame, so on a lossy link it is the frame most likely to lose a packet. With
/// several slices per picture (software encoders such as OpenH264) the
/// depayloader still passes on the slices it got, flagged as a keyframe, and
/// `avdec_h264` conceals the rest and keeps producing. That keyframe must not
/// count as the repair.
#[test]
fn a_keyframe_that_lost_a_packet_does_not_repair_the_slot() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let stream = encode(30, "threads=4 sliced-threads=true");
    assert_eq!(
        stream.keyframes,
        vec![0, 30, 60],
        "a keyframe every 30 frames"
    );
    let packets_of = |frame: usize| -> Vec<usize> {
        (0..stream.packets.len())
            .filter(|&i| stream.packets[i].frame == frame)
            .collect()
    };
    let first_of = |frame: usize| packets_of(frame)[0];
    let lossy = packets_of(25);
    let lost = lossy[lossy.len() / 2];
    let broken = packets_of(30);
    assert!(
        broken.len() >= 6,
        "keyframe spans {} packet(s)",
        broken.len()
    );
    // Well past the parameter sets and the first slices.
    let lost_from_keyframe = broken[broken.len() * 2 / 3];

    let slot = Slot::build();
    slot.push(&stream, 0..lost);
    wait_for(
        "the first frames to decode",
        Duration::from_secs(10),
        || {
            slot.check_bus();
            slot.decoded.load(Ordering::Relaxed) >= 20
        },
    );
    slot.push(&stream, lost + 1..lost_from_keyframe);
    wait_for(
        "the loss to damage the slot",
        Duration::from_secs(10),
        || {
            slot.check_bus();
            slot.damaged()
        },
    );

    // The damaged keyframe and the frames after it. Few enough since the loss
    // that the silent-decoder signal cannot be what keeps the flag up.
    let checked_until = 40;
    assert!(checked_until - 25 < SILENT_DECODER_FRAMES as usize);
    for frame in 31..=checked_until {
        let next = first_of(frame + 1);
        let from = if frame == 31 {
            lost_from_keyframe + 1
        } else {
            first_of(frame)
        };
        slot.push(&stream, from..next);
        std::thread::sleep(Duration::from_millis(10));
        slot.check_bus();
        assert!(
            slot.damaged(),
            "frame {frame}: a keyframe missing a packet repaired the slot"
        );
    }

    // The next intact keyframe repairs it.
    slot.push(&stream, first_of(checked_until + 1)..stream.packets.len());
    wait_for(
        "the intact keyframe to repair the slot",
        Duration::from_secs(10),
        || {
            slot.check_bus();
            !slot.damaged()
        },
    );
}
