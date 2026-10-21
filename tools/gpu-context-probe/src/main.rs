//! Measures how GPU video memory crosses a pipeline boundary, on whatever host
//! it runs on. Strom has two boundaries that carry decoded video between
//! pipelines: the Media Player's internal decode pipeline (appsink → appsrc)
//! and Inter Output → Inter Input between flows (intersink → intersrc).
//!
//! Subcommands:
//!
//! * `cross`    one case: producer pipeline A → bridge → consumer pipeline B
//! * `encoders` which installed video encoders take which memory directly
//! * `webrtc`   frames in one memory into a real `whepserversink`, watched by a
//!              `whepclientsrc`: which encoder webrtcsink picks, and whether
//!              video reaches the viewer
//! * `clip`     write a video-only H.264 clip for the `decode` cases
//! * `all`      every case this host supports, each in its own process
//!
//! Every line of output is `key=value` pairs, so results from several hosts can
//! be pasted side by side.

use gst::prelude::*;
use gst_gl::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_gl as gst_gl;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const CLIP: &str = "gpu-context-probe-clip.mp4";
const MEMORIES: [&str; 5] = ["system", "gl", "cuda", "d3d12", "d3d11"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    gst::init().expect("gst::init");
    gstrsinter::plugin_register_static().expect("register rsinter");
    gstrswebrtc::plugin_register_static().expect("register rswebrtc");

    let code = match args.first().map(String::as_str) {
        Some("cross") => cross(&Opts::parse(&args[1..])),
        Some("encoders") => encoders(&Opts::parse(&args[1..])),
        Some("webrtc") => webrtc(&Opts::parse(&args[1..])),
        Some("clip") => clip(),
        Some("all") => all(&Opts::parse(&args[1..])),
        _ => {
            eprintln!(
                "usage: gpu-context-probe <cross|encoders|clip|all> [options]\n\
                 cross:    --memory system|gl|cuda|d3d12|d3d11|decode  --bridge app|inter\n\
                 \x20         --share none|forward  --start a|b  [--producer LAUNCH] [--consumer LAUNCH]\n\
                 \x20         [--frames N] [--timeout SECS] [--verbose]\n\
                 encoders: [--run] [--frames N]\n\
                 webrtc:   --memory system|gl|cuda|d3d12|d3d11  --codec h264|h265|vp9|av1 [--timeout SECS]\n\
                 all:      [--frames N] [--timeout SECS]"
            );
            2
        }
    };
    std::process::exit(code);
}

#[derive(Clone)]
struct Opts {
    memory: String,
    bridge: String,
    share: String,
    start: String,
    producer: Option<String>,
    consumer: Option<String>,
    encoder: Option<String>,
    codec: String,
    frames: u32,
    timeout: u64,
    run: bool,
    verbose: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Self {
        let mut o = Opts {
            memory: "gl".into(),
            bridge: "app".into(),
            share: "none".into(),
            start: "a".into(),
            producer: None,
            consumer: None,
            encoder: None,
            codec: "h264".into(),
            frames: 60,
            timeout: 15,
            run: false,
            verbose: false,
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut val = || it.next().cloned().unwrap_or_default();
            match a.as_str() {
                "--memory" => o.memory = val(),
                "--bridge" => o.bridge = val(),
                "--share" => o.share = val(),
                "--start" => o.start = val(),
                "--producer" => o.producer = Some(val()),
                "--consumer" => o.consumer = Some(val()),
                "--encoder" => o.encoder = Some(val()),
                "--codec" => o.codec = val(),
                "--frames" => o.frames = val().parse().unwrap_or(60),
                "--timeout" => o.timeout = val().parse().unwrap_or(15),
                "--run" => o.run = true,
                "--verbose" => o.verbose = true,
                other => eprintln!("ignoring unknown option {other}"),
            }
        }
        o
    }
}

fn have(factory: &str) -> bool {
    gst::ElementFactory::find(factory).is_some()
}

/// The upload into `memory`, and the elements its default cases need.
fn needed(memory: &str) -> &'static [&'static str] {
    match memory {
        "gl" => &["glupload", "glcolorconvert", "gldownload"],
        "cuda" => &["cudaupload", "cudadownload"],
        "d3d12" => &["d3d12upload", "d3d12convert", "d3d12download"],
        "d3d11" => &["d3d11upload", "d3d11convert", "d3d11download"],
        "decode" => &["uridecodebin3", "glupload", "glcolorconvert", "gldownload"],
        _ => &["videoconvert"],
    }
}

fn raw(format: &str, frames: u32) -> String {
    format!(
        "videotestsrc num-buffers={frames} pattern=ball ! \
         video/x-raw,format={format},width={WIDTH},height={HEIGHT},framerate=30/1"
    )
}

/// Pipeline A without its bridge element: frames in `memory`.
fn default_producer(memory: &str, frames: u32) -> String {
    match memory {
        "gl" => format!(
            "{} ! glupload ! video/x-raw(memory:GLMemory),format=RGBA",
            raw("RGBA", frames)
        ),
        "cuda" => format!(
            "{} ! cudaupload ! video/x-raw(memory:CUDAMemory)",
            raw("NV12", frames)
        ),
        "d3d12" => format!(
            "{} ! d3d12upload ! video/x-raw(memory:D3D12Memory)",
            raw("NV12", frames)
        ),
        "d3d11" => format!(
            "{} ! d3d11upload ! video/x-raw(memory:D3D11Memory)",
            raw("NV12", frames)
        ),
        "decode" => {
            let path = std::fs::canonicalize(CLIP).unwrap_or_else(|_| CLIP.into());
            let uri = gst::glib::filename_to_uri(&path, None)
                .map(|u| u.to_string())
                .unwrap_or_default();
            format!("uridecodebin3 uri={uri}")
        }
        _ => raw("NV12", frames),
    }
}

/// Pipeline B between its bridge element and the counting sink: a consumer
/// that has to do GPU work on the frames in their own memory, then leaves it.
fn default_consumer(memory: &str) -> String {
    match memory {
        // What a GL mixer input does: convert in GL, the step that checks the
        // buffer's GL context.
        "gl" => "glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12 ! gldownload ! video/x-raw".into(),
        // The Vision Mixer's GL input front, fed by whatever the decoder chose.
        "decode" => "glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format=RGBA ! gldownload ! video/x-raw".into(),
        "cuda" => "cudadownload ! video/x-raw".into(),
        "d3d12" => "d3d12convert ! video/x-raw(memory:D3D12Memory),format=RGBA ! d3d12download ! video/x-raw".into(),
        "d3d11" => "d3d11convert ! video/x-raw(memory:D3D11Memory),format=RGBA ! d3d11download ! video/x-raw".into(),
        _ => "videoconvert ! video/x-raw,format=I420".into(),
    }
}

/// Warnings and errors from the GStreamer log, captured instead of printed.
fn capture_log(verbose: bool) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    if !verbose {
        gst::log::remove_default_log_function();
    }
    if gst::log::get_default_threshold() < gst::DebugLevel::Warning {
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
    }
    let sink = lines.clone();
    gst::log::add_log_function(move |cat, level, _file, _func, _line, obj, msg| {
        if level > gst::DebugLevel::Warning {
            return;
        }
        let text = msg.get().map(|m| m.to_string()).unwrap_or_default();
        let who = obj.map(|o| o.to_string()).unwrap_or_default();
        if let Ok(mut l) = sink.lock() {
            l.push(format!("{:?} {} {} {}", level, cat.name(), who, text));
        }
    });
    lines
}

fn launch(description: &str) -> Result<gst::Pipeline, String> {
    gst::parse::launch(description)
        .map_err(|e| format!("parse: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())
}

/// GL contexts held by the GL elements of `pipeline`, by pointer.
fn element_gl_contexts(pipeline: &gst::Pipeline) -> Vec<(String, gst_gl::GLContext)> {
    let mut out = Vec::new();
    for e in pipeline.iterate_recurse().into_iter().flatten() {
        let Some(spec) = e.find_property("context") else {
            continue;
        };
        if spec.value_type() != gst_gl::GLContext::static_type() {
            continue;
        }
        if let Some(ctx) = e.property::<Option<gst_gl::GLContext>>("context") {
            out.push((e.name().to_string(), ctx));
        }
    }
    out
}

fn ptr(ctx: &gst_gl::GLContext) -> usize {
    ctx.as_ptr() as usize
}

/// The memory feature of `caps`, `system` when it has none.
fn feature_of(caps: &gst::CapsRef) -> String {
    caps.features(0)
        .and_then(|f| f.iter().find(|f| f.starts_with("memory:")).map(|f| f.to_string()))
        .unwrap_or_else(|| "system".into())
}

fn cross(o: &Opts) -> i32 {
    let log = capture_log(o.verbose);
    let producer = o
        .producer
        .clone()
        .unwrap_or_else(|| default_producer(&o.memory, o.frames));
    let consumer = o
        .consumer
        .clone()
        .unwrap_or_else(|| default_consumer(&o.memory));
    let name = format!("probe-{}", std::process::id());
    let (sink_desc, src_desc) = match o.bridge.as_str() {
        "inter" => (
            format!("intersink name=bridge_sink producer-name={name}"),
            format!("intersrc name=bridge_src producer-name={name}"),
        ),
        _ => (
            "appsink name=bridge_sink sync=false".to_string(),
            "appsrc name=bridge_src format=time".to_string(),
        ),
    };
    let a = match launch(&format!("{producer} ! {sink_desc}")) {
        Ok(p) => p,
        Err(e) => return report(o, &format!("result=SETUP_FAIL where=A error=\"{e}\"")),
    };
    let b = match launch(&format!("{src_desc} ! {consumer} ! fakesink name=out sync=false")) {
        Ok(p) => p,
        Err(e) => return report(o, &format!("result=SETUP_FAIL where=B error=\"{e}\"")),
    };
    let bridge_sink = a.by_name("bridge_sink").unwrap();
    let bridge_src = b.by_name("bridge_src").unwrap();

    // Frames out of A, and the GL context of the first GL buffer A hands over.
    let frames_a = Arc::new(AtomicU64::new(0));
    let a_buffer_ctx = Arc::new(Mutex::new(None::<gst_gl::GLContext>));
    let a_caps = Arc::new(Mutex::new(None::<gst::Caps>));
    {
        let frames = frames_a.clone();
        let first_ctx = a_buffer_ctx.clone();
        bridge_sink
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if frames.fetch_add(1, Ordering::Relaxed) == 0 {
                    if let Some(gst::PadProbeData::Buffer(ref buf)) = info.data {
                        if buf.n_memory() > 0 {
                            if let Some(gl) = buf
                                .peek_memory(0)
                                .downcast_memory_ref::<gst_gl::GLBaseMemory>()
                            {
                                *first_ctx.lock().unwrap() = Some(gl.context().clone());
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
        let caps_seen = a_caps.clone();
        bridge_sink
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Event(ref ev)) = info.data {
                    if let gst::EventView::Caps(c) = ev.view() {
                        *caps_seen.lock().unwrap() = Some(c.caps_owned());
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }
    let frames_b = Arc::new(AtomicU64::new(0));
    {
        let frames = frames_b.clone();
        b.by_name("out")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                frames.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
    }

    if o.bridge == "app" {
        let appsink = bridge_sink.clone().downcast::<gst_app::AppSink>().unwrap();
        let appsrc = bridge_src.clone().downcast::<gst_app::AppSrc>().unwrap();
        let src_eos = appsrc.downgrade();
        let src_push = appsrc.downgrade();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    match src_push.upgrade() {
                        Some(src) => src.push_sample(&sample),
                        None => Err(gst::FlowError::Flushing),
                    }
                })
                .eos(move |_| {
                    if let Some(src) = src_eos.upgrade() {
                        let _ = src.end_of_stream();
                    }
                })
                .build(),
        );
    }

    // `forward`: what webrtcsink does for its internal pipelines. A's context
    // messages are posted again from B's bridge element, so B's bin answers
    // A's NEED_CONTEXT from its cache and caches A's HAVE_CONTEXT.
    let forwarded = Arc::new(AtomicUsize::new(0));
    if o.share == "forward" {
        let target = bridge_src.downgrade();
        let count = forwarded.clone();
        a.bus().unwrap().set_sync_handler(move |_, msg| {
            if matches!(
                msg.view(),
                gst::MessageView::NeedContext(..) | gst::MessageView::HaveContext(..)
            ) {
                if let Some(t) = target.upgrade() {
                    count.fetch_add(1, Ordering::Relaxed);
                    let _ = t.post_message(msg.clone());
                }
            }
            gst::BusSyncReply::Pass
        });
    }

    let (first, second) = if o.start == "b" { (&b, &a) } else { (&a, &b) };
    let mut error = String::new();
    if let Err(e) = first.set_state(gst::State::Playing) {
        error = format!("set_state first: {e}");
    }
    // Give the first pipeline time to create its contexts before the second
    // one asks for them.
    std::thread::sleep(Duration::from_millis(300));
    if error.is_empty() {
        if let Err(e) = second.set_state(gst::State::Playing) {
            error = format!("set_state second: {e}");
        }
    }

    let deadline = Instant::now() + Duration::from_secs(o.timeout);
    let mut eos = false;
    let (bus_a, bus_b) = (a.bus().unwrap(), b.bus().unwrap());
    while error.is_empty() && !eos && Instant::now() < deadline {
        for (which, bus) in [("A", &bus_a), ("B", &bus_b)] {
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        if error.is_empty() {
                            error = format!(
                                "{which}:{}: {} ({})",
                                msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                                e.error(),
                                e.debug().map(|d| d.to_string()).unwrap_or_default()
                            );
                        }
                    }
                    gst::MessageView::Eos(_) if which == "B" => eos = true,
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let a_ctxs = element_gl_contexts(&a);
    let b_ctxs = element_gl_contexts(&b);
    let buf_ctx = a_buffer_ctx.lock().unwrap().clone();
    let crossed = a_caps
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| feature_of(c))
        .unwrap_or_else(|| "none".into());

    let _ = a.set_state(gst::State::Null);
    let _ = b.set_state(gst::State::Null);

    let (fa, fb) = (frames_a.load(Ordering::Relaxed), frames_b.load(Ordering::Relaxed));
    let result = if !error.is_empty() {
        "FAIL"
    } else if eos && fb > 0 && fb + 1 >= fa {
        "OK"
    } else if fb > 0 {
        "PARTIAL"
    } else {
        "TIMEOUT"
    };

    // GL: is B working in the very context A's buffers carry, or only one that
    // shares with it?
    let gl = match (&buf_ctx, b_ctxs.first()) {
        (Some(buf), Some((_, bctx))) => format!(
            " gl_same_context={} gl_can_share={} gl_same_display={} gl_b_contexts={}",
            ptr(buf) == ptr(bctx),
            buf.can_share(bctx),
            buf.display().as_ptr() == bctx.display().as_ptr(),
            distinct(&b_ctxs)
        ),
        (Some(_), None) => " gl_b_contexts=0".into(),
        _ => String::new(),
    };
    let a_gl = if a_ctxs.is_empty() {
        String::new()
    } else {
        format!(" gl_a_contexts={}", distinct(&a_ctxs))
    };

    let lines = log.lock().unwrap().clone();
    let notable: Vec<&String> = lines
        .iter()
        .filter(|l| {
            let l = l.to_lowercase();
            NOTABLE
                .iter()
                .any(|k| l.contains(k))
        })
        .collect();

    let code = report(
        o,
        &format!(
            "result={result} crossed={crossed} frames_a={fa} frames_b={fb} forwarded_msgs={}{a_gl}{gl} warnings={} error=\"{}\"",
            forwarded.load(Ordering::Relaxed),
            lines.len(),
            error.replace('"', "'")
        ),
    );
    for l in notable.iter().take(5) {
        println!("    log: {}", l.replace('\n', " "));
    }
    code
}

const NOTABLE: [&str; 9] = [
    "context",
    "different",
    "register",
    "copy",
    "fallback",
    "not-negotiated",
    "failed",
    "codec",
    "encoder",
];

fn distinct(ctxs: &[(String, gst_gl::GLContext)]) -> usize {
    let mut p: Vec<usize> = ctxs.iter().map(|(_, c)| ptr(c)).collect();
    p.sort_unstable();
    p.dedup();
    p.len()
}

fn report(o: &Opts, rest: &str) -> i32 {
    println!(
        "cross memory={} bridge={} share={} start={} {}",
        o.memory, o.bridge, o.share, o.start, rest
    );
    if rest.starts_with("result=OK") {
        0
    } else {
        1
    }
}

const WEBRTC_CODECS: [&str; 5] = [
    "video/x-h264",
    "video/x-h265",
    "video/x-vp8",
    "video/x-vp9",
    "video/x-av1",
];

/// Memory features a factory's sink templates list, `system` for none.
fn sink_memories(factory: &gst::ElementFactory) -> Vec<String> {
    let mut out = Vec::new();
    for t in factory.static_pad_templates() {
        if t.direction() != gst::PadDirection::Sink {
            continue;
        }
        let caps = t.caps();
        if caps.is_any() {
            out.push("ANY".into());
            continue;
        }
        for (s, f) in caps.iter_with_features() {
            if s.name() != "video/x-raw" {
                continue;
            }
            let m = f
                .iter()
                .find(|x| x.starts_with("memory:"))
                .map(|x| x.to_string())
                .unwrap_or_else(|| "system".into());
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

fn codec_of(factory: &gst::ElementFactory) -> Option<String> {
    factory.static_pad_templates().into_iter().find_map(|t| {
        (t.direction() == gst::PadDirection::Src)
            .then(|| t.caps().structure(0).map(|s| s.name().to_string()))
            .flatten()
    })
}

fn memory_name(feature: &str) -> Option<&'static str> {
    match feature {
        "system" | "memory:SystemMemory" => Some("system"),
        "memory:GLMemory" => Some("gl"),
        "memory:CUDAMemory" => Some("cuda"),
        "memory:D3D12Memory" => Some("d3d12"),
        "memory:D3D11Memory" => Some("d3d11"),
        _ => None,
    }
}

/// Which installed WebRTC-codec encoders list which memory on their sink pad,
/// and with `--run`, which of those really encode frames in that memory.
fn encoders(o: &Opts) -> i32 {
    let _log = capture_log(o.verbose);
    let mut factories: Vec<gst::ElementFactory> = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::VIDEO_ENCODER,
        gst::Rank::NONE,
    )
    .into_iter()
    .collect();
    factories.sort_by_key(|f| f.name().to_string());
    for f in factories {
        let Some(codec) = codec_of(&f) else { continue };
        if !WEBRTC_CODECS.contains(&codec.as_str()) {
            continue;
        }
        if o.encoder.as_deref().is_some_and(|e| e != f.name()) {
            continue;
        }
        let mems = sink_memories(&f);
        println!(
            "encoder name={} codec={} rank={} lists={}",
            f.name(),
            codec,
            i32::from(f.rank()),
            mems.join(",")
        );
        if !o.run {
            continue;
        }
        for m in &mems {
            let Some(memory) = memory_name(m) else { continue };
            if !needed(memory).iter().all(|e| have(e)) {
                println!("    run memory={memory} result=SKIP (no {memory} upload here)");
                continue;
            }
            let producer = match memory {
                // Any format the encoder takes: this row is the baseline.
                "system" => format!(
                    "videotestsrc num-buffers={} ! \
                     video/x-raw,width={WIDTH},height={HEIGHT},framerate=30/1 ! videoconvert",
                    o.frames
                ),
                "gl" => format!(
                    "{} ! glupload ! video/x-raw(memory:GLMemory)",
                    raw("NV12", o.frames)
                ),
                other => default_producer(other, o.frames),
            };
            let desc = format!("{producer} ! {} ! fakesink sync=false", f.name());
            let (result, error) = run_to_eos(&desc, o.timeout);
            println!(
                "    run memory={memory} result={result} error=\"{}\"",
                error.replace('"', "'")
            );
        }
    }
    0
}

fn codec_caps(codec: &str) -> gst::Caps {
    let name = match codec {
        "h265" => "video/x-h265",
        "vp9" => "video/x-vp9",
        "av1" => "video/x-av1",
        "vp8" => "video/x-vp8",
        _ => "video/x-h264",
    };
    gst::Caps::new_empty_simple(name)
}

/// The encoder webrtcsink would pick for `caps`: it takes the first factory of
/// rank MARGINAL or above whose src template intersects, from a list in
/// decreasing rank, and builds a converter for the input memory in front of it
/// whether or not that encoder takes the memory.
fn webrtcsink_encoder(caps: &gst::Caps) -> Option<gst::ElementFactory> {
    gst::ElementFactory::factories_with_type(gst::ElementFactoryType::ENCODER, gst::Rank::MARGINAL)
        .into_iter()
        .find(|f| {
            f.static_pad_templates().iter().any(|t| {
                let tc = t.caps();
                t.direction() == gst::PadDirection::Src && !tc.is_any() && caps.can_intersect(&tc)
            })
        })
}

fn memory_feature(memory: &str) -> &'static str {
    match memory {
        "gl" => "memory:GLMemory",
        "cuda" => "memory:CUDAMemory",
        "d3d12" => "memory:D3D12Memory",
        "d3d11" => "memory:D3D11Memory",
        _ => "system",
    }
}

/// Frames in `--memory` into a real `whepserversink` that offers only
/// `--codec`, and a `whepclientsrc` on the same host as its viewer. Codec
/// discovery and the encoding chain run only once a viewer connects.
fn webrtc(o: &Opts) -> i32 {
    let log = capture_log(o.verbose);
    let caps = codec_caps(&o.codec);
    let feature = memory_feature(&o.memory);
    let (top, top_takes) = match webrtcsink_encoder(&caps) {
        Some(f) => {
            let mems = sink_memories(&f);
            let takes = mems.iter().any(|m| {
                m == feature
                    || m == "ANY"
                    || (feature == "system" && m == "memory:SystemMemory")
            });
            (f.name().to_string(), takes.to_string())
        }
        None => ("none".to_string(), "false".to_string()),
    };
    let head = format!(
        "webrtc memory={} codec={} top_encoder={top} top_takes_memory={top_takes}",
        o.memory, o.codec
    );

    let port = match std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()) {
        Ok(a) => a.port(),
        Err(e) => {
            println!("{head} result=SETUP_FAIL error=\"port: {e}\"");
            return 1;
        }
    };
    let producer = default_producer(&o.memory, o.frames).replacen(
        &format!("num-buffers={}", o.frames),
        "is-live=true",
        1,
    );
    let server = match launch(&format!("{producer} ! whepserversink name=ws")) {
        Ok(p) => p,
        Err(e) => {
            println!("{head} result=REFUSED error=\"{}\"", e.replace('"', "'"));
            return 1;
        }
    };
    let ws = server.by_name("ws").unwrap();
    ws.set_property("stun-server", None::<&str>);
    ws.set_property("do-fec", false);
    ws.set_property("audio-caps", gst::Caps::new_empty());
    ws.set_property("video-caps", &caps);
    ws.property::<gst::glib::Object>("signaller")
        .set_property("host-addr", format!("http://127.0.0.1:{port}"));

    let discovery_encoders = Arc::new(Mutex::new(Vec::<String>::new()));
    let session_encoders = Arc::new(Mutex::new(Vec::<String>::new()));
    let encoded = Arc::new(AtomicU64::new(0));
    {
        let (disc, sess, encoded) = (
            discovery_encoders.clone(),
            session_encoders.clone(),
            encoded.clone(),
        );
        ws.connect("encoder-setup", false, move |args| {
            let consumer = args[1].get::<String>().unwrap_or_default();
            if let Ok(enc) = args[3].get::<gst::Element>() {
                let name = enc
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if consumer == "discovery" {
                    disc.lock().unwrap().push(name);
                } else {
                    sess.lock().unwrap().push(name);
                    if let Some(pad) = enc.static_pad("src") {
                        let encoded = encoded.clone();
                        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                            encoded.fetch_add(1, Ordering::Relaxed);
                            gst::PadProbeReturn::Ok
                        });
                    }
                }
            }
            Some(false.to_value())
        });
    }

    let client = gst::Pipeline::new();
    let wc = match gst::ElementFactory::make("whepclientsrc").build() {
        Ok(e) => e,
        Err(e) => {
            println!("{head} result=SETUP_FAIL error=\"whepclientsrc: {e}\"");
            return 1;
        }
    };
    wc.set_property("stun-server", None::<&str>);
    wc.property::<gst::glib::Object>("signaller").set_property(
        "whep-endpoint",
        format!("http://127.0.0.1:{port}/whep/endpoint"),
    );
    client.add(&wc).unwrap();
    let received = Arc::new(AtomicU64::new(0));
    {
        let weak = client.downgrade();
        let received = received.clone();
        wc.connect_pad_added(move |_, pad| {
            let Some(pipeline) = weak.upgrade() else {
                return;
            };
            let Ok(sink) = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
            else {
                return;
            };
            let _ = pipeline.add(&sink);
            let _ = sink.sync_state_with_parent();
            let sink_pad = sink.static_pad("sink").unwrap();
            let received = received.clone();
            sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                received.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
            let _ = pad.link(&sink_pad);
        });
    }

    let mut error = String::new();
    if let Err(e) = server.set_state(gst::State::Playing) {
        error = format!("server set_state: {e}");
    }
    std::thread::sleep(Duration::from_millis(1000));
    if error.is_empty() {
        if let Err(e) = client.set_state(gst::State::Playing) {
            error = format!("client set_state: {e}");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(o.timeout);
    let (bus_s, bus_c) = (server.bus().unwrap(), client.bus().unwrap());
    while error.is_empty()
        && received.load(Ordering::Relaxed) < 30
        && Instant::now() < deadline
    {
        for (which, bus) in [("server", &bus_s), ("client", &bus_c)] {
            while let Some(msg) = bus.pop() {
                if let gst::MessageView::Error(e) = msg.view() {
                    if error.is_empty() {
                        error = format!(
                            "{which}:{}: {} ({})",
                            msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                            e.error(),
                            e.debug().map(|d| d.to_string()).unwrap_or_default()
                        );
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = client.set_state(gst::State::Null);
    let _ = server.set_state(gst::State::Null);

    let (enc, got) = (encoded.load(Ordering::Relaxed), received.load(Ordering::Relaxed));
    let sess = session_encoders.lock().unwrap().join(",");
    let mut disc = discovery_encoders.lock().unwrap().clone();
    disc.sort();
    disc.dedup();
    let result = if enc > 0 && got > 0 {
        "OK"
    } else if !error.is_empty() {
        "FAIL"
    } else if sess.is_empty() {
        "NO_SESSION_ENCODER"
    } else if enc == 0 {
        "NOT_ENCODED"
    } else {
        "NOT_RECEIVED"
    };
    println!(
        "{head} result={result} session_encoders={} discovery_encoders={} encoded={enc} received={got} error=\"{}\"",
        if sess.is_empty() { "none".into() } else { sess },
        if disc.is_empty() { "none".into() } else { disc.join(",") },
        error.replace('"', "'").replace('\n', " ")
    );
    let lines = log.lock().unwrap().clone();
    for l in lines
        .iter()
        .filter(|l| {
            let l = l.to_lowercase();
            NOTABLE.iter().any(|k| l.contains(k))
        })
        .take(6)
    {
        println!("    log: {}", l.replace('\n', " "));
    }
    i32::from(result != "OK")
}

fn run_to_eos(desc: &str, timeout: u64) -> (&'static str, String) {
    let p = match launch(desc) {
        Ok(p) => p,
        Err(e) => return ("SETUP_FAIL", e),
    };
    if let Err(e) = p.set_state(gst::State::Playing) {
        let _ = p.set_state(gst::State::Null);
        return ("FAIL", format!("set_state: {e}"));
    }
    let bus = p.bus().unwrap();
    let out = match bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(timeout),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(m) => match m.view() {
            gst::MessageView::Eos(_) => ("OK", String::new()),
            gst::MessageView::Error(e) => (
                "FAIL",
                format!(
                    "{}: {}",
                    m.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    e.error()
                ),
            ),
            _ => unreachable!(),
        },
        None => ("TIMEOUT", String::new()),
    };
    let _ = p.set_state(gst::State::Null);
    out
}

/// A video-only H.264 clip, so the `decode` cases go through this host's own
/// hardware decoder, the way the Media Player does.
fn clip() -> i32 {
    let Some(enc) = ["x264enc", "openh264enc", "vtenc_h264", "mfh264enc", "nvh264enc"]
        .into_iter()
        .find(|e| have(e))
    else {
        println!("clip result=SKIP error=\"no H.264 encoder\"");
        return 1;
    };
    let desc = format!(
        "{} ! videoconvert ! {enc} ! h264parse ! mp4mux ! filesink location={CLIP}",
        raw("I420", 150)
    );
    let (result, error) = run_to_eos(&desc, 60);
    println!("clip encoder={enc} file={CLIP} result={result} error=\"{error}\"");
    i32::from(result != "OK")
}

/// Every case this host can run, each in its own process: a crash or a hang
/// in one case cannot take the others with it, and no GL state leaks between
/// cases.
fn all(o: &Opts) -> i32 {
    println!(
        "host os={} arch={} gstreamer={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        gst::version_string()
    );
    let exe = std::env::current_exe().expect("current_exe");
    let frames = o.frames.to_string();
    let timeout = o.timeout.to_string();

    if !std::path::Path::new(CLIP).exists() {
        run_child(&exe, &["clip"], 90);
    }

    let mut memories: Vec<&str> = MEMORIES.to_vec();
    if std::path::Path::new(CLIP).exists() {
        memories.push("decode");
    }
    for memory in memories {
        let missing: Vec<&str> = needed(memory).iter().copied().filter(|e| !have(e)).collect();
        if !missing.is_empty() {
            println!("cross memory={memory} result=SKIP missing={}", missing.join(","));
            continue;
        }
        for bridge in ["app", "inter"] {
            for (share, start) in [("none", "a"), ("forward", "a"), ("forward", "b")] {
                run_child(
                    &exe,
                    &[
                        "cross", "--memory", memory, "--bridge", bridge, "--share", share,
                        "--start", start, "--frames", &frames, "--timeout", &timeout,
                    ],
                    o.timeout + 15,
                );
            }
        }
    }
    run_child(&exe, &["encoders", "--run", "--frames", "30"], 600);

    if have("whepserversink") && have("whepclientsrc") {
        for memory in MEMORIES {
            if !needed(memory).iter().all(|e| have(e)) {
                continue;
            }
            for codec in ["h264", "h265", "vp9", "av1"] {
                run_child(
                    &exe,
                    &["webrtc", "--memory", memory, "--codec", codec, "--timeout", "20"],
                    45,
                );
            }
        }
    }
    0
}

fn run_child(exe: &std::path::Path, args: &[&str], timeout: u64) {
    let mut child = match Command::new(exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            println!("child args=\"{}\" result=SPAWN_FAIL error=\"{e}\"", args.join(" "));
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                break None;
            }
        }
    };
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        use std::io::Read;
        let _ = so.read_to_string(&mut out);
    }
    print!("{out}");
    match status {
        None => println!("child args=\"{}\" result=HUNG (killed)", args.join(" ")),
        Some(s) if s.code().is_none() => {
            println!("child args=\"{}\" result=CRASHED status={s}", args.join(" "))
        }
        _ => {}
    }
}
