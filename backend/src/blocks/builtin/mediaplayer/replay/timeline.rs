//! Shared replay timeline.
//!
//! One timeline per (flow, replay group) owns the cursor T and the speed. T is
//! a function of the flow's running time, `T(rt) = t0 + speed * (rt - rt0)`,
//! so every player that asks for the same output tick gets the same frame:
//! the angles cannot drift apart. A command re-anchors the line at "now".

use gstreamer as gst;
use gstreamer::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Instant;
use strom_types::FlowId;

/// Telemetry of one player in the group.
pub struct PlayerStats {
    pub block_id: String,
    pub shown_frame: AtomicU64,
    pub misses: AtomicU64,
    pub late_ticks: AtomicU64,
    pub cached_segments: AtomicU64,
    pub decodes: AtomicU64,
    pub last_decode_ms: AtomicU64,
}

impl PlayerStats {
    pub fn new(block_id: &str) -> Self {
        Self {
            block_id: block_id.to_string(),
            shown_frame: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            late_ticks: AtomicU64::new(0),
            cached_segments: AtomicU64::new(0),
            decodes: AtomicU64::new(0),
            last_decode_ms: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy)]
struct Anchor {
    rt: i64,
    /// Cursor in ns from the start of the material.
    t: f64,
    speed: f64,
}

/// Operator commands.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Pause and move by a number of frames.
    Jog {
        frames: i64,
    },
    /// Go to an absolute frame, keeping the speed.
    Seek {
        frame: u64,
    },
    /// Move by milliseconds, keeping the speed.
    SeekRel {
        ms: i64,
    },
    /// Play at a speed (negative = reverse, 0 = pause).
    Speed {
        speed: f64,
    },
    Play,
    Pause,
    /// Go to the last frame and pause.
    End,
}

pub struct ReplayTimeline {
    anchor: Mutex<Anchor>,
    frame_ns: Mutex<f64>,
    total_frames: AtomicU64,
    clock_source: Mutex<Option<gst::glib::WeakRef<gst::Element>>>,
    created: Instant,
    players: Mutex<Vec<Weak<PlayerStats>>>,
    /// Last paused-position command: when it was applied and its frame, so a
    /// player can report how long until it pushed that frame.
    last_cmd: Mutex<Option<(Instant, u64)>>,
    pub cmd_to_push_us: AtomicU64,
}

/// Timelines keyed by (flow, replay group).
type TimelineMap = HashMap<(FlowId, String), Arc<ReplayTimeline>>;

static TIMELINES: LazyLock<Mutex<TimelineMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn get_or_create(flow_id: FlowId, group: &str) -> Arc<ReplayTimeline> {
    let mut map = TIMELINES.lock().unwrap_or_else(|p| p.into_inner());
    map.entry((flow_id, group.to_string()))
        .or_insert_with(|| Arc::new(ReplayTimeline::new()))
        .clone()
}

pub fn get(flow_id: FlowId, group: &str) -> Option<Arc<ReplayTimeline>> {
    TIMELINES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(flow_id, group.to_string()))
        .cloned()
}

impl ReplayTimeline {
    fn new() -> Self {
        Self {
            anchor: Mutex::new(Anchor {
                rt: 0,
                t: 0.0,
                speed: 0.0,
            }),
            frame_ns: Mutex::new(20_000_000.0),
            total_frames: AtomicU64::new(u64::MAX),
            clock_source: Mutex::new(None),
            created: Instant::now(),
            players: Mutex::new(Vec::new()),
            last_cmd: Mutex::new(None),
            cmd_to_push_us: AtomicU64::new(0),
        }
    }

    /// Called by each player at build: the group spans what every angle has.
    pub fn register_player(&self, stats: &Arc<PlayerStats>, frame_ns: f64, total_frames: u64) {
        *self.frame_ns.lock().unwrap_or_else(|p| p.into_inner()) = frame_ns;
        self.total_frames.fetch_min(total_frames, Ordering::Relaxed);
        let mut players = self.players.lock().unwrap_or_else(|p| p.into_inner());
        players.retain(|w| w.strong_count() > 0);
        players.push(Arc::downgrade(stats));
    }

    /// An element in the flow whose clock and base time define "now".
    pub fn set_clock_source(&self, element: &gst::Element) {
        let mut src = self.clock_source.lock().unwrap_or_else(|p| p.into_inner());
        let alive = src
            .as_ref()
            .and_then(|w| w.upgrade())
            .is_some_and(|e| e.current_state() == gst::State::Playing);
        if !alive {
            *src = Some(element.downgrade());
        }
    }

    pub fn frame_ns(&self) -> f64 {
        *self.frame_ns.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames.load(Ordering::Relaxed)
    }

    /// The flow's running time now, or a monotonic stand-in before it plays.
    pub fn now_rt(&self) -> i64 {
        let src = self.clock_source.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(el) = src.as_ref().and_then(|w| w.upgrade()) {
            if let (Some(clock), Some(base)) = (el.clock(), el.base_time()) {
                return clock.time().nseconds() as i64 - base.nseconds() as i64;
            }
        }
        self.created.elapsed().as_nanos() as i64
    }

    fn max_t(&self) -> f64 {
        let fd = self.frame_ns();
        let total = self.total_frames();
        if total == u64::MAX || total == 0 {
            return 0.0;
        }
        (total - 1) as f64 * fd + fd * 0.5
    }

    fn eval(&self, a: &Anchor, rt: i64) -> f64 {
        (a.t + a.speed * (rt - a.rt) as f64).clamp(0.0, self.max_t())
    }

    /// The cursor at a running time.
    pub fn t_at(&self, rt: i64) -> f64 {
        let a = *self.anchor.lock().unwrap_or_else(|p| p.into_inner());
        self.eval(&a, rt)
    }

    pub fn speed(&self) -> f64 {
        self.anchor.lock().unwrap_or_else(|p| p.into_inner()).speed
    }

    pub fn frame_of(&self, t: f64) -> u64 {
        (t / self.frame_ns()).floor().max(0.0) as u64
    }

    /// Centre of a frame, so float error never floors to the previous one.
    fn frame_centre(&self, frame: u64) -> f64 {
        let fd = self.frame_ns();
        frame as f64 * fd + fd * 0.5
    }

    /// Apply a command at "now". Returns the frame the cursor is on afterwards.
    pub fn apply(&self, cmd: Command) -> (u64, f64) {
        let now = self.now_rt();
        let fd = self.frame_ns();
        let mut a = self.anchor.lock().unwrap_or_else(|p| p.into_inner());
        let cur = self.eval(&a, now);
        let cur_frame = self.frame_of(cur);
        let (t, speed) = match cmd {
            Command::Jog { frames } => {
                let f = (cur_frame as i64 + frames).max(0) as u64;
                (self.frame_centre(f), 0.0)
            }
            Command::Seek { frame } => (self.frame_centre(frame), a.speed),
            Command::SeekRel { ms } => (cur + ms as f64 * 1e6, a.speed),
            Command::Speed { speed } => {
                if speed == 0.0 {
                    (self.frame_centre(cur_frame), 0.0)
                } else {
                    (cur, speed.clamp(-8.0, 8.0))
                }
            }
            Command::Play => (cur, 1.0),
            Command::Pause => (self.frame_centre(cur_frame), 0.0),
            Command::End => (self.max_t(), 0.0),
        };
        let t = t.clamp(0.0, self.max_t());
        *a = Anchor { rt: now, t, speed };
        let _ = fd;
        let frame = self.frame_of(t);
        *self.last_cmd.lock().unwrap_or_else(|p| p.into_inner()) =
            (speed == 0.0).then(|| (Instant::now(), frame));
        (frame, speed)
    }

    /// A player pushed `frame`: if it is the target of the last command, record
    /// how long that took (first player wins).
    pub fn note_pushed(&self, frame: u64) {
        let mut last = self.last_cmd.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, target)) = *last {
            if target == frame {
                self.cmd_to_push_us
                    .store(at.elapsed().as_micros() as u64, Ordering::Relaxed);
                *last = None;
            }
        }
    }

    pub fn telemetry(&self) -> serde_json::Value {
        let now = self.now_rt();
        let t = self.t_at(now);
        let players: Vec<serde_json::Value> = self
            .players
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter_map(|w| w.upgrade())
            .map(|p| {
                serde_json::json!({
                    "block": p.block_id,
                    "shown_frame": p.shown_frame.load(Ordering::Relaxed),
                    "misses": p.misses.load(Ordering::Relaxed),
                    "late_ticks": p.late_ticks.load(Ordering::Relaxed),
                    "cached_segments": p.cached_segments.load(Ordering::Relaxed),
                    "decodes": p.decodes.load(Ordering::Relaxed),
                    "last_decode_ms": p.last_decode_ms.load(Ordering::Relaxed),
                })
            })
            .collect();
        serde_json::json!({
            "type": "state",
            "frame": self.frame_of(t),
            "speed": self.speed(),
            "total_frames": self.total_frames(),
            "frame_ns": self.frame_ns(),
            "cmd_to_push_ms": self.cmd_to_push_us.load(Ordering::Relaxed) as f64 / 1000.0,
            "players": players,
        })
    }
}
