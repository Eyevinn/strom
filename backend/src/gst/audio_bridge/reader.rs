//! The clock-paced reader inside `stromaudiobridgesrc`.
//!
//! Every [`PERIOD_NS`] of pipeline clock it asks the [`Controller`] what to do
//! with the channel's backlog, takes `rate` periods of audio, and declares that
//! rate in its segment. `scaletempo`, next in the bin, reads the rate from the
//! segment and plays the audio back in one period, keeping pitch.
//!
//! The segment has to come from the element that owns the source pad: a rate
//! change pushed through `appsrc` never reaches `scaletempo`.

use super::channel::{self, Channel};
use super::control::{Config, Controller, Decision, MIN_SKIP_HEADROOM_NS, WINDOW_NS};
use super::{BPF, FADE_FRAMES, PERIOD_NS, RATE};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use gstreamer_base::subclass::base_src::CreateSuccess;
use gstreamer_base::subclass::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use strom_types::audio_bridge::{
    AudioBridgeStats, DEFAULT_MAX_LATENCY_MS, DEFAULT_MAX_RATE_CHANGE_PERCENT,
    DEFAULT_TARGET_LATENCY_MS,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "stromaudiobridgesrc",
        gst::DebugColorFlags::empty(),
        Some("Strom adaptive audio bridge source"),
    )
});

/// The rate declared when the controller says 1.0. At exactly 1.0
/// `scaletempo` switches to passthrough, and switching back and forth strands
/// the ~50 ms it has queued: every return to 1.0 was a click (18 in 10 s of
/// toggling, none when held just above 1.0). The reader takes this much more
/// audio per period, so the offset costs no drift.
pub const UNITY_RATE: f64 = 1.0 + 1e-6;

const PERIOD_FRAMES: f64 = (PERIOD_NS * RATE) as f64 / 1e9;

pub fn frames_to_ns(frames: u64) -> u64 {
    (frames as u128 * 1_000_000_000 / RATE as u128) as u64
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub channel: String,
    pub target_latency_ms: u64,
    pub max_rate_change_percent: f64,
    pub max_latency_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            channel: String::new(),
            target_latency_ms: DEFAULT_TARGET_LATENCY_MS,
            max_rate_change_percent: DEFAULT_MAX_RATE_CHANGE_PERCENT,
            max_latency_ms: DEFAULT_MAX_LATENCY_MS,
        }
    }
}

impl Settings {
    fn control(&self) -> Config {
        let target_ns = self.target_latency_ms * 1_000_000;
        Config {
            target_ns,
            max_rate_change: self.max_rate_change_percent / 100.0,
            // A skip threshold at or just above the target leaves no band for
            // the controller to drain in, so every backlog past the target is
            // discarded instead — the behaviour the bridge exists to replace.
            max_latency_ns: (self.max_latency_ms * 1_000_000).max(target_ns + MIN_SKIP_HEADROOM_NS),
            period_ns: PERIOD_NS,
        }
    }
}

/// Output timeline, shared with the restamp probe on the bin's source pad.
/// `scaletempo` derives its output timestamps from its input segment, which
/// jumps by its own latency at every rate change; the probe stamps output by
/// sample count from the reader's first running time instead.
#[derive(Default)]
pub struct Restamp {
    pub base_rt: AtomicU64,
    pub frames_out: AtomicU64,
    pub segment_sent: AtomicBool,
}

impl Restamp {
    fn reset(&self) {
        self.base_rt.store(0, Ordering::Relaxed);
        self.frames_out.store(0, Ordering::Relaxed);
        self.segment_sent.store(false, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Accounting {
    max_depth_ns: u64,
    underruns: u64,
    underrun_ns: u64,
    longest_underrun_ns: u64,
    /// Silence since the last underrun, while it lasts.
    current_underrun_ns: Option<u64>,
    drained_ns: u64,
    stretched_ns: u64,
    time_scaled_ns: u64,
    skips: u64,
    skipped_ns: u64,
    /// Set while the producer overrun has already been logged.
    overrun_reported: bool,
}

impl Accounting {
    fn end_underrun(&mut self) {
        if let Some(run) = self.current_underrun_ns.take() {
            self.underrun_ns += run;
            self.longest_underrun_ns = self.longest_underrun_ns.max(run);
        }
    }
}

struct State {
    channel: Arc<Channel>,
    /// The channel's writer generation last seen, to notice a new producer.
    writer_generation: u64,
    ctl: Controller,
    started: bool,
    /// Current segment: stream time `start` plays at running time `base`, and
    /// `frames` of audio have been pushed in it at `rate`.
    seg_start: u64,
    seg_base: u64,
    seg_rate: f64,
    seg_frames: u64,
    /// Fraction of a frame owed from earlier periods.
    carry: f64,
    depth_ns: u64,
    /// The last output frame, for an underrun that has nothing left to fade.
    last_frame: [u8; BPF],
    acct: Accounting,
}

impl State {
    /// Running time at which the next period starts.
    fn next_running_time(&self) -> u64 {
        self.seg_base + (frames_to_ns(self.seg_frames) as f64 / self.seg_rate) as u64
    }
}

#[derive(Default)]
struct ClockWait {
    clock_id: Option<gst::SingleShotClockId>,
    flushing: bool,
}

glib::wrapper! {
    pub struct AudioBridgeReader(ObjectSubclass<imp::AudioBridgeReader>)
        @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl AudioBridgeReader {
    pub fn settings(&self) -> Settings {
        self.imp().settings.lock().unwrap().clone()
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        f(&mut self.imp().settings.lock().unwrap());
    }

    pub fn restamp(&self) -> Arc<Restamp> {
        self.imp().restamp.clone()
    }

    pub fn stats(&self) -> AudioBridgeStats {
        let imp = self.imp();
        let settings = imp.settings.lock().unwrap().clone();
        let mut stats = AudioBridgeStats {
            target_latency_ms: settings.target_latency_ms,
            rate: 1.0,
            ..Default::default()
        };
        let ms = |ns: u64| ns as f64 / 1e6;
        let state = imp.state.lock().unwrap();
        let Some(state) = state.as_ref() else {
            return stats;
        };
        let a = &state.acct;
        let ongoing = a.current_underrun_ns.unwrap_or(0);
        stats.depth_ms = ms(state.depth_ns);
        stats.floor_ms = state.ctl.floor_ns().map(ms).unwrap_or(0.0);
        stats.max_depth_ms = ms(a.max_depth_ns);
        stats.rate = state.ctl.rate();
        stats.underruns = a.underruns;
        stats.underrun_ms = ms(a.underrun_ns + ongoing);
        stats.longest_underrun_ms = ms(a.longest_underrun_ns.max(ongoing));
        stats.drained_ms = ms(a.drained_ns);
        stats.stretched_ms = ms(a.stretched_ns);
        stats.time_scaled_ms = ms(a.time_scaled_ns);
        stats.skips = a.skips;
        stats.skipped_ms = ms(a.skipped_ns);
        stats.producer_attached = state.channel.has_writer();
        // The overrun outlives its producer by up to 5 s; once that flow has
        // stopped, there is no producer to be too fast.
        stats.producer_overrun = stats.producer_attached && state.ctl.overrun();
        let input = state.channel.lock().input;
        stats.input_gaps_50ms = input.gaps[0];
        stats.input_gaps_100ms = input.gaps[1];
        stats.input_gaps_200ms = input.gaps[2];
        stats.input_gaps_400ms = input.gaps[3];
        stats.longest_input_gap_ms = ms(input.longest_gap_ns);
        stats.overflow_ms = ms(input.overflow_ns);
        stats
    }
}

fn time_segment(start: u64, base: u64, rate: f64) -> gst::FormattedSegment<gst::ClockTime> {
    let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
    let start = gst::ClockTime::from_nseconds(start);
    seg.set_rate(rate);
    seg.set_start(start);
    seg.set_time(start);
    seg.set_position(start);
    seg.set_base(gst::ClockTime::from_nseconds(base));
    seg
}

/// Read up to `dst.len() / BPF` frames from the ring into `dst`.
fn read_frames(ring: &mut super::ring::Ring<channel::AudioChunk>, dst: &mut [u8]) -> usize {
    let mut got = 0;
    while got * BPF < dst.len() {
        match ring.read_front(|chunk| chunk.read_into(&mut dst[got * BPF..])) {
            Some(n) if n > 0 => got += n,
            _ => break,
        }
    }
    got
}

/// Multiply the first (`rising`) or last (`!rising`) [`FADE_FRAMES`] of
/// `frames` by a raised-cosine ramp.
fn fade(data: &mut [u8], frames: usize, rising: bool) {
    let len = frames.min(FADE_FRAMES);
    if len == 0 {
        return;
    }
    let first = if rising { 0 } else { frames - len };
    for i in 0..len {
        let x = (i as f32 + 0.5) / len as f32;
        let up = 0.5 - 0.5 * (std::f32::consts::PI * x).cos();
        let gain = if rising { up } else { 1.0 - up };
        let frame = &mut data[(first + i) * BPF..(first + i + 1) * BPF];
        for sample in frame.chunks_exact_mut(4) {
            let v = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]) * gain;
            sample.copy_from_slice(&v.to_le_bytes());
        }
    }
}

/// Fade the `got` frames read on an underrun tick out to silence over a full
/// [`FADE_FRAMES`]. When fewer than that were left, the last frame heard — the
/// last of `got`, or `last` from the previous tick when nothing was left — is
/// held to make up the length, so the output glides to silence instead of
/// cutting. Returns the frames that now carry sound.
fn fade_out_to_silence(data: &mut [u8], got: usize, last: &[u8; BPF]) -> usize {
    if got >= FADE_FRAMES {
        fade(data, got, false);
        return got;
    }
    let len = FADE_FRAMES.min(data.len() / BPF);
    let mut held = *last;
    if got > 0 {
        held.copy_from_slice(&data[(got - 1) * BPF..got * BPF]);
    }
    for frame in data[got * BPF..len * BPF].chunks_exact_mut(BPF) {
        frame.copy_from_slice(&held);
    }
    fade(data, len, false);
    len
}

/// Whether the backlog a producer leaves behind should be dropped when another
/// takes over. A producer that outran the reader leaves more than the maximum
/// latency queued, which no live producer does, and playing that ahead of the
/// new producer's audio would join the two with a jump. The reader skips such a
/// producer on almost every tick, so a handover slower than a tick finds only
/// what the last skip left, which is why a recent skip counts too. A live
/// producer reported by mistake skips seconds apart and keeps its backlog, as
/// does any ordinary restart.
fn backlog_is_stale(
    was_overrun: bool,
    depth_ns: u64,
    since_skip_ns: u64,
    max_latency_ns: u64,
) -> bool {
    was_overrun && (depth_ns > max_latency_ns || since_skip_ns < WINDOW_NS)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AudioBridgeReader {
        pub(super) settings: Mutex<Settings>,
        pub(super) state: Mutex<Option<State>>,
        pub(super) restamp: Arc<Restamp>,
        clock_wait: Mutex<ClockWait>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AudioBridgeReader {
        const NAME: &'static str = "StromAudioBridgeReader";
        type Type = super::AudioBridgeReader;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for AudioBridgeReader {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_live(true);
            obj.set_format(gst::Format::Time);
        }
    }

    impl GstObjectImpl for AudioBridgeReader {}

    impl ElementImpl for AudioBridgeReader {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                vec![gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &super::super::caps(),
                )
                .unwrap()]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseSrcImpl for AudioBridgeReader {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let settings = self.settings.lock().unwrap().clone();
            if settings.channel.is_empty() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["No channel name set"]
                ));
            }
            let channel = channel::acquire(&settings.channel);
            if !channel.claim_reader() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Busy,
                    [
                        "Audio bridge channel '{}' already has a reader",
                        settings.channel
                    ]
                ));
            }
            gst::debug!(
                CAT,
                imp = self,
                "Reading from channel '{}'",
                settings.channel
            );
            self.restamp.reset();
            *self.state.lock().unwrap() = Some(State {
                writer_generation: channel.writer_generation(),
                channel,
                ctl: Controller::new(settings.control()),
                started: false,
                seg_start: 0,
                seg_base: 0,
                seg_rate: UNITY_RATE,
                seg_frames: 0,
                carry: 0.0,
                depth_ns: 0,
                last_frame: [0; BPF],
                acct: Accounting::default(),
            });
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            if let Some(state) = self.state.lock().unwrap().take() {
                state.channel.release_reader();
            }
            Ok(())
        }

        fn is_seekable(&self) -> bool {
            false
        }

        fn query(&self, query: &mut gst::QueryRef) -> bool {
            if let gst::QueryViewMut::Latency(q) = query.view_mut() {
                q.set(
                    true,
                    gst::ClockTime::from_nseconds(PERIOD_NS),
                    gst::ClockTime::NONE,
                );
                return true;
            }
            BaseSrcImplExt::parent_query(self, query)
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            let mut wait = self.clock_wait.lock().unwrap();
            if let Some(id) = wait.clock_id.take() {
                id.unschedule();
            }
            wait.flushing = true;
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            self.clock_wait.lock().unwrap().flushing = false;
            Ok(())
        }
    }

    impl PushSrcImpl for AudioBridgeReader {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
            let obj = self.obj();
            let (clock, base_time) =
                Option::zip(obj.clock(), obj.base_time()).ok_or(gst::FlowError::Flushing)?;
            let control = self.settings.lock().unwrap().control();

            let wait_until = {
                let mut guard = self.state.lock().unwrap();
                let state = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
                if *state.ctl.config() != control {
                    state.ctl.set_config(control);
                }
                if !state.started {
                    let now = clock.time().saturating_sub(base_time).nseconds();
                    state.started = true;
                    state.seg_start = now;
                    state.seg_base = now;
                    self.restamp.base_rt.store(now, Ordering::Relaxed);
                    obj.new_segment(time_segment(now, now, UNITY_RATE).upcast_ref())
                        .map_err(|_| gst::FlowError::Error)?;
                }
                base_time + gst::ClockTime::from_nseconds(state.next_running_time())
            };
            self.wait_for(&clock, wait_until)?;

            let mut guard = self.state.lock().unwrap();
            let state = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            let channel = state.channel.clone();
            let mut shared = channel.lock();

            // Read before a new producer clears it.
            let was_overrun = state.ctl.overrun();
            let generation = channel.writer_generation();
            if generation != state.writer_generation {
                state.writer_generation = generation;
                if backlog_is_stale(
                    was_overrun,
                    shared.ring.depth_ns(),
                    state.ctl.since_skip_ns(),
                    state.ctl.config().max_latency_ns,
                ) {
                    shared.ring.clear();
                }
                state.ctl.producer_changed();
            }
            let depth_ns = shared.ring.depth_ns();
            let decision = state.ctl.tick(depth_ns);
            let rate = match decision {
                Decision::Play { rate, .. } if rate != 1.0 => rate,
                _ => UNITY_RATE,
            };
            let want = PERIOD_FRAMES * rate + state.carry;
            let frames = want.floor() as usize;
            state.carry = want - frames as f64;

            let mut buffer =
                gst::Buffer::with_size(frames * BPF).map_err(|_| gst::FlowError::Error)?;
            {
                let buffer = buffer.get_mut().unwrap();
                let mut map = buffer.map_writable().map_err(|_| gst::FlowError::Error)?;
                let data = map.as_mut_slice();
                // `got` is the audio read from the ring; `audible` also counts
                // the frames an underrun holds to complete its fade.
                let (got, audible) = match decision {
                    Decision::Wait => (0, 0),
                    Decision::Play {
                        fade_in,
                        discard_ns,
                        ..
                    } => {
                        if discard_ns > 0 {
                            let units = shared.ring.ns_to_units(discard_ns);
                            let discarded = shared.ring.discard(units);
                            gst::debug!(
                                CAT,
                                imp = self,
                                "Starting at target, trimmed {} ms",
                                shared.ring.units_to_ns(discarded) / 1_000_000
                            );
                        }
                        let got = read_frames(&mut shared.ring, data);
                        if fade_in {
                            fade(data, got, true);
                        }
                        (got, got)
                    }
                    Decision::Underrun => {
                        let got = read_frames(&mut shared.ring, data);
                        (got, fade_out_to_silence(data, got, &state.last_frame))
                    }
                    Decision::Skip { .. } => {
                        let got = read_frames(&mut shared.ring, data);
                        fade(data, got, false);
                        (got, got)
                    }
                };
                data[audible * BPF..].fill(0);
                if let Some(last) = data.rchunks_exact(BPF).next() {
                    state.last_frame.copy_from_slice(last);
                }

                let acct = &mut state.acct;
                let silence_ns = PERIOD_NS.saturating_sub(frames_to_ns(got as u64));
                // Silence while no producer flow is running, or before a new
                // one's first audio, is a stop or a start, not a dropout: it
                // ends the current underrun and starts none.
                let producing = channel.has_writer() && shared.writer_has_delivered();
                match decision {
                    Decision::Wait if !producing => acct.end_underrun(),
                    Decision::Wait => {
                        if let Some(run) = acct.current_underrun_ns.as_mut() {
                            *run += PERIOD_NS;
                        }
                    }
                    Decision::Play { rate, .. } => {
                        acct.end_underrun();
                        let content_ns = frames_to_ns(frames as u64);
                        if rate > 1.0 {
                            acct.drained_ns += content_ns.saturating_sub(PERIOD_NS);
                        } else if rate < 1.0 {
                            acct.stretched_ns += PERIOD_NS.saturating_sub(content_ns);
                        }
                        if rate != 1.0 {
                            acct.time_scaled_ns += PERIOD_NS;
                        }
                    }
                    Decision::Underrun if !producing => acct.end_underrun(),
                    Decision::Underrun => {
                        acct.underruns += 1;
                        acct.current_underrun_ns = Some(silence_ns);
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Underrun at {} ms backlog",
                            depth_ns / 1_000_000
                        );
                    }
                    Decision::Skip { discard_ns } => {
                        let units = shared.ring.ns_to_units(discard_ns);
                        let discarded = shared.ring.discard(units);
                        acct.skips += 1;
                        acct.skipped_ns += shared.ring.units_to_ns(discarded);
                        // A producer that outruns the reader skips on every
                        // tick for as long as it is connected, so this is said
                        // once rather than a hundred times a second.
                        if state.ctl.overrun() {
                            if !acct.overrun_reported {
                                acct.overrun_reported = true;
                                gst::warning!(
                                    CAT,
                                    imp = self,
                                    "Channel '{}': input delivers faster than the bridge can \
                                     play, so the backlog refills after every skip and the \
                                     output will keep skipping. The Audio Bridge Output \
                                     feeding this channel is likely connected to something \
                                     that is not live, such as a file.",
                                    channel.name()
                                );
                            }
                        } else {
                            gst::info!(
                                CAT,
                                imp = self,
                                "Channel '{}': backlog {} ms beyond recovery, skipped {} ms",
                                channel.name(),
                                depth_ns / 1_000_000,
                                shared.ring.units_to_ns(discarded) / 1_000_000
                            );
                        }
                    }
                }
                // The overrun can end on any tick (the producer runs dry or is
                // replaced), so a later one is warned about again.
                if !state.ctl.overrun() {
                    acct.overrun_reported = false;
                }
                state.depth_ns = shared.ring.depth_ns();
                // A startup trim is not a backlog anyone waited through.
                let held_ns = match decision {
                    Decision::Play { discard_ns, .. } => depth_ns - discard_ns,
                    _ => depth_ns,
                };
                acct.max_depth_ns = acct.max_depth_ns.max(held_ns);
            }
            drop(shared);

            if rate != state.seg_rate {
                let content_ns = frames_to_ns(state.seg_frames);
                state.seg_base = state.next_running_time();
                state.seg_start += content_ns;
                state.seg_frames = 0;
                state.seg_rate = rate;
                gst::log!(CAT, imp = self, "Rate {:.3}", rate);
                obj.new_segment(time_segment(state.seg_start, state.seg_base, rate).upcast_ref())
                    .map_err(|_| gst::FlowError::Error)?;
            }

            {
                let buffer = buffer.get_mut().unwrap();
                let pts = state.seg_start + frames_to_ns(state.seg_frames);
                state.seg_frames += frames as u64;
                let end = state.seg_start + frames_to_ns(state.seg_frames);
                buffer.set_pts(gst::ClockTime::from_nseconds(pts));
                buffer.set_duration(gst::ClockTime::from_nseconds(end - pts));
            }
            Ok(CreateSuccess::NewBuffer(buffer))
        }
    }

    impl AudioBridgeReader {
        fn wait_for(
            &self,
            clock: &gst::Clock,
            until: gst::ClockTime,
        ) -> Result<(), gst::FlowError> {
            let id = {
                let mut wait = self.clock_wait.lock().unwrap();
                if wait.flushing {
                    return Err(gst::FlowError::Flushing);
                }
                let id = clock.new_single_shot_id(until);
                wait.clock_id = Some(id.clone());
                id
            };
            let (res, _jitter) = id.wait();
            self.clock_wait.lock().unwrap().clock_id = None;
            if res == Err(gst::ClockError::Unscheduled) {
                return Err(gst::FlowError::Flushing);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_of(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|v| [v.to_le_bytes(), v.to_le_bytes()])
            .flatten()
            .collect()
    }

    fn left(data: &[u8], frame: usize) -> f32 {
        let s = &data[frame * BPF..frame * BPF + 4];
        f32::from_le_bytes([s[0], s[1], s[2], s[3]])
    }

    #[test]
    fn the_skip_threshold_is_kept_clear_of_the_target() {
        // A threshold at or just above the target leaves no band to drain in,
        // so every backlog past the target would be discarded instead.
        let settings = |target: u64, max: u64| Settings {
            channel: "c".into(),
            target_latency_ms: target,
            max_rate_change_percent: 10.0,
            max_latency_ms: max,
        };
        for (target, max) in [(40, 40), (1000, 1000), (40, 20), (200, 100)] {
            let cfg = settings(target, max).control();
            assert!(
                cfg.max_latency_ns >= cfg.target_ns + MIN_SKIP_HEADROOM_NS,
                "target {target} ms, max {max} ms left no draining band: \
                 threshold {} ms against target {} ms",
                cfg.max_latency_ns / 1_000_000,
                cfg.target_ns / 1_000_000,
            );
        }
        // A threshold already clear of the target is left where it is.
        let cfg = settings(DEFAULT_TARGET_LATENCY_MS, DEFAULT_MAX_LATENCY_MS).control();
        assert_eq!(cfg.max_latency_ns, DEFAULT_MAX_LATENCY_MS * 1_000_000);
    }

    #[test]
    fn fades_reach_silence_at_the_edge() {
        let mut data = frames_of(&vec![1.0; 480]);
        fade(&mut data, 480, false);
        assert!(left(&data, 479).abs() < 0.001);
        assert_eq!(left(&data, 480 - FADE_FRAMES - 1), 1.0);
        let mut data = frames_of(&vec![1.0; 480]);
        fade(&mut data, 480, true);
        assert!(left(&data, 0) < 0.001);
        assert_eq!(left(&data, FADE_FRAMES), 1.0);
    }

    fn largest_step(data: &[u8], frames: usize) -> f32 {
        (1..frames)
            .map(|i| (left(data, i) - left(data, i - 1)).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn an_underrun_with_nothing_left_glides_from_the_last_frame_heard() {
        let last: [u8; BPF] = frames_of(&[0.5]).try_into().unwrap();
        let mut data = vec![0u8; 480 * BPF];
        let audible = fade_out_to_silence(&mut data, 0, &last);
        assert_eq!(audible, FADE_FRAMES);
        assert!(
            (left(&data, 0) - 0.5).abs() < 0.01,
            "starts where the last tick ended"
        );
        assert!(
            left(&data, FADE_FRAMES - 1).abs() < 0.001,
            "ends in silence"
        );
        assert!(
            largest_step(&data, FADE_FRAMES) < 0.01,
            "no step larger than a fade makes: {}",
            largest_step(&data, FADE_FRAMES)
        );
    }

    #[test]
    fn an_underrun_with_a_little_left_completes_its_fade() {
        let mut data = frames_of(&[0.5; 480]);
        let audible = fade_out_to_silence(&mut data, 15, &[0; BPF]);
        assert_eq!(audible, FADE_FRAMES, "the fade runs its full length");
        assert!(largest_step(&data, FADE_FRAMES) < 0.01);
        assert!(left(&data, FADE_FRAMES - 1).abs() < 0.001);
    }

    #[test]
    fn an_underrun_with_enough_left_fades_what_it_read() {
        let mut data = frames_of(&[0.5; 480]);
        assert_eq!(fade_out_to_silence(&mut data, 300, &[0; BPF]), 300);
        assert_eq!(
            left(&data, 300 - FADE_FRAMES - 1),
            0.5,
            "no earlier than the last fade"
        );
        assert!(left(&data, 299).abs() < 0.001);
    }

    #[test]
    fn only_a_flooded_backlog_is_dropped_on_a_producer_change() {
        const MS: u64 = 1_000_000;
        assert!(
            backlog_is_stale(true, 9_990 * MS, 10 * MS, 1_000 * MS),
            "a non-live producer's flooded ring is dropped"
        );
        assert!(
            backlog_is_stale(true, 40 * MS, 30 * MS, 1_000 * MS),
            "what the last skip left of a flood is dropped"
        );
        assert!(
            !backlog_is_stale(true, 190 * MS, 4_000 * MS, 1_000 * MS),
            "a live producer reported by mistake keeps its backlog"
        );
        assert!(
            !backlog_is_stale(false, 9_990 * MS, 10 * MS, 1_000 * MS),
            "without an overrun the ring is left alone"
        );
    }

    #[test]
    fn a_short_fade_covers_what_there_is() {
        let mut data = frames_of(&[1.0; 10]);
        fade(&mut data, 10, false);
        assert!(left(&data, 0) > 0.9 && left(&data, 9) < 0.05);
    }
}
