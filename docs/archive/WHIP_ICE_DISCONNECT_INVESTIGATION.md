# WHIP ICE Disconnect Investigation (2026-02-10) — RESOLVED

> **Archived.** This problem is solved. The notes below are kept for transparency and
> in case anyone hits a similar WebRTC ICE-disconnect pattern. They do not describe
> current behaviour.

## Resolution

Two fixes together closed the WHIP/WHEP ICE-disconnect problem:

1. **Isolated pipeline per WHIP session (v0.4.0).** Each `whipserversrc` session runs in
   its own `gst::Pipeline`, with media bridged to the main pipeline via `appsink`→`appsrc`
   into pre-built per-slot output chains. This removed the hot-swap path that broke the
   `NiceAgent` consent-freshness timers on recreated elements.
2. **`drop-on-latency=true` on live RTP inputs (v0.4.5, #472).** Works around a GStreamer
   jitterbuffer stall after a mute/idle gap that could also surface as an ICE disconnect.

If you see a WHIP/WHEP connection drop after a few seconds, check both: that each
`whipserversrc` lives in its own pipeline (never share one across sessions) and that
`drop-on-latency=true` is set on the live input.

---

## Original symptom (historical)

Browser ICE disconnected after ~6–7 seconds on **recreated** `whipserversrc` elements,
while the initial element (first connection after server start) held fine.

- Server-side ICE stayed COMPLETED (never reported DISCONNECTED).
- Browser-side ICE went connected → disconnected after ~6.5s — matching the RFC 7675
  consent-freshness timeout exactly.
- Consistent across all recreated elements, and across GStreamer 1.24.2 and 1.26.6
  (so not environmental).

## Original root-cause hypothesis (historical)

GStreamer's `libgstwebrtcnice` enables consent freshness
(`NICE_AGENT_OPTION_CONSENT_FRESHNESS`) at `NiceAgent` creation time only — the property
is `CONSTRUCT_ONLY`. The leading theory was that hot-swapping a fresh `whipserversrc` into
a running pipeline left its `NiceAgent` consent timer attached to the wrong (or
non-iterated) GLib `MainContext`, so consent checks never ran. The isolated-pipeline
architecture sidesteps this entirely by never hot-swapping the element.

Ruled out along the way: libnice 0.1.21 → 0.1.22 (consent fix is compile-time, not
runtime), socket/resource leaks (fixed separately, ICE still dropped), STUN reachability
(initial element worked with the same config), and SDP extmap rewriting.
