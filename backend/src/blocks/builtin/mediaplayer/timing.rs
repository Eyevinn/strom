//! When each buffer leaves the internal pipeline, and what it is stamped with
//! in the main one.
//!
//! The internal pipeline runs on the flow's clock and base time (see
//! `MediaPlayerState::follow_main_clock`), so a running time means the same
//! in both pipelines and nothing needs converting.
//!
//! A file starts at running time zero, but a live stream does not: SVT's HLS
//! carries the broadcaster's own timeline, over a thousand hours in. Paced
//! against its raw running time, a clocksync would wait that long, and an
//! unpaced stream arrives in bursts, a segment at a time. So the first buffer
//! sets one clocksync `ts-offset` for every stream of the player: it leaves a
//! playout delay from now, and the rest keep their spacing from it. Each
//! buffer is stamped with the running time its clocksync let it go at.
//!
//! A stall longer than the delay would otherwise leave every later buffer
//! late for good. A buffer that far late re-syncs instead: the offset moves
//! on by the lateness plus the delay, which skips the gap in the output and
//! refills the delay.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tracing::{debug, info};

/// Not set yet; taken from the next buffer.
const UNSET: i64 = i64::MIN;

/// How late a paced buffer may reach the main pipeline before it re-syncs.
/// A busy CPU makes a buffer tens of ms late now and then, and the mixers'
/// latency covers that; a re-sync skips the delay's worth of output, so it
/// is kept for a source that has run dry.
pub const LATE_RESYNC_NS: i64 = 250_000_000;

/// Default playout delay.
pub const DEFAULT_PLAYOUT_DELAY_MS: u64 = 500;

/// Largest playout delay the block accepts. The data it holds back waits in
/// the source's own queues, which keep a few seconds.
pub const MAX_PLAYOUT_DELAY_MS: u64 = 5_000;

/// How far ahead of its time on air a stinger clip's frame leaves its
/// clocksync. A frame let go exactly when due still has to cross the bridge,
/// be uploaded and reach the mixer within the mixer's latency, which a loaded
/// machine misses now and then; a few frames' head start absorbs that while
/// keeping the frames held ahead small enough for the bridge's queues at 4K.
pub const STINGER_RELEASE_AHEAD_MS: u64 = 60;

/// Shared by every stream of one player, so audio and video stay together.
pub struct Timing {
    /// For unpaced buffers (sync off): what a buffer is stamped with is its
    /// running time plus this, taken from the first one's arrival.
    map_offset: AtomicI64,
    /// The offset from a buffer's running time to its time in the main
    /// pipeline. The clocksyncs pace with it, less `release_ahead`.
    sync_offset: AtomicI64,
    /// Playout delay in ns.
    delay: i64,
    /// When set, the next baseline puts the first buffer at this main-pipeline
    /// running time instead of a playout delay from now. A stinger take knows
    /// when its clip goes on air before the clip starts, so the mixer can be
    /// programmed for that frame up front.
    start_at: AtomicI64,
    /// How far ahead of its time each buffer leaves the clocksync (ns). Zero
    /// for an ordinary player.
    release_ahead: i64,
}

/// Where a buffer goes in the main pipeline.
#[derive(Debug, PartialEq, Eq)]
pub struct Placement {
    /// Its main-pipeline running time.
    pub running_time: i64,
    /// Set when it arrived this late (ns) and the baseline moved; the
    /// clocksyncs then need the new `sync_offset`.
    pub resynced_after: Option<i64>,
}

impl Timing {
    pub fn new(playout_delay_ms: u64) -> Self {
        Self {
            map_offset: AtomicI64::new(UNSET),
            sync_offset: AtomicI64::new(UNSET),
            delay: (playout_delay_ms.min(MAX_PLAYOUT_DELAY_MS) * 1_000_000) as i64,
            start_at: AtomicI64::new(UNSET),
            release_ahead: 0,
        }
    }

    /// Timing for a stinger clip source: frames leave their clocksync
    /// `release_ahead_ms` before they are due, and a take pins where the
    /// first one goes ([`Self::pin_start`]).
    pub fn for_stinger(release_ahead_ms: u64) -> Self {
        Self {
            release_ahead: (release_ahead_ms * 1_000_000) as i64,
            ..Self::new(0)
        }
    }

    /// Put the clip's first video frame at main-pipeline running time
    /// `running_time`. Call after [`Self::reset`].
    ///
    /// With `first_video_rt`, the first video frame's own running time, the
    /// baseline is set here, from video: audio and video share it, and a
    /// clip whose audio starts before or after its video would otherwise
    /// land its first video frame off the pin whenever its audio reached the
    /// bridge first. The audio keeps its offset from the video. Without it,
    /// the first buffer to reach [`Self::place`] takes the pin.
    pub fn pin_start(&self, running_time: i64, first_video_rt: Option<i64>) {
        match first_video_rt {
            Some(rt) => self.sync_offset.store(running_time - rt, Ordering::Release),
            None => self.start_at.store(running_time, Ordering::Release),
        }
    }

    /// The `ts-offset` the clocksyncs pace with, once a baseline is set.
    pub fn clocksync_offset(&self) -> Option<i64> {
        self.sync_offset().map(|offset| offset - self.release_ahead)
    }

    /// How far ahead of its time a buffer leaves its clocksync, in ns.
    pub fn release_ahead(&self) -> i64 {
        self.release_ahead
    }

    pub fn delay_ms(&self) -> i64 {
        self.delay / 1_000_000
    }

    pub fn sync_offset(&self) -> Option<i64> {
        Some(self.sync_offset.load(Ordering::Acquire)).filter(|&v| v != UNSET)
    }

    /// Start over from the next buffer: a new file, a seek, a resume.
    /// `internal` gets its clocksyncs ready to take the new baseline.
    pub fn reset(
        self: &Arc<Self>,
        internal: Option<&gst::Pipeline>,
        main: &gst::glib::WeakRef<gst::Pipeline>,
    ) {
        self.map_offset.store(UNSET, Ordering::Release);
        self.sync_offset.store(UNSET, Ordering::Release);
        self.start_at.store(UNSET, Ordering::Release);
        if let Some(pipeline) = internal {
            for clocksync in clocksyncs(pipeline) {
                arm(&clocksync, self, main);
            }
        }
    }

    /// The clocksync offset for a stream whose first buffer has running time
    /// `rt` when the flow is at `now`: it leaves `delay` from now.
    /// Whichever stream gets here first sets it for all of them.
    fn take_sync_offset(&self, rt: i64, now: i64) -> i64 {
        let pinned = self.start_at.load(Ordering::Acquire);
        let proposed = if pinned != UNSET {
            pinned - rt
        } else {
            now - rt + self.delay
        };
        match self.sync_offset.compare_exchange(
            UNSET,
            proposed,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.start_at.store(UNSET, Ordering::Release);
                proposed
            }
            Err(current) => current,
        }
    }

    /// Place a buffer with running time `rt`, reaching the bridge when the
    /// main pipeline is at `now`.
    ///
    /// A `paced` buffer left its clocksync at `rt + sync_offset`, which is
    /// its time in the main pipeline. One more than [`LATE_RESYNC_NS`] late
    /// found the source dry: the offset moves on by the lateness plus the
    /// delay, for every stream, so the next buffers wait out the delay again.
    ///
    /// Unpaced buffers - sync off - keep the spacing of their running times
    /// from the first one's arrival.
    pub fn place(&self, rt: i64, now: i64, paced: bool) -> Placement {
        let mut sync = self.sync_offset.load(Ordering::Acquire);
        // A pinned start takes its baseline from the first buffer to get
        // here. That is the stinger clip's parked first frame: it was already
        // waiting inside its clocksync when the take armed them, so no
        // clocksync probe sees it.
        if paced && sync == UNSET && self.start_at.load(Ordering::Acquire) != UNSET {
            sync = self.take_sync_offset(rt, now);
        }
        if !paced || sync == UNSET {
            let map = match self.map_offset.compare_exchange(
                UNSET,
                now - rt,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => now - rt,
                Err(current) => current,
            };
            return Placement {
                running_time: rt + map,
                resynced_after: None,
            };
        }
        let due = rt + sync;
        let lateness = now - due;
        if lateness <= LATE_RESYNC_NS {
            return Placement {
                running_time: due,
                resynced_after: None,
            };
        }
        // The stream that loses the race takes the winner's step.
        let step = lateness + self.delay;
        match self.sync_offset.compare_exchange(
            sync,
            sync + step,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Placement {
                running_time: due + step,
                resynced_after: Some(lateness),
            },
            Err(current) => Placement {
                running_time: rt + current,
                resynced_after: None,
            },
        }
    }
}

/// The clocksyncs that pace the bridged streams.
pub fn clocksyncs(pipeline: &gst::Pipeline) -> Vec<gst::Element> {
    pipeline
        .iterate_elements()
        .into_iter()
        .flatten()
        .filter(|e| e.factory().is_some_and(|f| f.name() == "clocksync"))
        .collect()
}

/// Set the shared clocksync offset on every clocksync of `pipeline`: the
/// timing's offset less its release-ahead ([`Timing::clocksync_offset`]).
pub fn apply_sync_offset(pipeline: &gst::Pipeline, offset: i64) {
    for clocksync in clocksyncs(pipeline) {
        clocksync.set_property("ts-offset", offset);
    }
}

/// Have `clocksync` take the shared offset from its next buffer, before it
/// paces that buffer. The probe removes itself, so it sees one buffer. It
/// holds the `Timing`, which holds no elements, and a weak ref to the flow's
/// pipeline, so there is no cycle.
pub fn arm(
    clocksync: &gst::Element,
    timing: &Arc<Timing>,
    main: &gst::glib::WeakRef<gst::Pipeline>,
) {
    let Some(pad) = clocksync.static_pad("sink") else {
        return;
    };
    let timing = Arc::clone(timing);
    let main = main.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(clocksync) = pad.parent_element() else {
            return gst::PadProbeReturn::Remove;
        };
        let rt = pad
            .sticky_event::<gst::event::Segment>(0)
            .and_then(|ev| {
                ev.segment()
                    .downcast_ref::<gst::ClockTime>()
                    .and_then(|s| s.to_running_time(buffer.pts()))
            })
            .or(buffer.pts());
        // An unstamped buffer is not paced; wait for one that is.
        let Some(rt) = rt else {
            return gst::PadProbeReturn::Ok;
        };
        // The flow's running time; the internal pipeline shares it. Without
        // it there is no offset to take, and a buffer let through on the old
        // one could wait out a live stream's whole timeline - SVT's is over a
        // thousand hours. Drop it and take the offset from the next.
        let Some(now) = main
            .upgrade()
            .and_then(|p| p.current_running_time())
            .map(|t| t.nseconds() as i64)
        else {
            return gst::PadProbeReturn::Drop;
        };
        let offset = timing.take_sync_offset(rt.nseconds() as i64, now);
        let ts_offset = offset - timing.release_ahead;
        clocksync.set_property("ts-offset", ts_offset);
        debug!(
            "Media Player: {} paces from running time {} with ts-offset {}",
            clocksync.name(),
            rt,
            ts_offset
        );
        gst::PadProbeReturn::Remove
    });
}

/// Log a re-sync. Rare: once per stall.
pub fn log_resync(instance: &str, media_type: &str, lateness: i64, timing: &Timing) {
    info!(
        "Media Player {}: {} arrived {} ms late, re-synced with {} ms playout delay",
        instance,
        media_type,
        lateness / 1_000_000,
        timing.delay_ms()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn the_shared_clocksync_offset_holds_the_first_buffer_for_the_delay() {
        let t = Timing::new(500);
        // A live stream: running time over a thousand hours in.
        let rt = 3_668_182 * 1_000 * MS;
        assert_eq!(
            t.take_sync_offset(rt, 2_000 * MS),
            2_000 * MS - rt + 500 * MS
        );
        // The second stream takes the same offset, whatever its own timing.
        assert_eq!(
            t.take_sync_offset(rt + 30 * MS, 2_010 * MS),
            2_000 * MS - rt + 500 * MS
        );
    }

    #[test]
    fn a_paced_buffer_is_stamped_with_the_time_its_clocksync_let_it_go() {
        let t = Timing::new(500);
        let rt = 3_668_182 * 1_000 * MS;
        let sync = t.take_sync_offset(rt, 7_000 * MS);
        let p = t.place(rt + 40 * MS, 7_540 * MS, true);
        assert_eq!(p.running_time, rt + 40 * MS + sync);
        assert_eq!(p.running_time, 7_540 * MS);
        assert_eq!(p.resynced_after, None);
    }

    /// Before, a stall left every later buffer late by the stall, so the
    /// mixers dropped them or played them in bursts. A late buffer has to
    /// move the offset, and the next one has to be on time again.
    #[test]
    fn a_late_buffer_moves_the_offset_and_the_next_is_on_time() {
        let t = Timing::new(500);
        t.take_sync_offset(0, 0); // due at rt + 500 ms
        assert_eq!(t.place(0, 500 * MS, true).running_time, 500 * MS);

        // 40 ms of content arrives 300 ms after it was due.
        let late = t.place(40 * MS, 840 * MS, true);
        assert_eq!(late.resynced_after, Some(300 * MS));
        // It goes out the delay ahead.
        assert_eq!(late.running_time, 840 * MS + 500 * MS);
        assert_eq!(t.sync_offset(), Some(500 * MS + 800 * MS));

        // The clocksync now lets the next one go on the new schedule.
        let next = t.place(80 * MS, 1_380 * MS, true);
        assert_eq!(next.resynced_after, None);
        assert_eq!(next.running_time, 1_380 * MS);
    }

    /// A busy CPU makes buffers tens of ms late; re-syncing on that skipped
    /// half a second of output each time.
    #[test]
    fn a_little_late_is_left_alone() {
        let t = Timing::new(500);
        t.take_sync_offset(0, 0);
        for late in [44 * MS, 64 * MS, LATE_RESYNC_NS] {
            let p = t.place(40 * MS, 540 * MS + late, true);
            assert_eq!(p.resynced_after, None, "{} ms", late / MS);
            assert_eq!(p.running_time, 540 * MS);
        }
    }

    #[test]
    fn unpaced_buffers_keep_their_spacing_and_never_resync() {
        let t = Timing::new(500);
        t.take_sync_offset(0, 0);
        assert_eq!(t.place(0, 100 * MS, false).running_time, 100 * MS);
        let p = t.place(40 * MS, 5_000 * MS, false);
        assert_eq!(p.resynced_after, None);
        assert_eq!(p.running_time, 140 * MS);
    }

    /// The offset comes from the flow's running time. When the flow has none
    /// yet, the probe used to remove itself without setting one, so the
    /// clocksync paced a live stream against its raw timeline and held the
    /// first buffer for a thousand hours. It has to wait for a buffer it can
    /// place, and place that one.
    #[test]
    fn arm_waits_for_the_flow_to_run_before_taking_the_offset() {
        let _ = gst::init();
        let internal = gst::Pipeline::new();
        let src = gstreamer_app::AppSrc::builder()
            .format(gst::Format::Time)
            .caps(&gst::Caps::builder("audio/x-raw").build())
            .build();
        let clocksync = gst::ElementFactory::make("clocksync")
            .property("sync", false)
            .build()
            .unwrap();
        let sink = gstreamer_app::AppSink::builder().sync(false).build();
        internal
            .add_many([src.upcast_ref(), &clocksync, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([src.upcast_ref(), &clocksync, sink.upcast_ref()]).unwrap();

        // The flow exists but is not playing: no running time.
        let main = gst::Pipeline::new();
        let main_weak = main.downgrade();
        let timing = Arc::new(Timing::new(500));
        arm(&clocksync, &timing, &main_weak);
        internal.set_state(gst::State::Playing).unwrap();

        let rt = 3_668_182 * 1_000 * MS;
        let push = |n: i64| {
            let mut buf = gst::Buffer::new();
            buf.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_nseconds((rt + n * 20 * MS) as u64));
            src.push_buffer(buf).unwrap();
            sink.try_pull_sample(gst::ClockTime::from_mseconds(500))
        };

        assert!(
            push(0).is_none(),
            "a buffer with no offset to pace it went through"
        );
        assert_eq!(timing.sync_offset(), None);

        main.set_state(gst::State::Playing).unwrap();
        let _ = main.state(gst::ClockTime::from_seconds(5));
        assert!(push(1).is_some());
        let offset = timing
            .sync_offset()
            .expect("the next buffer set the offset");
        assert_eq!(clocksync.property::<i64>("ts-offset"), offset);
        assert!(
            offset < -rt / 2,
            "paced against the raw timeline: {}",
            offset
        );

        let _ = internal.set_state(gst::State::Null);
        let _ = main.set_state(gst::State::Null);
    }

    /// A stinger take programs the mixer for the frame its clip lands on
    /// before the clip starts, so the first buffer has to land exactly there,
    /// whenever it happens to arrive.
    #[test]
    fn a_pinned_start_puts_the_first_buffer_where_the_take_said() {
        let t = Arc::new(Timing::for_stinger(60));
        t.pin_start(5_000 * MS, None);
        // The parked first frame reaches the bridge early, unpaced.
        let first = t.place(0, 4_870 * MS, true);
        assert_eq!(first.running_time, 5_000 * MS);
        assert_eq!(first.resynced_after, None);
        // The next frames keep their spacing from it.
        assert_eq!(t.place(40 * MS, 4_985 * MS, true).running_time, 5_040 * MS);
        // The clocksyncs let each one go 60 ms early.
        assert_eq!(t.clocksync_offset(), Some(5_000 * MS - 60 * MS));
        // The pin is used up: the next baseline is the ordinary one.
        t.reset(None, &gst::glib::WeakRef::new());
        assert_eq!(t.place(0, 9_000 * MS, true).running_time, 9_000 * MS);
    }

    /// Audio and video share one baseline. A clip whose audio starts 100 ms
    /// before its video, with the audio reaching the bridge first, used to
    /// take the pin from the audio, so the first video frame aired 100 ms
    /// after the frame the mixer was programmed for.
    #[test]
    fn a_pinned_start_lands_the_first_video_frame_when_audio_comes_first() {
        let t = Arc::new(Timing::for_stinger(60));
        t.pin_start(5_000 * MS, Some(100 * MS));
        let audio = t.place(0, 4_870 * MS, true);
        assert_eq!(audio.running_time, 4_900 * MS, "audio keeps its offset");
        let video = t.place(100 * MS, 4_871 * MS, true);
        assert_eq!(video.running_time, 5_000 * MS, "video lands on the pin");
        assert_eq!(t.clocksync_offset(), Some(4_900 * MS - 60 * MS));

        // Audio starting after the video, placed first, the same.
        t.reset(None, &gst::glib::WeakRef::new());
        t.pin_start(9_000 * MS, Some(0));
        assert_eq!(t.place(80 * MS, 8_900 * MS, true).running_time, 9_080 * MS);
        assert_eq!(t.place(0, 8_901 * MS, true).running_time, 9_000 * MS);
    }

    #[test]
    fn an_ordinary_player_paces_with_its_offset() {
        let t = Timing::new(500);
        t.take_sync_offset(0, 1_000 * MS);
        assert_eq!(t.clocksync_offset(), t.sync_offset());
    }

    #[test]
    fn reset_takes_a_new_baseline() {
        let t = Arc::new(Timing::new(500));
        t.take_sync_offset(0, 0);
        t.place(0, 1_000 * MS, false);
        t.reset(None, &gst::glib::WeakRef::new());
        assert_eq!(t.sync_offset(), None);
        assert_eq!(t.place(0, 9_000 * MS, false).running_time, 9_000 * MS);
    }
}
