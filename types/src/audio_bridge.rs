//! Defaults and statistics for the adaptive audio bridge
//! (`builtin.audio_bridge_output` → `builtin.audio_bridge_input`).
//!
//! Single source of truth shared by the backend and the frontend.

use crate::stats::{StatMetadata, StatValue, Statistic};
use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

pub const OUTPUT_BLOCK_ID: &str = "builtin.audio_bridge_output";
pub const INPUT_BLOCK_ID: &str = "builtin.audio_bridge_input";

/// Default target latency: the backlog the reader keeps in hand.
pub const DEFAULT_TARGET_LATENCY_MS: u64 = 40;
/// Lowest accepted target latency. A contributor's audio arrives one Opus
/// frame at a time, so the backlog drops by a frame between arrivals. A target
/// of one frame has nothing left to absorb that drop and runs dry even on an
/// unimpaired link; measured on a clean local producer, 20 ms underran on every
/// run and 40 ms on none. 40 is also the lowest target the rig covered.
pub const MIN_TARGET_LATENCY_MS: u64 = 40;
/// Highest accepted target latency.
pub const MAX_TARGET_LATENCY_MS: u64 = 1000;

/// Default bound on how far the playback rate may move from 1.0, in percent.
/// At 5 % a 400 ms backlog drains in about 8 s, at 10 % in about 4 s. A faster
/// drain spends the surplus a link's hiccups leave behind before the next one
/// arrives: on bursty packet loss 10 % lost about a quarter more audio than
/// 5 %. The catch-up is also inaudible on speech at 5 % and at most very
/// slightly noticeable at 10 %.
pub const DEFAULT_MAX_RATE_CHANGE_PERCENT: f64 = 5.0;
/// Upper bound on the rate bound.
pub const MAX_RATE_CHANGE_PERCENT: f64 = 20.0;

/// Default backlog above which the reader stops draining and skips back to
/// the target instead.
pub const DEFAULT_MAX_LATENCY_MS: u64 = 1000;
/// Highest accepted maximum latency. A skip needs the backlog floor above the
/// maximum latency, and the bridge holds 10 s of audio, so a maximum at or near
/// that capacity can never be exceeded: a producer faster than real time would
/// then be neither skipped nor reported.
pub const MAX_MAX_LATENCY_MS: u64 = 5000;

/// Statistics of one `builtin.audio_bridge_input` block.
///
/// The input side measures how the producer delivers (stalls), the output side
/// what the listener got (gaps, time-scaling, skips). All durations are
/// milliseconds of audio; counters count since this block started, on both
/// sides, so restarting the flow that feeds the channel does not reset them
/// but restarting this one does.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AudioBridgeStats {
    pub target_latency_ms: u64,
    /// Audio held in the bridge right now.
    pub depth_ms: f64,
    /// Lowest backlog over the control window — what the controller steers.
    pub floor_ms: f64,
    /// Highest backlog seen.
    pub max_depth_ms: f64,
    /// Current playback rate: above 1.0 drains, below 1.0 stretches.
    pub rate: f64,
    /// Times the bridge ran dry and played silence while a producer flow was
    /// running and had delivered audio. Silence after that flow stops, or
    /// before a new one's first audio, is not counted; silence from a running
    /// flow whose source has gone quiet is.
    pub underruns: u64,
    /// Total silence played after running dry, while a producer flow was
    /// running.
    pub underrun_ms: f64,
    /// Longest single stretch of silence after running dry, ending where the
    /// producer flow stopped if it did.
    pub longest_underrun_ms: f64,
    /// Audio removed by playing fast.
    pub drained_ms: f64,
    /// Audio added by playing slow.
    pub stretched_ms: f64,
    /// Wall-clock time spent at a rate other than 1.0.
    pub time_scaled_ms: f64,
    /// Times the backlog exceeded the maximum latency and was skipped.
    pub skips: u64,
    /// Audio discarded by those skips.
    pub skipped_ms: f64,
    /// Gaps between input buffers, beyond their own length, of at least 50,
    /// 100, 200 and 400 ms. Each gap counts in its largest bucket only.
    pub input_gaps_50ms: u64,
    pub input_gaps_100ms: u64,
    pub input_gaps_200ms: u64,
    pub input_gaps_400ms: u64,
    /// Longest gap between input buffers, beyond their own length.
    pub longest_input_gap_ms: f64,
    /// Audio dropped at the input because nothing was reading it.
    pub overflow_ms: f64,
    /// An Audio Bridge Output on this channel is running. False when the
    /// producer flow is stopped, or when no Output uses this channel name.
    pub producer_attached: bool,
    /// The producer is delivering faster than the bridge can play even at its
    /// maximum rate, typically because it is fed by something that is not
    /// live, such as a file, so the output keeps skipping. Reported only while
    /// a producer is attached. It clears at once when another producer takes
    /// its place, after 5 s with no audio, or once it has kept pace for 30 s.
    pub producer_overrun: bool,
}

impl AudioBridgeStats {
    /// Convert to generic statistics.
    pub fn to_statistics(&self) -> Vec<Statistic> {
        fn stat(id: &str, value: StatValue, name: &str, desc: &str, unit: &str) -> Statistic {
            Statistic {
                id: id.to_string(),
                value,
                metadata: StatMetadata {
                    display_name: name.to_string(),
                    description: desc.to_string(),
                    unit: Some(unit.to_string()),
                    category: Some("Audio Bridge".to_string()),
                },
            }
        }
        let ms = |v: f64| StatValue::Float((v * 10.0).round() / 10.0);
        vec![
            stat(
                "target_latency_ms",
                StatValue::Counter(self.target_latency_ms),
                "Target Latency",
                "Backlog the bridge steers towards",
                "ms",
            ),
            stat(
                "depth_ms",
                ms(self.depth_ms),
                "Backlog",
                "Audio held in the bridge now",
                "ms",
            ),
            stat(
                "floor_ms",
                ms(self.floor_ms),
                "Backlog Floor",
                "Lowest backlog over the control window",
                "ms",
            ),
            stat(
                "max_depth_ms",
                ms(self.max_depth_ms),
                "Peak Backlog",
                "Highest backlog seen",
                "ms",
            ),
            stat(
                "rate",
                StatValue::Float(self.rate),
                "Playback Rate",
                "Above 1 drains the backlog, below 1 stretches it",
                "x",
            ),
            stat(
                "underruns",
                StatValue::Counter(self.underruns),
                "Underruns",
                "Times the bridge ran dry and played silence while its producer flow was running",
                "events",
            ),
            stat(
                "underrun_ms",
                ms(self.underrun_ms),
                "Underrun Silence",
                "Total silence played after running dry, not counting time with no producer flow running",
                "ms",
            ),
            stat(
                "longest_underrun_ms",
                ms(self.longest_underrun_ms),
                "Longest Underrun",
                "Longest single silence after running dry, not counting time with no producer flow running",
                "ms",
            ),
            stat(
                "drained_ms",
                ms(self.drained_ms),
                "Drained",
                "Audio removed by playing fast",
                "ms",
            ),
            stat(
                "stretched_ms",
                ms(self.stretched_ms),
                "Stretched",
                "Audio added by playing slow",
                "ms",
            ),
            stat(
                "time_scaled_ms",
                ms(self.time_scaled_ms),
                "Time Scaled",
                "Time spent playing at a rate other than 1",
                "ms",
            ),
            stat(
                "skips",
                StatValue::Counter(self.skips),
                "Skips",
                "Times the backlog passed the maximum latency and was skipped",
                "events",
            ),
            stat(
                "skipped_ms",
                ms(self.skipped_ms),
                "Skipped",
                "Audio discarded by skips",
                "ms",
            ),
            stat(
                "input_gaps_50ms",
                StatValue::Counter(self.input_gaps_50ms),
                "Input Gaps 50-100 ms",
                "Input stalls of 50 to 100 ms",
                "events",
            ),
            stat(
                "input_gaps_100ms",
                StatValue::Counter(self.input_gaps_100ms),
                "Input Gaps 100-200 ms",
                "Input stalls of 100 to 200 ms",
                "events",
            ),
            stat(
                "input_gaps_200ms",
                StatValue::Counter(self.input_gaps_200ms),
                "Input Gaps 200-400 ms",
                "Input stalls of 200 to 400 ms",
                "events",
            ),
            stat(
                "input_gaps_400ms",
                StatValue::Counter(self.input_gaps_400ms),
                "Input Gaps 400+ ms",
                "Input stalls of 400 ms or more",
                "events",
            ),
            stat(
                "longest_input_gap_ms",
                ms(self.longest_input_gap_ms),
                "Longest Input Gap",
                "Longest stall in the input",
                "ms",
            ),
            stat(
                "overflow_ms",
                ms(self.overflow_ms),
                "Input Overflow",
                "Audio dropped because nothing was reading",
                "ms",
            ),
            stat(
                "producer_attached",
                StatValue::Bool(self.producer_attached),
                "Producer Attached",
                "An Audio Bridge Output on this channel is running; false when its flow is \
                 stopped or no Output uses this channel name",
                "",
            ),
            stat(
                "producer_overrun",
                StatValue::Bool(self.producer_overrun),
                "Producer Overrun",
                "The input delivers faster than the bridge can play, typically because it \
                 is fed by something that is not live, such as a file, so the output keeps \
                 skipping",
                "",
            ),
        ]
    }
}
