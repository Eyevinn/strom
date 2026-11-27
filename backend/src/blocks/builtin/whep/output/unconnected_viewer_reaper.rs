//! Ends a WHEP viewer's session when its connection never comes up.
//!
//! `whepserversink` (webrtcsink, gst-plugin-webrtc 0.15) keeps a viewer's
//! session, with its own webrtcbin, ICE agent, threads and sockets, until the
//! viewer sends DELETE or the peer connection reaches `failed`. A viewer that
//! gets its answer and then goes away before connecting (a browser that closes
//! the peer connection without DELETE, a page reload, a client that retries
//! on a new session) does neither: the server answered without any of the
//! viewer's candidates, so its ICE agent has no pair to check and never fails.
//! The session stays for the life of the pipeline, holding about 20 file
//! descriptors (measured on macOS; more on a host with many interfaces).
//!
//! Enough of them exhaust the process's file descriptor limit. The next
//! webrtcbin's thread then cannot create its GMainContext, and GLib ends the
//! process with `g_error` ("Creating pipes for GWakeup"), seen in production
//! as `trap int3` in `libglib` on a thread named `webrtcbin-<session id>`.
//!
//! This ends each session whose peer connection has not reached `connected`
//! within [`CONNECT_TIMEOUT`] the same way DELETE does: by emitting
//! `session-ended` on the signaller.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_webrtc::WebRTCPeerConnectionState;
use tracing::{info, warn};

/// How long a viewer has to bring its peer connection up. Browsers give up on
/// ICE after about 30 s themselves.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// webrtcsink names each viewer's webrtcbin `webrtcbin-<session id>`.
const WEBRTCBIN_PREFIX: &str = "webrtcbin-";

/// End every viewer session of `whepserversink` that does not connect within
/// `timeout`.
pub(super) fn install(whepserversink: &gst::Element, instance_id: &str, timeout: Duration) {
    if whepserversink.find_property("signaller").is_none() {
        return;
    }
    let sink = whepserversink.downgrade();
    let instance_id = instance_id.to_string();
    whepserversink.connect("consumer-added", false, move |values| {
        if let Some(webrtcbin) = values.get(2).and_then(|v| v.get::<gst::Element>().ok()) {
            watch(&webrtcbin, sink.clone(), instance_id.clone(), timeout);
        }
        None
    });
}

fn watch(
    webrtcbin: &gst::Element,
    sink: glib::WeakRef<gst::Element>,
    instance_id: String,
    timeout: Duration,
) {
    let Some(session_id) = webrtcbin
        .name()
        .strip_prefix(WEBRTCBIN_PREFIX)
        .map(str::to_string)
    else {
        return;
    };
    if webrtcbin.find_property("connection-state").is_none() {
        return;
    }

    let connected = Arc::new(AtomicBool::new(false));
    {
        let connected = Arc::clone(&connected);
        webrtcbin.connect_notify(Some("connection-state"), move |webrtcbin, _pspec| {
            if webrtcbin.property::<WebRTCPeerConnectionState>("connection-state")
                == WebRTCPeerConnectionState::Connected
            {
                connected.store(true, Ordering::Relaxed);
            }
        });
    }

    let clock = gst::SystemClock::obtain();
    let deadline = clock.time() + gst::ClockTime::from_nseconds(timeout.as_nanos() as u64);
    let id = clock.new_single_shot_id(deadline);
    let webrtcbin = webrtcbin.downgrade();
    let result = id.wait_async(move |_, _, _| {
        // Gone: the session already ended.
        let Some(webrtcbin) = webrtcbin.upgrade() else {
            return;
        };
        if connected.load(Ordering::Relaxed) {
            return;
        }
        drop(webrtcbin);
        let Some(sink) = sink.upgrade() else {
            return;
        };
        info!(
            "WHEP Output {}: viewer session {} did not connect within {:?}, ending it",
            instance_id, session_id, timeout
        );
        // Not on the system clock's thread: ending the session tears down a
        // pipeline, which may itself wait on that clock.
        std::thread::Builder::new()
            .name("whep-reaper".to_string())
            .spawn(move || {
                let signaller = sink.property::<glib::Object>("signaller");
                let _ = signaller.emit_by_name::<bool>("session-ended", &[&session_id.as_str()]);
            })
            .ok();
    });
    if let Err(e) = result {
        warn!(
            "WHEP Output: could not watch a viewer session for a connection timeout: {:?}",
            e
        );
    }
}
