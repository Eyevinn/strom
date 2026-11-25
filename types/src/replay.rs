//! Replay control API: the commands a replay group accepts and what they
//! return. One schema for the REST endpoint and the WebSocket.
//!
//! Times on the group's timeline are TAI nanoseconds (see [`crate::tai`]).
//! On the wire an instant is given as one of: a frame of the group's
//! reference grid (relative to the group origin), UTC (RFC 3339), TAI
//! (TAMS `"<s>:<ns>"`), or milliseconds before the live edge.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// A command for a replay group. JSON objects tagged by `cmd`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayCommand {
    /// Pause and move by a number of reference-grid frames.
    Jog { frames: i64 },
    /// Go to an instant: exactly one of `frame`, `utc`, `tai`,
    /// `from_live_ms`. Keeps the speed unless `speed` is given, in which case
    /// the seek and the speed change are one step (no frame in between).
    Seek {
        /// Reference frame, relative to the group origin.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<i64>,
        /// UTC, RFC 3339, e.g. `2026-11-24T13:21:56.080Z`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utc: Option<String>,
        /// TAI as TAMS `"<seconds>:<nanoseconds>"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tai: Option<String>,
        /// Milliseconds before the live edge (the last instant every angle
        /// has), e.g. 8000 to replay the last 8 s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_live_ms: Option<u64>,
        /// Speed from the new position, -8 .. 8; omitted keeps the speed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speed: Option<f64>,
    },
    /// Move by milliseconds, keeping the speed.
    SeekRel { ms: i64 },
    /// Play at a speed (negative = reverse, 0 = pause), -8 .. 8.
    Speed { speed: f64 },
    /// Play. Without `until`, a cue bounds the play (and a play at the cue's
    /// end starts again from its other end); with `until`, play runs to that
    /// instant (from the cue's start, if a cue holds the cursor).
    Play {
        /// Speed, -8 .. 8 except 0; default 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speed: Option<f64>,
        /// Where the play ends: ahead of the cursor in the direction of play.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<ReplayTimeRef>,
        /// What happens at the end; default the cue's, else `freeze`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_action: Option<ReplayEndAction>,
    },
    /// Cue a clip: hold `[in, out)` as the bound of play and park, paused, at
    /// `in`. Seeking or jogging outside the cue, `live` and `end` drop it.
    Cue {
        #[serde(rename = "in")]
        in_point: ReplayTimeRef,
        /// Exclusive: a freeze holds the last frame that starts before it.
        out: ReplayTimeRef,
        /// What happens when play reaches either end; default `freeze`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_action: Option<ReplayEndAction>,
    },
    /// Drop the cue; the cursor and speed stay.
    Uncue,
    /// Pause where the cursor is.
    Pause,
    /// Go to the live edge (the last instant every angle has) and pause.
    End,
    /// Play at 1x from the live edge minus a margin (default 1500 ms).
    Live {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        margin_ms: Option<u64>,
    },
}

/// What bounded play does when it reaches the end of its range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayEndAction {
    /// Pause on the last frame of the range.
    #[default]
    Freeze,
    /// Start again from the other end of the range.
    Loop,
    /// Go live: play at 1x behind the live edge (the `live` command's
    /// margin) and drop the cue.
    Live,
}

/// The bound of play the server holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayCue {
    #[serde(rename = "in")]
    pub in_point: ReplayInstant,
    /// Exclusive.
    pub out: ReplayInstant,
    pub end_action: ReplayEndAction,
}

/// Why the group went live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayLiveReason {
    /// Bounded play reached its end with `end_action: live`.
    EndAction,
    /// Forward play caught up with the live point (the live edge minus the
    /// `live` margin) and went on at 1x.
    CaughtUp,
}

/// Something the group did by itself, sent on the WebSocket as
/// `{"type": "event", "event": ..., ...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ReplayEvent {
    /// Bounded play reached the end of its range and froze there.
    Ended { at: ReplayInstant },
    /// The group went live: playing at 1x behind the live edge.
    Live {
        reason: ReplayLiveReason,
        at: ReplayInstant,
    },
}

/// A point on the group's timeline, as a command gives it: exactly one
/// field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ReplayTimeRef {
    /// Reference frame, relative to the group origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<i64>,
    /// UTC, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc: Option<String>,
    /// TAI as TAMS `"<seconds>:<nanoseconds>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tai: Option<String>,
    /// Milliseconds before the live edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_live_ms: Option<u64>,
}

/// An instant on the group's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayInstant {
    /// UTC, RFC 3339 with milliseconds.
    pub utc: String,
    /// TAI as TAMS `"<seconds>:<nanoseconds>"`.
    pub tai: String,
    /// TAI nanoseconds as a string (does not fit in a JavaScript number).
    pub tai_ns: String,
}

/// A frame on a grid: where it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayFrameTime {
    /// Frame relative to the group origin, on the same grid.
    pub frame: i64,
    /// UTC of the frame start, RFC 3339 with milliseconds.
    pub utc: String,
    /// TAI of the frame start, `"<seconds>:<nanoseconds>"`.
    pub tai: String,
    /// TAI nanoseconds of the frame start, as a string.
    pub tai_ns: String,
    /// The TAI as a calendar time (no zone), for display.
    pub tai_clock: String,
    /// Index of the frame among the frames that start in its UTC second.
    pub frame_in_second: i64,
}

/// What a command did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayCommandResult {
    /// The speed after the command.
    pub speed: f64,
    /// The cursor after the command.
    pub cursor_tai: ReplayInstant,
    /// The reference frame that holds the cursor, once the grid is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<i64>,
    /// That frame's time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<ReplayFrameTime>,
    /// The bound of play the server holds, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue: Option<ReplayCue>,
    /// Following the live edge (after `live`, or after catching up).
    pub live: bool,
}
