# WHEP outputs answer 502 for a whole production run

> **Archived — code is the source of truth.** This is a troubleshooting record from
> 2026-08-26/27, not a spec. It documents what was measured, which fixes failed and why,
> and the root cause that was finally proven. The code has very likely drifted since —
> read the code for the current implementation.

## Symptom

Starting a production sometimes produces WHEP outputs that nobody can play. Every
`POST /whep/<endpoint>` answers `502 Bad Gateway`, for the entire life of that flow.
Restarting the flow sometimes helps, sometimes not. The UI shows the production as
running the whole time, with no error anywhere.

In one 27-hour log window: 61 × 502, and 5 of 9 flow starts affected. Reproduced on two
independent deployments.

## Root cause

**Strom's own MPEG-TS/SRT output hands `srtsink` a `streamheader` that is a frozen
snapshot of the first PAT/PMT it ever wrote, and `srtsink` replays that snapshot to every
caller that connects afterwards.**

The MPEG-TS/SRT output block requests `mpegtsmux` sink pads dynamically, as caps arrive on
each of its chains. The muxer therefore produces its first output — and freezes its
`streamheader` caps — while only the video chain is linked. That header holds a PAT and a
**video-only PMT**. It never updates; the tables in the live stream keep moving, both as
the remaining chains link and as later caps changes bump the table version.

So a receiver connecting at any point, however long after the sender started, gets:

1. the frozen header: a program with video only;
2. a few packets later, the live PMT: video **and** audio.

Its `tsdemux` builds a program from the header, sees the real tables, and tears that
program down again — pushing EOS out of the pad it removes. The `h264parse` that
`decodebin` autoplugged for that short-lived pad has data but no complete access unit, so
`GstBaseParse` posts a **fatal** error. The element aborts its own state, which fails the
whole receiving pipeline's PAUSED → PLAYING transition. `whepserversink`'s signaller never
opens its HTTP port, and the proxy answers 502 for the rest of the flow's life.

Steps 1–3 happen on **100 % of connects**. Only the last step is a race — whether a full
access unit landed inside the window — which is why it presented as "sometimes the
production starts".

### The evidence

Captured from a sender that had been running unrestarted for four weeks. Three separate
connects, each a fresh SRT caller:

```
pkt  1  PMT v0  [0x41:h264]                    <- the frozen header
pkt 12  PMT v1  [0x41:h264, 0x42:aac-adts]     <- the live tables
```

The first two packets were **byte-identical across all three connects**, diverging only
from packet 3 — that is a replay, not live data:

```
first 188 bytes (1 pkt):  IDENTICAL
first 376 bytes (2 pkts): IDENTICAL     <- PAT + video-only PMT
first 752 bytes (4 pkts): differ        <- live data from here
```

The receiving side matched, pad for pad. `tsdemux` names pads
`<type>_<program_generation>_<pid>`, so the generation counter shows the program being
re-created with the same video PID:

```
video_0_0041  done adding pad
Draining previous program
video_1_0041  done adding pad
audio_1_0042  done adding pad
video_0_0041  Pushing out EOS
h264parse  error: No valid frames found before end of stream
```

A second sender showed a milder variant: its frozen header listed **both** streams but at
an older table version than the live stream. The version bump alone was enough to make
`tsdemux` re-create the program. **Completing the header is therefore not a fix** — any
frozen header eventually disagrees with a long-running stream. It has to go.

## The fix

Strip `streamheader` from the caps leaving `mpegtsmux`, with an `EVENT_DOWNSTREAM` pad
probe, so `srtsink` has nothing stale to replay. A caller then simply waits for the next
periodic PAT/PMT — 100 ms by default — and gets tables that match the data behind them.

Guarded by `mpegtssrt_streamheader_test.rs`, which builds the real block, feeds it video
and audio, and asserts that no caps event reaching `srtsink` carries a `streamheader`.
Verified to fail when the strip is removed.

Measured after the fix, against a locally reproduced sender:

- three consecutive connects no longer share a byte of their heads (nothing replayed);
- each connect sees exactly one PAT and one PMT, listing video and audio, with no version
  change;
- five consecutive receiver starts: no program drain, no parser error, program generation
  0 throughout.

Whether the EFP/SRT output needs the same treatment was not investigated — it uses a
different muxer, and nothing was measured for it.

## How it was found

The container log was 95.6 % one repeated warning (an unrelated `audiomixer` latency
message), which buried everything else and rotated real evidence away within a day. After
filtering that out, the 502s clustered perfectly: every failing run had
`No valid frames found before end of stream` from the input's `decodebin`, every
succeeding run did not. Nine starts, no exceptions.

The origin of the EOS was then captured live by raising the GStreamer log level at runtime
(`PUT /api/gst-log-level`) to
`GST_EVENT:5,basesrc:5,tsdemux:5,baseparse:5,decodebin:5,srtsrc:5` for one flow start,
then resetting it to `*:0`. 246 861 lines in 12 seconds.

Note: `current` in the `/api/gst-log-level` response is Strom's cached string, not
GStreamer's real thresholds. Verify against actual log output, not the API. Runtime log
levels do not reset themselves.

What finally settled it was a PSI tracer — a small script that reassembles PAT/PMT
sections from a capture and prints every version and content change with a timestamp,
plus continuity-counter errors so packet loss is not mistaken for a table change. Pointing
that at a plain `srtsrc ! filesink` capture of the live stream took the question from
inference to arithmetic. Anything similar will do; the point is to read the tables rather
than reason about them.

## Ruled out by experiment

None of these reproduce the failure — do not re-test them:

| Hypothesis | Result |
|---|---|
| Dead SRT peer | Warnings and reconnect only; pipeline reaches PLAYING |
| Peer connects, sends partial stream, disconnects | Same — no EOS |
| Joining a live stream mid-GOP into a prerolling pipeline | 6/6 clean |
| Non-default SRT config on the block | Production runs all defaults |
| The sending file looping | Irrelevant; the file is minutes long, the fault far more frequent |
| Mid-stream join to a sender with a correct header | 10/10 clean, no drain |
| A late joiner to a stream whose PMT already changed | Sees only the current version |

The failure *shape* also reproduces deterministically with a truncated transport stream —
`head -c $((188*40)) full.ts` fails, `$((188*60))` is clean — which is useful for
exercising the parser's EOS path but says nothing about the cause.

## Fixes tried before the cause was known

### Landed — report the failure honestly (#705)

`gst_element_get_state()` reports a still-running transition as `Ok(Async)`; `Err` is only
ever `GST_STATE_CHANGE_FAILURE`, whatever the pending state says. `start()` now fails on
any `Err` instead of checking `pending == VoidPending`.

This fixes no cause. It converts a green-but-dead production into an explicit start
failure, which is what the operator needed in order to know to restart.

### Failed — drop the EOS at the parser (#706, closed)

Idea: the EOS from a program change is meaningless for a live input that reconnects on its
own, so drop it at the sink pad of every parser `decodebin` autoplugs inside the block.

Validated against a real feed: the shield fired on the right element and the fatal error
went away — but `decodebin` then never emitted `pad-added` for the new program. The input
never linked its pads while SRT kept delivering, and the pipeline hung in PAUSED.

**The EOS is what drives decodebin's teardown of the removed chain.** Swallowing it trades
a loud failure for a silent hang. Dead end.

### Also measured — intercepting the ERROR message cannot work

```
without bus sync handler: result=Err(StateChangeError) current=Ready pending=Playing
with    bus sync handler: result=Err(StateChangeError) current=Ready pending=Playing
```

Dropping the message changes nothing: the erroring element aborts its **own** state; the
message is not what aborts the transition at the bin. A `GstBin` subclass overriding
`handle_message` would not help. Worth knowing before anyone reaches for one.

### Parked — re-drive the state change (#707)

Re-drive the failed transition, on the theory that the input recovers on its own and only
the pipeline's state stays poisoned. Parked once the root cause was found, and it had two
problems of its own worth recording:

- Reaching PLAYING is not proof of a live data path, so it grew a liveness check —
  "every sink is EOS means the input died". That discriminator assumes the doomed chain
  has its own sink. In the real graph the doomed chain sits inside `decodebin`, upstream
  of a shared output, so a single-output flow has no case where the check does its job.
- Its own regression test passes in isolation but fails under a full parallel suite run.

It was containment for a symptom that can be removed at the source.

## Still worth doing

A single input's internal decode error should not be able to fail the whole production's
state change. The architecturally clean answer is to isolate each input in its own
`gst::Pipeline` bridged with appsrc/appsink — the pattern the Media Player block already
uses, explicitly to isolate downstream from seeks, file switches and EOS. A log from this
investigation shows one healthy input felled by another input's error, so this is not
hypothetical.

That is a much larger change to the most critical input block, with its own risks around
timestamps, live sync and latency. It was not attempted here, and it is defence in depth
rather than a fix: a third-party sender can present the same stale-header behaviour, and
we do not control it.

## Testing pitfall that cost hours

**A CI artifact binary is not the same build as the Docker image**, and swapping one into a
running container produces misleading results. The image is built with different feature
flags per architecture — notably GUI on/off and GPU support on/off.

A CI binary built with the GUI enabled starts the native GUI inside the container. With no
accelerated display Mesa falls back to `llvmpipe`, and eight software-rendering threads
burn ~370 % CPU at idle — which starves the server and makes everything else look broken.
Every measurement taken on that container was worthless.

If you need to test a branch on a deployment host, build with the Dockerfile's flags for
that architecture, or build the image itself. Sanity-check a candidate binary against the
one in the image, for example by comparing which GPU and GUI symbols each contains.

Better still: reproduce locally. Both ends of this failure were Strom, so the whole chain
ran on a laptop — sender flow, receiver, and capture — which turned a 12-second window on
a production host into an experiment that could be repeated in seconds.

## Related

- The `audiomixer` latency warning flood that made this hard to see:
  `Impossible to configure latency: max 1.125 s < min 2.000 s`, 310 572 of 324 926 lines.
  Separate bug, not investigated here.
- `WHEP Output: Found free port` binds port 0, reads the number and drops the listener
  before handing it to `whepserversink` — a TOCTOU that has not bitten yet but shares an
  ephemeral range with everything else on a host-networked box.
- `start_flow()` checks `pipelines.contains_key()` under a read lock, releases it, then
  builds and starts the pipeline before inserting — a ~3.5 s window in which two concurrent
  starts of the same flow both pass the guard. Observed in the wild; the second insert
  replaces the first manager and leaves its WHEP registrations pointing at dead ports.
- The failure paths in `start_flow()` do not agree with each other on cleanup: the
  endpoint-conflict paths skip the Media Player registry and the CPU allocation that the
  other paths release. Worth folding into one shared teardown.
