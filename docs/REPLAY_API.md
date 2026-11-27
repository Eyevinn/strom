# Replay Control API

> Code is the source of truth — this may have drifted; read the code for the current implementation.

Media players in timeline mode (`mode = timeline`) that share a `group` form a **replay group**: one cursor and one speed for all angles. The group shares time, not frame numbers. The cursor is an instant on the group's time axis, and each angle shows its latest frame that starts at or before it. This page describes how to drive a group from outside: discovery, the commands, what they return, the events, and the state. This is **API version 2**.

The built-in operator page `/player/replay/{flow_id}/{group}` drives the group with this API only. Around it, the page also:

- reads the flow (`GET /api/flows/{flow_id}`) to find the group's players and the vision mixer they feed;
- picks angles through the vision mixer API (`PUT .../blocks/{mixer}/preview`, `POST .../transition`, `GET .../state`);
- finds its monitors with `GET .../blocks/{mixer}/pgm-endpoint` and `multiview-endpoint`, or without a mixer the nearest WHEP Output downstream of a player, and plays them over WHEP;
- keeps marks and clips in the browser's `localStorage`, so they belong to that browser.

The diagnostics page `/player/replay-diagnostics/{flow_id}/{group}` shows the state and the engine diagnostics, and measures the time from a command to the picture on screen.

## Endpoints

| Method | Path | Body | Returns |
|---|---|---|---|
| `GET` | `/api/replay` | | every replay group of the server, with `api_version` and `features` |
| `GET` | `/api/replay/{flow_id}` | | the flow's groups, same shape |
| `GET` | `/api/replay/{flow_id}/{group}` | | one group |
| `POST` | `/api/replay/{flow_id}/{group}/command` | a command | the command result |
| `GET` | `/api/replay/{flow_id}/{group}/state` | | the group state |
| `GET` | `/api/replay/{flow_id}/{group}/diagnostics` | | engine diagnostics |
| `GET` (WebSocket) | `/api/replay/{flow_id}/{group}/ws` | commands | state, events, acks, errors |

The schema is in the OpenAPI spec under the `replay` tag. Every WebSocket message is one `ReplayWsMessage`, tagged by `type`.

### Discovery

```json
{
  "api_version": 2,
  "features": ["follow_live", "client_ref", "mark_at", "offset_ms", "jog_grid", "start_mode", "diagnostics", "source_switch", "cue_next"],
  "groups": [
    {
      "flow_id": "...", "group": "r1", "time_domain": "tai", "ready": "ready",
      "grid": {"output": {"fps": "50/1", "frame_ns": 20000000.0}, "source": {"fps": "50/1", "frame_ns": 20000000.0}},
      "angles": [{"block": "cam1", "name": "Camera 1", "source_id": "<TAMS flow id>", "source": "tams+https://gateway.example.com/flows/<id>"}],
      "source": "tams+https://gateway.example.com"
    }
  ]
}
```

- A group exists only while its flow runs. A group that does not run is 404, or missing from the lists.
- `source` is a URI without credentials. The group's `source` is the store the angles share, if they share one.
- A player without a `group` has a group of its own, `@<block id>`.
- `GET /api/version` has `process_started_at`. A client that sees a new value knows the server restarted.

### The WebSocket

- The socket sends the state every 50 ms. `?state_ms=200` sets another rate (50 to 1000).
- `?diagnostics=1` adds a `{"type": "diagnostics", ...}` after each state.
- A command may carry a `seq`. It is echoed in the reply: `{"type": "ack", "seq": 7, ...result}` or `{"type": "error", "seq": 7, "code": "...", "error": "...", "client_ref": "..."}`.
- When the socket falls behind and drops events, it sends `{"type": "lagged", "missed": 3}`. Read the state then. The `event_seq` of the events also shows the jump.
- A WebSocket ends with its group: it closes (code 1001) once the group's players are gone, when the flow stops or is deleted, or when a new run of the flow makes a new group. Reconnect to follow the next run. An open socket does not keep the group alive.

### Errors

| HTTP | Code | When |
|---|---|---|
| 400 | `invalid` | Malformed command, unknown `cmd` or field, a value out of range (speed outside ±8, bad `t` or UTC text, not exactly one field in a target, `out` before `in`, a `client_ref` over 64 characters, a `margin_ms` over 60000, a mark `label` over 256 characters). |
| 404 | | No replay group with that name in that flow (the flow is not running). |
| 409 | `conflict` | The command is valid but does not apply now: no media yet, a jog before the frame rate is known, a `utc` target when the time domain is not `tai`, `until` behind the cursor, a mark before any picture has gone out. |
| 409 | `out_of_range` | A cue or `mark {at}` wholly outside the media. |

On REST the code is the error body's `error` field, the reason its `details`. On the WebSocket it is `code`. A refused command changes nothing.

## Time

- **`t`** is the group's time: integer nanoseconds, always as a decimal string (it does not fit in a JavaScript number; read it with `BigInt`). It is exact and stable over restarts and retention. It is what a client stores.
- **`time_domain`** says once per group what `t` is:

| `time_domain` | `t` | `utc` |
|---|---|---|
| `tai` | TAI ns since 1970-01-01 TAI: the same number as a TAMS timestamp `"<s>:<ns>"` | given (RFC 3339, ms) |
| `unanchored` | ns synced between the angles, but no real time (test material) | `null` |
| `unknown` | an angle has not read its index yet | `null` |

- An **instant** is `{"t": "...", "utc": "..."|null}`: the cursor, marks, the cue's `in` and `out`, the media's edges, extents and gaps.
- A **frame** is `{"t": "...", "utc": ..., "frame_in_second": 24}`: the frame of the **output** grid (the output's frame rate) that holds an instant. `t` is the frame's start, `frame_in_second` is the `FF` of a timecode `HH:MM:SS:FF`. A frame is `null` until the grid is known. Where both are given they stand side by side, `{"at": instant, "frame": frame}`; the exact `t` is never replaced by the frame's start.
- The group has two grids: `output` (timecodes) and `source` (the finest source rate). `jog` steps on the group's `jog_grid`, `source` by default, so slow motion from a high-frame-rate source shows every source frame.
- There are no absolute frame numbers. Positions are `t`, lengths milliseconds, and frames only relative steps (`jog`). A position in ms is `(t − range.start.t) / 1e6`, computed with `BigInt`.

### Targets

A command names a point with an object of **exactly one** field:

| Field | Meaning | Example |
|---|---|---|
| `t` | Group time, exact. | `{"t": "1795526557010000000"}` |
| `utc` | RFC 3339. Lands on the start of the **output** frame (the grid of `frame` and timecodes) nearest it, within half an output frame. So a frame's UTC from the API, or a timecode typed in, seeks back to that frame whatever the sources' rates; each angle then shows its source frame that holds that point. 409 unless the domain is `tai`. | `{"utc": "2026-11-24T13:21:56.080Z"}` |
| `from_live_ms` | Milliseconds before the live edge. `0` is the last frame every angle has (`range.live_edge − 1`). | `{"from_live_ms": 8000}` |
| `rel_ms` | Milliseconds from the cursor when the command arrives, negative is back. In `cue` and `play {until}` every field counts from the same cursor. | `{"rel_ms": -2000}` |

Targets outside the media:

- `seek` and `jog` clamp to what is there (`range.start` to `range.end − 1`).
- `cue` gives 409 `out_of_range` when the range is wholly outside. A range partly outside is clamped. `play {until}` is never wholly outside: `until` must be ahead of the cursor (behind it in reverse), else 409 `conflict`.
- `mark {at}` outside the media gives 409 `out_of_range`.

## Commands

Every command may carry `client_ref` (a string of at most 64 characters). It is echoed in the response and in the events the command causes, also later ones such as `ended` when a clip ends:

- The end of a cue (`ended`, or `live` with `end_action: live`; cause `cue_end` or `media_end`) carries the **cue's** ref: the one of the `cue` or `play {until}` that set it, or of a later `play` with a ref that plays it. Other commands while the cue is held (`speed`, `ramp`, `follow_live`, `play` without a ref) do not change it; a new cue replaces it, and it goes when the cue does.
- `caught_up` and `edge_moved` carry the ref of the last command that changed the motion (not `follow_live` or `mark`).
- `live` with cause `live` carries the `live` command's ref.

| `cmd` | Fields | What it does |
|---|---|---|
| `seek` | `to`, `speed?` | Go to the point. Keeps the speed (during a ramp, the ramp's speed now), or sets `speed` in the same step. |
| `jog` | `frames` | Pause and move a number of frames on `jog_grid`. The cursor rests in the middle of the frame. |
| `speed` | `speed` (−8..8) | Set the speed where the cursor is. `0` pauses. |
| `play` | `speed?` (default 1), `until?`, `end_action?` | Play. With a cue: within half a frame of the end play runs to, play starts again from the other end. `until` sets a new cue `[min(cue.in, cursor), until)`. `end_action` without `until` and without a cue is 400. |
| `pause` | | Speed 0 where the cursor is. |
| `ramp` | `to`, `duration_ms` (0–60000) | Change the speed linearly from the speed now to `to`; the server runs the ramp. |
| `cue` | `in`, `out`, `end_action?`, `source?`, `block?` | Hold `[in, out)` and park, paused, on `in`. With `source`, angle `block` also switches to that source (see below). |
| `uncue` | | Drop the cue. The cursor, the speed and a running ramp stay. |
| `live` | `margin_ms?` (0–60000) | Play at 1x from the live edge minus the margin. `margin_ms` sets the margin for the whole group for the rest of the run; leave it out to keep the group's margin. |
| `end` | | Go to the last frame every angle has, pause, drop the cue. |
| `mark` | `label?` (≤ 256 characters), `at?` | Stamp the picture going out now, or the point `at`. Changes nothing. |
| `follow_live` | `on` | Whether the group goes live by itself (`false`: review mode). |
| `source` | `source`, `block?` | Switch angle `block` to another source. The motion does not change. |
| `cue_next` | `in`, `out`, `speed?` (0–8, not 0; default 1), `end_action?`, `source?`, `block?`, `replace?` | Queue a clip to play when the clip playing reaches its out point (see below). The motion does not change. |
| `clear_queue` | | Empty the queue. The clip playing goes on to its out point and its end action. |

`end_action` is `freeze` (default), `loop` or `live`.

### What each command does to the motion

`=` is unchanged. **Live\*** means: off, but with `follow_live` on the group goes live by itself (cause `caught_up`) when it plays forward without a cue and reaches the live point; the speed becomes 1x and a ramp ends. Play that starts at or ahead of the live point (`end` then `play`, a seek near the edge, `play {speed: 2}` while live) has reached it once it runs at 1x or faster (with a ramp: and ramps to 1x or faster): it goes live at once. If it is then more than 250 ms ahead of the live point on the smooth edge estimate, it goes back to that point, since 1x from closer to the edge would hold at the edge on every step. Slower play from there stays slow motion: the edge runs away from it. Forward play that has not gone live stops on the last frame every angle has, at its speed, until media grows. With `follow_live` off it never goes live.

| `cmd` | cursor | speed | cue | live | ramp |
|---|---|---|---|---|---|
| `seek` | → `to` (clamped) | `=` (the ramp's now), or `speed` | kept if it holds `to` | Live\* | ends |
| `jog` | ± frames on `jog_grid` | 0 | kept if it holds the target | off | ends |
| `speed` | `=` | `speed` | kept | Live\* | ends |
| `play` | `=`, at the cue's end → the other end | `speed` (1) | kept; `until` sets a new one | Live\* | ends |
| `pause` | `=` | 0 | kept | off | ends |
| `ramp` | `=` | → `to` over the time | kept | Live\* (the ramp is cut at live) | starts |
| `cue` | → `in` | 0 | set | off | ends |
| `uncue` | `=` | `=` | dropped | as before (a group with a cue is not live); Live\* when playing forward | goes on |
| `live` | → edge − margin | 1 | dropped | **on** | ends |
| `end` | → last common frame | 0 | dropped | off | ends |
| `mark` | `=` | `=` | `=` | `=` | `=` |
| `follow_live` | `=` | `=` | `=` | `=` (off if `on: false` and live) | `=` |
| `source` | `=` | `=` | `=` | `=` | `=` |
| `cue_next`, `clear_queue` | `=` | `=` | `=` | `=` | `=` |

**Note:** with `follow_live` on, `play {speed: 2}` or a ramp towards 2x while the group is live reaches the live point almost at once and is 1x live again. A cue or `follow_live: false` stops that.

### Cue and bounded play

- `out` is exclusive. `freeze` holds the last frame that starts before `out` (or `in` when playing in reverse) and sends an `ended` event.
- `loop` wraps to the other end. `live` goes live at the margin, drops the cue, and sends a `live` event.
- On media that no longer grows, an `out` past the media ends play where the media does (cause `media_end`). While the media grows, play holds at the edge until the media reaches `out`.

### Switching source

`source` and `cue {source}` point one angle (a player) at another source while the flow runs, without a rebuild. A clip group can then be a small pool of players that are pointed at the camera each clip needs.

- `source` is a URI as the player's `source` property takes it: `tams+https://<gateway>/flows/<flow_id>` or a directory under the media path. It must be in the player's store: the same TAMS gateway (the block's credentials are only sent there), or another local directory.
- `block` names the angle. It may be left out when the group has one angle.
- The group must be on real TAI (`time_domain: tai`). In an `unanchored` group each angle has a time base of its own, and a new source would move the group's axis: 409 `conflict`. Also 409 while the domain is `unknown`, and for an export. An unknown `block`, a missing `block` in a group of several angles, or a source that does not parse or open is 400 `invalid`. A refused `cue {source}` changes nothing, the cue included.
- The player keeps its decoders and output. It fetches and decodes the new source at the cursor, holds its last picture until the new source has the frame for the cursor, then shows it and sends `source_changed`. A cue parks on `in`, so `cue {source}` followed by `play` starts from a decoded frame. For a switch without a visible hold, cue the clip in a group that is not on air (A/B groups through the mixer).
- While it switches, the angle state has `pending_source`, and the player holds two sources' frames, each within half the decoded budget.
- The switch gives up, and the old source stays, when the new source is not on real TAI, cannot be decoded, decodes into another layout (size, format) than the output, or gives no picture within 10 s. It sends `source_failed` with the reason. A later switch replaces one still pending (`source_failed`, "replaced").
- Both events carry the `client_ref` of the command that asked for the switch. After the switch the group's range follows the new source (`range_changed`), and discovery shows the new `source`.
- `offset_ms` stays the player's and applies to the new source too.

### The clip queue

`cue_next` queues clips that play one after the other without a gap, each from its `in` at its own `speed`. The queue only runs clips: the playlist (order, editing, several operators) is the client's, which fills the queue one clip ahead or more.

- The clip playing is the group's cue. When forward play reaches its `out` and a clip is queued, the group plays that clip from the running time the out point was reached: the tick before shows the last frame before `out`, the next tick the new clip's `in` (moved on by the time since the out was reached, at the clip's speed). The clip becomes the cue, with its `end_action`, and its `client_ref` is the cue's.
- With a clip queued the clip playing ends at its `out` even with `end_action: loop`. With nothing queued its own `end_action` applies (`freeze`, `loop`, `live`). A clip queued after the clip playing has ended (frozen) waits for the next `out` reached; `cue` and `play` start it by hand.
- Each player prepares the next clip while the current one plays: a second lane, fetched and decoded at the clip's `in` with the clip's `source` for angle `block`, else with the source the angle shows. It is shown from the tick the clip starts on, with what is decoded by then (the last picture holds until the rest is). One clip ahead is prepared at a time; while it is, each lane gets half the decoded budget.
- `source` and `block` are checked as for `source` (409 without a common time, 400 for an unknown angle or a source that does not open); `source_changed` (or `source_failed`) comes with the clip's `client_ref` when the angle shows it. A clip on the source the angle shows sends no source event.
- `rel_ms` targets count from the cursor when `cue_next` arrives. `out` not after `in`, or a speed that is not forward, is 400; a range outside the media is 409 `out_of_range`; a full queue (64 clips) is 409 `conflict`.
- `replace: true` empties the queue before the clip is added. `clear_queue` empties it. `live` and `end` leave clip play and empty it too; other commands keep it (pause a clip, play on, and the queue goes on at its out).
- Events: `ended` with the ref of the clip that ended, then `item_started` with the ref of the clip that started, both with cause `cue_end`. The state's `queue` has the clip's ref (`clip_ref`) and how many clips wait (`waiting`).
- `play {speed}` changes the clip playing only; the next clip plays at its own `speed`. Transitions other than a cut (a mix, a wipe) take two clip groups, A/B, through the mixer.

### Going live by itself

A group that follows live keeps to the margin. If the live point moves ahead of the cursor by more than one step of the edge (plus 250 ms), the group jumps forward to it and sends a `live` event with cause `edge_moved`. This happens when `live` came while an angle's index still lagged, for example right after a start.

## Command result

The result is the group's motion after the command, the command's `client_ref`, and the mark for `mark`:

```json
{
  "cursor": {"at": {"t": "1795526557010000000", "utc": "2026-11-24T13:21:20.010Z"},
             "frame": {"t": "1795526556980000000", "utc": "2026-11-24T13:21:19.980Z", "frame_in_second": 24}},
  "speed": 0.5,
  "live": false,
  "cue": {"in": {...}, "out": {...}, "end_action": "freeze"},
  "ramp": null,
  "client_ref": "clip-17",
  "mark": {"label": "goal", "at": {...}, "frame": {...}, "angles": [{"block": "cam1", "shown": {...}}]}
}
```

- `mark.at` without `at` in the command is the group time of the last tick the players pushed. A cue from `{"t": mark.at.t}` shows the same pictures.
- `mark.angles[].shown` is each angle's source frame, on the angle's own grid: the one it pushed, or with `at` the one that holds that point.

## Events (WebSocket)

```json
{"type": "event", "event_seq": 41, "event": "ended", "cause": "cue_end", "client_ref": "clip-17", "at": {...}}
{"type": "event", "event_seq": 42, "event": "live", "cause": "caught_up", "at": {...}}
{"type": "event", "event_seq": 43, "event": "range_changed", "cause": "retention", "range": {...}}
{"type": "event", "event_seq": 44, "event": "ready", "cause": "media", "ready": "ready"}
```

| `event` | Fields | When |
|---|---|---|
| `ended` | `at` | Bounded play reached its end with `freeze`. |
| `live` | `at` | The group went live (by `live`, an end action, catching up, the edge moving, or `start_mode: live`). |
| `range_changed` | `range` | The start or end of the media changed other than by the edge growing: retention removed the start, an angle came, went or was replaced by another (also by a source switch), a hole filled, the end or the live edge went back. The edge growing is in the state. |
| `ready` | `ready` | The group's readiness changed. |
| `source_changed` | `block`, `source` | Angle `block` shows its new source (cause `source`). |
| `source_failed` | `block`, `source`, `error` | Angle `block` gave up a switch and stays on its old source (cause `source`). |
| `item_started` | `at`, `waiting` | The queue started its next clip at `at` (its `in`); `client_ref` is the clip's, `waiting` the clips still queued (cause `cue_end`). |

- `event_seq` goes up by one per event of the group. A jump means a missed event.
- `cause` is the command that caused the event (`seek`, `play`, `live`, ...), or what happened by itself: `cue_end`, `media_end`, `caught_up`, `edge_moved`, `retention`, `media` (the media changed otherwise, or readiness changed), `start_mode`.
- `client_ref` is there when a command with a `client_ref` caused the event.

## State

`GET .../state` and the WebSocket's `{"type": "state"}` carry the same `ReplayState`. Unknown values are `null`, never `0`.

| Field | Meaning |
|---|---|
| `time_domain` | `tai`, `unanchored` or `unknown`. |
| `ready` | `indexing` (an index is not read), `warming` (read, but frames are not decoded in time yet), `ready`. The group is ready when every angle is; an angle is ready for the run once ten ticks in a row found their frame. |
| `grid` | `output` and `source`, each `{fps, frame_ns}` or `null`. |
| `jog_grid`, `follow_live` | As the properties and commands set them. |
| `live_margin` | `{ms, source}`: `computed` (the edge's step + 400 ms, clamped to 800–3000 ms) or `set` (a property, or `live {margin_ms}`). |
| `motion` | `cursor` (`at`, `frame`), `speed`, `live`, `cue`, `ramp` (`from`, `to`, `duration_ms`, `remaining_ms`). `null` until there is media. |
| `on_air` | The picture going out: `at`, `frame`, `behind_live_ms` (against `live_edge`, a sawtooth), `behind_live_estimate_ms` (against the estimate, smooth: the one to show), `age_ms` (only `tai`). `null` until a picture went out. |
| `range` | `start` (the earliest angle's start), `live_edge` (the end of what every angle has), `end` (the last any angle has; the cursor is clamped to `end − 1`), `live_edge_estimate` (the edge between the index's steps: smooth, updated every tick, never steps back, not even when `live_edge` does, e.g. an angle with less media comes; it then holds until the edge passes it), `growing`. Instants, not floored to frames. |
| `angles[]` | Per angle: `block`, `source` (without credentials), `pending_source` (the source it is switching to, else `null`), `ready`, `fps`, `frame_rate_missing`, `offset_ms`, `extent`, `available_from` (an instant: where the angle's buffer starts now, the first media the store still has; it moves later as the store deletes old media, and nothing before it can be shown; `null` until the extent is known), `shown` (the source frame going out, at its own time: a source that restarts can change its frame phase), `in_sync` (the last tick found the frame that holds the group time), `in_gap`, `gaps`. |
| `queue` | `clip_ref` (the `client_ref` of the clip the group holds as its cue, from `cue` or the queue, else `null`), `waiting` (clips queued). |

`gaps` are holes in the recording, each with `duration_ms`. A hole is reported once the index has listed it from the media before it to the media after it, so a long hole is one gap. Seams shorter than one frame period are not gaps; a single missing frame is. `in_gap` is the hole the group time is in: the angle shows the last frame before it.

### Diagnostics

`GET .../diagnostics` (and `{"type": "diagnostics"}` with `?diagnostics=1`) gives the engine's figures: the host clock, command-to-push latency, the live edge's step, age and when it last stepped, and per angle the decoder and its workers, decode times and the latest decodes, fetches, `gone_segments` (segments a fetch found deleted from the store by retention: not fetch errors, not retried), cache, misses, late ticks, the indexed ranges, and the bitstrip and burned-in time checks of test material.

## Player properties

Set in the flow, on each player. The group takes its value from its players as below.

| Property | Default | Group takes | Meaning |
|---|---|---|---|
| `live_margin_ms` | empty (computed) | the largest | Empty: the live edge's measured step + 400 ms, clamped to 800–3000 ms. Set: used as is. `live {margin_ms}` overrides it for the run (the built-in pages send one only when the operator picks a margin). The margin also governs `end_action: live`, `caught_up` and `edge_moved`. |
| `follow_live` | `true` | `false` if any is `false` | See the table above. |
| `jog_grid` | `source` | `source` if any is `source` | The grid `jog` steps on. |
| `start_mode` | `paused` | the first player's | `paused` (and `first`): paused at `range.start`. `live`: live as soon as the group is ready, unless a command came first. |
| `offset_ms` | `0` | per angle | Trim, within ±86400000 (a day): the angle's media moves this far on the group's axis, positive later. For cameras without a common time, or to adjust one. The times in the store do not change. |

## Examples

```sh
G=http://localhost:8080/api/replay/$FLOW/r1

# Replay the last 8 s at 50 %, in one step
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"seek","to":{"from_live_ms":8000},"speed":0.5}'

# Cue a clip by its stored times and play it at 50 %; it freezes on its last frame
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"cue","in":{"t":"1795526557000000000"},"out":{"t":"1795526563000000000"},"client_ref":"clip-17"}'
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"play","speed":0.5,"client_ref":"clip-17"}'

# Play to a point, then go live
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"play","until":{"from_live_ms":2000},"end_action":"live"}'

# Cue a clip from another camera on the group's only player, then play it
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"cue","in":{"t":"1795526557000000000"},"out":{"t":"1795526563000000000"},"source":"tams+https://gateway.example.com/flows/<cam2>","client_ref":"clip-18"}'

# A playlist: play clip 18, then clip 19 from camera 2 at half speed right after it
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"cue","in":{"t":"1795526557000000000"},"out":{"t":"1795526563000000000"},"client_ref":"clip-18"}'
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"play"}'
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"cue_next","in":{"t":"1795526570000000000"},"out":{"t":"1795526574000000000"},"speed":0.5,"source":"tams+https://gateway.example.com/flows/<cam2>","client_ref":"clip-19"}'

# Review mode: never go live by itself
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"follow_live","on":false}'

# Ease from 1x to 25 % over 800 ms
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"ramp","to":0.25,"duration_ms":800}'

# Mark what is on air, or 2 s ago
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"mark","label":"goal"}'
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"mark","at":{"rel_ms":-2000}}'
```

Over the WebSocket, send the same objects with a `seq`: `{"cmd":"jog","frames":-1,"seq":12}`.
