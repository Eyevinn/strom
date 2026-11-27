//! Replay control API, version 2: the commands a replay group accepts, what
//! they return, the group state, the events and every WebSocket message. One
//! schema for the REST endpoints and the WebSocket.
//!
//! Time on the wire is `t`: integer nanoseconds on the group's time axis, as
//! a decimal string (it does not fit in a JavaScript number). The group says
//! once what the axis is ([`ReplayTimeDomain`]); `utc` is only given when it
//! is real TAI. There are no absolute frame numbers: positions are `t`,
//! lengths milliseconds, and frames only relative steps (`jog`).

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Version of the replay API, in [`ReplayIndex::api_version`].
pub const REPLAY_API_VERSION: u32 = 2;

/// Optional capabilities of this server's replay API, in
/// [`ReplayIndex::features`].
pub const REPLAY_FEATURES: &[&str] = &[
    "follow_live",
    "client_ref",
    "mark_at",
    "offset_ms",
    "jog_grid",
    "start_mode",
    "diagnostics",
    "source_switch",
];

/// Longest `client_ref` a command may carry.
pub const MAX_CLIENT_REF_LEN: usize = 64;

/// Longest `label` a `mark` may carry, in characters.
pub const MAX_MARK_LABEL_LEN: usize = 256;

/// Largest margin a `live {margin_ms}` may set.
pub const MAX_LIVE_MARGIN_SET_MS: u64 = 60_000;

/// Lowest and highest `state_ms` for the WebSocket's state rate.
pub const MIN_STATE_MS: u64 = 50;
pub const MAX_STATE_MS: u64 = 1000;

// ---------------------------------------------------------------------------
// Time

/// What `t` on the group's time axis means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayTimeDomain {
    /// At least one angle has not read its index yet.
    #[default]
    Unknown,
    /// TAI nanoseconds since 1970-01-01 TAI (the TAMS timestamp as one
    /// number): `utc` is given.
    Tai,
    /// Nanoseconds synced between the angles, but no real time: `utc` is
    /// `null`.
    Unanchored,
}

/// An exact point on the group's time axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayInstant {
    /// Nanoseconds on the group's time axis, as a decimal string.
    #[cfg_attr(feature = "openapi", schema(example = "1795526557010000000"))]
    pub t: String,
    /// RFC 3339 with milliseconds; `null` unless the time domain is `tai`.
    pub utc: Option<String>,
}

/// A frame on a grid: where it starts, and its index in its UTC second (the
/// `FF` of a timecode `HH:MM:SS:FF`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayFrame {
    /// Start of the frame on the group's time axis, decimal string.
    pub t: String,
    /// UTC of the frame start; `null` unless the time domain is `tai`.
    pub utc: Option<String>,
    /// Index of the frame among the frames of its grid that start in the
    /// same UTC second (the same second of the axis when not `tai`).
    pub frame_in_second: i64,
}

/// An exact instant and the output-grid frame that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayPosition {
    pub at: ReplayInstant,
    /// The frame of the output grid that holds `at`; `null` until the grid
    /// is known.
    pub frame: Option<ReplayFrame>,
}

/// A frame rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayGridRate {
    /// `N/D`, e.g. `50/1` or `30000/1001`.
    pub fps: String,
    /// Frame period in ns (fractional for NTSC rates).
    pub frame_ns: f64,
}

/// The group's two grids.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayGrid {
    /// The output's frame rate (`output_framerate`): timecodes and
    /// [`ReplayFrame`]s are on it.
    pub output: Option<ReplayGridRate>,
    /// The finest frame rate among the sources.
    pub source: Option<ReplayGridRate>,
}

/// Which grid `jog` steps on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayJogGrid {
    /// The finest source grid: slow motion shows every source frame.
    #[default]
    Source,
    /// The output grid.
    Output,
}

/// A point in a command: an object with exactly one field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ReplayTarget {
    /// Group time, exact (decimal string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    /// RFC 3339; lands on the source frame whose start is nearest (within
    /// half a frame). 409 unless the time domain is `tai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc: Option<String>,
    /// Milliseconds before the live edge; `0` is the last frame every angle
    /// has (`range.live_edge - 1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_live_ms: Option<u64>,
    /// Milliseconds from the cursor when the command arrives, negative is
    /// back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rel_ms: Option<i64>,
}

impl ReplayTarget {
    /// The exact group time `t`.
    pub fn t(t: i64) -> Self {
        Self {
            t: Some(t.to_string()),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Commands

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
    /// Go live: play at 1x behind the live edge and drop the cue.
    Live,
}

// Commands without fields are empty struct variants, not unit variants:
// serde lets an internally tagged unit variant ignore extra fields even under
// `deny_unknown_fields`, so `{"cmd":"pause","x":1}` would pass.
/// A command for a replay group, tagged by `cmd`. On the wire it may also
/// carry `client_ref` (see [`ReplayRequest`]) and, on the WebSocket, `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayCommand {
    /// Go to a point (clamped to the media). Keeps the speed (during a ramp,
    /// the ramp's speed now), or sets `speed` in the same step.
    Seek {
        to: ReplayTarget,
        /// -8 .. 8; omitted keeps the speed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speed: Option<f64>,
    },
    /// Pause and move a number of frames on the group's `jog_grid`; the
    /// cursor rests in the middle of the frame.
    Jog { frames: i64 },
    /// Play at a speed where the cursor is, -8 .. 8; 0 pauses.
    Speed { speed: f64 },
    /// Play. With a cue: from its other end when the cursor is within half a
    /// frame of the end play runs to. `until` sets a new cue
    /// `[min(cue.in, cursor), until)`.
    Play {
        /// -8 .. 8 except 0; default 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speed: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<ReplayTarget>,
        /// Needs `until` or a cue.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_action: Option<ReplayEndAction>,
    },
    /// Speed 0 where the cursor is.
    Pause {},
    /// Change the speed linearly to `to` over `duration_ms` (0 .. 60000),
    /// run by the server.
    Ramp { to: f64, duration_ms: u64 },
    /// Hold `[in, out)` and park, paused, on `in`. With `source`, the angle
    /// `block` switches to that source too (see [`ReplayCommand::Source`]).
    Cue {
        #[serde(rename = "in")]
        in_point: ReplayTarget,
        /// Exclusive.
        out: ReplayTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_action: Option<ReplayEndAction>,
        /// A source URI for angle `block` to switch to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// The angle that switches; may be left out when the group has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block: Option<String>,
    },
    /// Switch angle `block` to another source in the same store, without a
    /// rebuild. The angle holds its last picture until the new source has
    /// the frame for the cursor, then shows it. Needs a group on real TAI.
    /// The motion does not change.
    Source {
        /// A source URI, as the player's `source` property takes it.
        source: String,
        /// The angle that switches; may be left out when the group has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block: Option<String>,
    },
    /// Drop the cue. The cursor, the speed and a running ramp stay.
    Uncue {},
    /// Play at 1x from the live edge minus the margin. `margin_ms` sets the
    /// margin for the rest of the run.
    Live {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        margin_ms: Option<u64>,
    },
    /// Go to the last frame every angle has, pause, drop the cue.
    End {},
    /// Stamp the picture going out now, or the point `at`. Changes nothing.
    Mark {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<ReplayTarget>,
    },
    /// Whether the group goes live by itself when forward play reaches the
    /// live point.
    FollowLive { on: bool },
}

impl ReplayCommand {
    /// The `cmd` name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Seek { .. } => "seek",
            Self::Jog { .. } => "jog",
            Self::Speed { .. } => "speed",
            Self::Play { .. } => "play",
            Self::Pause {} => "pause",
            Self::Ramp { .. } => "ramp",
            Self::Cue { .. } => "cue",
            Self::Uncue {} => "uncue",
            Self::Live { .. } => "live",
            Self::End {} => "end",
            Self::Mark { .. } => "mark",
            Self::FollowLive { .. } => "follow_live",
            Self::Source { .. } => "source",
        }
    }

    /// The event `cause` of what the command does.
    pub fn cause(&self) -> ReplayCause {
        match self {
            Self::Seek { .. } => ReplayCause::Seek,
            Self::Jog { .. } => ReplayCause::Jog,
            Self::Speed { .. } => ReplayCause::Speed,
            Self::Play { .. } => ReplayCause::Play,
            Self::Pause {} => ReplayCause::Pause,
            Self::Ramp { .. } => ReplayCause::Ramp,
            Self::Cue { .. } => ReplayCause::Cue,
            Self::Uncue {} => ReplayCause::Uncue,
            Self::Live { .. } => ReplayCause::Live,
            Self::End {} => ReplayCause::End,
            Self::Mark { .. } => ReplayCause::Mark,
            Self::FollowLive { .. } => ReplayCause::FollowLive,
            Self::Source { .. } => ReplayCause::Source,
        }
    }
}

/// A command as sent: the command and an optional `client_ref`, echoed in
/// the response and in the events the command causes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayRequest {
    #[serde(flatten)]
    pub command: ReplayCommand,
    /// At most 64 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
}

// ---------------------------------------------------------------------------
// Motion, state

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

/// A speed ramp the server is running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayRampState {
    pub from: f64,
    pub to: f64,
    pub duration_ms: u64,
    pub remaining_ms: u64,
}

/// How the group moves: cursor, speed, live, cue, ramp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayMotion {
    pub cursor: ReplayPosition,
    pub speed: f64,
    /// Following the live edge.
    pub live: bool,
    pub cue: Option<ReplayCue>,
    pub ramp: Option<ReplayRampState>,
}

/// Readiness of an angle or of the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayReady {
    /// The index has not been read.
    #[default]
    Indexing,
    /// The index is read, but frames are not decoded in time yet.
    Warming,
    /// The group is ready when every angle is.
    Ready,
}

/// Where the live margin comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayLiveMarginSource {
    /// The live edge's measured step + 400 ms, clamped to 800 .. 3000 ms.
    Computed,
    /// The `live_margin_ms` property, or a `live {margin_ms}` command.
    Set,
}

/// The distance behind the live edge the group plays at when live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayLiveMargin {
    pub ms: u64,
    pub source: ReplayLiveMarginSource,
}

/// The picture going out now: what the players last pushed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayOnAir {
    pub at: ReplayInstant,
    pub frame: Option<ReplayFrame>,
    /// Against `range.live_edge`, which steps a segment at a time: a
    /// sawtooth.
    pub behind_live_ms: i64,
    /// Against `range.live_edge_estimate`: smooth. The one to show.
    pub behind_live_estimate_ms: i64,
    /// Wall clock minus capture time; only when the domain is `tai`.
    pub age_ms: Option<i64>,
}

/// The group's media.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayRange {
    /// The earliest start among the angles.
    pub start: ReplayInstant,
    /// The end of the last media every angle has.
    pub live_edge: ReplayInstant,
    /// The last media any angle has; the cursor is clamped to `end - 1`.
    pub end: ReplayInstant,
    /// The live edge now, estimated between the index's steps: smooth, never
    /// steps back, updated every tick.
    pub live_edge_estimate: ReplayInstant,
    /// The live edge has stepped recently.
    pub growing: bool,
}

/// `[start, end)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayExtent {
    pub start: ReplayInstant,
    pub end: ReplayInstant,
}

/// `[start, end)` and its length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplaySpan {
    pub start: ReplayInstant,
    pub end: ReplayInstant,
    pub duration_ms: i64,
}

/// One angle (player) of the group, for the operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayAngleState {
    /// The player block's id.
    pub block: String,
    /// The source the angle shows, without credentials.
    pub source: Option<String>,
    /// The source the angle is switching to, until it shows it or gives up.
    pub pending_source: Option<String>,
    pub ready: ReplayReady,
    /// The source's frame rate, `N/D`.
    pub fps: Option<String>,
    /// No frame rate is known (index, caps, timestamps): nothing of this
    /// angle can be placed on the time axis.
    pub frame_rate_missing: bool,
    /// Trim: the angle's media is moved this far on the group's axis.
    pub offset_ms: i64,
    /// On the group's axis.
    pub extent: Option<ReplayExtent>,
    /// The source frame last pushed, on the angle's own grid.
    pub shown: Option<ReplayFrame>,
    /// The last tick found the frame that holds its time.
    pub in_sync: bool,
    /// The hole of the recording the last tick's time was in.
    pub in_gap: Option<ReplaySpan>,
    pub gaps: Vec<ReplaySpan>,
}

/// The state of a replay group. Unknown values are `null`, never `0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayState {
    pub time_domain: ReplayTimeDomain,
    pub ready: ReplayReady,
    pub grid: ReplayGrid,
    pub jog_grid: ReplayJogGrid,
    pub follow_live: bool,
    pub live_margin: ReplayLiveMargin,
    /// `null` until the group has media.
    pub motion: Option<ReplayMotion>,
    /// `null` until a picture has gone out.
    pub on_air: Option<ReplayOnAir>,
    /// `null` until the group has media.
    pub range: Option<ReplayRange>,
    pub angles: Vec<ReplayAngleState>,
}

// ---------------------------------------------------------------------------
// Diagnostics

/// One segment decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayDecodeRecord {
    pub segment: ReplaySpan,
    pub ms: u64,
    pub frames: usize,
    pub ok: bool,
    pub error: Option<String>,
}

/// One angle's engine counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayAngleDiagnostics {
    pub block: String,
    pub output_fps: Option<String>,
    /// The time the last tick asked this angle for.
    pub wanted: Option<ReplayInstant>,
    /// What the index has listed, on the group's axis.
    pub indexed: Vec<ReplaySpan>,
    /// Frame number read from the pushed picture's bitstrip, -1 if none.
    pub strip_frame: i64,
    pub strip_mismatches: u64,
    /// Burned-in UTC (µs) of the last new pushed picture, -1 if none.
    pub code_utc_us: i64,
    pub time_checked: u64,
    pub time_errors: u64,
    pub repeated_ticks: u64,
    pub skipped_frames: u64,
    pub misses: u64,
    pub late_ticks: u64,
    pub index_segments: u64,
    pub fetched_segments: u64,
    pub fetches: u64,
    pub fetch_errors: u64,
    pub last_fetch_ms: u64,
    pub cached_segments: u64,
    pub decodes: u64,
    pub last_decode_ms: u64,
    pub decode_failures: u64,
    pub decode_ms_avg: Option<f64>,
    pub decode_ms_max: u64,
    pub decoder: Option<String>,
    pub decode_workers: Option<usize>,
    pub recent_decodes: Vec<ReplayDecodeRecord>,
}

/// Engine diagnostics of a replay group, for measuring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayDiagnostics {
    /// The host's wall clock.
    pub now_utc: String,
    /// From the last paused command to the first push of its target.
    pub cmd_to_push_ms: f64,
    /// Median step of the live edge, once it has stepped twice.
    pub live_edge_step_ms: Option<i64>,
    pub live_edge_seen_ms_ago: Option<i64>,
    /// Wall clock minus the capture time of the live edge; only `tai`.
    pub live_edge_age_ms: Option<i64>,
    pub angles: Vec<ReplayAngleDiagnostics>,
}

// ---------------------------------------------------------------------------
// Responses, events, discovery

/// One angle's picture in a mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayMarkAngle {
    pub block: String,
    /// The angle's source frame, on its own grid.
    pub shown: ReplayFrame,
}

/// A stamped point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayMark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Without `at`: the group time of the last pushed tick. A cue from
    /// `{"t": at.t}` shows the same pictures.
    pub at: ReplayInstant,
    pub frame: Option<ReplayFrame>,
    /// Without `at`: what each angle pushed. With `at`: the frame each angle
    /// holds there.
    pub angles: Vec<ReplayMarkAngle>,
}

/// What a command did: the group's motion after it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayCommandResult {
    #[serde(flatten)]
    pub motion: ReplayMotion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
    /// For `mark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<ReplayMark>,
}

/// Why an event came: the command that caused it, or something that happened
/// by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayCause {
    Seek,
    Jog,
    Speed,
    Play,
    Pause,
    Ramp,
    Cue,
    Uncue,
    Live,
    End,
    Mark,
    FollowLive,
    Source,
    /// Bounded play reached the end of its cue.
    CueEnd,
    /// Forward play reached the live point (with `follow_live`).
    CaughtUp,
    /// Following live, the live edge moved ahead by more than its steps
    /// explain; the group jumped to the live point.
    EdgeMoved,
    /// Bounded play reached the end of media that no longer grows.
    MediaEnd,
    /// Retention removed the start of the media.
    Retention,
    /// The media changed otherwise: an angle came or went, a hole filled, an
    /// index was read, decoding caught up.
    Media,
    /// `start_mode: live` took the group live when it became ready.
    StartMode,
}

/// What an event says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ReplayEventKind {
    /// Bounded play reached the end of its range and froze there.
    Ended { at: ReplayInstant },
    /// The group went live.
    Live { at: ReplayInstant },
    /// The media's start or end changed other than by the edge growing.
    RangeChanged { range: ReplayRange },
    /// The group's readiness changed.
    Ready { ready: ReplayReady },
    /// Angle `block` shows its new source (`cue {source}` or `source`).
    SourceChanged {
        block: String,
        /// The source URI without credentials.
        source: Option<String>,
    },
    /// Angle `block` did not switch to `source` and stays on its old one.
    SourceFailed {
        block: String,
        source: Option<String>,
        error: String,
    },
}

/// Something the group did, sent on the WebSocket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayEvent {
    /// One more than the group's previous event: a jump means a missed one.
    pub event_seq: u64,
    pub cause: ReplayCause,
    /// The `client_ref` of the command that caused the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
    #[serde(flatten)]
    pub kind: ReplayEventKind,
}

/// The WebSocket's error codes (and REST's, in the error details).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReplayErrorCode {
    /// HTTP 400.
    Invalid,
    /// HTTP 409.
    Conflict,
    /// HTTP 409: the range a cue, `play {until}` or `mark {at}` names is
    /// outside the media.
    OutOfRange,
}

impl ReplayErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Conflict => "conflict",
            Self::OutOfRange => "out_of_range",
        }
    }
}

/// Every message the replay WebSocket sends, tagged by `type`.
// Built once per message and serialized at once: the size of a variant does
// not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplayWsMessage {
    /// The group state, every 50 ms (or `?state_ms=`).
    State(ReplayState),
    /// Something the group did.
    Event(ReplayEvent),
    /// The socket fell behind and dropped `missed` events: read the state.
    Lagged { missed: u64 },
    /// Engine diagnostics, with each state when the socket asked
    /// (`?diagnostics=1`).
    Diagnostics(ReplayDiagnostics),
    /// A command applied.
    Ack(ReplayAck),
    /// A command refused; nothing changed.
    Error(ReplayWsError),
}

/// The reply to a command on the WebSocket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayAck {
    /// The command's `seq`, as sent.
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub seq: serde_json::Value,
    #[serde(flatten)]
    pub result: ReplayCommandResult,
}

/// A refused command on the WebSocket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayWsError {
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub seq: serde_json::Value,
    pub code: ReplayErrorCode,
    pub error: String,
    /// The command's `client_ref`, when it parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
}

/// An angle of a group, for discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayAngleInfo {
    /// The player block's id.
    pub block: String,
    /// The block's name in the flow, if it has one.
    pub name: Option<String>,
    /// The source's own id: the TAMS flow id, or the directory name.
    pub source_id: Option<String>,
    /// The source URI without credentials.
    pub source: Option<String>,
}

/// A replay group, for discovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayGroupInfo {
    pub flow_id: String,
    pub group: String,
    pub time_domain: ReplayTimeDomain,
    pub grid: ReplayGrid,
    pub ready: ReplayReady,
    pub angles: Vec<ReplayAngleInfo>,
    /// The store the angles play from, without credentials (e.g.
    /// `tams+https://gateway.example.com`), when they share one.
    pub source: Option<String>,
}

/// The replay groups of the server (or of a flow).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ReplayIndex {
    /// [`REPLAY_API_VERSION`].
    pub api_version: u32,
    /// [`REPLAY_FEATURES`].
    pub features: Vec<String>,
    pub groups: Vec<ReplayGroupInfo>,
}

impl ReplayIndex {
    pub fn new(groups: Vec<ReplayGroupInfo>) -> Self {
        Self {
            api_version: REPLAY_API_VERSION,
            features: REPLAY_FEATURES.iter().map(|f| f.to_string()).collect(),
            groups,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn commands_without_fields_refuse_unknown_fields() {
        for cmd in ["pause", "uncue", "end"] {
            let ok: ReplayCommand = serde_json::from_value(json!({ "cmd": cmd })).unwrap();
            assert_eq!(serde_json::to_value(&ok).unwrap(), json!({ "cmd": cmd }));
            let bad = serde_json::from_value::<ReplayCommand>(json!({ "cmd": cmd, "frames": 3 }));
            assert!(bad.is_err(), "{} took an unknown field", cmd);
        }
        assert!(serde_json::from_value::<ReplayCommand>(
            json!({ "cmd": "jog", "frames": 1, "x": 1 })
        )
        .is_err());
    }

    #[test]
    fn v1_commands_and_targets_are_gone() {
        for bad in [
            json!({"cmd": "seek_rel", "ms": 100}),
            json!({"cmd": "seek", "frame": 3}),
            json!({"cmd": "seek", "to": {"tai": "1:0"}}),
            json!({"cmd": "seek", "to": {"frame": 3}}),
        ] {
            assert!(
                serde_json::from_value::<ReplayCommand>(bad.clone()).is_err(),
                "{}",
                bad
            );
        }
    }

    #[test]
    fn commands_round_trip() {
        for c in [
            json!({"cmd": "seek", "to": {"t": "1795526557010000000"}, "speed": 0.5}),
            json!({"cmd": "seek", "to": {"from_live_ms": 8000}}),
            json!({"cmd": "seek", "to": {"rel_ms": -2000}}),
            json!({"cmd": "cue", "in": {"t": "1"}, "out": {"utc": "2026-11-24T13:21:20.010Z"},
                   "end_action": "loop"}),
            json!({"cmd": "play", "until": {"rel_ms": 3000}, "end_action": "live"}),
            json!({"cmd": "mark", "label": "goal", "at": {"t": "5"}}),
            json!({"cmd": "follow_live", "on": false}),
            json!({"cmd": "live", "margin_ms": 900}),
            json!({"cmd": "ramp", "to": 0.25, "duration_ms": 800}),
            json!({"cmd": "cue", "in": {"t": "1"}, "out": {"t": "2"},
                   "source": "tams+https://gw.example.com/flows/b", "block": "p1"}),
            json!({"cmd": "source", "source": "cam2"}),
            json!({"cmd": "source", "source": "cam2", "block": "p1"}),
        ] {
            let parsed: ReplayCommand = serde_json::from_value(c.clone()).unwrap();
            assert_eq!(serde_json::to_value(&parsed).unwrap(), c);
            assert_eq!(c["cmd"], parsed.name());
        }
    }

    fn instant(t: &str) -> ReplayInstant {
        ReplayInstant {
            t: t.into(),
            utc: None,
        }
    }

    #[test]
    fn events_and_messages_are_tagged() {
        let ev = ReplayEvent {
            event_seq: 41,
            cause: ReplayCause::CueEnd,
            client_ref: Some("clip-17".into()),
            kind: ReplayEventKind::Ended { at: instant("7") },
        };
        let v = serde_json::to_value(ReplayWsMessage::Event(ev.clone())).unwrap();
        assert_eq!(
            v,
            json!({"type": "event", "event_seq": 41, "event": "ended", "cause": "cue_end",
                   "client_ref": "clip-17", "at": {"t": "7", "utc": null}})
        );
        let back: ReplayWsMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, ReplayWsMessage::Event(ev));
        assert_eq!(
            serde_json::to_value(ReplayWsMessage::Lagged { missed: 3 }).unwrap(),
            json!({"type": "lagged", "missed": 3})
        );
        let ready = ReplayEvent {
            event_seq: 44,
            cause: ReplayCause::Media,
            client_ref: None,
            kind: ReplayEventKind::Ready {
                ready: ReplayReady::Ready,
            },
        };
        assert_eq!(
            serde_json::to_value(ReplayWsMessage::Event(ready)).unwrap(),
            json!({"type": "event", "event_seq": 44, "event": "ready", "cause": "media",
                   "ready": "ready"})
        );
    }

    #[test]
    fn source_events_are_tagged() {
        let ev = ReplayEvent {
            event_seq: 7,
            cause: ReplayCause::Source,
            client_ref: Some("c1".into()),
            kind: ReplayEventKind::SourceChanged {
                block: "p1".into(),
                source: Some("cam2".into()),
            },
        };
        assert_eq!(
            serde_json::to_value(ReplayWsMessage::Event(ev)).unwrap(),
            json!({"type": "event", "event_seq": 7, "event": "source_changed", "cause": "source",
                   "client_ref": "c1", "block": "p1", "source": "cam2"})
        );
        let failed = ReplayEventKind::SourceFailed {
            block: "p1".into(),
            source: None,
            error: "x".into(),
        };
        assert_eq!(
            serde_json::to_value(failed).unwrap(),
            json!({"event": "source_failed", "block": "p1", "source": null, "error": "x"})
        );
        // A source command needs its source.
        assert!(serde_json::from_value::<ReplayCommand>(json!({"cmd": "source"})).is_err());
    }

    #[test]
    fn an_ack_is_the_motion_with_seq_and_client_ref() {
        let motion = ReplayMotion {
            cursor: ReplayPosition {
                at: instant("10"),
                frame: None,
            },
            speed: 1.0,
            live: false,
            cue: None,
            ramp: None,
        };
        let ack = ReplayWsMessage::Ack(ReplayAck {
            seq: json!(7),
            result: ReplayCommandResult {
                motion,
                client_ref: Some("a".into()),
                mark: None,
            },
        });
        let v = serde_json::to_value(&ack).unwrap();
        assert_eq!(v["type"], "ack");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["speed"], 1.0);
        assert_eq!(v["cursor"]["at"]["t"], "10");
        assert_eq!(v["client_ref"], "a");
        assert_eq!(serde_json::from_value::<ReplayWsMessage>(v).unwrap(), ack);
    }

    #[test]
    fn a_request_carries_client_ref_beside_the_command() {
        let r = ReplayRequest {
            command: ReplayCommand::Pause {},
            client_ref: Some("x".into()),
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"cmd": "pause", "client_ref": "x"})
        );
    }
}
