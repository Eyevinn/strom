//! The bridge's control law, in nanoseconds of backlog.
//!
//! Once per reader period the controller sees how much media the ring holds
//! and decides how much of it the reader should take. It knows nothing about
//! what the media is: the audio reader turns a rate into time-scaling, and a
//! video reader would turn "take more" into dropping frames.
//!
//! The quantity it steers is the backlog *floor*, the lowest depth over the
//! last [`WINDOW_NS`]. The instantaneous depth saws up and down with every
//! producer burst (20 ms Opus frames, and a jitterbuffer releasing a stall all
//! at once); steering on it makes the rate flap with each burst. The floor is
//! the backlog the reader never needed, which is the part it can drain.

use std::collections::VecDeque;

/// How far back the floor looks.
pub const WINDOW_NS: u64 = 500_000_000;
/// Floor error at which the rate reaches its bound. Below it the rate change
/// is proportional to the error, so the backlog settles on the target instead
/// of overshooting it.
pub const RAMP_NS: u64 = 100_000_000;
/// Hysteresis around the target, capped at half the target.
pub const HYSTERESIS_NS: u64 = 20_000_000;
/// Rates move in steps of this size, so a slowly changing floor does not
/// announce a new rate every period.
pub const RATE_STEP: f64 = 0.005;
/// Least the skip threshold may sit above the target. Draining starts once the
/// floor is [`HYSTERESIS_NS`] above the target and reaches its full rate a
/// further [`RAMP_NS`] up, so a threshold closer than this leaves the
/// controller skipping backlogs it was never given room to drain.
pub const MIN_SKIP_HEADROOM_NS: u64 = HYSTERESIS_NS + RAMP_NS;
/// Skips with no underrun between them after which the producer is declared
/// faster than real time. A live producer only overfills the ring by handing
/// over audio it held through a stall, and the reader ran dry during that
/// stall; a producer the reader cannot keep up with, even at its maximum rate,
/// skips again and again with no underrun in between.
pub const OVERRUN_SKIPS: u64 = 3;
/// A skip this long after the previous one starts the count over, so a rare
/// jump ahead is never added to one from long ago. A declared overrun also
/// clears after this long without a skip: a producer only slightly faster than
/// the reader's maximum rate skips several seconds apart, so a shorter
/// quiet spell would clear and re-declare it again and again.
pub const OVERRUN_WINDOW_NS: u64 = 30_000_000_000;
/// A declared overrun clears once the reader has waited this long with no
/// audio at all: the producer has stopped. A producer that merely pauses —
/// a file looping or seeking, a thread starved under load — comes back well
/// within it and stays declared.
pub const OVERRUN_STOPPED_NS: u64 = 5_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Backlog floor to hold.
    pub target_ns: u64,
    /// Bound on |rate - 1|. Zero disables time-scaling.
    pub max_rate_change: f64,
    /// A floor above this is not drained but skipped.
    pub max_latency_ns: u64,
    /// Output time per reader tick.
    pub period_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// Not playing: the backlog is below target after a start or an underrun.
    Wait,
    /// Discard `discard_ns` from the front, then take `rate` periods of
    /// media and play them in one period.
    Play {
        rate: f64,
        fade_in: bool,
        discard_ns: u64,
    },
    /// Less than one tick's worth is left. Play what there is, fading out, and
    /// wait for the backlog to refill to the target.
    Underrun,
    /// The backlog is beyond recovery. Play this tick fading out, discard
    /// `discard_ns` from the front, and fade in from there.
    Skip { discard_ns: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Steady,
    Draining,
    Stretching,
}

pub struct Controller {
    cfg: Config,
    playing: bool,
    fade_in: bool,
    phase: Phase,
    floor: SlidingMin,
    rate: f64,
    /// Ticks spent waiting since the last play; `None` before the first.
    waited: Option<u64>,
    /// Skips since the last underrun, within [`OVERRUN_WINDOW_NS`] of each other.
    unprovoked_skips: u64,
    /// Time since the last skip.
    since_skip_ns: u64,
    overrun: bool,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        let window = (WINDOW_NS / cfg.period_ns.max(1)).max(1) as usize;
        Self {
            cfg,
            playing: false,
            fade_in: false,
            phase: Phase::Steady,
            floor: SlidingMin::new(window),
            rate: 1.0,
            waited: None,
            unprovoked_skips: 0,
            since_skip_ns: u64::MAX,
            overrun: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Change the settings on a running controller. It keeps playing and
    /// steers towards the new target from the next tick. A lower target or
    /// maximum latency can skip the backlog the old one allowed, so skips
    /// counted before the change are not held against the producer.
    pub fn set_config(&mut self, cfg: Config) {
        let window = (WINDOW_NS / cfg.period_ns.max(1)).max(1) as usize;
        if window != self.floor.window {
            self.floor = SlidingMin::new(window);
        }
        if cfg != self.cfg {
            self.unprovoked_skips = 0;
        }
        self.cfg = cfg;
    }

    /// Time since the last [`Decision::Skip`], `u64::MAX` before the first.
    pub fn since_skip_ns(&self) -> u64 {
        self.since_skip_ns
    }

    /// Lowest depth over the window, or `None` while not playing.
    pub fn floor_ns(&self) -> Option<u64> {
        self.floor.min()
    }

    /// The rate of the last [`Decision::Play`], 1.0 otherwise.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// The producer is delivering faster than the reader can play even at its
    /// maximum rate, so the backlog refills after every skip and no setting
    /// recovers it. Clears when a different producer takes over, once the
    /// reader has waited [`OVERRUN_STOPPED_NS`] for any audio, or after
    /// [`OVERRUN_WINDOW_NS`] without a skip.
    pub fn overrun(&self) -> bool {
        self.overrun
    }

    /// A different producer started writing. If the reader is waiting, the
    /// next resume trims to the target as it does for the first producer: the
    /// new producer's opening backlog, such as a jitterbuffer releasing its
    /// latency, is not the tail of a stall anyone was listening to. An overrun
    /// belonged to the old producer, so it is cleared rather than reported
    /// against the new one.
    pub fn producer_changed(&mut self) {
        if !self.playing {
            self.waited = None;
        }
        self.clear_overrun();
    }

    fn clear_overrun(&mut self) {
        self.overrun = false;
        self.unprovoked_skips = 0;
    }

    pub fn tick(&mut self, depth_ns: u64) -> Decision {
        let cfg = self.cfg;
        let mut discard_ns = 0;
        self.since_skip_ns = self.since_skip_ns.saturating_add(cfg.period_ns);
        if !self.playing {
            // Refill to the target before playing again. Resuming on the first
            // buffer back would underrun again on the next producer burst.
            if depth_ns < cfg.target_ns.max(cfg.period_ns) {
                self.rate = 1.0;
                if let Some(waited) = self.waited.as_mut() {
                    *waited += 1;
                    if self.overrun && *waited * cfg.period_ns >= OVERRUN_STOPPED_NS {
                        self.clear_overrun();
                    }
                }
                return Decision::Wait;
            }
            // Audio that piled up while nothing played for longer than the
            // maximum latency is not the tail of a stall anyone was hearing:
            // it is a producer starting, such as a jitterbuffer releasing its
            // whole latency when a publisher connects. Start from the target
            // rather than play it late and drain it.
            let idle_ns = self.waited.map(|w| w * cfg.period_ns);
            if idle_ns.is_none_or(|idle| idle > cfg.max_latency_ns) {
                discard_ns = depth_ns - cfg.target_ns.max(cfg.period_ns);
            }
            self.playing = true;
            self.fade_in = true;
            self.phase = Phase::Steady;
            self.floor.clear();
            self.waited = Some(0);
        }
        let depth_ns = depth_ns - discard_ns;

        self.floor.push(depth_ns);
        let floor = self.floor.min().unwrap_or(depth_ns);

        if floor > cfg.max_latency_ns {
            self.floor.clear();
            self.phase = Phase::Steady;
            self.fade_in = true;
            self.rate = 1.0;
            if self.since_skip_ns > OVERRUN_WINDOW_NS {
                self.unprovoked_skips = 0;
            }
            self.unprovoked_skips += 1;
            self.since_skip_ns = 0;
            self.overrun |= self.unprovoked_skips >= OVERRUN_SKIPS;
            return Decision::Skip {
                discard_ns: depth_ns.saturating_sub(cfg.target_ns),
            };
        }
        if self.overrun && self.since_skip_ns >= OVERRUN_WINDOW_NS {
            self.clear_overrun();
        }

        let rate = self.steer(floor);
        let need = (cfg.period_ns as f64 * rate) as u64;
        if depth_ns < need {
            self.playing = false;
            self.floor.clear();
            self.rate = 1.0;
            self.waited = Some(0);
            // An underrun is not the end of an overrun: a non-live producer
            // pauses too. It does mean the next skips were provoked.
            self.unprovoked_skips = 0;
            return Decision::Underrun;
        }

        self.rate = rate;
        let fade_in = std::mem::take(&mut self.fade_in);
        Decision::Play {
            rate,
            fade_in,
            discard_ns,
        }
    }

    fn steer(&mut self, floor: u64) -> f64 {
        let cfg = &self.cfg;
        if cfg.max_rate_change <= 0.0 {
            return 1.0;
        }
        let hysteresis = HYSTERESIS_NS.min(cfg.target_ns / 2) as i64;
        let error = floor as i64 - cfg.target_ns as i64;
        self.phase = match self.phase {
            Phase::Steady if error > hysteresis => Phase::Draining,
            Phase::Steady if error < -hysteresis => Phase::Stretching,
            Phase::Draining if error <= 0 => Phase::Steady,
            Phase::Stretching if error >= 0 => Phase::Steady,
            phase => phase,
        };
        let magnitude = || {
            let share = (error.unsigned_abs() as f64 / RAMP_NS as f64).min(1.0);
            let change = (cfg.max_rate_change * share / RATE_STEP).ceil() * RATE_STEP;
            change.min(cfg.max_rate_change)
        };
        match self.phase {
            Phase::Steady => 1.0,
            Phase::Draining => 1.0 + magnitude(),
            Phase::Stretching => 1.0 - magnitude(),
        }
    }
}

/// Minimum over the last `window` samples.
struct SlidingMin {
    window: usize,
    seq: usize,
    /// (sequence number, value), values increasing from front to back.
    queue: VecDeque<(usize, u64)>,
}

impl SlidingMin {
    fn new(window: usize) -> Self {
        Self {
            window,
            seq: 0,
            queue: VecDeque::with_capacity(window + 1),
        }
    }

    fn push(&mut self, value: u64) {
        while self.queue.back().is_some_and(|&(_, v)| v >= value) {
            self.queue.pop_back();
        }
        self.queue.push_back((self.seq, value));
        self.seq += 1;
        while self
            .queue
            .front()
            .is_some_and(|&(s, _)| s + self.window < self.seq)
        {
            self.queue.pop_front();
        }
    }

    fn min(&self) -> Option<u64> {
        self.queue.front().map(|&(_, v)| v)
    }

    fn clear(&mut self) {
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::audio_bridge::{MAX_MAX_LATENCY_MS, MIN_TARGET_LATENCY_MS};

    const MS: u64 = 1_000_000;
    const PERIOD: u64 = 10 * MS;

    fn config(target_ms: u64, max_rate_change: f64) -> Config {
        Config {
            target_ns: target_ms * MS,
            max_rate_change,
            max_latency_ns: 1000 * MS,
            period_ns: PERIOD,
        }
    }

    /// A producer that delivers 20 ms every 20 ms, stalls on request and then
    /// delivers everything it held at once, feeding a reader driven by the
    /// controller. Times are in reader ticks.
    struct Sim {
        ctl: Controller,
        depth: u64,
        held: u64,
        stalled_until: u64,
        tick: u64,
        trace: Vec<(u64, Decision)>,
    }

    impl Sim {
        fn new(cfg: Config) -> Self {
            Self {
                ctl: Controller::new(cfg),
                depth: 0,
                held: 0,
                stalled_until: 0,
                tick: 0,
                trace: Vec::new(),
            }
        }

        fn stall(&mut self, ms: u64) {
            self.stalled_until = self.tick + ms * MS / PERIOD;
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms * MS / PERIOD {
                if self.tick.is_multiple_of(2) {
                    if self.tick < self.stalled_until {
                        self.held += 20 * MS;
                    } else {
                        self.depth += 20 * MS + std::mem::take(&mut self.held);
                    }
                }
                let decision = self.ctl.tick(self.depth);
                let taken = match decision {
                    Decision::Wait => 0,
                    Decision::Play {
                        rate, discard_ns, ..
                    } => discard_ns + (PERIOD as f64 * rate) as u64,
                    Decision::Underrun => self.depth,
                    Decision::Skip { discard_ns } => discard_ns + PERIOD,
                };
                self.depth -= taken.min(self.depth);
                self.trace.push((self.depth, decision));
                self.tick += 1;
            }
        }

        fn rate_changes(&self) -> usize {
            let rates: Vec<f64> = self
                .trace
                .iter()
                .filter_map(|(_, d)| match d {
                    Decision::Play { rate, .. } => Some(*rate),
                    _ => None,
                })
                .collect();
            rates.windows(2).filter(|w| w[0] != w[1]).count()
        }

        fn underruns(&self) -> usize {
            self.trace
                .iter()
                .filter(|(_, d)| *d == Decision::Underrun)
                .count()
        }
    }

    #[test]
    fn waits_for_the_target_then_plays_at_unity() {
        let mut sim = Sim::new(config(40, 0.05));
        sim.run(5_000);
        assert_eq!(sim.trace[0].1, Decision::Wait);
        let first_play = sim
            .trace
            .iter()
            .position(|(_, d)| matches!(d, Decision::Play { .. }))
            .unwrap();
        assert_eq!(
            sim.trace[first_play].1,
            Decision::Play {
                rate: 1.0,
                fade_in: true,
                discard_ns: 0,
            }
        );
        assert_eq!(sim.underruns(), 0);
        assert_eq!(
            sim.rate_changes(),
            0,
            "a producer bursting 20 ms at a time must not move the rate"
        );
    }

    #[test]
    fn drains_a_stall_back_to_target_without_flapping() {
        let mut sim = Sim::new(config(40, 0.05));
        sim.run(2_000);
        sim.stall(400);
        sim.run(400);
        let underruns = sim.underruns();
        assert_eq!(
            underruns, 1,
            "a stall longer than the target underruns once"
        );
        sim.run(20);
        let peak = sim.depth;
        assert!(peak >= 350 * MS, "the stalled audio is kept, depth {peak}");
        sim.run(12_000);
        let floor = sim.ctl.floor_ns().unwrap();
        assert!(
            (30 * MS..=60 * MS).contains(&floor),
            "floor back near the 40 ms target, got {} ms",
            floor / MS
        );
        assert_eq!(sim.underruns(), underruns, "draining caused no underrun");
        // Monotone approach: rate goes up to the bound and steps back down,
        // with no oscillation once the target is reached.
        assert!(
            sim.rate_changes() <= 20,
            "{} rate changes",
            sim.rate_changes()
        );
    }

    #[test]
    fn recovery_time_follows_the_rate_bound() {
        let recover = |max: f64| {
            let mut sim = Sim::new(config(40, max));
            sim.run(2_000);
            sim.stall(400);
            sim.run(420);
            let start = sim.tick;
            while sim.ctl.floor_ns().unwrap_or(0) > 60 * MS {
                sim.run(10);
                assert!(sim.tick - start < 10_000, "no recovery at {max}");
            }
            (sim.tick - start) * PERIOD / MS
        };
        let at_5 = recover(0.05);
        let at_10 = recover(0.10);
        assert!((6_000..=10_000).contains(&at_5), "5 %: {at_5} ms");
        assert!(at_10 < at_5 * 2 / 3, "10 %: {at_10} ms vs 5 %: {at_5} ms");
    }

    #[test]
    fn a_producer_starting_with_a_backlog_starts_at_target() {
        let mut ctl = Controller::new(config(40, 0.05));
        for _ in 0..200 {
            assert_eq!(ctl.tick(0), Decision::Wait);
        }
        // A publisher connects: its jitterbuffer hands over 400 ms at once.
        match ctl.tick(400 * MS) {
            Decision::Play {
                rate, discard_ns, ..
            } => {
                assert_eq!(rate, 1.0);
                assert_eq!(discard_ns, 360 * MS, "start from the 40 ms target");
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn a_stall_while_playing_is_not_trimmed() {
        let mut sim = Sim::new(config(40, 0.05));
        sim.run(2_000);
        sim.stall(600);
        sim.run(700);
        let trimmed: u64 = sim
            .trace
            .iter()
            .filter_map(|(_, d)| match d {
                Decision::Play { discard_ns, .. } => Some(*discard_ns),
                _ => None,
            })
            .sum();
        assert_eq!(trimmed, 0, "a stall's audio is kept and drained");
    }

    #[test]
    fn zero_rate_change_never_scales() {
        let mut sim = Sim::new(config(40, 0.0));
        sim.run(1_000);
        sim.stall(300);
        sim.run(5_000);
        assert_eq!(sim.rate_changes(), 0);
        assert!(sim.depth >= 250 * MS, "backlog kept, like interaudio");
    }

    #[test]
    fn backlog_beyond_max_latency_is_skipped() {
        let mut sim = Sim::new(Config {
            max_latency_ns: 500 * MS,
            ..config(40, 0.05)
        });
        sim.run(1_000);
        // More audio at once than the maximum latency, while playing: a
        // producer that jumped ahead, not a stall the reader waited through.
        sim.depth += 800 * MS;
        sim.run(1_500);
        let skips: Vec<_> = sim
            .trace
            .iter()
            .filter_map(|(_, d)| match d {
                Decision::Skip { discard_ns } => Some(*discard_ns),
                _ => None,
            })
            .collect();
        assert_eq!(skips.len(), 1, "{skips:?}");
        assert!(sim.depth < 100 * MS, "back near target after the skip");
    }

    #[test]
    fn a_starved_floor_stretches() {
        let mut sim = Sim::new(config(100, 0.05));
        sim.run(1_000);
        // Take the producer away for long enough to drain the floor below the
        // target without running dry.
        sim.stall(70);
        sim.run(1_000);
        let stretched = sim
            .trace
            .iter()
            .any(|(_, d)| matches!(d, Decision::Play { rate, .. } if *rate < 1.0));
        assert!(stretched);
        assert_eq!(sim.underruns(), 0);
    }

    #[test]
    fn the_lowest_offered_target_can_still_rebuild_its_margin() {
        // A backlog of one period is one late arrival from silence. The
        // deadband is capped at half the target, so at a low enough target it
        // swallows that state and the controller never stretches to recover.
        // Whatever target the block offers as its lowest must sit above that.
        let cfg = config(MIN_TARGET_LATENCY_MS, 0.10);
        let mut ctl = Controller::new(cfg);
        for _ in 0..200 {
            ctl.tick(cfg.target_ns + 50 * MS);
        }
        let mut rate = 1.0;
        for _ in 0..200 {
            if let Decision::Play { rate: r, .. } = ctl.tick(cfg.period_ns) {
                rate = r;
            }
        }
        assert!(
            rate < 1.0,
            "at the lowest offered target ({MIN_TARGET_LATENCY_MS} ms) the controller sat \
             at rate {rate} with one period of backlog left, so nothing rebuilds the margin"
        );
    }

    #[test]
    fn a_skip_threshold_at_the_target_still_leaves_room_to_drain() {
        // Settings::control() keeps this gap between the target and the skip
        // threshold. Without it the two coincide and every backlog past the
        // target is discarded, which is the behaviour the bridge replaces.
        let cfg = Config {
            max_latency_ns: 40 * MS + MIN_SKIP_HEADROOM_NS,
            ..config(40, 0.05)
        };
        let mut ctl = Controller::new(cfg);
        // A backlog inside the band: above the target, below the threshold.
        let inside = 40 * MS + MIN_SKIP_HEADROOM_NS / 2;
        let mut drained = false;
        for _ in 0..200 {
            match ctl.tick(inside) {
                Decision::Play { rate, .. } if rate > 1.0 => drained = true,
                Decision::Skip { .. } => panic!("skipped a backlog inside the band"),
                _ => {}
            }
        }
        assert!(drained, "a backlog inside the band is drained, not skipped");
    }

    /// Drive a controller against a producer that adds `produce(tick)` of
    /// media before each tick, consuming what each decision takes, with the
    /// ring holding at most `cap_ns`. Returns every decision and whether an
    /// overrun was declared after it.
    fn drive(
        mut ctl: Controller,
        ticks: u64,
        cap_ns: u64,
        mut produce: impl FnMut(u64) -> u64,
    ) -> (Controller, Vec<(Decision, bool)>) {
        let mut depth = 0u64;
        let mut trace = Vec::new();
        for tick in 0..ticks {
            depth = (depth + produce(tick)).min(cap_ns);
            let decision = ctl.tick(depth);
            let taken = match decision {
                Decision::Wait => 0,
                Decision::Play {
                    rate, discard_ns, ..
                } => discard_ns + (PERIOD as f64 * rate) as u64,
                Decision::Underrun => depth,
                Decision::Skip { discard_ns } => discard_ns + PERIOD,
            };
            depth -= taken.min(depth);
            trace.push((decision, ctl.overrun()));
        }
        (ctl, trace)
    }

    fn skips(trace: &[(Decision, bool)]) -> usize {
        trace
            .iter()
            .filter(|(d, _)| matches!(d, Decision::Skip { .. }))
            .count()
    }

    const RING_CAP: u64 = 10_000 * MS;

    #[test]
    fn a_producer_faster_than_real_time_is_reported_and_cleared() {
        // A non-live producer refills the ring to its capacity between ticks.
        let (ctl, trace) = drive(Controller::new(config(40, 0.10)), 400, RING_CAP, |_| {
            RING_CAP
        });
        let skip_ticks: Vec<usize> = trace
            .iter()
            .enumerate()
            .filter(|(_, (d, _))| matches!(d, Decision::Skip { .. }))
            .map(|(i, _)| i)
            .collect();
        assert!(skip_ticks.len() as u64 >= OVERRUN_SKIPS, "{skip_ticks:?}");
        let declared_at = skip_ticks[OVERRUN_SKIPS as usize - 1];
        assert!(
            trace[..declared_at].iter().all(|(_, overrun)| !overrun),
            "not declared before the {OVERRUN_SKIPS}th skip"
        );
        assert!(
            trace[declared_at].1,
            "declared at the {OVERRUN_SKIPS}th skip"
        );

        // The producer is rewired to a live one delivering in real time.
        let clear_ticks = OVERRUN_WINDOW_NS / PERIOD;
        let (_, after) = drive(ctl, clear_ticks + 50, RING_CAP, |t| {
            if t % 2 == 0 {
                20 * MS
            } else {
                0
            }
        });
        let cleared_at = after
            .iter()
            .position(|(_, overrun)| !overrun)
            .expect("cleared once the producer stops outrunning");
        assert!(
            cleared_at as u64 >= clear_ticks - 1,
            "cleared after {cleared_at} ticks, before {clear_ticks}"
        );
    }

    #[test]
    fn retuning_a_live_bridge_is_not_an_overrun() {
        // Lowering the target with the skip threshold at its floor makes each
        // step skip once, with no underrun between: three steps would look
        // exactly like a producer the reader cannot keep up with.
        let cfg = |target_ms: u64| Config {
            max_latency_ns: target_ms * MS + MIN_SKIP_HEADROOM_NS,
            ..config(target_ms, 0.10)
        };
        let mut ctl = Controller::new(cfg(1000));
        let mut depth = 0u64;
        let mut skips = 0;
        for tick in 0..1_200u64 {
            if tick >= 300 && tick % 100 == 0 && tick <= 600 {
                ctl.set_config(cfg(1000 - (tick - 200) * 2));
            }
            if tick % 2 == 0 {
                depth += 20 * MS;
            }
            let taken = match ctl.tick(depth) {
                Decision::Wait => 0,
                Decision::Play {
                    rate, discard_ns, ..
                } => discard_ns + (PERIOD as f64 * rate) as u64,
                Decision::Underrun => depth,
                Decision::Skip { discard_ns } => {
                    skips += 1;
                    discard_ns + PERIOD
                }
            };
            depth -= taken.min(depth);
            assert!(
                !ctl.overrun(),
                "a retune declared an overrun at tick {tick}"
            );
        }
        assert!(skips >= 3, "set-up: each step skips: {skips}");
    }

    /// A controller that has declared an overrun against a non-live producer.
    fn overrun_controller() -> Controller {
        let (ctl, trace) = drive(Controller::new(config(40, 0.10)), 400, RING_CAP, |_| {
            RING_CAP
        });
        assert!(trace.last().unwrap().1, "set-up: overrun declared");
        ctl
    }

    #[test]
    fn an_overrun_ends_when_its_producer_stops() {
        let mut ctl = overrun_controller();
        // The producer stops: the ring runs dry and the reader waits. Until it
        // has waited long enough to tell a stop from a pause, it stays set.
        assert_eq!(ctl.tick(0), Decision::Underrun);
        let stopped_ticks = OVERRUN_STOPPED_NS / PERIOD;
        for tick in 1..stopped_ticks {
            assert_eq!(ctl.tick(0), Decision::Wait);
            assert!(ctl.overrun(), "cleared after only {tick} ticks of waiting");
        }
        assert_eq!(ctl.tick(0), Decision::Wait);
        assert!(!ctl.overrun(), "a producer silent this long has stopped");
    }

    #[test]
    fn a_non_live_producer_that_pauses_stays_declared() {
        // A file-fed producer refills the ring between ticks, but pauses for
        // 60 ms every 2 s (looping, seeking, a starved thread). Each pause runs
        // the ring dry; none of them is the producer stopping.
        let (_, trace) = drive(Controller::new(config(40, 0.10)), 6_000, RING_CAP, |t| {
            if t % 200 < 6 {
                0
            } else {
                RING_CAP
            }
        });
        let declared =
            trace.windows(2).filter(|p| p[1].1 && !p[0].1).count() + usize::from(trace[0].1);
        let underruns = trace
            .iter()
            .filter(|(d, _)| *d == Decision::Underrun)
            .count();
        assert!(
            underruns >= 10,
            "set-up: the pauses run the ring dry: {underruns}"
        );
        assert_eq!(declared, 1, "declared {declared} times over 60 s, not once");
        let after = trace.iter().position(|(_, o)| *o).unwrap();
        assert!(
            trace[after..].iter().all(|(_, o)| *o),
            "cleared by a pause, so fragments were played between pauses"
        );
    }

    #[test]
    fn a_producer_slightly_too_fast_stays_declared_between_skips() {
        // 1.2x: more than the 10 % drain can absorb, so the ring refills to
        // the threshold every several seconds. A clear between skips would
        // play the skipping audio and warn again each time.
        let (_, trace) = drive(Controller::new(config(40, 0.10)), 18_000, RING_CAP, |_| {
            PERIOD * 6 / 5
        });
        assert!(skips(&trace) >= 5, "set-up: {} skips", skips(&trace));
        let after = trace
            .iter()
            .position(|(_, o)| *o)
            .expect("a producer the reader cannot keep up with is reported");
        let cleared = trace[after..].iter().filter(|(_, o)| !o).count();
        assert_eq!(
            cleared,
            0,
            "cleared for {} ms between skips after being declared",
            cleared as u64 * PERIOD / MS
        );
    }

    #[test]
    fn an_overrun_ends_when_another_producer_takes_over() {
        let mut ctl = overrun_controller();
        ctl.producer_changed();
        assert!(!ctl.overrun(), "the overrun belonged to the old producer");
        // The new producer is live, so its audio plays from the first tick
        // and fades in.
        let (_, trace) = drive(ctl, 200, RING_CAP, |t| if t % 2 == 0 { 20 * MS } else { 0 });
        assert!(trace.iter().all(|(_, overrun)| !overrun));
        let first = trace
            .iter()
            .find_map(|(d, _)| match d {
                Decision::Play { fade_in, .. } => Some(*fade_in),
                _ => None,
            })
            .expect("the new producer is played");
        assert!(first, "its first audio fades in");
    }

    #[test]
    fn a_new_producer_does_not_inherit_the_skip_count() {
        // Two unprovoked skips from the old producer, then a new one takes
        // over and jumps ahead once: that single skip is not an overrun.
        let mut ctl = Controller::new(config(40, 0.10));
        for _ in 0..200 {
            ctl.tick(50 * MS);
        }
        let mut skips = 0;
        while skips < 2 {
            if matches!(ctl.tick(2_000 * MS), Decision::Skip { .. }) {
                skips += 1;
            }
        }
        assert!(!ctl.overrun(), "set-up: two skips are not yet an overrun");
        ctl.producer_changed();
        let mut skipped = false;
        for _ in 0..200 {
            if matches!(ctl.tick(2_000 * MS), Decision::Skip { .. }) {
                skipped = true;
                break;
            }
        }
        assert!(skipped, "set-up: the new producer's jump ahead is skipped");
        assert!(
            !ctl.overrun(),
            "one skip from a new producer declared an overrun with the old one's count"
        );
    }

    #[test]
    fn one_skip_after_an_overrun_clears_does_not_declare_it_again() {
        let ctl = overrun_controller();
        // The producer settles to real time; the overrun clears after a quiet
        // spell.
        let clear_ticks = OVERRUN_WINDOW_NS / PERIOD + 50;
        let (mut ctl, trace) = drive(ctl, clear_ticks, RING_CAP, |t| {
            if t % 2 == 0 {
                20 * MS
            } else {
                0
            }
        });
        assert!(!trace.last().unwrap().1, "cleared");
        // One jump ahead, well inside the 30 s window.
        let mut skipped = false;
        for _ in 0..200 {
            if matches!(ctl.tick(2_000 * MS), Decision::Skip { .. }) {
                skipped = true;
                break;
            }
        }
        assert!(skipped, "set-up: the jump ahead is skipped");
        assert!(
            !ctl.overrun(),
            "a single skip after the overrun cleared must not declare it again"
        );
    }

    #[test]
    fn a_moderately_fast_producer_is_reported() {
        // 1.5x real time: more than the 10 % drain can absorb, but the ring
        // is refilled only every couple of seconds, so skips are far apart.
        let (_, trace) = drive(Controller::new(config(40, 0.10)), 6_000, RING_CAP, |_| {
            PERIOD * 3 / 2
        });
        assert!(skips(&trace) >= 3, "{} skips", skips(&trace));
        assert!(
            trace.iter().any(|(_, overrun)| *overrun),
            "a producer the reader cannot keep up with is reported"
        );
    }

    #[test]
    fn repeated_stalls_on_a_live_link_are_never_an_overrun() {
        // A low skip threshold turns every long stall into a skip: the
        // producer hands over the 600 ms it held, the reader resumes on it and
        // finds the floor above the threshold. That is a skip the reader ran
        // dry for first, so it must never be counted as a fast producer.
        let cfg = Config {
            max_latency_ns: 300 * MS + MIN_SKIP_HEADROOM_NS,
            ..config(300, 0.10)
        };
        let mut held = 0u64;
        let (_, trace) = drive(Controller::new(cfg), 12_000, RING_CAP, |t| {
            let in_stall = t >= 200 && (t - 200) % 300 < 60;
            if t % 2 == 1 {
                return 0;
            }
            if in_stall {
                held += 20 * MS;
                0
            } else {
                20 * MS + std::mem::take(&mut held)
            }
        });
        let underruns = trace
            .iter()
            .filter(|(d, _)| *d == Decision::Underrun)
            .count();
        assert!(
            skips(&trace) >= 3,
            "the scenario must skip: {}",
            skips(&trace)
        );
        assert!(
            trace.iter().all(|(_, overrun)| !overrun),
            "{} skips on a live link, after {underruns} underruns, declared an overrun",
            skips(&trace)
        );
    }

    #[test]
    fn a_new_producer_is_trimmed_like_a_start() {
        let mut ctl = Controller::new(config(40, 0.10));
        for _ in 0..200 {
            ctl.tick(50 * MS);
        }
        // The producing flow stops: the reader runs dry and waits.
        assert_eq!(ctl.tick(0), Decision::Underrun);
        for _ in 0..60 {
            assert_eq!(ctl.tick(0), Decision::Wait);
        }
        // A new writer claims the channel and its jitterbuffer hands over
        // 430 ms at once, well inside the maximum latency.
        ctl.producer_changed();
        match ctl.tick(430 * MS) {
            Decision::Play { discard_ns, .. } => {
                assert_eq!(discard_ns, 390 * MS, "start from the 40 ms target")
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn the_highest_max_latency_still_reports_an_overrun() {
        // A skip needs the floor above the threshold, and the floor can never
        // exceed what the ring holds. At a threshold the ring cannot reach,
        // a non-live producer is neither skipped nor reported.
        let cfg = Config {
            max_latency_ns: MAX_MAX_LATENCY_MS * MS,
            ..config(40, 0.10)
        };
        let cap = crate::gst::audio_bridge::RING_CAPACITY_NS;
        let (_, trace) = drive(Controller::new(cfg), 1_000, cap, |_| cap);
        assert!(
            trace.iter().any(|(_, overrun)| *overrun),
            "a non-live producer went unreported at the highest max latency \
             ({MAX_MAX_LATENCY_MS} ms) against a {} ms ring",
            cap / MS
        );
    }

    #[test]
    fn a_long_stall_is_never_mistaken_for_a_fast_producer() {
        // A stall hands its backlog over in one go and is trimmed on resume,
        // so it must not skip even once in a row, however long it ran.
        for stall_ms in [2_000u64, 5_000, 10_000] {
            let mut sim = Sim::new(config(40, 0.10));
            sim.run(2_000);
            sim.stall(stall_ms);
            sim.run(stall_ms + 2_000);
            let skips = sim
                .trace
                .iter()
                .filter(|(_, d)| matches!(d, Decision::Skip { .. }))
                .count();
            assert_eq!(skips, 0, "{stall_ms} ms stall skipped {skips} times");
            assert!(
                !sim.ctl.overrun(),
                "{stall_ms} ms stall declared an overrun"
            );
        }
    }

    #[test]
    fn sliding_min_forgets_old_values() {
        let mut m = SlidingMin::new(3);
        for v in [5, 1, 7, 8, 9] {
            m.push(v);
        }
        assert_eq!(m.min(), Some(7));
        m.push(2);
        assert_eq!(m.min(), Some(2));
    }
}
