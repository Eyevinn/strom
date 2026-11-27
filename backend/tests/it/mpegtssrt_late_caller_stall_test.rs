//! #1277: a caller that is already dialling when the MPEG-TS/SRT Input starts
//! connects, but with a clock-synced consumer downstream the receiver stops
//! reading: the sender's `packets-sent-dropped` grows as fast as
//! `packets-sent`.
//!
//! Exploratory for now: parameters come from the environment.
//! - `STALL_EARLY_S`: how long the caller runs before the receiver (30).
//! - `STALL_CALLER`: `leaky` (Strom's SRT output, #1259) or `blocking` (a bare
//!   `srtsink`, Strom 0.8.1) (`leaky`).
//! - `STALL_SYNC`: receiver sinks sync (true).
//! - `STALL_RUN_S`: how long to watch after the receiver starts (60).
//! - `STALL_DECODE`: the block's `decode` property (true).
//! - `STALL_RECEIVER_FIRST`: control, start the receiver 3 s before the
//!   caller instead (false).

use crate::common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const INSTANCE_ID: &str = "srt_stall";

const REQUIRED: &[&str] = &[
    "srtsrc",
    "srtsink",
    "tsdemux",
    "mpegtsmux",
    "videotestsrc",
    "x264enc",
    "h264parse",
    "audiotestsrc",
    "avenc_aac",
    "aacparse",
    "queue",
    "fakesink",
];

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn srt_port() -> u16 {
    let spread = (std::process::id() % 500) as u16 * 20;
    (0..1000u16)
        .map(|i| 30_000 + (spread + i) % 10_000)
        .find(|&port| std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok())
        .expect("no free UDP port")
}

struct Receiver {
    pipeline: gst::Pipeline,
    srtsrc: gst::Element,
    video: Arc<AtomicU64>,
    audio: Arc<AtomicU64>,
    /// Running time of the last buffer each sink got, ns (video, audio).
    video_rt: Arc<AtomicI64>,
    audio_rt: Arc<AtomicI64>,
    queues: Vec<gst::Element>,
}

fn running_time(pad: &gst::Pad, pts: gst::ClockTime) -> Option<i64> {
    let seg = pad
        .sticky_event::<gst::event::Segment>(0)?
        .segment()
        .clone()
        .downcast::<gst::ClockTime>()
        .ok()?;
    Some(match seg.to_running_time_full(pts)? {
        gst::Signed::Positive(t) => t.nseconds() as i64,
        gst::Signed::Negative(t) => -(t.nseconds() as i64),
    })
}

fn build_receiver(port: u16, sync: bool) -> Receiver {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://:{}?mode=listener", port)),
    );
    props.insert("latency".to_string(), PropertyValue::Int(125));
    props.insert(
        "decode".to_string(),
        PropertyValue::Bool(env("STALL_DECODE", true)),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = MpegTsSrtInputBuilder
        .build(INSTANCE_ID, &props, &ctx)
        .expect("MPEG-TS/SRT Input block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = by_id[&from.element_id]
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("internal link source pad");
        let sink = by_id[&to.element_id]
            .static_pad(to.pad_name.as_deref().unwrap_or("sink"))
            .expect("internal link sink pad");
        src.link(&sink).expect("link internal pads");
    }
    let srtsrc = by_id[&format!("{}:srtsrc", INSTANCE_ID)].clone();

    let video = Arc::new(AtomicU64::new(0));
    let audio = Arc::new(AtomicU64::new(0));
    let video_rt = Arc::new(AtomicI64::new(i64::MIN));
    let audio_rt = Arc::new(AtomicI64::new(i64::MIN));
    let mut queues = vec![];
    for (counter, last_rt, id) in [
        (
            video.clone(),
            video_rt.clone(),
            format!("{}:video_output", INSTANCE_ID),
        ),
        (
            audio.clone(),
            audio_rt.clone(),
            format!("{}:audio_output_0", INSTANCE_ID),
        ),
    ] {
        let queue = gst::ElementFactory::make("queue").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", sync)
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add_many([&queue, &sink]).unwrap();
        by_id[&id].link(&queue).expect("link output -> queue");
        queue.link(&sink).unwrap();
        // Test code: a segment lookup in a buffer probe is fine here.
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                counter.fetch_add(1, Ordering::Relaxed);
                if let Some(rt) = info
                    .buffer()
                    .and_then(|b| b.pts())
                    .and_then(|pts| running_time(pad, pts))
                {
                    last_rt.store(rt, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
        queues.push(queue);
    }
    Receiver {
        pipeline,
        srtsrc,
        video,
        audio,
        video_rt,
        audio_rt,
        queues,
    }
}

fn start_caller(port: u16, leaky: bool) -> gst::Pipeline {
    let queue = if leaky {
        "queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=3000000000 !"
    } else {
        ""
    };
    let caller = gst::parse::launch(&format!(
        "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=1280,height=720,framerate=30/1 \
         ! x264enc tune=zerolatency speed-preset=ultrafast bitrate=6000 key-int-max=25 \
         ! h264parse ! mux. \
         audiotestsrc is-live=true ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! mux. \
         mpegtsmux name=mux alignment=7 ! {queue} \
         srtsink name=caller uri=srt://127.0.0.1:{port}?mode=caller&latency=200 wait-for-connection=true"
    ))
    .expect("caller pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();
    caller.set_state(gst::State::Playing).expect("caller plays");
    caller
}

fn caller_stats(caller: &gst::Pipeline) -> (u64, u64) {
    let Some(sink) = caller.by_name("caller") else {
        return (0, 0);
    };
    let s: gst::Structure = sink.property("stats");
    // A caller reports its one connection at the top level.
    let get = |name: &str| -> u64 {
        s.get::<u64>(name)
            .ok()
            .or_else(|| s.get::<i64>(name).ok().map(|v| v as u64))
            .or_else(|| s.get::<i32>(name).ok().map(|v| v as u64))
            .unwrap_or(0)
    };
    (get("packets-sent"), get("packets-sent-dropped"))
}

#[test]
#[ignore = "exploratory #1277 reproduction, run by hand"]
fn late_caller_with_synced_consumer() {
    gst::init().unwrap();
    common::init_static_plugins();
    if !common::plugins_available(REQUIRED) {
        panic!("missing elements");
    }
    let early = env("STALL_EARLY_S", 30u64);
    let leaky = env("STALL_CALLER", "leaky".to_string()) == "leaky";
    let sync = env("STALL_SYNC", true);
    let run = env("STALL_RUN_S", 60u64);

    let receiver_first = env("STALL_RECEIVER_FIRST", false);
    eprintln!(
        "case: early={early}s caller={} sync={sync} receiver_first={receiver_first} gst={}",
        if leaky { "leaky" } else { "blocking" },
        gst::version_string()
    );

    let port = srt_port();
    let r = build_receiver(port, sync);
    let caller = if receiver_first {
        r.pipeline
            .set_state(gst::State::Playing)
            .expect("receiver plays");
        std::thread::sleep(Duration::from_secs(3));
        start_caller(port, leaky)
    } else {
        let caller = start_caller(port, leaky);
        std::thread::sleep(Duration::from_secs(early));
        r.pipeline
            .set_state(gst::State::Playing)
            .expect("receiver plays");
        caller
    };
    let t0 = Instant::now();
    let lead_ms = |last: &AtomicI64| -> String {
        let last = last.load(Ordering::Relaxed);
        match r.pipeline.current_running_time() {
            Some(now) if last != i64::MIN => {
                format!("{:>7}", (last - now.nseconds() as i64) / 1_000_000)
            }
            _ => "      -".to_string(),
        }
    };
    let mut prev = (0, 0);
    let mut sent_total = 0u64;
    let mut dropped_total = 0u64;
    for step in 1..=(run / 5) {
        std::thread::sleep(Duration::from_secs(5));
        let (sent, dropped) = caller_stats(&caller);
        if step == 1 {
            if let Some(sink) = caller.by_name("caller") {
                eprintln!(
                    "caller stats: {:?}",
                    sink.property::<gst::Structure>("stats")
                );
            }
        }
        let levels: Vec<String> = r
            .queues
            .iter()
            .map(|q| format!("{}ms", q.property::<u64>("current-level-time") / 1_000_000))
            .collect();
        // Counters reset on a reconnect.
        let (ds, dd) = if sent < prev.0 {
            (sent, dropped)
        } else {
            (sent - prev.0, dropped.saturating_sub(prev.1))
        };
        sent_total += ds;
        dropped_total += dd;
        eprintln!(
            "+{:>3}s sent {:>7} (+{:>6}) dropped {:>7} (+{:>6})  rx video {:>5} lead {} ms, audio {:>5} lead {} ms, queues {}",
            step * 5,
            sent,
            ds,
            dropped,
            dd,
            r.video.load(Ordering::Relaxed),
            lead_ms(&r.video_rt),
            r.audio.load(Ordering::Relaxed),
            lead_ms(&r.audio_rt),
            levels.join("/"),
        );
        prev = (sent, dropped);
    }
    eprintln!(
        "RESULT early={early} caller={} sync={sync} receiver_first={receiver_first}: sent {sent_total} dropped {dropped_total} ({:.1} %)",
        if leaky { "leaky" } else { "blocking" },
        100.0 * dropped_total as f64 / sent_total.max(1) as f64
    );
    eprintln!("elapsed {:?}", t0.elapsed());
    if let Some(sink) = caller.by_name("caller") {
        let _ = sink.set_state(gst::State::Null);
    }
    caller.set_state(gst::State::Null).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let _ = r.srtsrc.set_state(gst::State::Null);
    r.pipeline.set_state(gst::State::Null).unwrap();
}
