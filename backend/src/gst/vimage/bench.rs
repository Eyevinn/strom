//! A measurement harness for the vImage path. Not a guard.
//!
//! Nothing here asserts anything about performance — a timing assertion would
//! be flaky on a shared machine and would fail for reasons that have nothing
//! to do with this code. It exists so the numbers in the pull request can be
//! reproduced, and it is `#[ignore]`d so it never runs in CI.
//!
//! Run it through `.ab-bench/vimage-bench.sh`, which drives the chunking
//! described below. A single experiment can also be run directly:
//!
//! ```text
//! STROM_BENCH_PAIRS=10 cargo test --lib gst::vimage::bench::ab_vimage \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Method, which matters more than the numbers:
//!
//! * Arms are **paired**: both run back to back on the same host state, so a
//!   load spike hits both halves of a pair rather than one arm.
//! * Pair order is **counterbalanced and shuffled** — half the pairs run A
//!   first, half B first, in random order. Strict alternation is wrong here:
//!   it is a fixed ABBA schedule, so each arm permanently occupies one phase
//!   of the host's autocorrelated noise, and an A/A control run that way
//!   returns a significant difference where the truth is exactly zero.
//! * An **A/A control** with byte-identical arms runs alongside the A/B. Only
//!   the control licenses reading the A/B as a real effect rather than as
//!   harness bias.
//!
//! * Each experiment is a **separate test**, run in **short chunks of fresh
//!   processes**. macOS moves a windowless process into the background QoS
//!   band about thirty seconds after launch — priority 4, efficiency cores,
//!   throttled clock — and a test binary is exactly that shape. Ten pairs per
//!   process keeps every measurement inside that window. Both arms of a pair
//!   are always in the same process, so chunking adds between-process variance
//!   to the arms but none to the paired difference.
//!
//! Output is CSV on stdout (`pair,a_first,a_ms,b_ms`) so the statistics can be
//! redone without re-running the measurement.

use std::time::Instant;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Frames per run. Matches the figure the work was scoped against.
const FRAMES: u32 = 300;
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// Pairs per process. Small enough that a chunk finishes before macOS App Nap
/// would demote the process; the driver script runs several chunks to reach
/// the pair count the statistics need. At the ~5.6% per-pair spread measured
/// on this class of host, 40 pairs put roughly +/-1.8% around the mean.
fn pairs() -> usize {
    std::env::var("STROM_BENCH_PAIRS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
}

/// One discarded pair before measurement starts, so neither arm pays for the
/// first-touch page faults and registry warm-up. Deliberately just one: a
/// longer warm-up would spend the App Nap window on runs that get thrown away.
const WARMUP: usize = 1;

/// A deterministic generator, so a reported seed reproduces the exact
/// schedule. Only used to shuffle the counterbalanced order.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Time one pass of `frames` buffers through `converter`, from the pipeline
/// reaching PLAYING to end of stream.
///
/// `converter` is a `gst-launch`-style fragment so an arm can be a single
/// element, a configured element, or nothing at all. `tail` is everything
/// after it — usually a capsfilter pinning the output format, but for the
/// encoder experiment a real encoder with its own format preferences.
fn run_once(converter: &str, tail: &str) -> f64 {
    let description = format!(
        "videotestsrc num-buffers={FRAMES} is-live=false pattern=smpte \
         ! video/x-raw,format=RGBA,width={WIDTH},height={HEIGHT},framerate=30/1 \
         {converter} {tail} \
         ! fakesink sync=false"
    );

    let pipeline = gst::parse::launch(&description)
        .unwrap_or_else(|e| panic!("pipeline {description:?}: {e}"))
        .downcast::<gst::Pipeline>()
        .expect("parse::launch returns a pipeline");

    // Reach PLAYING before starting the clock: state changes allocate and
    // negotiate, and neither is what is being measured.
    pipeline.set_state(gst::State::Playing).expect("play");
    let _ = pipeline.state(gst::ClockTime::from_seconds(10));

    let start = Instant::now();
    let bus = pipeline.bus().expect("pipeline bus");
    let message = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(120),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let elapsed = start.elapsed();

    if let Some(gst::MessageView::Error(err)) = message.as_ref().map(|m| m.view()) {
        panic!("pipeline {description:?} failed: {}", err.error());
    }
    assert!(message.is_some(), "pipeline {description:?} never finished");

    pipeline.set_state(gst::State::Null).expect("null");
    elapsed.as_secs_f64() * 1000.0
}

/// Run one paired experiment and print its rows as CSV.
fn experiment(label: &str, arm_a: &str, arm_b: &str, tail: &str, seed: u64) {
    // Counterbalance: exactly half the pairs put A first, then shuffle which.
    let n = pairs();
    let mut order: Vec<bool> = (0..n).map(|i| i < n / 2).collect();
    let mut rng = XorShift(seed | 1);
    for i in (1..order.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }

    for _ in 0..WARMUP {
        run_once(arm_a, tail);
        run_once(arm_b, tail);
    }

    println!("# experiment={label} seed={seed} frames={FRAMES} size={WIDTH}x{HEIGHT}");
    println!("# arm_a={arm_a:?} arm_b={arm_b:?} tail={tail:?}");
    println!("{label},pair,a_first,a_ms,b_ms");
    for (pair, &a_first) in order.iter().enumerate() {
        let (a_ms, b_ms) = if a_first {
            let a = run_once(arm_a, tail);
            let b = run_once(arm_b, tail);
            (a, b)
        } else {
            let b = run_once(arm_b, tail);
            let a = run_once(arm_a, tail);
            (a, b)
        };
        println!("{label},{pair},{a_first},{a_ms:.3},{b_ms:.3}");
    }
}

/// The pinned-format tail: what the Video Format block builds when an operator
/// sets an output format, which is where the vImage path is actually reached.
const NV12_TAIL: &str = "! video/x-raw,format=NV12 ";

/// The unpinned tail: a real VideoToolbox encoder, left to state its own
/// format preference. Strom's Video Encoder block builds exactly this when no
/// format has been pinned upstream, and neither converter picks NV12 from the
/// menu it offers — both expand to a 16-bit intermediate. This experiment
/// exists to show the change does not make that path worse.
const VTENC_TAIL: &str = "! vtenc_h264_hw ! h264parse ";

/// Shared setup: initialise GStreamer, register the element, and resolve the
/// seed. A fixed `STROM_BENCH_SEED` reproduces a chunk's exact schedule.
fn setup() -> u64 {
    gst::init().expect("gst init");
    assert!(super::register(), "the vImage plugin must register");
    std::env::var("STROM_BENCH_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos() as u64
        })
}

/// The arm to beat is what production does today: `videoconvert` with the
/// thread pool `configure_video_convert` gives it.
fn threaded_videoconvert() -> String {
    format!(
        "! videoconvert n-threads={} ",
        crate::gpu::video_convert_threads()
    )
}

/// Source and sink with a passthrough where the converter goes, so the
/// converter cost can be reported net of the rest of the pipeline. Both arms
/// are the same fragment, which makes this a second A/A control.
#[test]
#[ignore = "measurement harness, not a test; see the module docs to run it"]
fn baseline() {
    let seed = setup();
    experiment(
        "baseline",
        "! identity ",
        "! identity ",
        "! video/x-raw,format=RGBA ",
        seed,
    );
}

/// The A/A control: byte-identical arms. Whatever this returns is the
/// harness's own bias, and the A/B has to beat it to mean anything.
#[test]
#[ignore = "measurement harness, not a test; see the module docs to run it"]
fn aa_control() {
    let seed = setup();
    let arm = threaded_videoconvert();
    experiment("aa-control", &arm, &arm, NV12_TAIL, seed);
}

/// The same comparison against *stock* `videoconvert`, which is what this ran
/// on before #726 raised the thread count. Only here to complete the table;
/// the number that matters for this change is [`ab_vimage`].
#[test]
#[ignore = "measurement harness, not a test; see the module docs to run it"]
fn ab_stock() {
    let seed = setup();
    experiment(
        "ab-stock",
        "! videoconvert n-threads=1 ",
        &format!("! {} ", super::ELEMENT_NAME),
        NV12_TAIL,
        seed,
    );
}

/// The unpinned encoder path, where neither converter reaches vImage. Guards
/// against the change regressing what it cannot speed up.
#[test]
#[ignore = "measurement harness, not a test; see the module docs to run it"]
fn ab_vtenc() {
    let seed = setup();
    experiment(
        "ab-vtenc",
        &threaded_videoconvert(),
        &format!("! {} ", super::ELEMENT_NAME),
        VTENC_TAIL,
        seed,
    );
}

/// The comparison that matters: today's threaded `videoconvert` against the
/// vImage element, both converting RGBA to NV12.
#[test]
#[ignore = "measurement harness, not a test; see the module docs to run it"]
fn ab_vimage() {
    let seed = setup();
    experiment(
        "ab-vimage",
        &threaded_videoconvert(),
        &format!("! {} ", super::ELEMENT_NAME),
        NV12_TAIL,
        seed,
    );
}
