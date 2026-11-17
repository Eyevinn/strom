//! Replay player engine: frame store + output tick.
//!
//! - Decode workers turn whole segments into decoded frames, as fast as they
//!   can, into a cache keyed by segment.
//! - A scheduler keeps the segments around the cursor decoded, ahead in the
//!   direction of travel, and evicts the rest.
//! - The output tick runs on the flow clock, one frame period at a time. Each
//!   tick asks the group timeline for T at that running time and pushes the
//!   cached frame for T, restamped with the tick's running time. Pause repeats
//!   a frame, slow motion repeats frames, reverse walks backwards: the output
//!   never stops, flushes or seeks.

use super::source::SegmentIndex;
use super::timeline::{PlayerStats, ReplayTimeline};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

type Frames = Arc<Vec<gst::Buffer>>;

pub struct ReplayPlayer {
    pub block_id: String,
    index: Arc<SegmentIndex>,
    timeline: Arc<ReplayTimeline>,
    pub stats: Arc<PlayerStats>,
    appsrc: gst::glib::WeakRef<gst_app::AppSrc>,
    cache: Mutex<HashMap<usize, Frames>>,
    inflight: Mutex<HashSet<usize>>,
    caps: Mutex<Option<gst::Caps>>,
    decoder: &'static str,
    workers: usize,
    /// Segments kept behind / ahead of the cursor, in its direction of travel.
    keep_behind: usize,
    keep_ahead: usize,
    started: AtomicBool,
    stop: AtomicBool,
}

fn pick_decoder() -> &'static str {
    for name in ["vtdec_hw", "nvh264dec", "vah264dec", "avdec_h264"] {
        if gst::ElementFactory::find(name).is_some() {
            return name;
        }
    }
    "decodebin"
}

impl ReplayPlayer {
    pub fn new(
        block_id: &str,
        index: Arc<SegmentIndex>,
        timeline: Arc<ReplayTimeline>,
        appsrc: &gst_app::AppSrc,
        cache_ahead_s: u64,
    ) -> Arc<Self> {
        let stats = Arc::new(PlayerStats::new(block_id));
        timeline.register_player(&stats, index.frame_ns, index.total_frames);
        let decoder = pick_decoder();
        info!("Replay player {}: decoder {}", block_id, decoder);
        Arc::new(Self {
            block_id: block_id.to_string(),
            index,
            timeline,
            stats,
            appsrc: appsrc.downgrade(),
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            caps: Mutex::new(None),
            decoder,
            workers: 2,
            keep_behind: 2,
            keep_ahead: cache_ahead_s.max(1) as usize,
            started: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        })
    }

    /// Start the threads, once, when the flow starts.
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let (tx, rx) = mpsc::channel::<usize>();
        let rx = Arc::new(Mutex::new(rx));
        for w in 0..self.workers {
            let me = Arc::clone(self);
            let rx = Arc::clone(&rx);
            std::thread::Builder::new()
                .name(format!("replay-dec-{}-{}", self.block_id, w))
                .spawn(move || me.decode_worker(rx))
                .ok();
        }
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("replay-sched-{}", self.block_id))
            .spawn(move || me.scheduler(tx))
            .ok();
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("replay-tick-{}", self.block_id))
            .spawn(move || me.tick_loop())
            .ok();
    }

    fn alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    fn halt(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    // --- decoding -----------------------------------------------------------

    fn decode_worker(self: Arc<Self>, rx: Arc<Mutex<mpsc::Receiver<usize>>>) {
        while self.alive() {
            let job = {
                let rx = rx.lock().unwrap_or_else(|p| p.into_inner());
                rx.recv_timeout(Duration::from_millis(100))
            };
            let seg = match job {
                Ok(s) => s,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let started = Instant::now();
            let result = decode_segment(&self.index.segments[seg].path, self.decoder);
            let ms = started.elapsed().as_millis() as u64;
            match result {
                Ok((frames, caps)) => {
                    let expected = self.index.segments[seg].frames as usize;
                    if frames.len() != expected {
                        warn!(
                            "Replay {}: segment {} decoded {} frames, index says {}",
                            self.block_id,
                            seg,
                            frames.len(),
                            expected
                        );
                    }
                    debug!(
                        "Replay {}: segment {} decoded in {} ms",
                        self.block_id, seg, ms
                    );
                    self.caps
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get_or_insert(caps);
                    self.cache
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(seg, Arc::new(frames));
                    self.stats.decodes.fetch_add(1, Ordering::Relaxed);
                    self.stats.last_decode_ms.store(ms, Ordering::Relaxed);
                }
                Err(e) => warn!("Replay {}: segment {} failed: {}", self.block_id, seg, e),
            }
            self.inflight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&seg);
        }
    }

    /// Segments to hold around `seg`, most urgent first.
    fn wanted(&self, seg: usize, dir: i64) -> Vec<usize> {
        let n = self.index.segments.len() as i64;
        let mut out = vec![seg as i64];
        for i in 1..=self.keep_ahead.max(self.keep_behind) as i64 {
            if i <= self.keep_ahead as i64 {
                out.push(seg as i64 + dir * i);
            }
            if i <= self.keep_behind as i64 {
                out.push(seg as i64 - dir * i);
            }
        }
        out.into_iter()
            .filter(|s| *s >= 0 && *s < n)
            .map(|s| s as usize)
            .collect()
    }

    fn scheduler(self: Arc<Self>, tx: mpsc::Sender<usize>) {
        while self.alive() {
            let rt = self.timeline.now_rt();
            let t = self.timeline.t_at(rt);
            let speed = self.timeline.speed();
            let frame = self.timeline.frame_of(t);
            if let Some(seg) = self.index.segment_of(frame) {
                let dir = if speed < 0.0 { -1 } else { 1 };
                let wanted = self.wanted(seg, dir);
                {
                    let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
                    let mut inflight = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
                    for s in &wanted {
                        if inflight.len() >= self.workers {
                            break;
                        }
                        if !cache.contains_key(s) && !inflight.contains(s) {
                            inflight.insert(*s);
                            if tx.send(*s).is_err() {
                                return;
                            }
                        }
                    }
                }
                let keep: HashSet<usize> = wanted.into_iter().collect();
                let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
                cache.retain(|s, _| keep.contains(s));
                self.stats
                    .cached_segments
                    .store(cache.len() as u64, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn frame_at(&self, frame: u64) -> Option<gst::Buffer> {
        let seg = self.index.segment_of(frame)?;
        let s = &self.index.segments[seg];
        let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache
            .get(&seg)
            .and_then(|f| f.get((frame - s.first_frame) as usize).cloned())
    }

    // --- output -------------------------------------------------------------

    fn tick_loop(self: Arc<Self>) {
        // Wait for the flow to play and for the first frames to exist.
        let (appsrc, clock, base) = loop {
            if !self.alive() {
                return;
            }
            let Some(appsrc) = self.appsrc.upgrade() else {
                self.halt();
                return;
            };
            let has_caps = self
                .caps
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some();
            if appsrc.current_state() == gst::State::Playing && has_caps {
                if let (Some(clock), Some(base)) = (appsrc.clock(), appsrc.base_time()) {
                    self.timeline.set_clock_source(appsrc.upcast_ref());
                    break (appsrc, clock, base);
                }
            }
            drop(appsrc);
            std::thread::sleep(Duration::from_millis(10));
        };

        let caps = self
            .caps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .expect("caps checked above");
        let mut caps = caps;
        caps.make_mut().set(
            "framerate",
            gst::Fraction::new(self.index.fps_n, self.index.fps_d),
        );
        appsrc.set_caps(Some(&caps));
        info!("Replay {}: output running, caps {}", self.block_id, caps);
        let weak = appsrc.downgrade();
        drop(appsrc);

        let fd = self.index.frame_ns;
        let now_rt = || clock.time().nseconds() as i64 - base.nseconds() as i64;
        let mut k = (now_rt() as f64 / fd).ceil() as i64;
        let mut last: Option<gst::Buffer> = None;

        while self.alive() {
            let rt = (k as f64 * fd) as i64;
            let id =
                clock.new_single_shot_id(base + gst::ClockTime::from_nseconds(rt.max(0) as u64));
            let _ = id.wait();

            let Some(appsrc) = weak.upgrade() else {
                break;
            };
            if appsrc.current_state() == gst::State::Null {
                break;
            }
            // Woke up late: skip to the current tick instead of bursting.
            let now = now_rt();
            if now > rt + 2 * fd as i64 {
                self.stats.late_ticks.fetch_add(1, Ordering::Relaxed);
                k = (now as f64 / fd).ceil() as i64;
                continue;
            }

            let t = self.timeline.t_at(rt);
            let frame = self.timeline.frame_of(t);
            let hit = match self.frame_at(frame) {
                Some(b) => {
                    last = Some(b);
                    self.stats.shown_frame.store(frame, Ordering::Relaxed);
                    true
                }
                None => {
                    self.stats.misses.fetch_add(1, Ordering::Relaxed);
                    false
                }
            };
            if let Some(b) = &last {
                let mut out = b.copy();
                if let Some(o) = out.get_mut() {
                    o.set_pts(gst::ClockTime::from_nseconds(rt as u64));
                    o.set_dts(gst::ClockTime::NONE);
                    o.set_duration(gst::ClockTime::from_nseconds(fd as u64));
                }
                let pushed = appsrc.push_buffer(out);
                if hit {
                    self.timeline.note_pushed(frame);
                }
                if let Err(e) = pushed {
                    debug!("Replay {}: push stopped: {:?}", self.block_id, e);
                    break;
                }
            }
            k += 1;
        }
        info!("Replay {}: output stopped", self.block_id);
        self.halt();
    }
}

impl Drop for ReplayPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Decode one MPEG-TS segment to raw NV12 frames, in presentation order.
fn decode_segment(
    path: &std::path::Path,
    decoder: &str,
) -> Result<(Vec<gst::Buffer>, gst::Caps), String> {
    let desc = format!(
        "filesrc location=\"{}\" ! tsdemux ! h264parse ! {} ! videoconvert ! \
         video/x-raw,format=NV12 ! appsink name=sink sync=false",
        path.display(),
        decoder
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or("no appsink")?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| e.to_string())?;
    let mut frames = Vec::with_capacity(64);
    let mut caps = None;
    while let Ok(sample) = sink.pull_sample() {
        if caps.is_none() {
            caps = sample.caps_owned();
        }
        if let Some(b) = sample.buffer_owned() {
            frames.push(b);
        }
    }
    let err = pipeline.bus().and_then(|bus| {
        bus.pop_filtered(&[gst::MessageType::Error])
            .map(|m| match m.view() {
                gst::MessageView::Error(e) => e.error().to_string(),
                _ => String::new(),
            })
    });
    let _ = pipeline.set_state(gst::State::Null);
    if let Some(e) = err {
        return Err(e);
    }
    let caps = caps.ok_or("no frames decoded")?;
    Ok((frames, caps))
}
