# Stream Synchronization

> **Code is the source of truth.** This guide describes intended behaviour and may have
> drifted from the current implementation. When in doubt, read the code and check the in-app UI.

How to align multiple input streams on a shared timeline so that buffers with
the same source-wallclock PTS line up at the mixer/compositor input, regardless
of independent network paths, jitter, or receiver latency.

This document is the reference for anyone configuring EFP/SRT inputs (and
eventually other protocols) for synchronized playout.

## Concept

Synchronization in a GStreamer pipeline happens in three places:

1. **At the sender.** Each buffer gets a PTS that is interpreted on the
   sender's clock. If that clock is globally meaningful (NTP/PTP/wallclock),
   the PTS encodes "absolute time of this frame".
2. **In transport.** EFP preserves PTS exactly. If the sender wrote "20.040 s
   since epoch", the receiver reads "20.040 s since epoch".
3. **At the receiver.** The pipeline's running-time is what downstream elements
   (mixers, compositors) sync on. Running-time is derived from buffer PTS via
   `running_time = PTS - segment.start + segment.base`. To make running-time
   across two demux instances comparable, `segment.start` must be identical
   (typically 0) on both.

If any of these three links is broken, synchronization fails.

## Pipeline clock

The pipeline clock determines what "now" means in a running-time domain. Pick
one *globally meaningful* clock when you care about cross-source alignment:

| Clock | When to use | `normalize_segment=never` works? |
|---|---|---|
| `GstSystemClock` — realtime | Single machine, all peers share the same OS wallclock (NTP-synced OS). | Yes |
| `GstSystemClock` — TAI | Same as realtime, leap-seconds-free. | Yes |
| `GstNetClientClock` | Receiver slaves to a sender's clock over UDP. | Yes |
| `GstNtpClock` | NTP-synced clock without OS-level NTP required. | Yes |
| `GstPtpClock` | Broadcast-grade sub-microsecond sync via PTP (IEEE 1588). | Yes |
| `GstSystemClock` — monotonic (default) | Playout / transcode where only frame cadence matters. | **No — config error** |

Monotonic clock is the default; GStreamer picks it silently if you don't
override. It cannot be used for cross-source sync, because its zero is
"receiver boot time" — incommensurable with any sender's timeline.

### Setting a non-default clock

The flow's `clock_type` property controls which clock is installed before the
pipeline transitions to PLAYING. Pick it from the Flow Properties dialog in
the UI (Monotonic / Realtime / TAI / PTP / NTP). The wiring lives in
`backend/src/gst/pipeline/construction.rs::configure_clock` — that function
is the canonical reference for what each value does, including the
`direct_media_timing` interaction (forces `base_time=0, start_time=NONE`,
required for AES67 and useful for EFP cross-source PTS alignment).

If you'd rather set the clock from code (e.g. for a test harness), the
underlying calls are:

```rust
use gstreamer::prelude::*;

let clock: gst::SystemClock = glib::Object::builder()
    .property("clock-type", gst::ClockType::Realtime)
    .build();
pipeline.use_clock(Some(&clock));
```

For PTP, replace with `gst::PtpClock::new(Some("eth0"), 0)`. For NTP, use
`gst::NtpClock::new(None, "ntp.server.example", 123, gst::ClockTime::ZERO)`.

## `normalize_segment` on EFP inputs

The EFP demuxer has a `normalize_segment` property with three values:

- **`auto`** (default). The demuxer inspects the pipeline clock: a monotonic
  `GstSystemClock` means normalize (legacy behaviour), any realtime/TAI/NTP/PTP
  clock means pass absolute PTS through. This is the right choice for almost
  all users.
- **`always`**. Force normalization. Segment start is rewritten to match the
  first large PTS seen on each pad; running-time starts near 0 per pad. Use
  this only for legacy pipelines that assumed this behaviour.
- **`never`**. Never rewrite the segment. Running-time equals absolute PTS.
  Required for cross-source synchronization. Only valid when the pipeline
  clock is globally meaningful — if you set `never` with a monotonic clock,
  strom logs a warning at pipeline start.

### Choosing a value

| Scenario | `normalize_segment` | Clock |
|---|---|---|
| Single stream playout / transcode (no sync needed) | `auto` | any |
| Multiple EFP inputs mixed/composited by real-world time | `never` | realtime / NTP / PTP |
| Legacy pipeline expecting running-time-from-zero per pad | `always` | any |
| "I'm not sure" | `auto` | Match what you actually want: monotonic for simplicity, realtime/NTP/PTP for sync |

## Sender side

`efpmux` copies `buffer.pts()` from each input buffer directly into the EFP
frame; it does not rewrite timestamps. For the sender's PTS to be globally
meaningful, the upstream pipeline must have been timestamping in the
wallclock/NTP/PTP domain — typically because the pipeline clock was chosen
that way and the source element populates PTS from `clock.time() - base_time`.

If your sender clock is monotonic, you can still force wallclock stamping by
inserting a `clocksync` with `sync=true` just before `efpmux` while running
the sender's pipeline clock as realtime/NTP/PTP.

## Verifying sync works

1. Confirm the sender pipeline clock is realtime / NTP / PTP.
2. Confirm the receiver pipeline clock is the same family (realtime, NTP, or
   PTP).
3. Set `normalize_segment=never` on each receiving EFP input block.
4. Start the flow. Look for `EFPSRT Input {id}: normalize_segment=never
   running on clock 'GstSystemClock' — OK` in the log (or equivalent for
   NTP/PTP). If you see a warning about monotonic clock, fix the config.
5. Compare running-times on the receiving side using a mixer latency probe
   or `gst_debug_bin_to_dot_file` to confirm the two streams align within
   expected jitter.

## Reference: tests

- `gst-plugin-efp/tests/pipeline.rs`:
  `pts_preservation_roundtrip_with_normalize_never` — end-to-end proof that
  absolute PTS survives `efpmux → efpdemux` when `normalize-segment=never`.
- `gst-plugin-efp/tests/pipeline.rs`:
  `normalize_segment_auto_{monotonic,realtime}_clock_*` — auto-mode clock
  detection.
- `backend/src/blocks/builtin/efpsrt_input.rs::tests::normalize_segment_*` —
  strom block wiring tests.
