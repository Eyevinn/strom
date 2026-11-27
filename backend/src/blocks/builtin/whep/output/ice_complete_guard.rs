//! Keeps a second "ICE gathering complete" of a WHEP viewer away from the
//! WHEP server signaller, which aborts the process on it.
//!
//! `whepserversink`'s signaller (gst-plugin-webrtc 0.15, `whep_signaller`)
//! connects a `notify::ice-gathering-state` handler to every viewer's
//! webrtcbin. Each time that handler reads `complete`, it takes the sender
//! that carries the SDP answer to the waiting POST out of a map with
//! `.expect("SDP answer Sender needs to be valid")`, while it holds the
//! signaller's settings lock. The sender is there exactly once per session, so
//! a second `complete` on the same webrtcbin panics. The handler is a GObject
//! signal callback and cannot unwind, so the panic aborts Strom; any thread
//! that was waiting for the lock panics on `PoisonError` as well.
//!
//! webrtcbin can go back from `complete` to `gathering` and then reach
//! `complete` again. Any notify that reads `complete` after the first one does
//! the same.
//!
//! The answer has already gone out with the first `complete`, so the
//! signaller has nothing left to do on a later one. This guard is connected in
//! `consumer-added`, which webrtcsink emits before `webrtcbin-ready`, where the
//! signaller connects its handler. It therefore runs first, and on every
//! `complete` after the first it stops the emission before the signaller's
//! handler sees it.

use std::sync::atomic::{AtomicBool, Ordering};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_webrtc::WebRTCICEGatheringState;
use tracing::warn;

const NOTIFY_SIGNAL: &str = "notify::ice-gathering-state";

/// Guard every viewer of `whepserversink`.
pub(super) fn install(whepserversink: &gst::Element, instance_id: &str) {
    let instance_id = instance_id.to_string();
    whepserversink.connect("consumer-added", false, move |values| {
        let consumer_id = values
            .get(1)
            .and_then(|v| v.get::<String>().ok())
            .unwrap_or_default();
        if let Some(webrtcbin) = values.get(2).and_then(|v| v.get::<gst::Element>().ok()) {
            guard(&webrtcbin, instance_id.clone(), consumer_id);
        }
        None
    });
}

/// Let the first `complete` of `webrtcbin` through and stop every later one.
fn guard(webrtcbin: &gst::Element, instance_id: String, consumer_id: String) {
    if webrtcbin.find_property("ice-gathering-state").is_none() {
        return;
    }
    let completed = AtomicBool::new(false);
    webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
        let state = webrtcbin.property::<WebRTCICEGatheringState>("ice-gathering-state");
        if state != WebRTCICEGatheringState::Complete {
            return;
        }
        if completed.swap(true, Ordering::SeqCst) {
            warn!(
                "WHEP Output {}: ICE gathering completed again for viewer {}; \
                 not passing it on to the WHEP server signaller, which would abort",
                instance_id, consumer_id
            );
            webrtcbin.stop_signal_emission_by_name(NOTIFY_SIGNAL);
        }
    });
}
