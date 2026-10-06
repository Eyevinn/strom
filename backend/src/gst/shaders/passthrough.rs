//! Passthrough for FX slots that hold no effect.
//!
//! A `glshader` slot holding the identity fragment still renders every frame:
//! a full-frame copy into a pool buffer, an FBO render and a GL sync. Neither
//! `glshader` nor its GL base classes ever enter `GstBaseTransform`
//! passthrough on their own, but both honour it (`GstGLFilter` forwards caps
//! unchanged, `GstGLBaseFilter` forwards the ALLOCATION query, and the base
//! transform pushes the input buffer). So an idle slot is put in passthrough,
//! and taken out of it before an effect is programmed.
//!
//! The switch must not race the streaming thread: `GstBaseTransform` reads
//! its passthrough flag once to pick the output buffer and again to decide
//! whether to render into it, so flipping it from another thread mid-buffer
//! can push an unrendered pool buffer, or render a frame into its own input.
//! The flip therefore runs in a probe on the sink pad, which fires under the
//! pad's stream lock before the next buffer reaches the element, and that
//! buffer is rendered with the newly programmed shader.
//!
//! Entering passthrough keeps the slot's GL output pool: it does not
//! renegotiate, so the pool set up when the slot first negotiated stays
//! allocated, and leaving passthrough renders into it straight away. Leaving
//! must not renegotiate: that sends an ALLOCATION query downstream, and a
//! serialized query into the mixer can block the branch's streaming thread
//! for as long as the mixer takes to drain its pad. On a slow GL host that
//! outlasted a whole wipe: the mixer kept compositing the last unmasked frame
//! with the pad already made visible, and the masked frames arrived too late
//! to be shown — the wipe became a cut. Only a slot whose pool is gone (its
//! caps changed while in passthrough) renegotiates on the way out.
//!
//! Each slot keeps its requested state in one atomic word (a generation
//! counter plus the wanted flag) and has at most one flip probe pending, so
//! requests on a slot that receives no buffers do not pile up, and the last
//! request wins by construction.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::debug;

/// Requested state of one slot.
#[derive(Default)]
struct SlotState {
    /// `generation << 1 | want_passthrough`. Every request bumps the
    /// generation, so a deferred request can tell whether it is still the
    /// latest one.
    word: AtomicU64,
    /// A flip probe is installed and has not fired yet.
    pending: AtomicBool,
    /// Flip probes installed over the slot's lifetime.
    probes_added: AtomicU64,
}

impl SlotState {
    fn want(&self) -> bool {
        self.word.load(Ordering::SeqCst) & 1 == 1
    }
}

/// Side table: element → its slot state. Touched only from API/build
/// threads, never from a probe (probes hold their own `Arc`).
static SLOTS: Mutex<Vec<(glib::WeakRef<gst::Element>, Arc<SlotState>)>> = Mutex::new(Vec::new());

fn slot_state(elem: &gst::Element) -> Arc<SlotState> {
    let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    slots.retain(|(weak, _)| weak.upgrade().is_some());
    if let Some((_, state)) = slots
        .iter()
        .find(|(weak, _)| weak.upgrade().as_ref() == Some(elem))
    {
        return state.clone();
    }
    let state = Arc::new(SlotState::default());
    slots.push((elem.downgrade(), state.clone()));
    state
}

/// Put the slot in or out of passthrough. Must run on the streaming thread,
/// under the sink pad's stream lock (i.e. inside a sink-pad probe).
///
/// Passthrough is only entered while both pads carry the same caps: in
/// passthrough the input buffer goes out as is, so a slot negotiated to
/// output a different size or format than it receives keeps rendering.
fn apply(transform: &gst_base::BaseTransform, passthrough: bool) {
    if transform.is_passthrough() == passthrough {
        return;
    }
    if passthrough {
        let caps_of = |name: &str| {
            transform
                .static_pad(name)
                .and_then(|pad| pad.current_caps())
        };
        let (sink, src) = (caps_of("sink"), caps_of("src"));
        if sink.is_none() || sink != src {
            debug!(
                "{}: FX slot stays rendering, input and output caps differ",
                transform.name()
            );
            return;
        }
    }
    transform.set_passthrough(passthrough);
    // No renegotiation on entering: it would release the output pool, and
    // leaving would then have to query a new one on the take's critical path.
    if !passthrough && transform.buffer_pool().is_none() {
        transform.reconfigure_src();
    }
    debug!("{}: FX slot passthrough={}", transform.name(), passthrough);
}

/// A request made by [`request_passthrough`]; lets a later step act only if
/// no newer request has been made on the slot since.
#[derive(Clone)]
pub struct PassthroughTicket {
    state: Arc<SlotState>,
    word: u64,
}

impl PassthroughTicket {
    /// Whether this is still the latest request on the slot. Lock-free.
    pub fn is_current(&self) -> bool {
        self.state.word.load(Ordering::SeqCst) == self.word
    }

    /// Put the slot in passthrough if no newer request was made since this
    /// one, e.g. once a transition shader has run to its identity end state.
    /// Must be called from a probe on the slot's sink pad.
    pub fn enter_passthrough_if_current(&self, elem: &gst::Element) -> bool {
        let next = ((self.word >> 1) + 1) << 1 | 1;
        if self
            .state
            .word
            .compare_exchange(self.word, next, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        if let Some(transform) = elem.downcast_ref::<gst_base::BaseTransform>() {
            apply(transform, true);
        }
        true
    }
}

/// Put a `glshader` FX slot in or out of passthrough, from its next buffer.
///
/// `passthrough` must be true only while the slot's program is an identity
/// pass (the identity fragment, or a transition shader parked neutral).
/// The last request before the next buffer wins; at most one probe is
/// pending per slot however many requests arrive while no buffers flow.
pub fn request_passthrough(elem: &gst::Element, passthrough: bool) -> PassthroughTicket {
    let state = slot_state(elem);
    let word = state
        .word
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |w| {
            Some(((w >> 1) + 1) << 1 | passthrough as u64)
        })
        .map(|prev| ((prev >> 1) + 1) << 1 | passthrough as u64)
        .unwrap_or_default();
    let ticket = PassthroughTicket {
        state: state.clone(),
        word,
    };

    if state.pending.swap(true, Ordering::SeqCst) {
        // A probe is already waiting; it reads the state when it fires.
        return ticket;
    }
    let (Some(pad), Some(transform)) = (
        elem.static_pad("sink"),
        elem.downcast_ref::<gst_base::BaseTransform>(),
    ) else {
        state.pending.store(false, Ordering::SeqCst);
        return ticket;
    };
    state.probes_added.fetch_add(1, Ordering::SeqCst);
    // Pads own their probes: a weak ref, never a strong one (leak rule).
    let weak = transform.downgrade();
    // One-shot: fires on the next buffer and removes itself. Never per buffer.
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, _info| {
            // Clear `pending` before reading the wanted state: a request
            // that still sees `pending` set wrote its state before this
            // read, and one that sees it cleared installs its own probe.
            state.pending.store(false, Ordering::SeqCst);
            if let Some(transform) = weak.upgrade() {
                apply(&transform, state.want());
            }
            gst::PadProbeReturn::Remove
        },
    );
    ticket
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_stream(elem: &gst::Element) {
        let sink = elem.static_pad("sink").unwrap();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", 16i32)
            .field("height", 16i32)
            .build();
        assert!(sink.send_event(gst::event::StreamStart::new("fx-passthrough-test")));
        assert!(sink.send_event(gst::event::Caps::new(&caps)));
        assert!(
            sink.send_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new()))
        );
    }

    fn feed_one_buffer(elem: &gst::Element) {
        let sink = elem.static_pad("sink").unwrap();
        // The src pad is unlinked; the probe runs before the chain function.
        let _ = sink.chain(gst::Buffer::with_size(16 * 16 * 4).unwrap());
    }

    fn transform() -> gst::Element {
        gst::init().unwrap();
        // Any BaseTransform with an in-place transform can leave passthrough.
        let elem = gst::ElementFactory::make("identity").build().unwrap();
        elem.set_state(gst::State::Playing).unwrap();
        elem
    }

    #[test]
    fn requests_on_an_unfed_slot_keep_one_probe_and_the_last_one_wins() {
        let elem = transform();
        let bt = elem.downcast_ref::<gst_base::BaseTransform>().unwrap();
        bt.set_passthrough(true);

        for i in 0..1000 {
            request_passthrough(&elem, i % 2 == 1);
        }
        // Last request: out of passthrough.
        request_passthrough(&elem, false);
        start_stream(&elem);
        let state = slot_state(&elem);
        assert_eq!(
            state.probes_added.load(Ordering::SeqCst),
            1,
            "requests on a slot without buffers must not pile up probes"
        );
        assert!(bt.is_passthrough(), "nothing may flip before a buffer");

        feed_one_buffer(&elem);
        assert!(!bt.is_passthrough(), "the last request was not applied");
        assert!(!state.pending.load(Ordering::SeqCst));

        // A later request installs a fresh probe.
        request_passthrough(&elem, true);
        assert_eq!(state.probes_added.load(Ordering::SeqCst), 2);
        let _ = elem.set_state(gst::State::Null);
    }

    #[test]
    fn passthrough_is_not_entered_while_output_caps_differ() {
        let elem = transform();
        let bt = elem.downcast_ref::<gst_base::BaseTransform>().unwrap();
        bt.set_passthrough(false);
        // Output negotiated to another size than the input, as a GL filter
        // that scales would be.
        let src_caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", 32i32)
            .field("height", 32i32)
            .build();
        start_stream(&elem);
        let src = elem.static_pad("src").unwrap();
        let _ = src.push_event(gst::event::Caps::new(&src_caps));
        assert_eq!(src.current_caps().as_ref(), Some(&src_caps));

        request_passthrough(&elem, true);
        feed_one_buffer(&elem);
        let sink_caps = elem.static_pad("sink").unwrap().current_caps();
        if sink_caps.as_ref() != src.current_caps().as_ref() {
            assert!(
                !bt.is_passthrough(),
                "entered passthrough with output caps that differ from the input"
            );
        } else {
            panic!("test setup: the caps event overwrote the src caps");
        }
        let _ = elem.set_state(gst::State::Null);
    }

    #[test]
    fn a_stale_ticket_does_not_enter_passthrough() {
        let elem = transform();
        let old = request_passthrough(&elem, false);
        assert!(old.is_current());
        let _newer = request_passthrough(&elem, false);
        assert!(!old.is_current());
        assert!(!old.enter_passthrough_if_current(&elem));
        let state = slot_state(&elem);
        assert!(!state.want(), "a stale ticket overwrote the newer request");
        let _ = elem.set_state(gst::State::Null);
    }
}
