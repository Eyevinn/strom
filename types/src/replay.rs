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
    /// Play at 1x.
    Play,
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
}
