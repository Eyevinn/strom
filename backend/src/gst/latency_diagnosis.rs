//! Names the inputs behind an aggregator's failed latency configuration.
//!
//! An aggregator has to wait for its slowest input, so it needs the largest
//! minimum latency among its inputs. Every input has to buffer that long, so
//! it can wait no longer than the smallest maximum. When the first is larger
//! than the second, GStreamer posts "Impossible to configure latency: max X <
//! min Y" and names only the aggregator. This finds the input on each side, so
//! the operator knows which blocks to look at.

use gstreamer as gst;
use gstreamer::prelude::*;

/// What one live aggregator input reports, and where its data comes from.
#[derive(Debug, Clone)]
pub struct InputLatency {
    /// The element at the upstream end of the input, see [`origin`].
    pub origin: String,
    pub min: gst::ClockTime,
    /// `None` when the input can buffer without limit.
    pub max: Option<gst::ClockTime>,
}

/// Upstream elements walked before giving up on finding an input's origin.
const MAX_STEPS: usize = 256;

/// The latency each live input of `aggregator` reports. Non-live inputs are
/// left out, as the aggregator leaves them out of its own configuration.
pub fn input_latencies(aggregator: &gst::Element) -> Vec<InputLatency> {
    aggregator
        .sink_pads()
        .iter()
        .filter_map(|pad| {
            let mut query = gst::query::Latency::new();
            if !pad.peer_query(&mut query) {
                return None;
            }
            let (live, min, max) = query.result();
            live.then(|| InputLatency {
                origin: origin(pad),
                min,
                max,
            })
        })
        .collect()
}

/// The element an aggregator input's data comes from: walk upstream while
/// each element has exactly one linked input, and stop at a source or at an
/// element that combines several inputs (another mixer bus).
fn origin(sink_pad: &gst::Pad) -> String {
    let mut pad = sink_pad.clone();
    let mut last = None;
    for _ in 0..MAX_STEPS {
        let Some(element) = pad.peer().and_then(|peer| peer.parent_element()) else {
            break;
        };
        let mut linked = element.sink_pads().into_iter().filter(|p| p.is_linked());
        let next = linked.next();
        let several = linked.next().is_some();
        last = Some(element);
        match next {
            Some(next) if !several => pad = next,
            _ => break,
        }
    }
    last.map(|e| e.name().to_string())
        .unwrap_or_else(|| sink_pad.name().to_string())
}

/// Which input sets the aggregator's minimum and which caps its maximum, when
/// the minimum is the larger. `label` turns an origin element name into what
/// the operator knows it by.
pub fn describe(inputs: &[InputLatency], label: impl Fn(&str) -> String) -> Option<String> {
    let needs = inputs.iter().max_by_key(|i| i.min)?;
    let (holds, max) = inputs
        .iter()
        .filter_map(|i| i.max.map(|max| (i, max)))
        .min_by_key(|(_, max)| *max)?;
    if needs.min <= max {
        return None;
    }
    Some(format!(
        "{} arrives {} ms late, but {} can only be held back {} ms",
        label(&needs.origin),
        needs.min.mseconds(),
        label(&holds.origin),
        max.mseconds()
    ))
}

/// For an aggregator's "Impossible to configure latency" warning, which inputs
/// cause it. `None` for any other warning, or when the inputs no longer
/// disagree by the time they are asked.
pub fn diagnose_warning(
    source: Option<&gst::Object>,
    debug: Option<&str>,
    label: impl Fn(&str) -> String,
) -> Option<String> {
    if !debug?.contains("Impossible to configure latency") {
        return None;
    }
    let aggregator = source?.downcast_ref::<gstreamer_base::Aggregator>()?;
    describe(&input_latencies(aggregator.upcast_ref()), label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(origin: &str, min_ms: u64, max_ms: Option<u64>) -> InputLatency {
        InputLatency {
            origin: origin.to_string(),
            min: gst::ClockTime::from_mseconds(min_ms),
            max: max_ms.map(gst::ClockTime::from_mseconds),
        }
    }

    #[test]
    fn names_the_slowest_input_and_the_one_that_cannot_wait_for_it() {
        let inputs = [
            input("srt", 2000, None),
            input("whip", 0, Some(1000)),
            input("clip", 0, Some(1500)),
            input("other", 100, Some(4000)),
        ];
        assert_eq!(
            describe(&inputs, |o| format!("'{o}'")).as_deref(),
            Some("'srt' arrives 2000 ms late, but 'whip' can only be held back 1000 ms")
        );
    }

    #[test]
    fn nothing_to_say_when_every_input_can_wait() {
        let inputs = [input("srt", 2000, None), input("whip", 0, Some(10_000))];
        assert_eq!(describe(&inputs, str::to_string), None);
        assert_eq!(describe(&[input("a", 2000, None)], str::to_string), None);
        assert_eq!(describe(&[], str::to_string), None);
    }
}
