//! `stromaudiobridgesink` → `stromaudiobridgesrc` across two real pipelines.
//!
//! A producer delivers a 437 Hz tone in 20 ms buffers, stalls, then delivers
//! everything it held at once, which is how a WHIP seat's jitterbuffer hands
//! over audio after a network stall. The bridge must keep that audio, play it
//! late, and drain back to its target by time-scaling: no click, no pitch
//! change, and an output that keeps pace with the clock.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::gst::audio_bridge::control::OVERRUN_SKIPS;
use strom::gst::audio_bridge::{self, src::AudioBridgeSrc};
use strom_types::audio_bridge::AudioBridgeStats;

const RATE: usize = 48_000;
/// Not a whole number of cycles per 20 ms buffer, so a stall cuts the tone
/// mid-cycle and an unfaded edge is a step.
const TONE_HZ: f64 = 437.0;
const AMPLITUDE: f32 = 0.5;
/// Largest sample-to-sample step a clean tone at this level can make.
const MAX_STEP: f32 = AMPLITUDE * 2.0 * std::f32::consts::PI * TONE_HZ as f32 / RATE as f32;

fn init() {
    gst::init().unwrap();
    audio_bridge::register().unwrap();
}

fn has_scaletempo() -> bool {
    gst::ElementFactory::find("scaletempo").is_some()
}

struct Output {
    /// (pts ns, duration ns, left-channel samples)
    buffers: Vec<(u64, u64, Vec<f32>)>,
    segments: Vec<(f64, f64)>,
}

struct Run {
    output: Output,
    /// Stats sampled every 100 ms: (seconds since start, stats).
    trace: Vec<(f64, AudioBridgeStats)>,
    elapsed: Duration,
}

/// Produce `seconds` of tone, stalling for `stall_ms` at `stall_at`, and read
/// it through a bridge configured by `configure`.
fn run(
    channel: &str,
    seconds: f64,
    stall_at: f64,
    stall_ms: u64,
    configure: impl FnOnce(&gst::Element),
) -> Run {
    let producer = gst::parse::launch(&format!(
        "appsrc name=src is-live=true format=time ! {sink} channel={channel}",
        sink = audio_bridge::SINK_FACTORY,
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let consumer = gst::parse::launch(&format!(
        "{src} name=bridge channel={channel} ! appsink name=out sync=false",
        src = audio_bridge::SRC_FACTORY,
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let bridge = consumer.by_name("bridge").unwrap();
    configure(&bridge);

    let output = Arc::new(Mutex::new(Output {
        buffers: Vec::new(),
        segments: Vec::new(),
    }));
    let sink = consumer
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let out = output.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let segment = sample.segment().unwrap();
                let buffer = sample.buffer().unwrap();
                let map = buffer.map_readable().unwrap();
                let left: Vec<f32> = map
                    .as_slice()
                    .chunks_exact(8)
                    .map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]]))
                    .collect();
                let mut out = out.lock().unwrap();
                let seg = (segment.rate(), segment.applied_rate());
                if out.segments.last() != Some(&seg) {
                    out.segments.push(seg);
                }
                out.buffers.push((
                    buffer.pts().unwrap().nseconds(),
                    buffer.duration().unwrap().nseconds(),
                    left,
                ));
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    producer.set_state(gst::State::Playing).unwrap();
    consumer.set_state(gst::State::Playing).unwrap();
    let appsrc = producer
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    appsrc.set_caps(Some(&audio_bridge::caps()));

    let start = Instant::now();
    let chunk = RATE / 50;
    let mut sample_index = 0usize;
    let mut held = 0usize;
    let mut pushed_chunks = 0usize;
    let mut trace = Vec::new();
    let mut next_trace = 0.0;
    let stall = stall_at..stall_at + stall_ms as f64 / 1000.0;
    loop {
        let t = start.elapsed().as_secs_f64();
        if t >= seconds {
            break;
        }
        let due = (t * 50.0) as usize + 1;
        while pushed_chunks + held < due {
            if stall.contains(&t) {
                held += 1;
                continue;
            }
            for _ in 0..=held {
                let mut data = Vec::with_capacity(chunk * 8);
                for _ in 0..chunk {
                    let v = AMPLITUDE
                        * (2.0 * std::f64::consts::PI * TONE_HZ * sample_index as f64 / RATE as f64)
                            .sin() as f32;
                    data.extend_from_slice(&v.to_le_bytes());
                    data.extend_from_slice(&v.to_le_bytes());
                    sample_index += 1;
                }
                let mut buffer = gst::Buffer::from_mut_slice(data);
                buffer
                    .get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_nseconds(
                        (pushed_chunks as u64) * 20_000_000,
                    ));
                appsrc.push_buffer(buffer).unwrap();
                pushed_chunks += 1;
            }
            held = 0;
        }
        if t >= next_trace {
            let stats = bridge.downcast_ref::<AudioBridgeSrc>().unwrap().stats();
            trace.push((t, stats));
            next_trace += 0.1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let elapsed = start.elapsed();
    consumer.set_state(gst::State::Null).unwrap();
    producer.set_state(gst::State::Null).unwrap();
    let output = std::mem::replace(
        &mut *output.lock().unwrap(),
        Output {
            buffers: Vec::new(),
            segments: Vec::new(),
        },
    );
    Run {
        output,
        trace,
        elapsed,
    }
}

/// Positions (seconds of output) where one sample jumps further than a clean
/// tone can, with some headroom for WSOLA's crossfades.
fn clicks(samples: &[f32]) -> Vec<f64> {
    samples
        .windows(2)
        .enumerate()
        .filter(|(_, w)| (w[1] - w[0]).abs() > 2.0 * MAX_STEP)
        .map(|(i, _)| i as f64 / RATE as f64)
        .collect()
}

/// Frequency from rising zero crossings.
fn pitch(samples: &[f32]) -> f64 {
    let crossings: Vec<usize> = samples
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
        .map(|(i, _)| i)
        .collect();
    let (first, last) = (crossings[0], *crossings.last().unwrap());
    (crossings.len() - 1) as f64 * RATE as f64 / (last - first) as f64
}

fn stats_at(run: &Run, t: f64) -> &AudioBridgeStats {
    &run.trace
        .iter()
        .find(|(at, _)| *at >= t)
        .unwrap_or_else(|| run.trace.last().unwrap())
        .1
}

#[test]
fn a_stall_is_kept_and_drained_back_to_target() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    let run = run("bridge-test-drain", 7.0, 1.5, 300, |bridge| {
        bridge.set_property("target-latency", 40u32);
        bridge.set_property("max-rate-change", 10.0f64);
    });
    let out = &run.output;
    let end = run.trace.last().unwrap().1.clone();

    // The stall underruns once; the held audio is kept, then drained.
    assert_eq!(end.underruns, 1, "{end:?}");
    assert!(end.max_depth_ms >= 250.0, "stalled audio kept: {end:?}");
    assert_eq!(end.skips, 0);
    assert!(end.drained_ms >= 200.0, "{end:?}");
    assert!(
        end.floor_ms <= 60.0,
        "back near the 40 ms target by the end: {end:?}"
    );
    let during = stats_at(&run, 2.5);
    assert!(during.rate > 1.0, "draining after the stall: {during:?}");
    assert!(end.input_gaps_200ms >= 1, "the stall shows as an input gap");

    // Downstream sees one ordinary rate-1.0 segment and contiguous timestamps.
    assert_eq!(out.segments, vec![(1.0, 1.0)]);
    for w in out.buffers.windows(2) {
        let (pts, dur, _) = &w[0];
        assert_eq!(pts + dur, w[1].0, "contiguous output timestamps");
    }

    // The output keeps pace with the clock: nothing downstream is starved.
    let (first_pts, ..) = out.buffers[0];
    let (last_pts, last_dur, _) = out.buffers.last().unwrap();
    let span = (last_pts + last_dur - first_pts) as f64 / 1e9;
    let wall = run.elapsed.as_secs_f64();
    assert!(
        (wall - span).abs() < 0.15,
        "output span {span:.3} s against {wall:.3} s of wall clock"
    );

    // Pitch holds while draining, and there is no click anywhere: not at the
    // underrun (faded), not at the resume (faded), and not where the rate
    // returns to 1.0.
    let samples: Vec<f32> = out
        .buffers
        .iter()
        .flat_map(|b| b.2.iter().copied())
        .collect();
    let second = |s: f64| (s * RATE as f64) as usize;
    let draining = &samples[second(2.2)..second(3.2)];
    let hz = pitch(draining);
    assert!(
        (hz - TONE_HZ).abs() < 2.0,
        "pitch while draining: {hz:.1} Hz"
    );
    let found = clicks(&samples);
    assert!(found.is_empty(), "clicks at {found:?} s");
}

#[test]
fn without_rate_change_the_backlog_stays() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    let run = run("bridge-test-no-drain", 4.0, 1.0, 300, |bridge| {
        bridge.set_property("target-latency", 40u32);
        bridge.set_property("max-rate-change", 0.0f64);
    });
    let end = run.trace.last().unwrap().1.clone();
    assert_eq!(end.drained_ms, 0.0);
    assert!(
        end.floor_ms >= 200.0,
        "with time-scaling off the stall's backlog is never drained: {end:?}"
    );
}

#[test]
fn a_channel_takes_one_reader() {
    init();
    let make = || {
        let p = gst::parse::launch(&format!(
            "{} channel=bridge-test-one-reader ! fakesink",
            audio_bridge::SRC_FACTORY
        ))
        .unwrap();
        p
    };
    let first = make();
    first.set_state(gst::State::Playing).unwrap();
    let second = make();
    let result = second.set_state(gst::State::Playing);
    let bus = second.bus().unwrap();
    let error = bus.timed_pop_filtered(gst::ClockTime::from_seconds(2), &[gst::MessageType::Error]);
    // Shut both down before asserting: a failed assertion would otherwise drop
    // two PLAYING pipelines while unwinding and crash the whole test binary.
    second.set_state(gst::State::Null).unwrap();
    first.set_state(gst::State::Null).unwrap();
    assert!(
        result.is_err() || error.is_some(),
        "a second reader on one channel is refused"
    );
}

/// A producing flow restarted within the maximum latency. The new writer's
/// opening backlog, a jitterbuffer handing over its latency at once, is trimmed
/// to the target as it is for the first writer, not kept and played fast.
#[test]
fn a_restarted_producer_is_trimmed_like_a_start() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    let channel = "bridge-test-restart";
    let consumer = gst::parse::launch(&format!(
        "{} name=bridge channel={channel} ! fakesink sync=false",
        audio_bridge::SRC_FACTORY
    ))
    .unwrap();
    let bridge = consumer
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name("bridge")
        .unwrap()
        .downcast::<AudioBridgeSrc>()
        .unwrap();
    consumer.set_state(gst::State::Playing).unwrap();

    // One producing flow: `opening_ms` handed over at once, then 20 ms of
    // audio every 20 ms for `seconds`.
    let produce = |opening_ms: usize, seconds: f64| {
        let producer = gst::parse::launch(&format!(
            "appsrc name=src is-live=true format=time ! {} channel={channel}",
            audio_bridge::SINK_FACTORY
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        producer.set_state(gst::State::Playing).unwrap();
        let appsrc = producer
            .by_name("src")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        appsrc.set_caps(Some(&audio_bridge::caps()));
        let chunk = RATE / 50;
        let mut pushed = 0u64;
        let mut push = |n: usize| {
            for _ in 0..n {
                let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; chunk * 8]);
                buffer
                    .get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_nseconds(pushed * 20_000_000));
                appsrc.push_buffer(buffer).unwrap();
                pushed += 1;
            }
        };
        push(opening_ms / 20);
        let start = Instant::now();
        let mut done = 0usize;
        while start.elapsed().as_secs_f64() < seconds {
            let due = (start.elapsed().as_secs_f64() * 50.0) as usize + 1;
            push(due - done);
            done = due;
            std::thread::sleep(Duration::from_millis(2));
        }
        producer.set_state(gst::State::Null).unwrap();
    };

    produce(0, 2.0);
    // The flow restarts: the reader runs dry and waits, well inside the
    // 1000 ms maximum latency.
    std::thread::sleep(Duration::from_millis(600));
    let before = bridge.stats();
    produce(440, 3.0);
    let after = bridge.stats();
    consumer.set_state(gst::State::Null).unwrap();

    assert!(
        after.max_depth_ms < 200.0,
        "the new producer's opening backlog was kept: {after:?}"
    );
    assert!(
        after.drained_ms - before.drained_ms < 100.0,
        "the new producer's opening backlog was drained rather than trimmed: \
         {} ms drained",
        after.drained_ms - before.drained_ms
    );
}

/// Stopping the producer flow is not a dropout. The underrun statistics are
/// the rehearsal signal for a contributor's link, so the silence while that
/// flow is down must not count, and `producer_attached` says it is down.
#[test]
fn a_stopped_producer_flow_is_not_counted_as_a_dropout() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    let channel = "bridge-test-stopped-producer";
    let consumer = gst::parse::launch(&format!(
        "{} name=bridge channel={channel} ! fakesink sync=false",
        audio_bridge::SRC_FACTORY
    ))
    .unwrap();
    let bridge = consumer
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name("bridge")
        .unwrap()
        .downcast::<AudioBridgeSrc>()
        .unwrap();
    consumer.set_state(gst::State::Playing).unwrap();

    // One producing flow: 20 ms of audio every 20 ms for `seconds`, then
    // `stall` with nothing delivered before the flow stops.
    let produce = |seconds: f64, stall: Duration| {
        let producer = gst::parse::launch(&format!(
            "appsrc name=src is-live=true format=time ! {} channel={channel}",
            audio_bridge::SINK_FACTORY
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        producer.set_state(gst::State::Playing).unwrap();
        let appsrc = producer
            .by_name("src")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        appsrc.set_caps(Some(&audio_bridge::caps()));
        let chunk = RATE / 50;
        let start = Instant::now();
        let mut pushed = 0u64;
        while start.elapsed().as_secs_f64() < seconds {
            let due = (start.elapsed().as_secs_f64() * 50.0) as u64 + 1;
            while pushed < due {
                let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; chunk * 8]);
                buffer
                    .get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_nseconds(pushed * 20_000_000));
                appsrc.push_buffer(buffer).unwrap();
                pushed += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(stall);
        let attached = bridge.stats().producer_attached;
        producer.set_state(gst::State::Null).unwrap();
        attached
    };

    // A clean stop: the bridge runs dry only after the flow has gone.
    let attached_while_running = produce(2.0, Duration::ZERO);
    let stopped = bridge.stats();
    std::thread::sleep(Duration::from_secs(2));
    let down = bridge.stats();
    // A stop in the middle of a stall: the stall is a dropout, the stop is not.
    let attached_after_restart = produce(1.0, Duration::from_millis(300));
    let stalled = bridge.stats();
    std::thread::sleep(Duration::from_secs(2));
    let stalled_down = bridge.stats();
    // A restart whose first audio comes 1 s after it claimed the channel,
    // claimed before the previous flow's last audio has played out.
    produce(1.0, Duration::ZERO);
    let restarting = bridge.stats();
    let slow = gst::parse::launch(&format!(
        "appsrc name=src is-live=true format=time ! {} channel={channel}",
        audio_bridge::SINK_FACTORY
    ))
    .unwrap();
    slow.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let slow_start = bridge.stats();
    slow.set_state(gst::State::Null).unwrap();
    consumer.set_state(gst::State::Null).unwrap();

    assert!(attached_while_running, "a running producer is attached");
    assert!(
        !down.producer_attached,
        "a stopped producer is not: {down:?}"
    );
    assert!(attached_after_restart, "a restarted producer is attached");
    assert_eq!(
        down.underruns, stopped.underruns,
        "stopping the producer flow counted as an underrun: {down:?}"
    );
    assert!(
        down.underrun_ms - stopped.underrun_ms < 100.0,
        "a producer flow stopped for 2 s was counted as {:.0} ms of dropout: {down:?}",
        down.underrun_ms - stopped.underrun_ms
    );
    assert!(
        stalled.underrun_ms - down.underrun_ms >= 150.0,
        "set-up: the 300 ms stall before the stop is a dropout: {stalled:?}"
    );
    assert!(
        stalled_down.underrun_ms - stalled.underrun_ms < 100.0
            && stalled_down.longest_underrun_ms < 500.0,
        "the 2 s after a stall's flow stopped were counted as {:.0} ms of dropout: \
         {stalled_down:?}",
        stalled_down.underrun_ms - stalled.underrun_ms
    );
    assert!(
        slow_start.producer_attached,
        "set-up: the new flow holds the channel"
    );
    assert!(
        slow_start.underrun_ms - restarting.underrun_ms < 100.0,
        "a new producer's 1 s before its first audio was counted as {:.0} ms of dropout: \
         {slow_start:?}",
        slow_start.underrun_ms - restarting.underrun_ms
    );
}

/// A source that is not clock-paced pushes as fast as the machine allows, so
/// the bridge can never catch up. It must report that once, in the statistics
/// and in one warning naming the channel, and keep playing: the report is a
/// guess from how the producer behaves, and a wrong guess must never silence a
/// live contributor.
#[test]
fn a_producer_faster_than_real_time_is_reported_once_and_keeps_playing() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    // Count this test's own log lines; other tests share the process.
    let fast_channel = "bridge-test-overrun-fast";
    let tag = format!("Channel '{fast_channel}'");
    let warnings = Arc::new(AtomicUsize::new(0));
    let skip_lines = Arc::new(AtomicUsize::new(0));
    gst::log::set_active(true);
    gst::log::set_threshold_for_name("stromaudiobridgesrc", gst::DebugLevel::Info);
    let logger = {
        let (warnings, skip_lines) = (warnings.clone(), skip_lines.clone());
        gst::log::add_log_function(move |cat, level, _, _, _, _, message| {
            if cat.name() != "stromaudiobridgesrc" {
                return;
            }
            let Some(text) = message.get() else { return };
            if !text.contains(tag.as_str()) {
                return;
            }
            if level == gst::DebugLevel::Warning {
                warnings.fetch_add(1, Ordering::Relaxed);
            } else if text.contains("beyond recovery") {
                skip_lines.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    // Statistics after 2 s, and the peak level of the second half of the output.
    let run = |is_live: bool, channel: &str| {
        let producer = gst::parse::launch(&format!(
            "audiotestsrc is-live={is_live} ! audioconvert ! audioresample ! \
             capsfilter name=caps ! {} channel={channel}",
            audio_bridge::SINK_FACTORY
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        producer
            .by_name("caps")
            .unwrap()
            .set_property("caps", audio_bridge::caps());
        let consumer = gst::parse::launch(&format!(
            "{} name=bridge channel={channel} ! appsink name=out sync=false",
            audio_bridge::SRC_FACTORY
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let bridge = consumer
            .by_name("bridge")
            .unwrap()
            .downcast::<AudioBridgeSrc>()
            .unwrap();
        let peaks = Arc::new(Mutex::new(Vec::<f32>::new()));
        let sink_peaks = peaks.clone();
        consumer
            .by_name("out")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap()
            .set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().unwrap();
                        let map = buffer.map_readable().unwrap();
                        let peak = map
                            .as_slice()
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs())
                            .fold(0.0, f32::max);
                        sink_peaks.lock().unwrap().push(peak);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        consumer.set_state(gst::State::Playing).unwrap();
        producer.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        let stats = bridge.stats();
        producer.set_state(gst::State::Null).unwrap();
        consumer.set_state(gst::State::Null).unwrap();
        // The overrun is declared about half a second in; the second second
        // of output is well clear of it.
        let peaks = peaks.lock().unwrap();
        let tail = peaks[peaks.len() / 2..].iter().copied().fold(0.0, f32::max);
        (stats, tail)
    };

    let (live, live_tail) = run(true, "bridge-test-overrun-live");
    let (fast, fast_tail) = run(false, fast_channel);
    gst::log::remove_log_function(logger);

    assert!(
        !live.producer_overrun,
        "a live producer is not an overrun: {live:?}"
    );
    assert_eq!(live.skips, 0, "{live:?}");
    assert!(
        live_tail > 0.1,
        "a live producer is heard: peak {live_tail}"
    );

    assert!(
        fast.producer_overrun,
        "a producer faster than real time is reported: {fast:?}"
    );
    assert!(
        fast.overflow_ms > 1000.0,
        "it overran the input, which is what makes it unrecoverable: {fast:?}"
    );
    assert!(
        fast_tail > 0.1,
        "the output was silenced after the overrun was declared: peak {fast_tail}"
    );
    assert_eq!(
        warnings.load(Ordering::Relaxed),
        1,
        "the overrun is warned about once, naming the channel"
    );
    let skips_logged = skip_lines.load(Ordering::Relaxed) as u64;
    assert!(
        skips_logged < OVERRUN_SKIPS,
        "{skips_logged} skip lines logged for {} skips: skips are not logged once the \
         overrun is declared",
        fast.skips
    );
}

/// A live producer takes over back to back from one that outran the reader,
/// while the old one's backlog is still queued. The old producer's leftovers
/// must not be played ahead of the new one's audio: the output fades out, goes
/// quiet, and the new producer fades in, with no jump anywhere after that.
#[test]
fn a_producer_handover_after_an_overrun_does_not_click() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    let channel = "bridge-test-handover";
    let producer = |live: bool| {
        let p = gst::parse::launch(&format!(
            "audiotestsrc is-live={live} freq=200 volume=0.8 ! audioconvert ! audioresample ! \
             capsfilter name=caps ! {} channel={channel}",
            audio_bridge::SINK_FACTORY
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        p.by_name("caps")
            .unwrap()
            .set_property("caps", audio_bridge::caps());
        p.set_state(gst::State::Playing).unwrap();
        p
    };
    let consumer = gst::parse::launch(&format!(
        "{} name=bridge channel={channel} ! appsink name=out sync=false",
        audio_bridge::SRC_FACTORY
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let bridge = consumer
        .by_name("bridge")
        .unwrap()
        .downcast::<AudioBridgeSrc>()
        .unwrap();
    let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let sink_samples = samples.clone();
    consumer
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap()
        .set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let map = sample.buffer().unwrap().map_readable().unwrap();
                    sink_samples.lock().unwrap().extend(
                        map.as_slice()
                            .chunks_exact(8)
                            .map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]])),
                    );
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    consumer.set_state(gst::State::Playing).unwrap();

    let mut handovers = Vec::new();
    for _ in 0..3 {
        let fast = producer(false);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            bridge.stats().producer_overrun,
            "set-up: the overrun is declared"
        );
        let at = samples.lock().unwrap().len();
        fast.set_state(gst::State::Null).unwrap();
        drop(fast);
        // Longer than a reader tick, as any real handover is, so the reader
        // skips the flood down to the target before the new producer arrives.
        std::thread::sleep(Duration::from_millis(15));
        let live = producer(true);
        handovers.push(at);
        std::thread::sleep(Duration::from_millis(1500));
        live.set_state(gst::State::Null).unwrap();
        drop(live);
        std::thread::sleep(Duration::from_millis(300));
    }
    consumer.set_state(gst::State::Null).unwrap();

    // A 200 Hz sine at 0.8 moves at most 0.021 per sample; a fade adds little.
    // Before the handover the old producer's output is chopped by skips, which
    // is what an overrun sounds like; what matters is what follows it.
    let samples = std::mem::take(&mut *samples.lock().unwrap());
    for (i, at) in handovers.into_iter().enumerate() {
        let end = (at + 24_000).min(samples.len());
        let mut quiet_until = None;
        let mut j = at;
        while j < end {
            if samples[j] == 0.0 {
                let run = j;
                while j < end && samples[j] == 0.0 {
                    j += 1;
                }
                if j - run >= 480 {
                    quiet_until = Some(j);
                }
            }
            j += 1;
        }
        let quiet_until = quiet_until.unwrap_or_else(|| {
            panic!("handover {i}: the new producer was joined to the old one's audio, no gap")
        });
        let after = &samples[quiet_until..end];
        assert!(
            after.iter().any(|v| v.abs() > 0.1),
            "handover {i}: the new producer is never heard"
        );
        let step = after
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            step < 0.06,
            "handover {i}: a {step:.3} jump in one sample after the handover"
        );
    }
}

/// A dropout always fades out, even when the ring empties exactly on a reader
/// tick and nothing is left to fade. With time-scaling off every tick takes
/// exactly one period from a ring filled in 20 ms buffers, so every underrun
/// lands on that boundary. A steady signal makes the edge readable: a full
/// fade falls by at most ~0.003 per sample, a cut by the whole level.
#[test]
fn a_dropout_fades_out_even_when_nothing_is_left_to_fade() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    const LEVEL: f32 = 0.5;
    let channel = "bridge-test-dropout-edge";
    let producer = gst::parse::launch(&format!(
        "appsrc name=src is-live=true format=time ! {} channel={channel}",
        audio_bridge::SINK_FACTORY
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let consumer = gst::parse::launch(&format!(
        "{} name=bridge channel={channel} max-rate-change=0 ! appsink name=out sync=false",
        audio_bridge::SRC_FACTORY
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let sink_samples = samples.clone();
    consumer
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap()
        .set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let map = sample.buffer().unwrap().map_readable().unwrap();
                    sink_samples.lock().unwrap().extend(
                        map.as_slice()
                            .chunks_exact(8)
                            .map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]])),
                    );
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    producer.set_state(gst::State::Playing).unwrap();
    consumer.set_state(gst::State::Playing).unwrap();
    let appsrc = producer
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    appsrc.set_caps(Some(&audio_bridge::caps()));

    // 20 ms buffers of a steady level; a 300 ms stall at 1.5 s, then what was
    // held is handed over at once.
    let chunk = RATE / 50;
    let start = Instant::now();
    let (mut pushed, mut held) = (0usize, 0usize);
    while start.elapsed().as_secs_f64() < 4.0 {
        let t = start.elapsed().as_secs_f64();
        let due = (t * 50.0) as usize + 1;
        while pushed + held < due {
            if (1.5..1.8).contains(&t) {
                held += 1;
                continue;
            }
            for _ in 0..=held {
                let data: Vec<u8> = std::iter::repeat_n(LEVEL.to_le_bytes(), chunk * 2)
                    .flatten()
                    .collect();
                let mut buffer = gst::Buffer::from_mut_slice(data);
                buffer
                    .get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_nseconds(pushed as u64 * 20_000_000));
                appsrc.push_buffer(buffer).unwrap();
                pushed += 1;
            }
            held = 0;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    consumer.set_state(gst::State::Null).unwrap();
    producer.set_state(gst::State::Null).unwrap();

    // Each run of silence longer than a period is a dropout; look at the
    // largest single-sample fall in the 600 samples before it.
    let samples = std::mem::take(&mut *samples.lock().unwrap());
    let mut dropouts = Vec::new();
    let mut i = 600;
    while i < samples.len() {
        if samples[i] == 0.0 {
            let begin = i;
            while i < samples.len() && samples[i] == 0.0 {
                i += 1;
            }
            if i - begin >= 480 {
                let fall = samples[begin - 600..=begin]
                    .windows(2)
                    .map(|w| w[0] - w[1])
                    .fold(0.0f32, f32::max);
                dropouts.push((begin as f64 / RATE as f64, fall));
            }
        }
        i += 1;
    }
    assert!(!dropouts.is_empty(), "set-up: the stall causes a dropout");
    for (at, fall) in dropouts {
        assert!(
            fall < 0.01,
            "the dropout at {at:.3} s cut in: {fall:.3} fall in one sample at level {LEVEL}"
        );
    }
}

/// An overrun belongs to the producer that caused it. A live producer that
/// takes its place is heard at once; when a producer stops, the report clears
/// after a few seconds with no audio; and each non-live producer is reported,
/// and warned about, once.
#[test]
fn an_overrun_ends_with_its_producer() {
    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    let channel = "bridge-test-overrun-ends";
    let tag = format!("Channel '{channel}'");
    let warnings = Arc::new(AtomicUsize::new(0));
    gst::log::set_active(true);
    gst::log::set_threshold_for_name("stromaudiobridgesrc", gst::DebugLevel::Warning);
    let logger = {
        let warnings = warnings.clone();
        gst::log::add_log_function(move |cat, level, _, _, _, _, message| {
            if cat.name() == "stromaudiobridgesrc"
                && level == gst::DebugLevel::Warning
                && message.get().is_some_and(|m| m.contains(tag.as_str()))
            {
                warnings.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let consumer = gst::parse::launch(&format!(
        "{} name=bridge channel={channel} ! appsink name=out sync=false",
        audio_bridge::SRC_FACTORY
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let bridge = consumer
        .by_name("bridge")
        .unwrap()
        .downcast::<AudioBridgeSrc>()
        .unwrap();
    // When, after `listen` is set, the output first carries sound.
    let listen: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let heard: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let (l, h) = (listen.clone(), heard.clone());
    consumer
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap()
        .set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let map = sample.buffer().unwrap().map_readable().unwrap();
                    let peak = map
                        .as_slice()
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs())
                        .fold(0.0, f32::max);
                    if l.lock().unwrap().is_some() && peak > 0.1 {
                        h.lock().unwrap().get_or_insert_with(Instant::now);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    consumer.set_state(gst::State::Playing).unwrap();

    let producer = |live: bool| {
        let p = gst::parse::launch(&format!(
            "audiotestsrc is-live={live} ! audioconvert ! audioresample ! \
             capsfilter name=caps ! {} channel={channel}",
            audio_bridge::SINK_FACTORY
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        p.by_name("caps")
            .unwrap()
            .set_property("caps", audio_bridge::caps());
        p.set_state(gst::State::Playing).unwrap();
        p
    };

    // A non-live producer is reported, then stopped, and a live producer takes
    // its place a second later, before the stop alone would clear the report.
    let fast = producer(false);
    std::thread::sleep(Duration::from_secs(2));
    let during = bridge.stats();
    fast.set_state(gst::State::Null).unwrap();
    drop(fast);
    std::thread::sleep(Duration::from_secs(1));
    let started = Instant::now();
    *listen.lock().unwrap() = Some(started);
    let live = producer(true);
    std::thread::sleep(Duration::from_secs(2));
    let heard_after = heard.lock().unwrap().map(|t| t.duration_since(started));
    let replaced = bridge.stats();
    live.set_state(gst::State::Null).unwrap();
    drop(live);

    // A second non-live producer, then stopped with nothing after it.
    std::thread::sleep(Duration::from_millis(500));
    let again = producer(false);
    std::thread::sleep(Duration::from_secs(2));
    let second = bridge.stats();
    again.set_state(gst::State::Null).unwrap();
    drop(again);
    std::thread::sleep(Duration::from_millis(300));
    let just_stopped = bridge.stats();
    std::thread::sleep(Duration::from_secs(6));
    let after_stop = bridge.stats();
    consumer.set_state(gst::State::Null).unwrap();
    gst::log::remove_log_function(logger);

    assert!(during.producer_overrun, "set-up: the overrun is reported");
    let heard_after = heard_after.expect("the live producer that replaced it is heard");
    assert!(
        heard_after < Duration::from_millis(500),
        "the live producer was silenced for {heard_after:?} by the previous overrun"
    );
    assert!(
        !replaced.producer_overrun,
        "the report outlived the producer it was about: {replaced:?}"
    );
    assert!(
        second.producer_overrun,
        "a second non-live producer is reported"
    );
    assert!(
        !just_stopped.producer_overrun && !just_stopped.producer_attached,
        "an overrun is reported for a producer whose flow has stopped: {just_stopped:?}"
    );
    assert!(
        !after_stop.producer_overrun,
        "the report outlived a producer that stopped: {after_stop:?}"
    );
    assert_eq!(
        warnings.load(Ordering::Relaxed),
        2,
        "each overrun is warned about once"
    );
}

/// Both blocks in real flows, through `PipelineManager`. The conversation flow
/// keeps running while the program flow is restarted, which only works if a
/// stopped writer gives its role back. Every element of both flows must be
/// finalized afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocks_survive_a_restart_of_the_other_flow_and_release_everything() {
    use strom::blocks::BlockRegistry;
    use strom::events::EventBroadcaster;
    use strom::gst::pipeline::PipelineManager;
    use strom_types::{BlockInstance, Flow, Link, PropertyValue};

    init();
    if !has_scaletempo() {
        panic!("scaletempo (gst-plugins-good) is required");
    }
    let channel = "bridge-test-lifecycle";
    let props = |pairs: &[(&str, PropertyValue)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    };
    let block = |id: &str, def: &str, p: &[(&str, PropertyValue)]| BlockInstance {
        id: id.to_string(),
        block_definition_id: def.to_string(),
        name: None,
        properties: props(p),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    };
    let element = |id: &str, kind: &str, p: &[(&str, PropertyValue)]| strom_types::Element {
        id: id.to_string(),
        element_type: kind.to_string(),
        properties: props(p),
        position: [0.0, 0.0].into(),
        pad_properties: Default::default(),
    };

    let mut program = Flow::new("bridge_program");
    program.elements.push(element(
        "src",
        "audiotestsrc",
        &[("is-live", PropertyValue::Bool(true))],
    ));
    program.blocks.push(block(
        "out",
        "builtin.audio_bridge_output",
        &[("channel", PropertyValue::String(channel.into()))],
    ));
    program.links.push(Link {
        from: "src:src".into(),
        to: "out:audio_in".into(),
    });

    let mut conversation = Flow::new("bridge_conversation");
    conversation.blocks.push(block(
        "in",
        "builtin.audio_bridge_input",
        &[
            ("channel", PropertyValue::String(channel.into())),
            ("target_latency_ms", PropertyValue::UInt(60)),
        ],
    ));
    conversation.elements.push(element(
        "sink",
        "fakesink",
        &[("sync", PropertyValue::Bool(false))],
    ));
    conversation.links.push(Link {
        from: "in:audio_out".into(),
        to: "sink:sink".into(),
    });

    let registry_file = tempfile::NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(registry_file.path());
    let manager = |flow: &Flow| {
        PipelineManager::new(
            flow,
            EventBroadcaster::new(10),
            &registry,
            vec![],
            "all".to_string(),
            None,
            std::env::temp_dir(),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        )
        .expect("PipelineManager")
    };
    let finalized = |mut m: PipelineManager, what: &str| {
        let pipeline = m.pipeline_weak();
        let elements = m.element_weak_refs();
        m.stop().expect("stop");
        drop(m);
        assert!(pipeline.upgrade().is_none(), "{what}: pipeline leaked");
        let leaked: Vec<_> = elements
            .iter()
            .filter_map(|(name, weak)| weak.upgrade().map(|_| name.clone()))
            .collect();
        assert!(leaked.is_empty(), "{what}: elements leaked: {leaked:?}");
    };

    let mut conv = manager(&conversation);
    conv.start().expect("start conversation");
    for round in 0..2 {
        let mut prog = manager(&program);
        let state = prog.start().expect("start program");
        assert_eq!(state, strom_types::PipelineState::Playing, "round {round}");
        tokio::time::sleep(Duration::from_millis(500)).await;

        let bridge = conv
            .pipeline()
            .by_name("in:bridge")
            .unwrap()
            .downcast::<AudioBridgeSrc>()
            .unwrap();
        let stats = bridge.stats();
        assert_eq!(stats.target_latency_ms, 60);
        assert!(
            stats.depth_ms > 0.0,
            "round {round}: audio arrives: {stats:?}"
        );
        drop(bridge);
        finalized(prog, "program");
        // Let the reader drain what is left, so the next round shows the new
        // writer's audio rather than the old one's.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    finalized(conv, "conversation");
}
