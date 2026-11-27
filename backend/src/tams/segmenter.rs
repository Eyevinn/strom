//! Cut encoded streams into segment files at scheduled running times, and close
//! each file as soon as the first frame after its end has arrived.
//!
//! `splitmuxsink` decides whether a GOP starts a new file only once that GOP is
//! complete, that is when the keyframe after it has arrived
//! (`check_completed_gop` / `handle_gathered_gop` / `need_new_fragment` in
//! gst-plugins-good `gst/multifile/gstsplitmuxsink.c`). The file that ends at a
//! split is therefore finalized one whole GOP after its last frame. TAMS
//! Output knows where each segment ends (the next point on its TAI grid, with
//! a keyframe requested there), so it splits itself: the first keyframe at or
//! after the split point closes the open file at once.
//!
//! Each fragment is muxed by its own small pipeline (`appsrc`s → muxer →
//! `filesink`), outside the flow's pipeline, so finishing a file (EOS through
//! the muxer, waiting for the sink) never holds the flow's streaming threads.
//! The flow's pipeline ends each essence in an `appsink`; [`Segmenter::push`]
//! routes every buffer into the fragment it belongs to by its running time.
//!
//! - The **reference** essence (video if any, otherwise the first audio)
//!   decides where a fragment starts: at its first keyframe whose running time
//!   is at or after the scheduled split.
//! - Other essences follow by running time. A buffer at or past a scheduled
//!   split that the reference has not reached yet is held until the reference
//!   decides (the keyframe may come late, or not at all if the encoder ignored
//!   the request), as long as the reference lags it by at most
//!   [`MAX_HOLD_NS`]: a video encoder with lookahead delivers its frames well
//!   after the audio of the same time. Before the reference's first keyframe,
//!   other essences are held whatever the lag (bounded in number), since the
//!   first fragment starts there. A fragment that has been split off stays
//!   open until every other essence has passed the split, so a lagging audio
//!   track still ends up in the right file; a track that lags more than
//!   [`MAX_LAG_NS`] is not waited for.

use crate::tams::uploader::{FragmentReady, PadMediaEnd};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, warn};

/// Longest a fragment waits for EOS out of its muxer before it is torn down.
const FINISH_TIMEOUT: Duration = Duration::from_secs(3);

/// Longest a split-off fragment waits for the other essences to pass the
/// split, measured on the reference's running time.
pub const MAX_LAG_NS: u64 = 1_000_000_000;

/// How far ahead of the reference another essence's buffers are held for a
/// split the reference has not reached yet. Larger than the latency of a
/// video encoder with lookahead; past it the reference is taken to have
/// stalled, and held buffers go to the open fragment.
pub const MAX_HOLD_NS: u64 = 5_000_000_000;

/// Most buffers held per essence.
const MAX_HELD: usize = 1000;

/// Most reference buffers held back for frames that come later in decode
/// order but earlier in presentation (B-frames), see [`Inner::reorder`].
/// More than an encoder's reorder depth; a hole that stays longer (frames
/// lost upstream) is written as it is.
const MAX_REORDER: usize = 16;

/// What the segmenter hands the owner when it opens a fragment.
pub struct OpenRequest<'a> {
    /// Index of the fragment (0, 1, ...), for the file name.
    pub fragment_id: u32,
    /// Running time of the fragment's first (reference) buffer.
    pub running_time_ns: u64,
    /// The fragment's first reference sample (buffer, caps and segment).
    pub sample: &'a gst::Sample,
    /// The `appsink` that received it (clock and base time of the flow).
    pub element: &'a gst::Element,
}

/// The owner's answer to an [`OpenRequest`].
pub struct OpenDecision {
    /// File to write the new fragment to.
    pub location: PathBuf,
    /// The fragment this one ends, ready to upload once its file is complete.
    pub prev_ready: Option<FragmentReady>,
    /// Running time at which the new fragment is to end.
    pub next_split_ns: Option<u64>,
}

pub type OpenFn = Box<dyn FnMut(OpenRequest<'_>) -> OpenDecision + Send>;
pub type DeliverFn = Box<dyn Fn(FragmentReady) + Send>;

/// One open segment file: its muxing pipeline and one `appsrc` per essence
/// whose caps were known when it opened.
struct Fragment {
    #[cfg_attr(not(test), allow(dead_code))]
    id: u32,
    pipeline: gst::Pipeline,
    srcs: Vec<Option<gst_app::AppSrc>>,
    location: PathBuf,
}

/// A fragment that has been split off but waits for other essences to pass
/// the split.
struct Closing {
    fragment: Fragment,
    split_ns: u64,
    ready: Option<FragmentReady>,
}

struct Inner {
    reference: usize,
    caps: Vec<Option<gst::Caps>>,
    /// Latest running time seen per essence.
    pos: Vec<Option<u64>>,
    eos: Vec<bool>,
    /// Non-reference buffers waiting for the reference to decide a split.
    held: Vec<VecDeque<(u64, gst::Sample)>>,
    current: Option<Fragment>,
    closing: Option<Closing>,
    pending_split: Option<u64>,
    next_fragment_id: u32,
    /// After a stop drain nothing more is written.
    stopped: bool,
    /// Reference buffers not written yet, in decode order, with their
    /// presentation interval in running time: a frame whose presentation
    /// leaves a hole before it (a P-frame ahead of its B-frames) waits until
    /// the hole is filled. A stop then never leaves a fragment that ends on a
    /// P-frame whose B-frames the encoder never delivered, so where its media
    /// ends (from presentation times) is what the file holds.
    reorder: VecDeque<(u64, u64, gst::Sample)>,
    /// End of the reference's written presentation, in running time.
    ref_end: Option<u64>,
    /// Held buffers at or past a split written to the fragment before it,
    /// because the reference lagged more than [`MAX_HOLD_NS`].
    late_flushes: u64,
    on_open: OpenFn,
}

/// Where a finished fragment goes: its pipeline is waited on and torn down on
/// a separate thread, then its [`FragmentReady`] (if any) is delivered.
struct FinishJob {
    pipeline: gst::Pipeline,
    location: PathBuf,
    ready: Option<FragmentReady>,
    done: Option<mpsc::SyncSender<()>>,
}

pub struct Segmenter {
    label: String,
    muxer_factory: &'static str,
    mpegts: bool,
    media_end: Vec<Arc<PadMediaEnd>>,
    inner: Mutex<Inner>,
    finisher: Mutex<Option<mpsc::Sender<FinishJob>>>,
    /// Every buffer written: (fragment id, essence, running time).
    #[cfg(test)]
    written: Mutex<Vec<(u32, usize, u64)>>,
}

/// Running time of a sample's PTS (or DTS when it has none).
fn running_time_ns(sample: &gst::Sample) -> Option<u64> {
    let buf = sample.buffer()?;
    let ts = buf.pts().or(buf.dts())?;
    match sample.segment() {
        Some(seg) => seg
            .downcast_ref::<gst::ClockTime>()
            .and_then(|s| s.to_running_time(ts))
            .map(|t| t.nseconds()),
        None => Some(ts.nseconds()),
    }
}

impl Segmenter {
    /// `essences` inputs, of which `reference` decides the splits. One
    /// [`PadMediaEnd`] per essence in `media_end` tracks where the written
    /// media ends. `on_open` is called with the segmenter locked, on the
    /// reference's streaming thread; `deliver` on the finisher thread.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        label: String,
        muxer_factory: &'static str,
        mpegts: bool,
        essences: usize,
        reference: usize,
        media_end: Vec<Arc<PadMediaEnd>>,
        on_open: OpenFn,
        deliver: DeliverFn,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<FinishJob>();
        let thread_label = label.clone();
        let spawned = std::thread::Builder::new()
            .name("tams-finish".into())
            .spawn(move || {
                for job in rx {
                    finish(&thread_label, job, &deliver);
                }
            });
        if let Err(e) = &spawned {
            error!("TAMS {}: cannot start the segment finisher: {}", label, e);
        }
        Arc::new(Self {
            label,
            muxer_factory,
            mpegts,
            media_end,
            inner: Mutex::new(Inner {
                reference,
                caps: vec![None; essences],
                pos: vec![None; essences],
                eos: vec![false; essences],
                held: (0..essences).map(|_| VecDeque::new()).collect(),
                current: None,
                closing: None,
                pending_split: None,
                next_fragment_id: 0,
                stopped: false,
                late_flushes: 0,
                reorder: VecDeque::new(),
                ref_end: None,
                on_open,
            }),
            finisher: Mutex::new(spawned.ok().map(|_| tx)),
            #[cfg(test)]
            written: Mutex::new(Vec::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A buffer of essence `idx` arrived at the `appsink` `element`.
    pub fn push(&self, idx: usize, sample: gst::Sample, element: &gst::Element) {
        let mut g = self.lock();
        if g.stopped || idx >= g.caps.len() {
            return;
        }
        if let Some(caps) = sample.caps() {
            if g.caps[idx].as_ref().map(|c| c.as_ref()) != Some(caps) {
                g.caps[idx] = Some(caps.to_owned());
            }
        }
        let Some(rt) = running_time_ns(&sample) else {
            return;
        };
        if idx == g.reference {
            self.push_reference(&mut g, sample, rt, element);
        } else {
            self.push_other(&mut g, idx, sample, rt);
        }
    }

    fn push_reference(&self, g: &mut Inner, sample: gst::Sample, rt: u64, element: &gst::Element) {
        let key = sample
            .buffer()
            .is_some_and(|b| !b.flags().contains(gst::BufferFlags::DELTA_UNIT));
        let r = g.reference;
        let split = match (&g.current, g.pending_split) {
            (None, _) => key,
            (Some(_), Some(at)) => key && rt >= at,
            (Some(_), None) => false,
        };
        if g.current.is_none() && !split {
            // Nothing can be decoded before the first keyframe.
            return;
        }
        if split {
            // Reference frames held back belong to the fragment this ends.
            self.release_reorder(g, true);
            // Held buffers before the split belong to the fragment it ends.
            self.release_held(g, Some(rt));
            let fragment_id = g.next_fragment_id;
            g.next_fragment_id += 1;
            let decision = (g.on_open)(OpenRequest {
                fragment_id,
                running_time_ns: rt,
                sample: &sample,
                element,
            });
            // A fragment still waiting on a lagging track is closed now.
            if let Some(c) = g.closing.take() {
                self.finish_fragment(c.fragment, c.ready, None);
            }
            if let Some(prev) = g.current.take() {
                g.closing = Some(Closing {
                    fragment: prev,
                    split_ns: rt,
                    ready: decision.prev_ready,
                });
            } else if let Some(ready) = decision.prev_ready {
                // Its file never opened (see the error above), so there is
                // nothing to upload.
                warn!(
                    "TAMS {}: segment {} was not written",
                    self.label,
                    ready.path.display()
                );
            }
            g.pending_split = decision.next_split_ns;
            match self.open_fragment(g, fragment_id, &decision.location) {
                Ok(f) => g.current = Some(f),
                Err(e) => error!(
                    "TAMS {}: cannot open segment {}: {}",
                    self.label,
                    decision.location.display(),
                    e
                ),
            }
            // Held buffers from the split on start the new fragment; at the
            // first fragment, earlier ones are dropped. Those at or past the
            // new fragment's own split stay held for the fragment after it.
            let start = rt;
            let next = g.pending_split;
            for idx in 0..g.held.len() {
                let held: Vec<_> = g.held[idx].drain(..).collect();
                for (t, s) in held {
                    if next.is_some_and(|n| t >= n) {
                        g.held[idx].push_back((t, s));
                    } else if t >= start {
                        self.write(g, idx, &s, false);
                    }
                }
            }
        }
        let duration = sample.buffer().and_then(|b| b.duration());
        match duration {
            Some(d) => {
                g.reorder
                    .push_back((rt, rt.saturating_add(d.nseconds()), sample));
                self.release_reorder(g, false);
            }
            None => {
                self.release_reorder(g, true);
                self.write(g, r, &sample, false);
            }
        }
        g.pos[r] = Some(rt);
        if !split {
            // The next split is at a keyframe after this buffer.
            self.release_held(g, Some(rt));
        }
        self.check_closing(g, rt);
    }

    fn push_other(&self, g: &mut Inner, idx: usize, sample: gst::Sample, rt: u64) {
        g.pos[idx] = Some(g.pos[idx].map_or(rt, |p| p.max(rt)));
        let ref_pos = g.pos[g.reference];
        let ref_done = g.eos[g.reference];
        if let Some(c) = &g.closing {
            if rt < c.split_ns {
                self.write(g, idx, &sample, true);
                let at = ref_pos.unwrap_or(rt);
                self.check_closing(g, at);
                return;
            }
        }
        let hold = !ref_done
            && match (&g.current, g.pending_split) {
                // Before the first fragment: wait for the reference to start.
                (None, _) => true,
                (Some(_), Some(at)) => rt >= at && ref_pos.is_none_or(|r| rt >= r),
                (Some(_), None) => false,
            };
        if hold {
            g.held[idx].push_back((rt, sample));
            // Do not hold on for a reference that does not come. Before the
            // first fragment the reference may not have started yet (its
            // encoder is slower): only the number held is bounded then.
            let started = g.current.is_some();
            while let Some((t, _)) = g.held[idx].front() {
                let too_far = started && ref_pos.is_none_or(|r| *t > r.saturating_add(MAX_HOLD_NS));
                if g.held[idx].len() <= MAX_HELD && !too_far {
                    break;
                }
                if let Some((t, s)) = g.held[idx].pop_front() {
                    if g.current.is_some() {
                        if g.pending_split.is_some_and(|at| t >= at) {
                            g.late_flushes += 1;
                            if g.late_flushes == 1 || g.late_flushes.is_multiple_of(500) {
                                warn!(
                                    "TAMS {}: the reference track lags another by more \
                                     than {} s; that track's media past a split goes into \
                                     the segment before it ({} buffers so far)",
                                    self.label,
                                    MAX_HOLD_NS / 1_000_000_000,
                                    g.late_flushes
                                );
                            }
                        }
                        self.write(g, idx, &s, false);
                    }
                }
            }
        } else if g.current.is_some() {
            self.write(g, idx, &sample, false);
        }
        if let Some(r) = ref_pos {
            self.check_closing(g, r);
        }
    }

    /// Write the held-back reference frames to the open fragment, in decode
    /// order: all of them with `force`, else once their presentation leaves no
    /// hole after what was written (or too many are held).
    fn release_reorder(&self, g: &mut Inner, force: bool) {
        if g.reorder.is_empty() {
            return;
        }
        if !force && g.reorder.len() <= MAX_REORDER {
            let mut spans: Vec<(u64, u64)> = g.reorder.iter().map(|(s, e, _)| (*s, *e)).collect();
            spans.sort_unstable();
            let mut end = g.ref_end.unwrap_or(spans[0].0);
            for (s, e) in spans {
                // Half a frame of slack for rounded timestamps.
                if s > end.saturating_add((e - s) / 2) {
                    return;
                }
                end = end.max(e);
            }
        }
        let r = g.reference;
        let frames: Vec<_> = g.reorder.drain(..).collect();
        for (_, e, s) in frames {
            g.ref_end = Some(g.ref_end.map_or(e, |x| x.max(e)));
            self.write(g, r, &s, false);
        }
    }

    /// Write held buffers before `before` (all when `None`) to the open
    /// fragment.
    fn release_held(&self, g: &mut Inner, before: Option<u64>) {
        for idx in 0..g.held.len() {
            while let Some((t, _)) = g.held[idx].front() {
                if before.is_some_and(|b| *t >= b) {
                    break;
                }
                if let Some((_, s)) = g.held[idx].pop_front() {
                    self.write(g, idx, &s, false);
                }
            }
        }
    }

    /// Close the split-off fragment once every other essence has passed the
    /// split (or ended, or never started), or once the reference at `ref_rt`
    /// is [`MAX_LAG_NS`] past it.
    fn check_closing(&self, g: &mut Inner, ref_rt: u64) {
        let Some(c) = &g.closing else {
            return;
        };
        let passed = (0..g.pos.len())
            .filter(|&i| i != g.reference && !g.eos[i])
            .all(|i| g.pos[i].is_none_or(|p| p >= c.split_ns));
        if passed || ref_rt >= c.split_ns.saturating_add(MAX_LAG_NS) {
            if !passed {
                debug!(
                    "TAMS {}: closing a segment without waiting for a lagging track",
                    self.label
                );
            }
            if let Some(c) = g.closing.take() {
                self.finish_fragment(c.fragment, c.ready, None);
            }
        }
    }

    /// Push one sample into the open fragment (`to_closing`: the split-off one).
    fn write(&self, g: &mut Inner, idx: usize, sample: &gst::Sample, to_closing: bool) {
        let fragment = if to_closing {
            g.closing.as_ref().map(|c| &c.fragment)
        } else {
            g.current.as_ref()
        };
        #[cfg(test)]
        if let (Some(f), Some(rt)) = (fragment, running_time_ns(sample)) {
            self.written.lock().unwrap().push((f.id, idx, rt));
        }
        let Some(src) = fragment
            .and_then(|f| f.srcs.get(idx))
            .and_then(|s| s.as_ref())
        else {
            // The essence had no caps when the fragment opened: it starts in
            // the next one.
            return;
        };
        if let Some(end) = self.media_end.get(idx) {
            if let Some(seg) = sample.segment() {
                end.set_segment(seg);
            }
            if let Some(buf) = sample.buffer() {
                end.add_buffer(
                    buf.pts().or(buf.dts()).map(|t| t.nseconds()),
                    buf.duration().map_or(0, |d| d.nseconds()),
                );
            }
        }
        if let Err(e) = src.push_sample(sample) {
            debug!(
                "TAMS {}: segment muxer refused a buffer: {:?}",
                self.label, e
            );
        }
    }

    /// Build and start the muxing pipeline of a new fragment.
    fn open_fragment(
        &self,
        g: &Inner,
        id: u32,
        location: &std::path::Path,
    ) -> Result<Fragment, String> {
        let pipeline = gst::Pipeline::new();
        // Outside the flow: threads picked from GStreamer's pool may still
        // carry a WebRTC session's priority and CPUs.
        crate::gst::thread_priority::reset_streaming_threads(&pipeline);
        let mux = gst::ElementFactory::make(self.muxer_factory)
            .build()
            .map_err(|e| e.to_string())?;
        if self.mpegts && mux.has_property("alignment") {
            mux.set_property("alignment", 7i32);
        }
        let sink = gst::ElementFactory::make("filesink")
            .property("location", location.to_string_lossy().as_ref())
            .property("sync", false)
            .property("async", false)
            .build()
            .map_err(|e| e.to_string())?;
        pipeline
            .add_many([&mux, &sink])
            .map_err(|e| e.to_string())?;
        mux.link(&sink).map_err(|e| e.to_string())?;
        let mut srcs = Vec::with_capacity(g.caps.len());
        // The reference first, so that it gets the muxer's first pad (its
        // PCR stream in MPEG-TS).
        let mut order: Vec<usize> = (0..g.caps.len()).collect();
        order.sort_by_key(|&i| i != g.reference);
        srcs.resize_with(g.caps.len(), || None);
        for idx in order {
            let Some(caps) = &g.caps[idx] else {
                continue;
            };
            let src = gst_app::AppSrc::builder()
                .format(gst::Format::Time)
                .is_live(false)
                .max_bytes(0)
                .block(false)
                .caps(caps)
                .build();
            src.set_property("handle-segment-change", true);
            pipeline.add(&src).map_err(|e| e.to_string())?;
            // Request the pad by caps: an automatic link can pick the wrong
            // template (mp4mux offered an `audio_%u` pad to H.264).
            let tmpl = mux
                .pad_template_list()
                .into_iter()
                .find(|t| t.direction() == gst::PadDirection::Sink && t.caps().can_intersect(caps))
                .ok_or_else(|| format!("{} takes no {}", self.muxer_factory, caps))?;
            let sink_pad = mux
                .request_pad(&tmpl, None, None)
                .ok_or_else(|| format!("{}: no pad from {}", self.muxer_factory, tmpl.name()))?;
            src.static_pad("src")
                .ok_or("appsrc has no src pad")?
                .link(&sink_pad)
                .map_err(|e| e.to_string())?;
            srcs[idx] = Some(src);
        }
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;
        debug!("TAMS {}: opened segment {}", self.label, location.display());
        Ok(Fragment {
            id,
            pipeline,
            srcs,
            location: location.to_path_buf(),
        })
    }

    /// End a fragment's streams and hand it to the finisher.
    fn finish_fragment(
        &self,
        fragment: Fragment,
        ready: Option<FragmentReady>,
        done: Option<mpsc::SyncSender<()>>,
    ) {
        for src in fragment.srcs.iter().flatten() {
            let _ = src.end_of_stream();
        }
        let job = FinishJob {
            pipeline: fragment.pipeline,
            location: fragment.location,
            ready,
            done,
        };
        let sender = self.finisher.lock().unwrap_or_else(|e| e.into_inner());
        match sender.as_ref() {
            Some(tx) => {
                if let Err(mpsc::SendError(job)) = tx.send(job) {
                    let _ = job.pipeline.set_state(gst::State::Null);
                }
            }
            None => {
                let _ = job.pipeline.set_state(gst::State::Null);
            }
        }
    }

    /// Essence `idx` reached EOS. Once all have, the open fragment is
    /// finished (it stays the tail, flushed by the uploader at teardown).
    pub fn eos(&self, idx: usize) {
        let mut g = self.lock();
        if idx >= g.eos.len() {
            return;
        }
        g.eos[idx] = true;
        if idx == g.reference {
            // The encoder delivered everything: what is still held back has
            // a real hole before it.
            self.release_reorder(&mut g, true);
            self.release_held(&mut g, None);
        }
        if let Some(r) = g.pos[g.reference] {
            self.check_closing(&mut g, r);
        }
        if g.eos.iter().all(|e| *e) {
            self.close_all(&mut g, None);
        }
    }

    fn close_all(&self, g: &mut Inner, done: Option<&mpsc::SyncSender<()>>) -> usize {
        // Reference frames still held back wait for frames that will not come
        // (a stop: the encoder is not drained); the fragment ends before them.
        g.reorder.clear();
        let mut n = 0;
        if let Some(c) = g.closing.take() {
            self.finish_fragment(c.fragment, c.ready, done.cloned());
            n += 1;
        }
        self.release_held(g, None);
        if let Some(f) = g.current.take() {
            self.finish_fragment(f, None, done.cloned());
            n += 1;
        }
        n
    }

    /// Stop drain: finish every open fragment and wait, bounded, until their
    /// files are complete. Buffers that arrive afterwards are dropped. The
    /// last fragment stays the tail, which the uploader flushes at teardown.
    pub fn drain(&self) -> bool {
        let (tx, rx) = mpsc::sync_channel::<()>(4);
        let n = {
            let mut g = self.lock();
            g.stopped = true;
            self.close_all(&mut g, Some(&tx))
        };
        drop(tx);
        let deadline = Instant::now() + FINISH_TIMEOUT + Duration::from_millis(500);
        for _ in 0..n {
            let left = deadline.saturating_duration_since(Instant::now());
            if rx.recv_timeout(left).is_err() {
                warn!(
                    "TAMS {}: the last segment was not finished on stop; it may end short",
                    self.label
                );
                return false;
            }
        }
        true
    }
}

impl Drop for Segmenter {
    fn drop(&mut self) {
        // Teardown without a stop drain: finish what is open, then let the
        // finisher run out, which closes the uploader's channel.
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.close_all(&mut g, None);
        drop(g);
        self.finisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

/// Wait for a fragment's EOS (or error), tear its pipeline down and deliver
/// its [`FragmentReady`].
fn finish(label: &str, job: FinishJob, deliver: &DeliverFn) {
    let FinishJob {
        pipeline,
        location,
        ready,
        done,
    } = job;
    let mut ok = false;
    if let Some(bus) = pipeline.bus() {
        match bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(FINISH_TIMEOUT.as_nanos() as u64),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(_) => ok = true,
                gst::MessageView::Error(e) => error!(
                    "TAMS {}: writing segment {} failed: {} ({:?})",
                    label,
                    location.display(),
                    e.error(),
                    e.debug()
                ),
                _ => {}
            },
            None => warn!(
                "TAMS {}: segment {} not finished within {:?}",
                label,
                location.display(),
                FINISH_TIMEOUT
            ),
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    if ok {
        debug!("TAMS {}: segment closed {}", label, location.display());
        if let Some(r) = ready {
            deliver(r);
        }
    } else {
        // A file the muxer did not finish is not a segment: leave a hole.
        let _ = std::fs::remove_file(&location);
    }
    if let Some(done) = done {
        let _ = done.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const SEG: u64 = 1000 * MS;

    fn available() -> bool {
        gst::init().is_ok() && gst::ElementFactory::find("mpegtsmux").is_some()
    }

    /// A segmenter for video (essence 0, the reference) and one audio track,
    /// splitting on a 1 s grid, writing into a fresh temp directory.
    fn segmenter() -> (Arc<Segmenter>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("strom-segmenter-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = dir.clone();
        let on_open = move |req: OpenRequest<'_>| OpenDecision {
            location: files.join(format!("seg_{}.ts", req.fragment_id)),
            prev_ready: None,
            next_split_ns: Some((req.running_time_ns / SEG + 1) * SEG),
        };
        let seg = Segmenter::new(
            "test".into(),
            "mpegtsmux",
            true,
            2,
            0,
            vec![Arc::default(), Arc::default()],
            Box::new(on_open),
            Box::new(|_| {}),
        );
        (seg, dir)
    }

    fn sample(video: bool, rt: u64, key: bool) -> gst::Sample {
        let caps = if video {
            gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build()
        } else {
            gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4i32)
                .field("stream-format", "adts")
                .build()
        };
        let mut buf = gst::Buffer::with_size(8).unwrap();
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(rt));
            b.set_duration(gst::ClockTime::from_nseconds(20 * MS));
            if !key {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        gst::Sample::builder()
            .buffer(&buf)
            .caps(&caps)
            .segment(&segment)
            .build()
    }

    /// Video written, as running times in write order.
    fn video_written(seg: &Segmenter) -> Vec<u64> {
        seg.written
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, idx, _)| *idx == 0)
            .map(|(_, _, rt)| *rt)
            .collect()
    }

    /// With B-frames the encoder delivers a P-frame before the B-frames shown
    /// before it. A stop (the encoder is not drained) can come between them:
    /// the P-frame is then not written, so the fragment never claims media
    /// up to its end while frames before it are missing. Frames come out in
    /// decode order once complete.
    #[test]
    fn a_stop_leaves_no_p_frame_without_its_b_frames() {
        if !available() {
            return;
        }
        let (seg, dir) = segmenter();
        let el = gst::ElementFactory::make("fakesink").build().unwrap();
        let f = |n: u64| n * 20 * MS;
        // Decode order: I0 P3 B1 B2 P6 B4 (B5 never comes).
        for (n, key) in [
            (0, true),
            (3, false),
            (1, false),
            (2, false),
            (6, false),
            (4, false),
        ] {
            let s = sample(true, f(n), key);
            seg.push(0, s, &el);
        }
        assert!(seg.drain());
        assert_eq!(video_written(&seg), vec![f(0), f(3), f(1), f(2)]);
        drop(seg);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audio written per fragment, as (fragment, running time).
    fn audio_written(seg: &Segmenter) -> Vec<(u32, u64)> {
        seg.written
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, idx, _)| *idx == 1)
            .map(|(f, _, rt)| (*f, *rt))
            .collect()
    }

    /// Audio that reaches the segmenter before the video's first keyframe
    /// (the video encoder is slower), but whose running time is at or after
    /// it, starts the first segment. It was dropped.
    #[test]
    fn audio_before_the_first_keyframe_starts_the_first_segment() {
        if !available() {
            return;
        }
        let (seg, dir) = segmenter();
        let el = gst::ElementFactory::make("fakesink").build().unwrap();
        let audio: Vec<u64> = (0..30).map(|i| 60 * MS + i * 20 * MS).collect();
        for &rt in &audio {
            seg.push(1, sample(false, rt, true), &el);
        }
        for i in 0..10 {
            seg.push(0, sample(true, 100 * MS + i * 40 * MS, i == 0), &el);
        }
        let want: Vec<(u32, u64)> = audio
            .iter()
            .filter(|&&rt| rt >= 100 * MS)
            .map(|&rt| (0, rt))
            .collect();
        assert_eq!(audio_written(&seg), want);
        drop(seg);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Video that lags the audio by more than [`MAX_LAG_NS`] (an encoder with
    /// lookahead): audio past a split waits for the split, and goes into the
    /// segment its own running time belongs to. It went into the segment
    /// before.
    #[test]
    fn audio_goes_to_its_own_segment_when_video_lags() {
        if !available() {
            return;
        }
        let (seg, dir) = segmenter();
        let el = gst::ElementFactory::make("fakesink").build().unwrap();
        let video = |seg: &Segmenter, from: u64| {
            for i in 0..25 {
                let rt = from + i * 40 * MS;
                seg.push(0, sample(true, rt, i == 0), &el);
            }
        };
        video(&seg, 0);
        // Audio 2 s ahead of the video.
        let audio: Vec<u64> = (0..150).map(|i| i * 20 * MS).collect();
        for &rt in &audio {
            seg.push(1, sample(false, rt, true), &el);
        }
        video(&seg, SEG);
        video(&seg, 2 * SEG);
        video(&seg, 3 * SEG);
        let want: Vec<(u32, u64)> = audio.iter().map(|&rt| ((rt / SEG) as u32, rt)).collect();
        let mut got = audio_written(&seg);
        got.sort_by_key(|&(_, rt)| rt);
        assert_eq!(got, want);
        drop(seg);
        let _ = std::fs::remove_dir_all(dir);
    }
}
