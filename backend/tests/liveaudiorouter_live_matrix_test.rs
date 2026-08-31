//! Regression test: `builtin.liveaudiorouter` must apply a `routing_matrix`
//! change to an already-running pipeline.
//!
//! The existing `builtin.audiorouter` snapshots its matrix at `build()` time and
//! declares `routing_matrix` non-live, so rerouting a channel needs a flow
//! restart (issue #661). The live block wraps `audiomixmatrix`, whose `matrix`
//! property can be rewritten while the element is PLAYING.
//!
//! The runtime test feeds one tone into a two-channel input, routes it to output
//! channel 0, then rewrites the matrix mid-stream to send it to output channel 1
//! instead, and reads the `level` element's per-channel peaks on both sides of
//! the change. Asserting on measured peaks rather than on the property value is
//! deliberate: reading `matrix` back would pass even if the element ignored the
//! new value for the audio it is actually producing.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::MessageView;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::liveaudiorouter::{self, LiveAudioRouterBuilder};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

/// Elements this test needs beyond core GStreamer. `audiomixmatrix` is in
/// gst-plugins-bad and `level` in gst-plugins-good; both packages are in the
/// Linux job's list in `.github/workflows/ci.yml`.
const REQUIRED: &[&str] = &[
    "audiotestsrc",
    "audioconvert",
    "capsfilter",
    "audiomixmatrix",
    "level",
    "fakesink",
];

/// Peak (dBFS) above which an output channel counts as carrying the tone.
const LOUD_DB: f64 = -20.0;
/// Peak (dBFS) below which an output channel counts as silent.
const SILENT_DB: f64 = -60.0;

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    // `ElementFactory::find` panics if the registry has not been loaded yet, and
    // this runs before the pipeline is built.
    gst::init().unwrap();
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var("STROM_REQUIRE_GST_PLUGINS").is_err(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

fn properties(entries: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn an_empty_matrix_routes_every_input_channel_straight_through() {
    assert_eq!(
        liveaudiorouter::routing_matrix_to_gst_array("{}", 2, 2)
            .expect("an empty matrix is valid input"),
        "<<(gdouble)1.0, (gdouble)0.0>, <(gdouble)0.0, (gdouble)1.0>>"
    );
}

#[test]
fn a_routing_entry_lands_in_the_row_of_its_destination_channel() {
    // audiomixmatrix indexes its matrix [output][input], so routing input
    // channel 0 to output channel 1 must fill row 1 column 0, not row 0.
    assert_eq!(
        liveaudiorouter::routing_matrix_to_gst_array(r#"{"i0c0": ["o0c1"]}"#, 2, 2)
            .expect("valid JSON"),
        "<<(gdouble)0.0, (gdouble)0.0>, <(gdouble)1.0, (gdouble)0.0>>"
    );
}

#[test]
fn one_input_channel_can_fan_out_to_several_output_channels() {
    assert_eq!(
        liveaudiorouter::routing_matrix_to_gst_array(r#"{"i0c0": ["o0c0", "o0c1"]}"#, 2, 2)
            .expect("valid JSON"),
        "<<(gdouble)1.0, (gdouble)0.0>, <(gdouble)1.0, (gdouble)0.0>>"
    );
}

#[test]
fn unparseable_json_is_rejected_instead_of_reaching_gstreamer() {
    // The live path hands this string to set_property_from_str, which panics on
    // a value it cannot deserialize. Rejecting it here is what keeps a typo in
    // the properties panel from taking the backend down.
    let rejected = liveaudiorouter::routing_matrix_to_gst_array("{not json", 2, 2);
    assert!(rejected.is_none());
}

struct RouterPipe {
    pipeline: gst::Pipeline,
    matrix: gst::Element,
    matrix_id: String,
    bus: gst::Bus,
    level_name: String,
}

impl RouterPipe {
    /// `audiotestsrc` (mono tone) -> `audioconvert` -> two-channel caps -> the
    /// block under test -> `level` -> `fakesink`.
    ///
    /// The mono source is upmixed to two identical input channels, so which
    /// *output* channel carries the tone is decided purely by the matrix.
    fn new(routing_matrix: &str) -> Self {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();

        let props = properties(&[
            ("in_channels", PropertyValue::UInt(2)),
            ("out_channels", PropertyValue::UInt(2)),
            (
                "routing_matrix",
                PropertyValue::String(routing_matrix.to_string()),
            ),
        ]);
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let built = LiveAudioRouterBuilder
            .build("router", &props, &ctx)
            .expect("the block builds");

        let by_id: HashMap<String, gst::Element> = built.elements.iter().cloned().collect();
        for (_, elem) in &built.elements {
            pipeline.add(elem).expect("add block element");
        }
        for (from, to) in &built.internal_links {
            let src = by_id
                .get(&from.element_id)
                .expect("internal link source element exists");
            let sink = by_id
                .get(&to.element_id)
                .expect("internal link sink element exists");
            src.link(sink).expect("internal link");
        }

        let block_in = by_id
            .get("router:live_router_in")
            .expect("block exposes an input element")
            .clone();
        let block_out = by_id
            .get("router:live_router_out")
            .expect("block exposes an output element")
            .clone();
        let matrix_id = "router:live_router_matrix".to_string();
        let matrix = by_id
            .get(&matrix_id)
            .expect("block exposes a matrix element")
            .clone();

        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property_from_str("wave", "sine")
            .property("freq", 1000.0_f64)
            // Short buffers so `level` reports often and the assertion window
            // after the matrix change stays small.
            .property("samplesperbuffer", 48_i32)
            .build()
            .expect("audiotestsrc");
        let convert = gst::ElementFactory::make("audioconvert")
            .build()
            .expect("audioconvert");
        let caps = gst::Caps::builder("audio/x-raw")
            .field("channels", 2i32)
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &caps)
            .build()
            .expect("capsfilter");
        let level_name = "router_level";
        let level = gst::ElementFactory::make("level")
            .name(level_name)
            .property("post-messages", true)
            .property("interval", gst::ClockTime::from_mseconds(10).nseconds())
            .build()
            .expect("level");
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink");

        pipeline
            .add_many([&src, &convert, &capsfilter, &level, &sink])
            .expect("add harness elements");
        gst::Element::link_many([&src, &convert, &capsfilter, &block_in])
            .expect("link the source chain into the block");
        gst::Element::link_many([&block_out, &level, &sink])
            .expect("link the block into the measuring chain");

        let bus = pipeline.bus().expect("pipeline has a bus");
        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline reaches PLAYING");

        RouterPipe {
            pipeline,
            matrix,
            matrix_id,
            bus,
            level_name: level_name.to_string(),
        }
    }

    /// Apply a new matrix the way a live property update from the API does.
    fn set_routing_matrix(&self, json: &str) {
        let handled = liveaudiorouter::try_apply_live_matrix(
            &self.matrix,
            &self.matrix_id,
            "routing_matrix",
            &PropertyValue::String(json.to_string()),
        );
        assert!(
            handled,
            "the live-update interceptor must claim routing_matrix for this element"
        );
    }

    /// Per-channel peaks from the next `level` message posted by our own level
    /// element.
    fn next_peaks(&self, timeout: Duration) -> Vec<f64> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Some(msg) = self.bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(100),
                &[gst::MessageType::Element],
            ) else {
                continue;
            };
            let MessageView::Element(element_msg) = msg.view() else {
                continue;
            };
            let Some(s) = element_msg.structure() else {
                continue;
            };
            if s.name() != "level" {
                continue;
            }
            let from_our_level = msg
                .src()
                .map(|src| src.name().to_string())
                .is_some_and(|name| name == self.level_name);
            if !from_our_level {
                continue;
            }
            if let Ok(array) = s.get::<glib::ValueArray>("peak") {
                let peaks: Vec<f64> = array.iter().filter_map(|v| v.get::<f64>().ok()).collect();
                if !peaks.is_empty() {
                    return peaks;
                }
            }
        }
        panic!("no level message arrived within {:?}", timeout);
    }

    /// Poll `level` until `expected_loud` is the only output channel carrying
    /// the tone. Polling rather than reading a single message tolerates the
    /// buffers already in flight when the matrix changed.
    fn wait_for_only_channel(&self, expected_loud: usize, timeout: Duration) -> Vec<f64> {
        let deadline = Instant::now() + timeout;
        let mut last: Vec<f64> = Vec::new();
        while Instant::now() < deadline {
            let peaks = self.next_peaks(Duration::from_secs(2));
            if peaks.len() == 2 {
                let other = 1 - expected_loud;
                if peaks[expected_loud] > LOUD_DB && peaks[other] < SILENT_DB {
                    return peaks;
                }
            }
            last = peaks;
        }
        panic!(
            "output channel {} never became the only channel carrying audio \
             (loud > {} dB, other < {} dB); last peaks seen: {:?}",
            expected_loud, LOUD_DB, SILENT_DB, last
        );
    }
}

impl Drop for RouterPipe {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// The guard for issue #661: rerouting must take effect without a flow restart.
///
/// Reverting the block to the old snapshot-at-build behaviour makes the second
/// `wait_for_only_channel` time out, because the tone stays on channel 0.
#[test]
fn a_routing_matrix_change_moves_audio_between_output_channels_while_playing() {
    if !plugins_available() {
        return;
    }

    // Start with the tone on output channel 0 only.
    let pipe = RouterPipe::new(r#"{"i0c0": ["o0c0"]}"#);
    let before = pipe.wait_for_only_channel(0, Duration::from_secs(10));

    // Reroute to output channel 1 while the pipeline keeps playing.
    pipe.set_routing_matrix(r#"{"i0c0": ["o0c1"]}"#);

    let after = pipe.wait_for_only_channel(1, Duration::from_secs(10));

    assert!(
        before[0] > after[0],
        "output channel 0 should have lost the tone: before={:?} after={:?}",
        before,
        after
    );
    assert!(
        after[1] > before[1],
        "output channel 1 should have gained the tone: before={:?} after={:?}",
        before,
        after
    );
}
