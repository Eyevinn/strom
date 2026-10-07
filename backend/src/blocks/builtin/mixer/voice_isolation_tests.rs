use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::PropertyValue;

use super::tests::{assemble, feed, small_mixer_props, Assembled};

/// The element `id`'s src pad feeds, by element name without the block prefix.
fn feeds(m: &Assembled, id: &str) -> String {
    let next = m
        .element(id)
        .static_pad("src")
        .and_then(|p| p.peer())
        .and_then(|p| p.parent_element())
        .unwrap_or_else(|| panic!("{id} feeds nothing"));
    next.name().trim_start_matches("mx:").to_string()
}

#[test]
fn test_voice_isolation_sits_between_hpf_and_gate() {
    let m = assemble(&small_mixer_props(&[
        ("ch1_voice_isolation", PropertyValue::Bool(true)),
        ("ch1_voice_isolation_limit", PropertyValue::Float(30.0)),
    ]));

    let on = m.element("voiceiso_0");
    assert!(on.property::<bool>("enabled"));
    assert_eq!(on.property::<f64>("attenuation-limit"), 30.0);
    // Present on every channel so it can be switched on live.
    assert!(!m.element("voiceiso_1").property::<bool>("enabled"));

    for ch in 0..2 {
        assert_eq!(feeds(&m, &format!("hpf_{ch}")), format!("voiceiso_{ch}"));
        assert_eq!(feeds(&m, &format!("voiceiso_{ch}")), format!("gate_{ch}"));
    }
    assert!(
        !m.has_element("voiceiso_0_resample_in"),
        "no conversion at 48 kHz"
    );
}

/// The model runs only at 48 kHz. A mixer at 44.1 kHz converts around it;
/// without that the strip would refuse to link.
#[test]
fn test_voice_isolation_converts_around_a_non_48k_mixer() {
    let mut properties = small_mixer_props(&[("ch1_voice_isolation", PropertyValue::Bool(true))]);
    properties.insert(
        "sample_rate".to_string(),
        PropertyValue::String("44100".to_string()),
    );
    let m = assemble(&properties);
    assert!(m.has_element("voiceiso_0_resample_in"));
    assert!(m.has_element("voiceiso_0_resample_out"));

    feed(&m, 0, true);
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    m.pipeline.add(&sink).unwrap();
    m.element("main_out_tee")
        .link_pads(Some("src_%u"), &sink, None)
        .unwrap();
    let buffers = Arc::new(AtomicUsize::new(0));
    let counter = buffers.clone();
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

    m.pipeline.set_state(gst::State::Playing).unwrap();
    let bus = m.pipeline.bus().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while buffers.load(Ordering::Relaxed) < 20 {
        assert!(Instant::now() < deadline, "no audio reached main_out");
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) {
            panic!("pipeline error: {msg:?}");
        }
    }
    let rate = m
        .element("voiceiso_0")
        .static_pad("sink")
        .and_then(|p| p.current_caps())
        .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("rate").ok()));
    assert_eq!(rate, Some(48_000));
}
