//! What one more WHEP viewer costs the server, measured on the real block.
//!
//! Ignored: it needs `whepserversink` and `whepclientsrc` from gst-plugins-rs,
//! spawns consumer processes and runs for about a minute.
//!
//! Builds the WHEP Output block, feeds it `videotestsrc` in a chosen pixel
//! format, measures this process's CPU with no consumers and again with
//! `STROM_BENCH_CONSUMERS` of them, and reports the difference per consumer.
//! Each consumer negotiates a single codec so the arm being measured is known.
//!
//!   STROM_BENCH_FORMAT=RGBA STROM_BENCH_CODEC=H265 \
//!     cargo test --test whep_fanout_bench -- --ignored --nocapture

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use strom::blocks::builtin::whep::WHEPOutputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Cumulative CPU time of a process in seconds. macOS `ps` prints `MM:SS.ss`,
/// Linux `MM:SS`.
fn cpu_seconds(pid: u32) -> f64 {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "time="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .split(':')
        .fold(0.0, |acc, part| {
            acc * 60.0 + part.parse::<f64>().unwrap_or(0.0)
        })
}

/// One consumer, in its own process so its decryption and depayloading do not
/// land in the measurement. `fpsdisplaysink` reports what actually arrived —
/// a session that connects and then receives nothing looks identical from the
/// server side.
fn spawn_consumer(port: u16, codec: &str) -> Child {
    Command::new("gst-launch-1.0")
        .arg("-v")
        .arg("whepclientsrc")
        .arg(format!(
            "signaller::whep-endpoint=http://127.0.0.1:{}/whep/endpoint",
            port
        ))
        .arg(format!("video-codecs=<{}>", codec))
        .arg("audio-codecs=<>")
        .args(["!", "application/x-rtp", "!", "fpsdisplaysink"])
        .args(["video-sink=fakesink", "text-overlay=false", "sync=false"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gst-launch-1.0 whepclientsrc")
}

#[test]
#[ignore]
fn fanout_cpu_per_consumer() {
    gst::init().unwrap();
    strom::gpu::detect_gpu_capabilities();
    for element in ["whepserversink", "videotestsrc", "fpsdisplaysink"] {
        assert!(
            gst::ElementFactory::find(element).is_some(),
            "{} is required for this benchmark",
            element
        );
    }

    let format = env_or("STROM_BENCH_FORMAT", "RGBA");
    let codec = env_or("STROM_BENCH_CODEC", "H265");
    let consumers: usize = env_or("STROM_BENCH_CONSUMERS", "2").parse().unwrap();
    let window: u64 = env_or("STROM_BENCH_WINDOW", "15").parse().unwrap();
    let settle: u64 = env_or("STROM_BENCH_SETTLE", "10").parse().unwrap();

    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(0));
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    let built = WHEPOutputBuilder
        .build("bench", &props, &ctx)
        .expect("build WHEP Output");
    let port = ctx
        .take_whep_endpoints()
        .first()
        .expect("endpoint registered")
        .internal_port;

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    let filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", &format)
                .field("width", 1920i32)
                .field("height", 1080i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .build()
        .unwrap();
    pipeline.add_many([&src, &filter]).unwrap();

    let by_id: HashMap<String, gst::Element> = built.elements.iter().cloned().collect();
    for (_, element) in &built.elements {
        pipeline.add(element).unwrap();
    }
    let video_queue = by_id.get("bench:video_queue").expect("video_queue");
    src.link(&filter).unwrap();
    filter.link(video_queue).unwrap();
    for (from, to) in &built.internal_links {
        let from_el = by_id.get(&from.element_id).expect("link source");
        let to_el = by_id.get(&to.element_id).expect("link sink");
        let src_pad = from_el
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("src pad");
        let name = to.pad_name.as_deref().unwrap_or("sink");
        let sink_pad = to_el
            .static_pad(name)
            .or_else(|| to_el.request_pad_simple(name))
            .expect("sink pad");
        src_pad.link(&sink_pad).expect("link");
    }

    // What each consumer session ends up encoding, and what its own converter
    // had to produce to feed that encoder. Without this the CPU number cannot
    // be attributed.
    let sink = by_id.get("bench:whepserversink").expect("whepserversink");
    let encoders: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let encoders_for_signal = encoders.clone();
    sink.connect("encoder-setup", false, move |args| {
        let encoder: gst::Element = args[3].get().unwrap();
        let factory = encoder
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_default();
        let encoders = encoders_for_signal.clone();
        // Nothing is negotiated yet at setup time, so read the caps later.
        if let Some(pad) = encoder.static_pad("sink") {
            pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let Some(gst::PadProbeData::Event(ref event)) = info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                if event.type_() != gst::EventType::Caps {
                    return gst::PadProbeReturn::Ok;
                }
                let gst::EventView::Caps(caps_event) = event.view() else {
                    return gst::PadProbeReturn::Ok;
                };
                let caps = caps_event.caps().to_string();
                let _ = pad;
                encoders
                    .lock()
                    .unwrap()
                    .push(format!("{} <- {}", factory, caps));
                gst::PadProbeReturn::Remove
            });
        }
        Some(false.to_value())
    });

    pipeline.set_state(gst::State::Playing).unwrap();
    let pid = std::process::id();

    sleep(Duration::from_secs(settle));
    let idle_start = cpu_seconds(pid);
    sleep(Duration::from_secs(window));
    let idle = cpu_seconds(pid) - idle_start;

    let mut procs: Vec<Child> = (0..consumers)
        .map(|_| spawn_consumer(port, &codec))
        .collect();
    sleep(Duration::from_secs(settle));
    let loaded_start = cpu_seconds(pid);
    sleep(Duration::from_secs(window));
    let loaded = cpu_seconds(pid) - loaded_start;

    let mut received = Vec::new();
    for mut child in procs.drain(..) {
        let _ = child.kill();
        let output = child.wait_with_output().expect("consumer output");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        received.push(
            text.split("rendered: ")
                .skip(1)
                .filter_map(|rest| {
                    rest.split(|c: char| !c.is_ascii_digit())
                        .next()
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .max()
                .unwrap_or(0),
        );
    }
    pipeline.set_state(gst::State::Null).unwrap();

    let percent = |seconds: f64| 100.0 * seconds / window as f64;
    let per_consumer = (loaded - idle) / consumers as f64;
    println!("feed format   : {}", format);
    println!("consumer codec: {}", codec);
    println!(
        "consumers     : {} (RTP buffers received: {:?})",
        consumers, received
    );
    println!("idle          : {:.1}% of a core", percent(idle));
    println!("loaded        : {:.1}% of a core", percent(loaded));
    println!("per consumer  : {:.1}% of a core", percent(per_consumer));
    for line in encoders.lock().unwrap().iter() {
        println!("encoder       : {}", line);
    }
}
