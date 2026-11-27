# Replay Control API

> Code is the source of truth — this may have drifted; read the code for the current implementation.

Media players in timeline mode (`mode = timeline`) that share a `group` form a **replay group**: one cursor and one speed for all angles. The group shares time, not frame numbers. The cursor is a TAI instant, and each angle shows its latest frame that starts at or before it. This page describes how to drive a group from outside: the commands, what they return, the events, and the state.

The built-in operator page `/player/replay/{flow_id}/{group}` drives the group with this API only. Around it, the page also:

- reads the flow (`GET /api/flows/{flow_id}`) to find the group's players and the vision mixer they feed;
- picks angles through the vision mixer API (`PUT .../blocks/{mixer}/preview`, `POST .../transition`, `GET .../state`);
- finds its monitors with `GET .../blocks/{mixer}/pgm-endpoint` and `multiview-endpoint`, and plays them with the shared WHEP client (`/static/whep.js`). Without a vision mixer it asks `GET .../blocks/{player}/whep-endpoint?pad=video_out` for each player's own WHEP output and shows one player at a time (keys 1–9 or the player buttons choose which);
- keeps marks and clips in the browser's `localStorage`, so they belong to that browser.

## Endpoints

| Method | Path | Body | Returns |
|---|---|---|---|
| `POST` | `/api/replay/{flow_id}/{group}/command` | a command | the command result |
| `GET` | `/api/replay/{flow_id}/{group}/state` | | the group state |
| `GET` (WebSocket) | `/api/replay/{flow_id}/{group}/ws` | commands | acks, errors, events, state every 50 ms |

A WebSocket ends with its group: it closes (code 1001) once the group's players are gone, when the flow stops or is deleted, or when a new run of the flow makes a new group. Reconnect to follow the next run. An open socket does not keep the group alive, also while a slow client holds up a send. A command that arrives after the group is gone is answered with `{"type": "error", "seq": ..., "code": "gone", ...}` before the close. Opening a socket on a group that does not exist returns 404 with a JSON error, like REST.

The schema is in the OpenAPI spec (`ReplayCommand`, `ReplayCommandResult`, `ReplayEvent`, under the `replay` tag).

REST and the WebSocket take the same commands and return the same result. On the WebSocket a command may carry a `seq`. It is echoed in the reply: `{"type": "ack", "seq": 7, ...result}` or `{"type": "error", "seq": 7, "code": "...", "error": "..."}`.

### Errors

| HTTP | WS `code` | When |
|---|---|---|
| 400 | `invalid` | Malformed command, unknown `cmd` or field, a value out of range (speed outside ±8, bad UTC/TAI text, more than one target, `out` before `in`). |
| 404 | `gone` | No replay group with that name in that flow (the flow is not running). On the WebSocket: the group ended while the socket was open. |
| 409 | `conflict` | The command is valid but does not apply now: no media yet, frame numbers before the frame rate is known or frame 0 is fixed, `until` behind the cursor, a cue outside the media, a mark before any picture has gone out. |

A refused command changes nothing.

## Naming an instant

Commands name a point on the timeline in one of four ways. Give exactly one.

| Field | Meaning | Example |
|---|---|---|
| `frame` | Frame of the group's reference grid (its finest source rate), counted from the group origin: frame 0 holds the start of the earliest angle, fixed once every angle has reported its range (or after 10 s, without an angle whose source has not answered). It then stays put for the run, so frame numbers do not depend on which source answered first. | `{"frame": 1250}` |
| `utc` | UTC, RFC 3339. Names the reference frame whose start is nearest (within half a frame), and goes to that start: frame times come in UTC to the millisecond, while a frame can start anywhere inside its millisecond, so a UTC the API gave seeks back to the same frame. | `{"utc": "2026-11-24T13:21:56.080Z"}` |
| `tai` | TAI as TAMS `"<seconds>:<nanoseconds>"`. Exact, to the nanosecond. | `{"tai": "1795526553:80598792"}` |
| `from_live_ms` | Milliseconds before the live edge (the last instant every angle has). | `{"from_live_ms": 8000}` |

`seek` takes these fields directly. `cue` and `play` take them as objects (`"in": {...}`, `"out": {...}`, `"until": {...}`).

## Commands

| `cmd` | Fields | What it does |
|---|---|---|
| `seek` | one target, `speed?` | Go to the instant. Keeps the speed, or sets `speed` in the same step. |
| `seek_rel` | `ms` | Move by milliseconds, keeping the speed. |
| `jog` | `frames` | Pause and move by reference frames. |
| `speed` | `speed` | Play at a speed, −8 to 8 (negative is reverse, 0 pauses). |
| `ramp` | `to`, `duration_ms` | Change the speed linearly from the speed now to `to` over `duration_ms` (at most 60000). The server runs it. |
| `play` | `speed?`, `until?`, `end_action?` | Play (default 1x). A held cue bounds it. `until` bounds a play without a cue. |
| `pause` | | Pause where the cursor is. |
| `cue` | `in`, `out`, `end_action?` | Hold `[in, out)` as the bound of play and park, paused, at `in`. |
| `uncue` | | Drop the cue. The cursor and speed stay. |
| `live` | `margin_ms?` | Play at 1x from the live edge minus the margin (default 1500 ms). |
| `end` | | Go to the live edge and pause. |
| `mark` | `label?` | Stamp the picture going out now. Changes nothing. |

### Cue and bounded play

- `out` is exclusive. `freeze` holds the last frame that starts before `out` (or `in` when playing in reverse).
- `end_action` is one of:
  - `freeze` (default): pause at the end and send an `ended` event;
  - `loop`: wrap to the other end;
  - `live`: go live at the `live` margin, drop the cue, and send a `live` event.
- `play` at the end of a cue starts again from its other end, so pressing play after a clip has frozen plays it again.
- `speed`, `ramp`, `pause` and moves inside the cue keep it. A seek or jog outside the cue drops it, and so do `live`, `end` and `uncue`.
- `play {until}` without a cue plays from the cursor to `until`. With a cue that holds the cursor, it plays from the cue's `in` to `until`.

### Going live by itself

Forward play without a cue that started behind the live point (the live edge minus the `live` margin) and reaches it goes on from there at 1x. It follows the edge and sends a `live` event with reason `caught_up`. It does not jump back and does not stop. Slow motion started inside the margin stays slow motion.

A group that follows live keeps to the margin. If the live point moves ahead of the cursor by more than one step of the edge (plus 250 ms), the group jumps forward to it and sends a `live` event with reason `edge_moved`. This happens when `live` came while an angle's index still lagged, for example right after a start, when one angle's index still ends where the previous run ended.

### Ramps

A ramp keeps the cue, so a ramp into `out` still freezes there. Any other command that sets the speed ends the ramp where it is.

## Command result

```json
{
  "speed": 0.5,
  "live": false,
  "cursor_tai": {"utc": "2026-11-24T13:22:00.000Z", "tai": "1795526557:0", "tai_ns": "1795526557000000000"},
  "frame": 195,
  "time": {"frame": 195, "utc": "2026-11-24T13:21:59.980Z", "tai": "...", "tai_ns": "...", "tai_clock": "...", "frame_in_second": 49},
  "cue": {"in": {...}, "out": {...}, "end_action": "freeze"},
  "mark": {...}
}
```

- `frame` and `time` give the reference frame that holds the cursor, once the frame rate is known.
- `cue` is the bound the server holds, if any.
- `mark` is only present for `mark`:
  - `cursor_tai`: the group time of the last tick the players pushed;
  - `time`: the reference frame that holds it;
  - `angles`: each player's last pushed source frame (`block`, `shown`).

`tai_ns` is always a string, because it does not fit in a JavaScript number.

## Events (WebSocket)

Things the group does by itself, as `{"type": "event", "event": ..., ...}`:

| `event` | Fields | When |
|---|---|---|
| `ended` | `at` | Bounded play reached its end with `freeze`. |
| `live` | `reason` (`end_action`, `caught_up` or `edge_moved`), `at` | The group went live by itself. |

A socket that falls behind (more than 64 events unread) gets `{"type": "lagged", "missed": n}` in place of the events it lost. Read the state to catch up.

## State

`GET .../state` and the WebSocket's `{"type": "state"}` carry the same object. The main fields:

| Field | Meaning |
|---|---|
| `speed`, `live`, `cue`, `ramp` | Motion now. `ramp` is present while a ramp runs (`from`, `to`, `duration_ms`, `remaining_ms`). |
| `cursor`, `frame` | The cursor's reference frame. |
| `pushed` | The picture going out: frame time, `behind_live_ms`, `behind_live_estimate_ms`, and `age_ms` when the source has real TAI. |
| `start`, `end`, `live_edge` | The group's range. The live edge is where every angle has media. |
| `live_edge_estimate`, `live_growing`, `behind_live_estimate_ms` | The live edge between index steps, estimated from when the steps arrived. It equals `live_edge` when the media does not grow. |
| `reference_fps`, `frame_ns`, `time_domain` | Grid and time base (`tai`, `unanchored`, `unknown`). |
| `players[]` | Per angle: `block`, `fps`, `output_fps`, `extent`, `indexed`, `gaps`, `shown` (the source frame going out, at its own capture time: a source that restarts can change its frame phase, and each frame keeps its own time), `in_sync` (the last tick found the frame that holds the group time), `in_gap` (see below), and counters. |

`indexed` and `gaps` describe the recording as far as the player's index has seen it:

- `indexed`: the stretches of the source the index has listed so far. It lists around the cursor (and follows a hole to its other end), so this grows as the cursor moves. It says what the player knows, not where the media is.
- `gaps`: holes in the recording, between two pieces of media, each with `duration_ms`. A hole is reported once the index has listed it from the media before it to the media after it, however many listings that took, so a long hole is one gap, never pieces of it. Seams shorter than one frame period are not gaps; a single missing frame is.
- `in_gap`: the hole the group time is in, if any (`start`, `end`, `duration_ms`). An angle shows the last frame before the hole there, whether play ran into it or a seek landed in it; `shown` is that frame. Until the hole's end has been listed, `end` is as far as the index has looked.

## Examples

```sh
G=http://localhost:8080/api/replay/$FLOW/r1

# Replay the last 8 s at 50 %, in one step
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"seek","from_live_ms":8000,"speed":0.5}'

# Cue a clip and play it at 50 %; it freezes on its last frame
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"cue","in":{"utc":"2026-11-24T13:22:00Z"},"out":{"utc":"2026-11-24T13:22:06Z"}}'
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"play","speed":0.5}'

# Play to an instant, then go live
curl -XPOST $G/command -H 'content-type: application/json' \
  -d '{"cmd":"play","until":{"from_live_ms":2000},"end_action":"live"}'

# Ease from 1x to 25 % over 800 ms
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"ramp","to":0.25,"duration_ms":800}'

# Mark what is on air
curl -XPOST $G/command -H 'content-type: application/json' -d '{"cmd":"mark","label":"goal"}'
```

Over the WebSocket, send the same objects with a `seq`: `{"cmd":"jog","frames":-1,"seq":12}`.
