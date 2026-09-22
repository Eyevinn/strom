//! Adaptive audio bridge between two flows.
//!
//! `stromaudiobridgesink` in one flow appends audio to a named channel;
//! `stromaudiobridgesrc` in another flow plays it out at a low target latency.
//! It replaces `interaudiosink`/`interaudiosrc` where latency matters more than
//! keeping the producer's timeline, which is the conversation return: what
//! contributors hear of each other.
//!
//! `interaudiosrc` takes one period per clock tick, so a backlog left by a
//! network stall is never drained: it either plays on permanently delayed, or
//! the sink discards whole periods to get back under `buffer-time`, which is a
//! skip with a click at both edges. The bridge's reader instead takes more than
//! a period when behind and less when starved, and hands what it took to a
//! `scaletempo` inside the same bin, which plays it back in one period without
//! changing pitch. Downstream sees an ordinary rate-1.0 stream.
//!
//! ```text
//! sink flow:  … → stromaudiobridgesink ─┐
//!                                        │ Ring (named channel)
//! src flow:   stromaudiobridgesrc ◄──────┘
//!               reader → scaletempo → [restamp] → src
//! ```
//!
//! Time-scaled audio cannot be put back on the producer's timeline. The bridge
//! is for monitoring only: never feed its output to a program output or a
//! recording.
//!
//! [`ring`] and [`control`] know nothing about audio, so a video bridge can
//! reuse them with a reader that drops late frames instead of time-scaling.

pub mod channel;
pub mod control;
pub mod reader;
pub mod restamp;
pub mod ring;
pub mod sink;
pub mod src;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::OnceLock;

pub const SINK_FACTORY: &str = "stromaudiobridgesink";
pub const SRC_FACTORY: &str = "stromaudiobridgesrc";

/// The bridge carries one fixed format: what Opus decodes to, as float so the
/// reader's fades need no conversion. The blocks convert on the way in.
pub const RATE: u64 = 48_000;
pub const CHANNELS: u64 = 2;
/// Bytes per frame (all channels of one sample).
pub const BPF: usize = 4 * CHANNELS as usize;

/// Output time per reader tick.
pub const PERIOD_NS: u64 = 10_000_000;
/// Length of the fades at an underrun, a resume and a skip.
pub const FADE_FRAMES: usize = 240;
/// Most audio a channel holds. A reader that is not running lets the writer
/// fill it; beyond this the oldest audio is dropped so the writer never waits.
pub const RING_CAPACITY_NS: u64 = 10_000_000_000;

pub fn caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", RATE as i32)
        .field("channels", CHANNELS as i32)
        .field("layout", "interleaved")
        .field("channel-mask", gst::Bitmask::new(0x3))
        .build()
}

/// Register the bridge elements. Safe to call more than once.
pub fn register() -> Result<(), glib::BoolError> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            gst::Element::register(
                None,
                SINK_FACTORY,
                gst::Rank::NONE,
                sink::AudioBridgeSink::static_type(),
            )
            .and_then(|_| {
                gst::Element::register(
                    None,
                    SRC_FACTORY,
                    gst::Rank::NONE,
                    src::AudioBridgeSrc::static_type(),
                )
            })
            .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| glib::bool_error!("{}", e))
}
