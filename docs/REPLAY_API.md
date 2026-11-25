# Replay Control API

> Code is the source of truth — this may have drifted; read the code for the current implementation.

Media players in timeline mode (`mode = timeline`) that share a `group` form a **replay group**: one cursor and one speed for all angles. The group shares time, not frame numbers. The cursor is a TAI instant, and each angle shows its latest frame that starts at or before it. This page describes how to drive a group from outside: the commands, what they return, the events, and the state.

The built-in operator page `/player/replay/{flow_id}/{group}` uses only this API.

## Endpoints

| Method | Path | Body | Returns |
|---|---|---|---|
| `POST` | `/api/replay/{flow_id}/{group}/command` | a command | the command result |
| `GET` | `/api/replay/{flow_id}/{group}/state` | | the group state |
| `GET` (WebSocket) | `/api/replay/{flow_id}/{group}/ws` | commands | acks, errors, events, state every 50 ms |

The schema is in the OpenAPI spec (`ReplayCommand`, `ReplayCommandResult`, `ReplayEvent`, under the `replay` tag).

REST and the WebSocket take the same commands and return the same result. On the WebSocket a command may carry a `seq`. It is echoed in the reply: `{"type": "ack", "seq": 7, ...result}` or `{"type": "error", "seq": 7, "code": "...", "error": "..."}`.

### Errors

| HTTP | WS `code` | When |
|---|---|---|
| 400 | `invalid` | Malformed command, unknown `cmd` or field, a value out of range (speed outside ±8, bad UTC/TAI text, more than one target, `out` before `in`). |
| 404 | | No replay group with that name in that flow (the flow is not running). |
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
| `live` | `reason` (`end_action` or `caught_up`), `at` | The group went live by itself. |

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
| `players[]` | Per angle: `block`, `fps`, `output_fps`, `extent`, `indexed` (ranges its index has listed), `gaps` (holes inside them, with `duration_ms`), `shown` (the source frame going out, at its own capture time: a source that restarts can change its frame phase, and each frame keeps its own time), `in_sync` (the last tick found the frame that holds the group time), and counters. |

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
