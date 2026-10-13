//! An input that ends must not stop a live audio bus from following the clock.
//!
//! A live source that gives up pushes EOS: `ndisrc` after its receive timeout,
//! `srtsrc` when a caller leaves and `keep_listening` is off, and either of
//! those through an inter-flow link. `audiomixer` counts an EOS pad as ready,
//! and with `ignore-inactive-pads` it skips every pad that has never had a
//! buffer. When the ended input is the only one that ever carried audio, every
//! pad it still looks at is ready, so it aggregates without waiting for the
//! clock: silence, as fast as the CPU allows. A synced consumer then sits on a
//! backlog, and a source that joins afterwards is stamped behind the bus and
//! dropped.
//!
//! Every test here runs through the block's own builder, so what is guarded is
//! the block, not a hand-built `audiomixer`.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strom::blocks::builtin::{liveaudiorouter, mixer};
use strom::blocks::{BlockBuildContext, BlockBuildResult, BlockBuilder};
use strom_types::PropertyValue;

/// CI installs gstreamer1.0-plugins-{base,good,bad}, which covers all of
/// these, so a missing element is a failure and not a reason to skip.
const REQUIRED_ELEMENTS: &[&str] = &[
    "audiomixer",
    "audioconvert",
    "audiotestsrc",
    "volume",
    "level",
    "tee",
    "queue",
    "capsfilter",
    "capssetter",
    "deinterleave",
    "audiointerleave",
    "valve",
    "rglimiter",
    "identity",
    "appsink",
];

fn require_elements() {
    gst::init().unwrap();
    let missing: Vec<&str> = REQUIRED_ELEMENTS
        .iter()
        .copied()
        .filter(|n| gst::ElementFactory::find(n).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "missing GStreamer elements {missing:?} — install gstreamer1.0-plugins-{{base,good,bad}}"
    );
}

fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

struct Harness {
    pipeline: gst::Pipeline,
    elements: HashMap<String, gst::Element>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Request pads created at build time are already present, so look in `pads()`
/// before asking for a new one.
fn resolve_pad(element: &gst::Element, name: &str) -> gst::Pad {
    if let Some(pad) = element.static_pad(name) {
        return pad;
    }
    if let Some(pad) = element.pads().into_iter().find(|p| p.name() == name) {
        return pad;
    }
    element
        .request_pad_simple(name)
        .unwrap_or_else(|| panic!("element {} has no pad {name}", element.name()))
}

/// Assemble a builder's result into a pipeline the way the pipeline manager does.
fn assemble(result: BlockBuildResult) -> Harness {
    let pipeline = gst::Pipeline::new();
    let mut elements: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &result.elements {
        pipeline.add(element).expect("add element");
        elements.insert(id.clone(), element.clone());
    }
    for (from, to) in &result.internal_links {
        let src = &elements[&from.element_id];
        let dst = &elements[&to.element_id];
        match (&from.pad_name, &to.pad_name) {
            (Some(src_pad), Some(dst_pad)) => {
                resolve_pad(src, src_pad)
                    .link(&resolve_pad(dst, dst_pad))
                    .unwrap_or_else(|e| {
                        panic!(
                            "link {}:{src_pad} -> {}:{dst_pad}: {e:?}",
                            from.element_id, to.element_id
                        )
                    });
            }
            (Some(src_pad), None) => {
                let dp = dst
                    .compatible_pad(&resolve_pad(src, src_pad), None)
                    .expect("compatible sink pad");
                resolve_pad(src, src_pad).link(&dp).expect("pad link");
            }
            _ => src.link(dst).expect("element link"),
        }
    }
    Harness { pipeline, elements }
}

fn build_mixer(instance: &str, force_live: bool) -> Harness {
    require_elements();
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = mixer::MixerBuilder
        .build(
            instance,
            &props(&[
                ("num_channels", PropertyValue::UInt(2)),
                ("force_live", PropertyValue::Bool(force_live)),
            ]),
            &ctx,
        )
        .expect("mixer build");
    assemble(result)
}

fn build_router(instance: &str, force_live: bool) -> Harness {
    require_elements();
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = liveaudiorouter::LiveAudioRouterBuilder
        .build(
            instance,
            &props(&[
                ("force_live", PropertyValue::Bool(force_live)),
                ("num_inputs", PropertyValue::UInt(2)),
                ("num_outputs", PropertyValue::UInt(1)),
                ("input_0_channels", PropertyValue::UInt(1)),
                ("input_1_channels", PropertyValue::UInt(1)),
                ("output_0_channels", PropertyValue::UInt(1)),
                (
                    "routing_matrix",
                    PropertyValue::String(r#"{"i0c0":["o0c0"],"i1c0":["o0c0"]}"#.to_string()),
                ),
            ]),
            &ctx,
        )
        .expect("liveaudiorouter build");
    assemble(result)
}

/// A live mono tone into `target`, ending with EOS after `buffers` buffers of
/// 480 samples if given. Live `audiotestsrc` stamps running time, as a live
/// network source does. The rate is left to the block: on GStreamer 1.24 a
/// force-live bus can settle on its rate before any input links, and the mixer
/// channel has no resampler, so a pinned rate fails with not-negotiated.
fn feed(h: &Harness, target: &str, volume: f64, buffers: Option<i32>) {
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property("freq", 440.0)
        .property("volume", volume)
        .property("samplesperbuffer", 480i32)
        .property("num-buffers", buffers.unwrap_or(-1))
        .build()
        .expect("audiotestsrc");
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("channels", 1i32)
                .build(),
        )
        .build()
        .expect("capsfilter");
    h.pipeline.add_many([&src, &caps]).expect("add source");
    src.link(&caps).expect("link source");
    caps.link(&h.elements[target])
        .expect("link source to block");
    for e in [&src, &caps] {
        e.sync_state_with_parent().expect("sync source state");
    }
}

/// One output buffer as the consumer saw it.
#[derive(Clone, Copy, Debug)]
struct Seen {
    pts: gst::ClockTime,
    /// Pipeline running time when the buffer reached the consumer.
    arrived: gst::ClockTime,
    peak: f32,
}

/// Attach a consumer after `upstream`. `sync` picks a consumer that plays out
/// on the clock, as a WHEP viewer or an audio output does, or one that takes
/// buffers as they come.
fn tap(h: &Harness, upstream: &str, sync: bool) -> Arc<Mutex<Vec<Seen>>> {
    let queue = gst::ElementFactory::make("queue").build().expect("queue");
    let format = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .build(),
        )
        .build()
        .expect("tap capsfilter");
    let sink = gst_app::AppSink::builder().sync(sync).build();
    h.pipeline
        .add_many([&queue, &format, sink.upcast_ref()])
        .expect("add tap");
    gst::Element::link_many([&queue, &format, sink.upcast_ref()]).expect("link tap");
    h.elements[upstream]
        .link(&queue)
        .expect("link tap to block");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_cb = seen.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let (Some(buffer), Some(arrived)) = (sample.buffer(), sink.current_running_time())
                else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                let Some(pts) = buffer.pts() else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let peak = map
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b).abs())
                    .fold(0.0f32, f32::max);
                seen_cb.lock().unwrap().push(Seen { pts, arrived, peak });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    seen
}

fn run_for(h: &Harness, d: Duration) {
    let bus = h.pipeline.bus().expect("bus");
    let deadline = std::time::Instant::now() + d;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(left.as_millis() as u64))
        else {
            break;
        };
        if let gst::MessageView::Error(e) = msg.view() {
            panic!(
                "pipeline error from {:?}: {} ({:?})",
                e.src().map(|s| s.path_string()),
                e.error(),
                e.debug()
            );
        }
    }
}

/// How far the bus ran ahead of the clock: the largest output PTS minus the
/// running time it reached the consumer at.
fn max_lead(seen: &[Seen]) -> gst::ClockTime {
    seen.iter()
        .map(|s| s.pts.saturating_sub(s.arrived))
        .max()
        .unwrap_or(gst::ClockTime::ZERO)
}

fn describe(seen: &[Seen]) -> String {
    format!(
        "{} buffers, last pts {:?} arrived at {:?}",
        seen.len(),
        seen.last().map(|s| s.pts),
        seen.last().map(|s| s.arrived)
    )
}

/// The bus may legitimately stamp output up to its own latency ahead of when a
/// free-running consumer takes it. A runaway is seconds to hours ahead.
const LEAD_LIMIT: gst::ClockTime = gst::ClockTime::from_mseconds(300);

fn assert_follows_clock(what: &str, h: &Harness, output: &str) {
    let seen = tap(h, output, false);
    h.pipeline.set_state(gst::State::Playing).expect("Playing");
    run_for(h, Duration::from_millis(2500));
    let seen = seen.lock().unwrap().clone();
    let lead = max_lead(&seen);
    eprintln!("{what}: max lead {lead}, {}", describe(&seen));
    assert!(!seen.is_empty(), "{what}: bus produced nothing");
    assert!(
        lead <= LEAD_LIMIT,
        "{what}: after its only input ended the bus ran {lead} ahead of the clock \
         ({})",
        describe(&seen)
    );
}

/// How soon a source that joins after the first input ended must be heard.
/// With an idle bus it is heard within about 100 ms; behind a runaway, a synced
/// consumer's queue holds up to a second of silence stamped ahead of it.
const JOIN_LIMIT: gst::ClockTime = gst::ClockTime::from_mseconds(400);

fn assert_later_input_heard_promptly(what: &str, h: &Harness, output: &str, later_input: &str) {
    let seen = tap(h, output, true);
    h.pipeline.set_state(gst::State::Playing).expect("Playing");
    run_for(h, Duration::from_millis(1200));
    feed(h, later_input, 0.5, None);
    let joined = h.pipeline.current_running_time().expect("running time");
    run_for(h, Duration::from_millis(1500));
    let seen = seen.lock().unwrap().clone();
    let heard_after = seen
        .iter()
        .find(|s| s.arrived > joined && s.peak > 0.1)
        .map(|s| s.arrived - joined);
    eprintln!(
        "{what}: joined at {joined}, heard after {heard_after:?}, {}",
        describe(&seen)
    );
    let heard_after = heard_after.unwrap_or_else(|| {
        panic!(
            "{what}: a source that joined after the first input ended was never heard ({})",
            describe(&seen)
        )
    });
    assert!(
        heard_after <= JOIN_LIMIT,
        "{what}: a source that joined after the first input ended was heard only after \
         {heard_after} ({})",
        describe(&seen)
    );
}

#[test]
fn mixer_main_bus_follows_clock_after_its_only_input_ends() {
    let h = build_mixer("eos_mix_a", true);
    feed(&h, "eos_mix_a:convert_0", 0.5, Some(40));
    assert_follows_clock("mixer main", &h, "eos_mix_a:main_out_tee");
}

#[test]
fn mixer_hears_a_later_input_promptly_after_the_first_one_ends() {
    let h = build_mixer("eos_mix_b", true);
    feed(&h, "eos_mix_b:convert_0", 0.5, Some(40));
    assert_later_input_heard_promptly(
        "mixer main",
        &h,
        "eos_mix_b:main_out_tee",
        "eos_mix_b:convert_1",
    );
}

#[test]
fn liveaudiorouter_output_follows_clock_after_its_only_input_ends() {
    let h = build_router("eos_rt_a", true);
    feed(&h, "eos_rt_a:identity_in_0", 0.5, Some(40));
    assert_follows_clock("router out", &h, "eos_rt_a:queue_out_0");
}

#[test]
fn liveaudiorouter_hears_a_later_input_promptly_after_the_first_one_ends() {
    let h = build_router("eos_rt_b", true);
    feed(&h, "eos_rt_b:identity_in_0", 0.5, Some(40));
    assert_later_input_heard_promptly(
        "router out",
        &h,
        "eos_rt_b:queue_out_0",
        "eos_rt_b:identity_in_1",
    );
}

/// Without force-live a bus is meant to end with its inputs, so the ended
/// input's EOS must still get through.
fn assert_ends_with_its_input(what: &str, h: &Harness, output: &str) {
    let seen = tap(h, output, false);
    h.pipeline.set_state(gst::State::Playing).expect("Playing");
    let msg = h.pipeline.bus().expect("bus").timed_pop_filtered(
        gst::ClockTime::from_seconds(3),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let seen = seen.lock().unwrap().clone();
    assert!(
        matches!(
            msg.as_ref().map(|m| m.view()),
            Some(gst::MessageView::Eos(_))
        ),
        "{what} without force-live must end when its only input ends, got {msg:?} ({})",
        describe(&seen)
    );
}

#[test]
fn mixer_without_force_live_ends_with_its_input() {
    let h = build_mixer("eos_mix_nfl", false);
    feed(&h, "eos_mix_nfl:convert_0", 0.5, Some(40));
    assert_ends_with_its_input("mixer main", &h, "eos_mix_nfl:main_out_tee");
}

#[test]
fn liveaudiorouter_without_force_live_ends_with_its_input() {
    let h = build_router("eos_rt_nfl", false);
    feed(&h, "eos_rt_nfl:identity_in_0", 0.5, Some(40));
    assert_ends_with_its_input("router out", &h, "eos_rt_nfl:queue_out_0");
}
