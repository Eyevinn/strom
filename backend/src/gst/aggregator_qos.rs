//! Late-data QoS reporting on audio aggregator inputs.
//!
//! `GstAudioAggregator` (`audiomixer`, `liveadder`) drops input data that
//! arrives after its deadline, and posts a QoS message for it only when the
//! input pad's `qos-messages` property is set. It defaults to false, so a mixer
//! input can be dropped for a whole connection without a trace (#1265).

use gstreamer as gst;
use gstreamer::prelude::*;

/// Make every sink pad of `aggregator`, present and future, post a QoS message
/// when it drops late data. Call it on the element before its pads are
/// requested; pads it already has are covered too.
///
/// The `qos-messages` pad property exists from GStreamer 1.20, below our 1.22
/// floor. The check keeps an aggregator without it (another element class)
/// working, without the reporting.
pub(crate) fn report_late_input_drops(aggregator: &gst::Element) {
    for pad in aggregator.sink_pads() {
        enable_qos_messages(&pad);
    }
    // The handler captures nothing: the pad is an argument, so no reference
    // cycle with the element that owns the handler.
    aggregator.connect_pad_added(|_, pad| {
        if pad.direction() == gst::PadDirection::Sink {
            enable_qos_messages(pad);
        }
    });
}

fn enable_qos_messages(pad: &gst::Pad) {
    if pad.find_property("qos-messages").is_some() {
        pad.set_property("qos-messages", true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_audiomixer_pads_post_qos_messages() {
        gst::init().unwrap();
        let mixer = gst::ElementFactory::make("audiomixer").build().unwrap();
        let before = mixer.request_pad_simple("sink_%u").unwrap();
        report_late_input_drops(&mixer);
        let after = mixer.request_pad_simple("sink_%u").unwrap();
        assert!(before.property::<bool>("qos-messages"));
        assert!(after.property::<bool>("qos-messages"));
    }
}
