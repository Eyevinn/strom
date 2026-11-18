//! Let a live video aggregator catch up with the clock when it falls behind.
//!
//! A live `compositor` or `glvideomixerelement` blends every output frame in
//! turn. When the machine cannot blend at the output frame rate, its output
//! falls behind the pipeline clock, and nothing brings it back: the
//! aggregator only skips frames on QoS events from downstream, and a mixer
//! whose output leaves through an `appsink` (`intersink`, thumbnail taps)
//! never gets one. The lag then grows for as long as the load lasts, and
//! every live input, stamped with the clock, sits that far in the
//! aggregator's future: the output shows old or frozen pictures.
//!
//! [`install`] measures the lag on the aggregator's src pad and, once an
//! output buffer is more than a frame late, sends the aggregator the QoS
//! event a synchronizing sink would send. `GstVideoAggregator` then skips
//! the output frames that are already late (it neither converts nor blends
//! them) until it is back on time. The event stops at the aggregator: it
//! does not travel on to the inputs (see [`stop_upstream`]).
//!
//! Each skipped frame shows up as a QoS message of the aggregator on the
//! bus, as for any element that drops late buffers.
//!
//! Only a live aggregator is affected: without a latency (not live) the
//! probe sends nothing, so offline processing keeps every frame.
//!
//! The probe checks every [`CHECK_EVERY`]th buffer only; the others cost an
//! atomic increment, no lock and no allocation.

use gstreamer as gst;
use gstreamer::glib::translate::IntoGlib;
use gstreamer::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The lag is measured on every this-many output buffers (6 times a second
/// at 30 fps). Between two checks an overloaded aggregator falls behind by
/// a few frames at most.
const CHECK_EVERY: u32 = 5;

/// How late a buffer may be when its duration is unknown.
const DEFAULT_TOLERANCE: gst::ClockTime = gst::ClockTime::from_mseconds(40);

/// Make the live video aggregator `element` skip late output frames, so its
/// output stays on the clock under CPU overload. Does nothing for an element
/// that is not an aggregator.
pub(crate) fn install(element: &gst::Element) {
    if !element.is::<gst_base::Aggregator>() {
        return;
    }
    let Some(src) = element.static_pad("src") else {
        return;
    };
    // Seqnum of the last QoS event sent, to keep it from going upstream.
    let sent = Arc::new(AtomicU32::new(0));
    for pad in element.sink_pads() {
        stop_upstream(&pad, &sent);
    }
    {
        let sent = sent.clone();
        element.connect_pad_added(move |_, pad| {
            if pad.direction() == gst::PadDirection::Sink {
                stop_upstream(pad, &sent);
            }
        });
    }
    let count = AtomicU32::new(0);
    src.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if count
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(CHECK_EVERY)
        {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                if let Some(qos) = late_by(pad, buffer) {
                    sent.store(qos.seqnum().into_glib(), Ordering::Relaxed);
                    pad.send_event(qos);
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// Keep the aggregator's own QoS events (by seqnum) from leaving through
/// sink `pad`. The aggregator hands a QoS event on to all its inputs; there
/// a transform with QoS on would drop every input frame older than the
/// mixer's output, for good when an input path lags the mixer (a restarted
/// source under the same load): that input then freezes. Only the
/// aggregator itself is meant to skip.
///
/// The aggregator sends the event straight to each sink pad's peer, past
/// the sink pad's own probes, so the probe sits on the peer, on every peer
/// the pad is linked to, and leaves it again on unlink. Upstream events
/// are rare: it costs nothing per buffer.
fn stop_upstream(pad: &gst::Pad, sent: &Arc<AtomicU32>) {
    // The peer carrying the probe, and the probe; locked on (un)link only.
    let probe: Arc<Mutex<Option<(gst::Pad, gst::PadProbeId)>>> = Arc::default();
    {
        let (sent, probe) = (sent.clone(), probe.clone());
        pad.connect_linked(move |_, peer| guard(peer, &sent, &probe));
    }
    {
        let probe = probe.clone();
        pad.connect_unlinked(move |_, _| {
            if let Some((peer, id)) = probe.lock().unwrap().take() {
                peer.remove_probe(id);
            }
        });
    }
    // Connected first, so a link in between is not missed; `guard` keeps a
    // second call for the same peer from adding a second probe.
    if let Some(peer) = pad.peer() {
        guard(&peer, sent, &probe);
    }
}

fn guard(
    peer: &gst::Pad,
    sent: &Arc<AtomicU32>,
    probe: &Mutex<Option<(gst::Pad, gst::PadProbeId)>>,
) {
    let mut probe = probe.lock().unwrap();
    if probe.as_ref().is_some_and(|(guarded, _)| guarded == peer) {
        return;
    }
    if let Some((old, id)) = probe.take() {
        old.remove_probe(id);
    }
    let sent = sent.clone();
    // `Handled`, not `Drop`: gstreamer-rs has already taken the event out
    // of the probe info, and before GStreamer 1.26 `Drop` makes the pad
    // unref the now NULL event (a CRITICAL per dropped event).
    *probe = peer
        .add_probe(
            gst::PadProbeType::EVENT_UPSTREAM,
            move |_, info| match &info.data {
                Some(gst::PadProbeData::Event(event))
                    if event.type_() == gst::EventType::Qos
                        && event.seqnum().into_glib() == sent.load(Ordering::Relaxed) =>
                {
                    gst::PadProbeReturn::Handled
                }
                _ => gst::PadProbeReturn::Ok,
            },
        )
        .map(|id| (peer.clone(), id));
}

/// The QoS event for `buffer` leaving `pad`, if it is more than its own
/// duration behind the clock.
fn late_by(pad: &gst::Pad, buffer: &gst::BufferRef) -> Option<gst::Event> {
    let pts = buffer.pts()?;
    let segment = pad.sticky_event::<gst::event::Segment>(0)?;
    let running_time = segment
        .segment()
        .downcast_ref::<gst::ClockTime>()?
        .to_running_time(pts)?;
    let element = pad.parent_element()?;
    // A live aggregator puts out a frame when the clock reaches its running
    // time plus its latency; later than that, and it is behind. Not live
    // (no latency): never behind.
    let latency = element.downcast_ref::<gst_base::Aggregator>()?.latency()?;
    let now = element.current_running_time()?;
    let due = running_time + latency;
    let tolerance = buffer.duration().unwrap_or(DEFAULT_TOLERANCE);
    if now <= due + tolerance {
        return None;
    }
    let diff = (now - due).nseconds() as i64;
    Some(gst::event::Qos::new(
        gst::QOSType::Underflow,
        1.0,
        diff,
        running_time,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    /// Run a live compositor whose every output frame takes 100 ms (at 30
    /// fps it has 33 ms) for 3 s, and return its largest lag behind the
    /// clock in the last second, and how many QoS events reached its input.
    fn lag_of_slow_mixer(catch_up: bool) -> (Duration, u32) {
        gst::init().unwrap();
        let pipeline = gst::parse::launch(
            "videotestsrc is-live=true pattern=ball \
             ! video/x-raw,width=320,height=180,framerate=30/1 \
             ! identity name=input \
             ! compositor name=mix force-live=true start-time-selection=zero \
             ! fakesink name=out sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let mix = pipeline.by_name("mix").unwrap();
        if catch_up {
            install(&mix);
        }
        // The slow blending: after the catch-up probe, before the output.
        mix.static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, |_, _| {
                std::thread::sleep(Duration::from_millis(100));
                gst::PadProbeReturn::Ok
            });
        let worst = Arc::new(AtomicU64::new(0));
        let measuring = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let worst = worst.clone();
            let measuring = measuring.clone();
            let out = pipeline.by_name("out").unwrap();
            let weak = out.downgrade();
            out.static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    let (Some(gst::PadProbeData::Buffer(buffer)), Some(out)) =
                        (&info.data, weak.upgrade())
                    else {
                        return gst::PadProbeReturn::Ok;
                    };
                    if let (true, Some(pts), Some(now)) = (
                        measuring.load(Ordering::Relaxed),
                        buffer.pts(),
                        out.current_running_time(),
                    ) {
                        // Segment from 0 (start-time-selection=zero): the
                        // PTS is the running time.
                        let lag = now.saturating_sub(pts).nseconds();
                        worst.fetch_max(lag, Ordering::Relaxed);
                    }
                    gst::PadProbeReturn::Ok
                });
        }
        let upstream_qos = Arc::new(AtomicU32::new(0));
        {
            let upstream_qos = upstream_qos.clone();
            pipeline
                .by_name("input")
                .unwrap()
                .static_pad("src")
                .unwrap()
                .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                    if let Some(gst::PadProbeData::Event(event)) = &info.data {
                        if event.type_() == gst::EventType::Qos {
                            upstream_qos.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
        }
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        measuring.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(1));
        pipeline.set_state(gst::State::Null).unwrap();
        (
            Duration::from_nanos(worst.load(Ordering::Relaxed)),
            upstream_qos.load(Ordering::Relaxed),
        )
    }

    /// Without the catch-up the lag grows by about 0.7 s each second (about
    /// 2 s at the end); with it, it stays near half a second (one check
    /// interval of slow frames), and no QoS event reaches the input.
    #[test]
    fn a_slow_mixer_stays_on_the_clock() {
        let (without, _) = lag_of_slow_mixer(false);
        let (with, upstream_qos) = lag_of_slow_mixer(true);
        assert!(
            without > Duration::from_millis(1500),
            "the slow mixer should fall behind without catch-up: {without:?}"
        );
        assert!(
            with < Duration::from_millis(1000),
            "the mixer stayed {with:?} behind the clock"
        );
        // Only the mixer skips: its inputs keep every frame.
        assert_eq!(upstream_qos, 0, "QoS events reached the mixer's input");
    }
}
