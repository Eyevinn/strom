//! Tests for `builtin.audioprocessor`, built through its real builder.
//!
//! The block wraps strom-dsp's `stromdsp-broadcastprocessor`. Strom registers
//! strom-dsp statically, so the element is part of every build and these
//! tests never skip on it: a build without it fails them.
//!

use crate::common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::audioprocessor::{AudioProcessorBuilder, ELEMENT_FACTORY};
use strom::blocks::BlockBuilder;
use strom::events::EventBroadcaster;
use strom_types::audioprocessor::*;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Flow, Link, PropertyValue, StromEvent};

const NEEDS_ELEMENT: &str = "needs stromdsp-broadcastprocessor from strom-dsp (phase 2)";

/// Initialise GStreamer and register strom-dsp, through the same once-per-
/// process helper the mixer tests use.
fn init() {
    gst::init().expect("GStreamer initialises");
    strom::gst::register_dsp_plugins();
}

fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn s(v: &str) -> PropertyValue {
    PropertyValue::String(v.to_string())
}

/// `PropertyValue` has no `PartialEq`; compare the float a reading holds.
fn float(v: Option<&PropertyValue>) -> Option<f64> {
    match v {
        Some(PropertyValue::Float(f)) => Some(*f),
        _ => None,
    }
}

fn preset_nick(element: &gst::Element) -> String {
    let v = element.property_value("preset");
    gst::glib::EnumValue::from_value(&v)
        .map(|(_, e)| e.nick().to_string())
        .expect("preset is an enum")
}

/// The block between `src` and a sink, built and linked.
struct Assembled {
    pipeline: gst::Pipeline,
    processor: gst::Element,
    sink: gst::Element,
    built: strom::blocks::BlockBuildResult,
}

fn assemble(src: &str, properties: &HashMap<String, PropertyValue>) -> Assembled {
    let ctx = common::block::context();
    let built = AudioProcessorBuilder
        .build("proc", properties, &ctx)
        .unwrap_or_else(|e| panic!("build failed: {e}"));
    let pipeline = gst::Pipeline::new();
    let elements = common::block::install(&pipeline, &built);
    let src = gst::parse::bin_from_description(src, true).expect("source");
    // A fakesink, not an appsink: an appsink holds EOS back until every
    // queued sample has been pulled.
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline.add(&src).unwrap();
    pipeline.add(&sink).unwrap();
    src.link(&elements["proc:convert"]).expect("link source");
    elements["proc:processor"]
        .link(&sink)
        .expect("link processor to sink");
    Assembled {
        pipeline,
        processor: elements["proc:processor"].clone(),
        sink,
        built,
    }
}

/// Run to EOS. Panics with the error a pipeline posts instead.
fn run_to_eos(pipeline: &gst::Pipeline) {
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("EOS or an error within 20 s");
    if let gst::MessageView::Error(e) = msg.view() {
        panic!("pipeline error: {} ({:?})", e.error(), e.debug());
    }
}

#[test]
fn strom_registers_the_broadcast_processor() {
    init();
    assert!(
        gst::ElementFactory::find(ELEMENT_FACTORY).is_some(),
        "{NEEDS_ELEMENT}"
    );
}

/// The element takes F32LE interleaved only. The block converts on its own
/// input (BLOCK_GUIDELINES: the consumer adapts), so 16-bit audio at 44.1 kHz
/// goes through, at the rate it arrived in. Fails with not-negotiated if the
/// converter is taken out.
#[test]
fn s16_input_is_converted_and_its_rate_kept() {
    init();
    let a = assemble(
        "audiotestsrc num-buffers=50 ! capsfilter caps=\"audio/x-raw,format=S16LE,rate=44100,channels=2\"",
        &HashMap::new(),
    );
    run_to_eos(&a.pipeline);
    let caps = a
        .sink
        .static_pad("sink")
        .and_then(|p| p.current_caps())
        .expect("negotiated caps");
    let st = caps.structure(0).unwrap();
    assert_eq!(st.get::<&str>("format").unwrap(), "F32LE");
    assert_eq!(st.get::<i32>("rate").unwrap(), 44100);
    assert_eq!(st.get::<i32>("channels").unwrap(), 2);
    a.pipeline.set_state(gst::State::Null).unwrap();
}

/// With Custom the block writes every setting itself.
#[test]
fn custom_preset_writes_the_block_settings() {
    init();
    let a = assemble(
        "audiotestsrc num-buffers=1",
        &props(&[
            ("preset", s(PRESET_CUSTOM)),
            ("target_loudness", PropertyValue::Float(-14.0)),
            ("agc_enabled", PropertyValue::Bool(false)),
            ("num_bands", PropertyValue::UInt(3)),
            ("spike_threshold", PropertyValue::Float(12.0)),
            ("clip_budget", PropertyValue::Float(4.0)),
            ("bass_clipper_enabled", PropertyValue::Bool(false)),
        ]),
    );
    let p = &a.processor;
    assert_eq!(preset_nick(p), PRESET_CUSTOM);
    assert_eq!(p.property::<f32>("spike-threshold"), 12.0);
    assert_eq!(p.property::<f32>("clip-budget"), 4.0);
    assert!(!p.property::<bool>("bass-clipper-enabled"));
    assert!(p.property::<bool>("governor-enabled"));
    // Not stored, so it follows the -14 LUFS target: target + 9 dB.
    assert_eq!(
        p.property::<f32>("bass-clip-threshold"),
        bass_clip_threshold_for_target(-14.0)
    );
    assert_eq!(
        p.property::<f32>("clipper-bandwidth"),
        DEFAULT_CLIPPER_BANDWIDTH_HZ
    );
    assert_eq!(p.property::<f32>("expander-hold"), DEFAULT_EXPANDER_HOLD_MS);
    assert_eq!(p.property::<f32>("target-loudness"), -14.0);
    assert!(!p.property::<bool>("agc-enabled"));
    assert_eq!(p.property::<u32>("num-bands"), 3);
    assert_eq!(
        p.property::<f32>("true-peak-ceiling"),
        DEFAULT_TRUE_PEAK_CEILING_DBTP
    );
    // Not stored: the standard latency mode's lookahead.
    assert_eq!(p.property::<f32>("limiter-lookahead"), 1.5);
    for n in 0..5 {
        assert_eq!(
            p.property::<f32>(&format!("band{n}-ratio")),
            DEFAULT_BAND_RATIO[n]
        );
        assert_eq!(
            p.property::<f32>(&format!("band{n}-threshold")),
            band_threshold_for_target(-14.0, n)
        );
        assert_eq!(
            p.property::<f32>(&format!("band{n}-expander-threshold")),
            band_expander_threshold_for_target(-14.0, n)
        );
        assert_eq!(
            p.property::<f32>(&format!("band{n}-expander-ratio")),
            DEFAULT_BAND_EXPANDER_RATIO[n]
        );
        assert_eq!(
            p.property::<f32>(&format!("band{n}-expander-floor")),
            DEFAULT_BAND_EXPANDER_FLOOR_DB[n]
        );
    }
    for (n, f) in DEFAULT_CROSSOVER_FREQUENCIES_HZ.iter().enumerate() {
        assert_eq!(p.property::<f32>(&format!("crossover{n}-frequency")), *f);
    }
}

/// A named preset is applied by the element, and the block's own settings do
/// not override it.
#[test]
fn named_preset_is_applied_by_the_element() {
    init();
    let a = assemble(
        "audiotestsrc num-buffers=1",
        &props(&[
            ("preset", s("streaming")),
            ("target_loudness", PropertyValue::Float(-30.0)),
            ("meter_interval", PropertyValue::UInt(250)),
            ("limiter_lookahead", PropertyValue::Float(10.0)),
        ]),
    );
    let p = &a.processor;
    // Presets do not set the lookahead, so the block sets it with every
    // preset, and doing so leaves the preset in place.
    assert_eq!(preset_nick(p), "streaming");
    assert_eq!(p.property::<f32>("limiter-lookahead"), 10.0);
    // The streaming preset's target (strom-dsp phase 2 spec, 3.2).
    assert_eq!(p.property::<f32>("target-loudness"), -16.0);
    assert_eq!(p.property::<u32>("meter-interval"), 250);
}

fn enum_nick(element: &gst::Element, property: &str) -> String {
    let v = element.property_value(property);
    gst::glib::EnumValue::from_value(&v)
        .map(|(_, e)| e.nick().to_string())
        .expect("an enum property")
}

/// The latency mode applies with a named preset and sets the limiter
/// lookahead, and the preset does not undo it.
#[test]
fn low_latency_mode_applies_with_a_named_preset() {
    init();
    let a = assemble(
        "audiotestsrc num-buffers=1",
        &props(&[("preset", s("speech")), ("latency_mode", s("low"))]),
    );
    let p = &a.processor;
    assert_eq!(preset_nick(p), "speech");
    assert_eq!(enum_nick(p, "latency-mode"), "low");
    // The low mode's limiter lookahead (strom-dsp broadcastprocessor docs).
    assert_eq!(p.property::<f32>("limiter-lookahead"), 0.6);
}

/// The default is the standard mode, with its own limiter lookahead.
#[test]
fn standard_latency_mode_is_the_default() {
    init();
    let a = assemble("audiotestsrc num-buffers=1", &HashMap::new());
    let p = &a.processor;
    assert_eq!(enum_nick(p, "latency-mode"), DEFAULT_LATENCY_MODE);
    // The standard mode's lookahead (strom-dsp broadcastprocessor docs).
    assert_eq!(p.property::<f32>("limiter-lookahead"), 1.5);
}

/// A lookahead of 0 follows the latency mode; an explicit one overrides it,
/// even in the low mode.
#[test]
fn an_explicit_limiter_lookahead_overrides_the_latency_mode() {
    init();
    let a = assemble(
        "audiotestsrc num-buffers=1",
        &props(&[
            ("latency_mode", s("low")),
            ("limiter_lookahead", PropertyValue::Float(0.0)),
        ]),
    );
    assert_eq!(a.processor.property::<f32>("limiter-lookahead"), 0.6);

    let a = assemble(
        "audiotestsrc num-buffers=1",
        &props(&[
            ("latency_mode", s("low")),
            ("limiter_lookahead", PropertyValue::Float(1.5)),
        ]),
    );
    assert_eq!(enum_nick(&a.processor, "latency-mode"), "low");
    assert_eq!(a.processor.property::<f32>("limiter-lookahead"), 1.5);
}

/// The element's meter message reaches clients as `AudioProcessorData`, keyed
/// by the block id, with one gain reduction and one expander reduction per
/// band.
#[test]
fn meter_messages_become_events() {
    init();
    let mut a = assemble(
        "audiotestsrc is-live=true wave=pink-noise ! capsfilter caps=\"audio/x-raw,rate=48000,channels=2\"",
        &props(&[
            ("num_bands", PropertyValue::UInt(3)),
            ("meter_interval", PropertyValue::UInt(100)),
        ]),
    );
    let events = EventBroadcaster::with_capacity(64);
    let mut rx = events.subscribe();
    let flow_id = strom_types::FlowId::new_v4();
    let bus = a.pipeline.bus().unwrap();
    bus.add_signal_watch();
    let connect = a.built.bus_message_handler.take().expect("meter handler");
    let handler = connect(&bus, flow_id, events.clone());

    // Signal watches dispatch on the default main context.
    let main_loop = gst::glib::MainLoop::new(None, false);
    let ml = main_loop.clone();
    std::thread::spawn(move || ml.run());

    a.pipeline.set_state(gst::State::Playing).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = None;
    while got.is_none() {
        match rx.try_recv() {
            Ok(StromEvent::AudioProcessorData {
                flow_id: f,
                element_id,
                band_gain_reduction,
                output_true_peak,
                band_expander_gain_reduction,
                clip_depth,
                ..
            }) => {
                got = Some((
                    f,
                    element_id,
                    band_gain_reduction,
                    output_true_peak,
                    band_expander_gain_reduction,
                    clip_depth,
                ))
            }
            Ok(_) => {}
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    a.pipeline.set_state(gst::State::Null).unwrap();
    bus.disconnect(handler);
    bus.remove_signal_watch();
    main_loop.quit();

    let (f, element_id, bands, tp, expander, clip_depth) =
        got.expect("an AudioProcessorData event within 10 s");
    assert_eq!(f, flow_id);
    assert_eq!(element_id, "proc");
    assert_eq!(bands.len(), 3);
    assert!(tp.is_finite());
    assert_eq!(expander.len(), 3);
    assert!(clip_depth.is_finite());
}

// ---- Live writes through AppState ----

fn flow(preset: &str, extra: &[(&str, PropertyValue)]) -> Flow {
    let mut flow = Flow::new("audioprocessor-test");
    let mut src_props = HashMap::new();
    src_props.insert("is-live".to_string(), PropertyValue::Bool(true));
    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "audiotestsrc".to_string(),
        properties: src_props,
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    });
    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::new(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    });
    let mut properties = props(extra);
    properties.insert(PRESET_PROPERTY.to_string(), s(preset));
    flow.blocks.push(BlockInstance {
        id: "proc".to_string(),
        block_definition_id: BLOCK_ID.to_string(),
        name: None,
        properties,
        position: Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "proc:audio_in".to_string(),
    });
    flow.links.push(Link {
        from: "proc:audio_out".to_string(),
        to: "sink:sink".to_string(),
    });
    flow
}

/// Under a named preset the preset owns the custom settings: a live write to
/// one is refused, since the preset would win again on the next restart.
/// Sending Custom in the same batch lets it through. Needs no running flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_only_setting_is_refused_under_a_named_preset() {
    init();
    let state = common::state::new();
    let flow = flow("ebu-r128", &[]);
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");

    let (_, rejected) = state
        .update_block_properties(
            &flow_id,
            "proc",
            props(&[("target_loudness", PropertyValue::Float(-14.0))]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");
    // Refused by the property's `visible_when`, which names the condition
    // and the current preset.
    let reason = rejected.get("target_loudness").expect("refused");
    assert!(reason.contains("preset is one of custom"), "{reason}");
    assert!(reason.contains("(preset is ebu-r128)"), "{reason}");

    let (_, rejected) = state
        .update_block_properties(
            &flow_id,
            "proc",
            props(&[
                ("preset", s(PRESET_CUSTOM)),
                ("target_loudness", PropertyValue::Float(-14.0)),
            ]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");
    // The flow is not running, so the write itself fails, but not because of
    // the preset.
    if let Some(reason) = rejected.get("target_loudness") {
        assert!(!reason.contains("applies only"), "{reason}");
    }
}

/// Switching a running block to Custom applies the stored custom settings,
/// which the element alone would not do: writing `custom` to it changes
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_to_custom_live_applies_the_stored_settings() {
    init();
    let state = common::state::new();
    let flow = flow(
        "streaming",
        &[("target_loudness", PropertyValue::Float(-14.0))],
    );
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");
    state.start_flow(&flow_id).await.expect("start_flow");

    let before = state
        .get_block_properties(&flow_id, "proc")
        .await
        .expect("block properties");
    assert_eq!(
        float(before.get("target_loudness")),
        Some(-16.0),
        "the streaming preset's target"
    );

    let (_, rejected) = state
        .update_block_properties(
            &flow_id,
            "proc",
            props(&[("preset", s(PRESET_CUSTOM))]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");
    assert!(rejected.is_empty(), "{rejected:?}");

    let after = state
        .get_block_properties(&flow_id, "proc")
        .await
        .expect("block properties");
    assert_eq!(float(after.get("target_loudness")), Some(-14.0));
    assert_eq!(
        float(after.get("agc_max_gain")),
        Some(DEFAULT_AGC_MAX_GAIN_DB as f64)
    );

    // The loudness reset endpoint restarts the processor's integrated
    // measurement too.
    state
        .reset_loudness(&flow_id, "proc")
        .await
        .expect("reset integrated loudness");

    state.stop_flow(&flow_id).await.expect("stop_flow");
}

/// Changing the target of a running custom block moves the values that
/// follow it (an unstored bass clip threshold, the band and expander
/// thresholds) with it, as the next start would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_custom_target_change_moves_the_target_relative_values() {
    init();
    let state = common::state::new();
    let flow = flow(PRESET_CUSTOM, &[]);
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");
    state.start_flow(&flow_id).await.expect("start_flow");

    let (_, rejected) = state
        .update_block_properties(
            &flow_id,
            "proc",
            props(&[("target_loudness", PropertyValue::Float(-16.0))]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");
    assert!(rejected.is_empty(), "{rejected:?}");

    let after = state
        .get_block_properties(&flow_id, "proc")
        .await
        .expect("block properties");
    assert_eq!(float(after.get("target_loudness")), Some(-16.0));
    assert_eq!(float(after.get("bass_clip_threshold")), Some(-7.0));

    state.stop_flow(&flow_id).await.expect("stop_flow");
}
