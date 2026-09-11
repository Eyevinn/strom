# Producer Switching over the API — Operator Guide

> **Code is the source of truth — this may have drifted; read the code for the current
> implementation.** This guide describes behaviour observed on a running rig at one point
> in time. When in doubt, read the code and check the in-app UI.

How a producer changes what the audience sees — all participants, one participant, or
anything in between — by driving the vision mixer block over HTTP instead of the operator
page. Companion to the [Vision Mixer Operator Guide](VISION_MIXER_OPERATOR_GUIDE.md),
which covers the same mixer from the GUI. Read that one first for the concepts; this one
is the call-by-call recipe, the on-air behaviour of each move, and what breaks.

Everything below was exercised against a live five-seat meeting flow and checked against
captured program frames, not against HTTP status codes. Where a claim is about what the
audience sees, it was read off a frame.

---

## 1. The model in one minute

The mixer has two buses, **PGM** (on air) and **PVW** (preview). Each bus holds one
**source**, and a source is one of two things:

- `{"input": N}` — a single input, full frame.
- `{"pip": K}` — a **PiP**, which despite the name is a whole composition: an optional
  full-region background input plus a list of **zones**. Each zone is a rectangle holding
  one or more inputs that auto-tile inside it.

So "all five up" and "one participant full frame" are not different features. They are
two sources you can put on either bus. Cutting between them is an ordinary take.

Three endpoints do all the work:

| Call | What it does |
|---|---|
| `PUT /api/flows/{flow_id}/blocks/{block_id}/pip/{pip_idx}` | Replace a PiP's composition. Applies live. |
| `PUT /api/flows/{flow_id}/blocks/{block_id}/preview` | Put a source on PVW. |
| `POST /api/flows/{flow_id}/blocks/{block_id}/transition` | Take: swap PGM and PVW. |
| `GET  /api/flows/{flow_id}/blocks/{block_id}/state` | Read back both buses and every PiP. |

The broadcast pattern is: build the next look on a PiP that is **not** on air, preview it,
take it. A mixer with `num_pips: 2` supports this indefinitely, because the take swaps the
buses and hands the previous PGM composition back to you on preview to rebuild.

### The take is a swap, and it ignores half its own request body

`POST .../transition` takes `from_input` and `to_input`, but whenever the block has live
state the engine ignores them and swaps whatever is currently on PGM and PVW. Send `0` for
both. What matters is `transition_type` and `duration_ms`.

```bash
FLOW=945fa329-...   BLOCK=vmix
API=http://127.0.0.1:8123/api/flows/$FLOW/blocks/$BLOCK

curl -X POST -H 'content-type: application/json' \
  -d '{"from_input":0,"to_input":0,"transition_type":"cut","duration_ms":0}' \
  $API/transition
```

After a take, `GET .../state` reports the swap: the composition that was on air is now on
`preview_pip` (or `preview_input`), ready to be rebuilt for the next move.

One reporting quirk: when a PiP with a background is on air, `program_input` reports that
background's index rather than `null`. Read `program_pip` to know which composition is on
air.

---

## 2. The four layouts, verified

All examples assume a five-input mixer with two PiPs, inputs `0..4` fed by participants
P1..P5. Every layout below was taken to air and read off a captured program frame.

### All five up

One zone, no rect (fills the PiP region), all five inputs. The zone auto-tiles them.

```bash
curl -X PUT -H 'content-type: application/json' -d '{
  "bg": null, "transforms": {},
  "zones": [ { "rect": null, "sources": [0,1,2,3,4] } ]
}' $API/pip/0

curl -X PUT -H 'content-type: application/json' -d '{"source":{"pip":0}}' $API/preview
curl -X POST -H 'content-type: application/json' \
  -d '{"from_input":0,"to_input":0,"transition_type":"cut","duration_ms":0}' $API/transition
```

Result: three tiles across the top, two on the second row. The tiling is a
`cols = ceil(sqrt(N))` by `rows = ceil(N/cols)` grid, so five sources give a 3×2 grid with
one empty cell. The grid **as a block** is centred in the region, but cells fill row-major
left to right, so the short last row sits to the **left**, not centred under the row above.
If you want the odd participant centred, do not use a single auto-tiling zone — give each
row its own zone, or each source its own zone.

### One participant full frame

No PiP involved. Preview the input and take.

```bash
curl -X PUT -H 'content-type: application/json' -d '{"source":{"input":2}}' $API/preview
curl -X POST -H 'content-type: application/json' \
  -d '{"from_input":0,"to_input":0,"transition_type":"cut","duration_ms":0}' $API/transition
```

### Two up

Same as five up with two sources. Two sources tile side by side, each cell aspect-fitted,
the pair vertically centred with black above and below when the region is wider than two
16:9 cells.

```bash
curl -X PUT -H 'content-type: application/json' -d '{
  "bg": null, "transforms": {},
  "zones": [ { "rect": null, "sources": [0,1] } ]
}' $API/pip/1
```

### One plus three

Do **not** express this as one big zone plus one auto-tiling zone of three. Three sources
in a zone tile as a 2×2 grid with a hole, not as a vertical stack. Give each small box its
own zone. Rects are normalised to the PiP region, `x`/`y` is the top-left corner.

```bash
curl -X PUT -H 'content-type: application/json' -d '{
  "bg": null, "transforms": {},
  "zones": [
    {"rect": {"x":0.02,"y":0.14,"w":0.64,"h":0.72}, "sources":[0]},
    {"rect": {"x":0.68,"y":0.06,"w":0.30,"h":0.28}, "sources":[1]},
    {"rect": {"x":0.68,"y":0.36,"w":0.30,"h":0.28}, "sources":[2]},
    {"rect": {"x":0.68,"y":0.66,"w":0.30,"h":0.28}, "sources":[3]}
  ]
}' $API/pip/0
```

### Background plus boxes

Set `bg` to fill the whole region behind the zones. Useful for a full-frame speaker with
two guests boxed over the corner. Zones can carry a border, which draws outside the picture
edge so it never covers content.

```bash
curl -X PUT -H 'content-type: application/json' -d '{
  "bg": 0, "transforms": {},
  "zones": [
    {"rect": {"x":0.62,"y":0.06,"w":0.34,"h":0.26}, "sources":[1],
     "border": {"color":"#FFCC00","width":4}},
    {"rect": {"x":0.62,"y":0.36,"w":0.34,"h":0.26}, "sources":[2],
     "border": {"color":"#FFCC00","width":4}}
  ]
}' $API/pip/1
```

---

## 3. Editing the PiP that is on air

**It is safe, and it is not a cut.** Changing zones on the PiP currently on PGM produces an
animated re-tile: every source that stays in the composition slides and scales from its old
box to its new one over about 250 ms, sources that are leaving fade out, sources that are
arriving fade in. No black frame, no flash, no dropped frame. Frame-by-frame inspection of
a full mirror-image layout change (big box moved from left to right, three small boxes moved
from right to left) showed a smooth eight-frame move with every source visible throughout.

So the choice between editing on air and preview-then-take is editorial, not technical:

- **Edit on air** when you want the audience to see the move — opening a box for a guest who
  just joined, dropping a box when someone leaves. It reads as a deliberate animated
  rearrangement, which is what a viewer expects from a video call layout.
- **Preview then take** when you want the change to be invisible until you commit, or when
  you are building something complex and do not want half-finished states on air. Editing
  the PiP that is on PVW provably does not touch PGM: a full re-layout of the preview PiP
  left every program frame unchanged.

Preview-then-take is therefore **not mandatory**. It is the right habit for anything you
are still composing, and unnecessary for a single deliberate move.

---

## 4. What each take actually looks like

`transition_type` accepts `cut`, `fade`, `slide_left`, `slide_right`, `slide_up`,
`slide_down`. What you get depends on whether a PiP is involved.

| From → To | `transition_type` | What the audience sees |
|---|---|---|
| anything → anything | `cut` (or `duration_ms: 0`) | One-frame switch. Verified: last frame of the old look, next frame the new look, nothing between. |
| input → input | `fade` | A true dissolve. Frames mid-transition show both pictures blended. |
| PiP → PiP, **no shared sources** | `fade` | A true dissolve between the two compositions. |
| PiP → anything, **sharing a source** | `fade` | **Not a dissolve.** The shared source animates from its old box to its new one — going from a four-up to that participant full frame reads as a zoom-in, with the other tiles covered as the box grows. |
| either bus is a PiP | `slide_*` | Silently downgraded to `fade`. The server logs the downgrade; the HTTP response reports the transition that actually ran in `actual_transition_type`. |

The shared-source case is the one that surprises people. The engine animates pads, not
pictures: a source present in both the outgoing and incoming composition is treated as
*moving*, and only sources exclusive to one side cross-fade. If you want a genuine dissolve
out of a multi-box layout, take to a composition that shares no inputs with it, or use a
cut.

Check `actual_transition_type` in the response if you care which one ran.

---

## 5. Failure modes

### A zone names a seat that is not publishing

**Nothing warns you, and the layout does not re-flow.** The call succeeds, the layout keeps
a cell for that participant, and the cell is simply empty. A five-up including one absent
seat renders as a 3×2 grid with a black hole where they would be — not as a tidy four-up.

The fix is to rebuild the zone without that source, which re-tiles the remaining
participants into a full 2×2. There is no automatic compaction.

### A seat drops while it is on air

**The tile freezes on its last frame and stays there indefinitely.** It does not go black,
it is not removed, and the composition does not re-flow. On the rig the freeze followed the
publisher's death within about two seconds — the measurement cannot separate the jitter
buffer from the mixer's own output latency, so treat it as immediate. The frozen frame was
still on air minutes later, long after the ingest session had been reaped for inactivity.

This is the most dangerous failure in the set, because a frozen participant looks exactly
like a still one. Two consequences for a live show:

- Do not trust the program picture to tell you a seat is gone. Watch the ingest session
  state or the per-seat block health instead.
- The recovery is automatic: when the participant rejoins, their tile resumes in place with
  no operator action and no layout change.

If you need the seat gone from the picture, you must remove it from the zone yourself —
which is a live edit and animates as described in §3.

### A zone holds more sources than its capacity

**Silently truncated, oldest first, and the API lies to you about it.** A zone with
`"capacity": 2` and `"sources": [0,1,2]` renders inputs 1 and 2 only — input 0 is dropped
because it is the oldest entry. No error, no warning. `GET .../pip/{idx}` reads back all
three sources, so state and picture disagree: the state is the intent, the picture is the
newest `capacity` entries.

Capacity is a feature, not a guard — capacity 1 is "swap mode", where pushing a new source
cross-fades it over the old one. If you do not want eviction, leave `capacity` unset.

### Things that *are* rejected

These all return HTTP 400 with a readable reason, before anything reaches the picture:

| Request | Response |
|---|---|
| Zone source index ≥ number of inputs | `Zone source 5 out of range (num_inputs=5)` |
| A zone source that is also the background | `Zone source 1 duplicates bg` |
| The same source in two zones of one PiP | `Zone source 1 appears in more than one zone` |
| Border colour that is not `#RRGGBB`/`#RRGGBBAA` | `Invalid border color "red"` |
| PiP index ≥ `num_pips` | `PiP index 7 out of range (configured: 2)` |
| Preview to an input or PiP that does not exist | `Input 9 out of range (max 4)` |

Rects are **clamped**, not rejected — a rect running past the region edge is silently
pulled back inside. More than 15 overlay sources across all zones of one PiP is rejected,
but that ceiling is unreachable on a mixer with fewer than 16 inputs.

---

## 6. Verifying what is actually on air

**A 200 is not evidence.** Every claim in this guide was checked against program frames,
and two of the findings (the shared-source "fade", the frozen dropped seat) look completely
correct from the API side.

Two things make verification reliable:

1. **Give every source a visible clock.** A frozen tile is indistinguishable from a live
   one unless the picture itself is moving. Burning a running timecode into each
   participant feed turns "is this seat alive?" into something you can read off a single
   frame. Two seats reading the same time and one reading an older time is the whole
   diagnosis.
2. **Tap the program output to a file** rather than capturing it over the network. Adding a
   video encoder plus a recorder block fed from the mixer's PGM output gives frame-accurate
   material with no transport in the way, and the recorder's split endpoint closes a
   segment on demand so you can read it while the show continues.

Expect the recording to sit a couple of seconds behind your API calls — the mixer's output
queue plus the encoder. Wait 6–10 s after a move before closing the segment, or the moment
you care about lands in the next file.

One environment caveat worth knowing: on the macOS development rig, pulling the program
with a GStreamer WHEP client failed ICE negotiation every time, roughly two seconds into
the session, after the first connectivity check had already succeeded. The same client
against a plain standalone WHEP sink on the same machine worked. This was not chased down;
it is a capture-path problem, not a mixer problem, and the file tap sidesteps it entirely.

---

## 7. Quick reference

```bash
API=http://127.0.0.1:8123/api/flows/$FLOW/blocks/$BLOCK

# Read everything: both buses, every PiP composition
curl -s $API/state

# Read one PiP (useful for saving a look you want to restore later)
curl -s $API/pip/0

# Build a look on an off-air PiP
curl -X PUT -H 'content-type: application/json' -d '{"bg":null,"transforms":{},"zones":[...]}' $API/pip/0

# Preview it
curl -X PUT -H 'content-type: application/json' -d '{"source":{"pip":0}}' $API/preview

# Take it
curl -X POST -H 'content-type: application/json' \
  -d '{"from_input":0,"to_input":0,"transition_type":"cut","duration_ms":0}' $API/transition
```

`GET .../pip/{idx}` and `PUT .../pip/{idx}` use the same shape, so a GET is how you snapshot
a look and a PUT is how you restore it. Saving the four or five looks a show needs as JSON
files, and replaying them by PUT, is the whole of a producer's cue stack.
