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
//! This ends each session whose peer connection has not come up in time the
//! same way DELETE does: by emitting `session-ended` on the signaller. The
//! viewer gets [`CONNECT_TIMEOUT`] from the moment the answer goes out (the
//! WHEP server sends it when the server's ICE gathering completes), so codec
//! discovery and gathering do not eat into the viewer's ICE time. A session
//! that never gets that far is ended [`ANSWER_TIMEOUT`] after it was created.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_webrtc::{WebRTCICEGatheringState, WebRTCPeerConnectionState};
use tracing::{info, warn};

/// How long a viewer has to bring its peer connection up after it got its
/// answer. Browsers give up on ICE after about 30 s themselves.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a session may go without an answer before it is ended. The WHEP
/// server stops waiting for the answer after its `timeout` (15 s by default)
/// and the viewer never gets one after that; this leaves room for codec
/// discovery on a long GOP.
pub(crate) const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// webrtcsink names each viewer's webrtcbin `webrtcbin-<session id>`.
const WEBRTCBIN_PREFIX: &str = "webrtcbin-";

/// End every viewer session of `whepserversink` that does not connect within
/// `connect_timeout` of its answer, or gets no answer within
/// `answer_timeout`.
pub(super) fn install(
    whepserversink: &gst::Element,
    instance_id: &str,
    connect_timeout: Duration,
    answer_timeout: Duration,
) {
    if whepserversink.find_property("signaller").is_none() {
        return;
    }
    let sink = whepserversink.downgrade();
    let instance_id = instance_id.to_string();
    whepserversink.connect("consumer-added", false, move |values| {
        if let Some(webrtcbin) = values.get(2).and_then(|v| v.get::<gst::Element>().ok()) {
            watch(
                &webrtcbin,
                sink.clone(),
                instance_id.clone(),
                connect_timeout,
                answer_timeout,
            );
        }
        None
    });
}

/// What a timer needs to end one session.
#[derive(Clone)]
struct Reaper {
    sink: glib::WeakRef<gst::Element>,
    instance_id: String,
    session_id: String,
}

/// Whether the peer connection is still on its way up. A session in any
/// other state has either connected or is being torn down by webrtcsink.
fn still_connecting(webrtcbin: &gst::Element) -> bool {
    matches!(
        webrtcbin.property::<WebRTCPeerConnectionState>("connection-state"),
        WebRTCPeerConnectionState::New | WebRTCPeerConnectionState::Connecting
    )
}

fn answered(webrtcbin: &gst::Element) -> bool {
    webrtcbin.property::<WebRTCICEGatheringState>("ice-gathering-state")
        == WebRTCICEGatheringState::Complete
}

fn watch(
    webrtcbin: &gst::Element,
    sink: glib::WeakRef<gst::Element>,
    instance_id: String,
    connect_timeout: Duration,
    answer_timeout: Duration,
) {
    let Some(session_id) = webrtcbin
        .name()
        .strip_prefix(WEBRTCBIN_PREFIX)
        .map(str::to_string)
    else {
        return;
    };
    if webrtcbin.find_property("connection-state").is_none()
        || webrtcbin.find_property("ice-gathering-state").is_none()
    {
        return;
    }
    let reaper = Reaper {
        sink,
        instance_id,
        session_id,
    };

    // Backstop for a session that never gets its answer out. Once it has,
    // the connect timer below owns the session.
    arm(webrtcbin, reaper.clone(), answer_timeout, true);

    // The connect timer starts when the answer goes out. Armed once, from
    // the notify or from here if gathering already completed.
    let armed = Arc::new(AtomicBool::new(false));
    {
        let armed = Arc::clone(&armed);
        let reaper = reaper.clone();
        webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
            if answered(webrtcbin) && !armed.swap(true, Ordering::SeqCst) {
                arm(webrtcbin, reaper.clone(), connect_timeout, false);
            }
        });
    }
    if answered(webrtcbin) && !armed.swap(true, Ordering::SeqCst) {
        arm(webrtcbin, reaper, connect_timeout, false);
    }
}

/// End the session `timeout` from now unless its peer connection has come
/// up by then. With `before_answer`, skip it as well once the answer is out.
fn arm(webrtcbin: &gst::Element, reaper: Reaper, timeout: Duration, before_answer: bool) {
    let clock = gst::SystemClock::obtain();
    let deadline = clock.time() + gst::ClockTime::from_nseconds(timeout.as_nanos() as u64);
    let id = clock.new_single_shot_id(deadline);
    let webrtcbin = webrtcbin.downgrade();
    let result = id.wait_async(move |_, _, _| {
        // Gone: the session already ended.
        let Some(strong) = webrtcbin.upgrade() else {
            return;
        };
        if !still_connecting(&strong) || (before_answer && answered(&strong)) {
            return;
        }
        drop(strong);
        let Reaper {
            sink,
            instance_id,
            session_id,
        } = reaper;
        let Some(sink) = sink.upgrade() else {
            return;
        };
        info!(
            "WHEP Output {}: viewer session {} did not connect within {:?} of {}, ending it",
            instance_id,
            session_id,
            timeout,
            if before_answer {
                "its offer"
            } else {
                "its answer"
            }
        );
        // Not on the system clock's thread: ending the session tears down a
        // pipeline, which may itself wait on that clock.
        let spawned = std::thread::Builder::new()
            .name("whep-reaper".to_string())
            .spawn({
                let session_id = session_id.clone();
                move || {
                    // The connection may have come up since the check above.
                    match webrtcbin.upgrade() {
                        Some(webrtcbin) if still_connecting(&webrtcbin) => {}
                        _ => return,
                    }
                    let signaller = sink.property::<glib::Object>("signaller");
                    let _ =
                        signaller.emit_by_name::<bool>("session-ended", &[&session_id.as_str()]);
                }
            });
        if let Err(e) = spawned {
            // Most likely near the fd or thread limit, the case this exists for.
            warn!(
                "WHEP Output {}: could not start a thread to end viewer session {}: {}",
                instance_id, session_id, e
            );
        }
    });
    if let Err(e) = result {
        warn!(
            "WHEP Output: could not watch a viewer session for a connection timeout: {:?}",
            e
        );
    }
}
