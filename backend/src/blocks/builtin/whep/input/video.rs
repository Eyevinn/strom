//! WHEP Input video outputs and the routing of incoming video streams to them.
//!
//! The block declares `num_video_tracks` slots at build time. Each slot is an
//! `input-selector` feeding a static `tee`, which the external pad maps to
//! (`video_out`, `video_out_1`, ...), so a consumer can link to it before any
//! WebRTC session exists.
//!
//! Every incoming video stream starts unrouted, into a fakesink, undecoded.
//! [`SlotPlan`] decides which stream feeds which slot (see its docs for the
//! `fill` and `newest` assignment); a stream that gets a slot whose output is
//! linked is decoded into that slot's selector. A stream that has been silent
//! for the timeout gives its slot up, so the next stream takes it. SFUs leave
//! the pad of a stream that stopped in place, so silence is the signal, not
//! the pad going away. When a stream's pad does go away, its slot frees at
//! once.
//!
//! Routing changes run one at a time on the pipeline's async thread, under
//! `VideoSlots::ops`: relinking and state changes must not run on a
//! streaming thread, and two changes to one stream must not interleave.

use super::slot_plan::{Action, Assignment, SlotPlan};
use crate::blocks::builtin::inputs::explicit_track_count;
use crate::blocks::{BlockBuildContext, BlockBuildError};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
use strom_types::block::{
    ExternalPad, DEFAULT_WHEP_VIDEO_STREAM_TIMEOUT_MS, MAX_WHEP_VIDEO_STREAM_TIMEOUT_MS,
    MIN_WHEP_VIDEO_STREAM_TIMEOUT_MS,
};
use strom_types::element::ElementPadRef;
use strom_types::{MediaType, PropertyValue};
use tracing::{debug, error, info, warn};

/// Property holding the number of video outputs. Default 0: a flow saved
/// before video output existed stores no value and keeps its audio-only pads.
pub(super) const NUM_VIDEO_TRACKS: &str = "num_video_tracks";
/// Property choosing how streams take slots: `fill` (default) or `newest`.
pub(super) const VIDEO_ASSIGNMENT: &str = "video_assignment";
/// Property holding how long a video stream may be silent before it gives up
/// its slot.
pub(super) const VIDEO_STREAM_TIMEOUT_MS: &str = "video_stream_timeout_ms";

/// Declared video slots, from `num_video_tracks` (0..=8, default 0).
pub(super) fn num_video_tracks(properties: &HashMap<String, PropertyValue>) -> usize {
    explicit_track_count(properties, NUM_VIDEO_TRACKS).unwrap_or(0)
}

fn video_assignment(properties: &HashMap<String, PropertyValue>) -> Assignment {
    Assignment::from_property(properties.get(VIDEO_ASSIGNMENT).and_then(|v| match v {
        PropertyValue::String(s) => Some(s.as_str()),
        _ => None,
    }))
}

fn video_stream_timeout_ms(properties: &HashMap<String, PropertyValue>) -> u64 {
    let ms = match properties.get(VIDEO_STREAM_TIMEOUT_MS) {
        Some(PropertyValue::UInt(u)) => *u,
        Some(PropertyValue::Int(i)) => (*i).max(0) as u64,
        _ => DEFAULT_WHEP_VIDEO_STREAM_TIMEOUT_MS,
    };
    ms.clamp(
        MIN_WHEP_VIDEO_STREAM_TIMEOUT_MS,
        MAX_WHEP_VIDEO_STREAM_TIMEOUT_MS,
    )
}

/// Internal element ID of a slot's output tee.
pub(super) fn tee_id(slot: usize) -> String {
    format!("video_out_tee_{}", slot)
}

/// Internal element ID of a slot's input selector.
fn selector_id(slot: usize) -> String {
    format!("video_selector_{}", slot)
}

/// External pad name of a slot. Slot 0 is unsuffixed, like every other
/// block's first output.
fn pad_name(slot: usize) -> String {
    if slot == 0 {
        "video_out".to_string()
    } else {
        format!("video_out_{}", slot)
    }
}

/// External video output pads for `num` slots.
pub(super) fn external_pads(num: usize) -> Vec<ExternalPad> {
    (0..num)
        .map(|slot| ExternalPad {
            label: (num > 1).then(|| format!("V{}", slot)),
            name: pad_name(slot),
            media_type: MediaType::Video,
            internal_element_id: tee_id(slot),
            internal_pad_name: "src_%u".to_string(),
        })
        .collect()
}

/// One declared video output.
struct Slot {
    selector: gst::glib::WeakRef<gst::Element>,
    tee: gst::glib::WeakRef<gst::Element>,
    /// Whether the slot's output is linked to anything. Decided once, by the
    /// element-setup hook, after the flow is linked and before it starts.
    connected: AtomicBool,
}

/// The selector pads a decoder feeds. `removed` is set before the decoder is
/// stopped, so a pad it exposes during removal is not linked in.
#[derive(Default)]
struct DecoderPads {
    removed: bool,
    pads: Vec<gst::glib::WeakRef<gst::Pad>>,
}

type SharedPads = Arc<Mutex<DecoderPads>>;

/// What a stream's identity feeds right now.
enum Feed {
    /// A fakesink: the stream runs, undecoded.
    Parked(gst::glib::WeakRef<gst::Element>),
    /// A decodebin, into `slot`'s selector once the decoder has exposed its
    /// output.
    Decoding {
        decodebin: gst::glib::WeakRef<gst::Element>,
        pads: SharedPads,
        slot: usize,
    },
}

struct StreamRoute {
    identity: gst::glib::WeakRef<gst::Element>,
    /// Buffers the stream has delivered, counted by its BUFFER probe.
    buffers: Arc<AtomicU64>,
    /// `buffers` as last seen, so a change means the stream sent since.
    buffers_seen: u64,
    feed: Feed,
}

struct Inner {
    plan: SlotPlan,
    streams: HashMap<usize, StreamRoute>,
    pipeline: Option<gst::glib::WeakRef<gst::Pipeline>>,
}

impl Inner {
    /// Tell the plan which streams sent since the last look, as of `now_ms`.
    fn refresh(&mut self, now_ms: u64) {
        for (&id, route) in self.streams.iter_mut() {
            let count = route.buffers.load(Ordering::Relaxed);
            if count != route.buffers_seen {
                route.buffers_seen = count;
                self.plan.seen(id, now_ms);
            }
        }
    }
}

/// The block's video slots and the streams routed to them, shared with the
/// pad-added path.
pub(super) struct VideoSlots {
    instance_id: String,
    slots: Vec<Slot>,
    epoch: Instant,
    inner: Mutex<Inner>,
    /// Held while a routing change is carried out; see the module docs.
    ops: Mutex<()>,
    monitor_started: AtomicBool,
    /// Makes element names unique: a stream can be parked and decoded more
    /// than once.
    generation: AtomicUsize,
    me: Weak<VideoSlots>,
}

/// Namespaced block elements, as `BlockBuildResult::elements` holds them.
type NamedElements = Vec<(String, gst::Element)>;
/// Internal links, as `BlockBuildResult::internal_links` holds them.
type NamedLinks = Vec<(ElementPadRef, ElementPadRef)>;

/// Whether anything downstream is linked to the tee's request src pads.
fn output_is_connected(tee: &gst::glib::WeakRef<gst::Element>) -> bool {
    tee.upgrade()
        .map(|t| t.src_pads().iter().any(|p| p.is_linked()))
        .unwrap_or(false)
}

/// Build the selector and tee for each declared video slot and register the
/// hook that records which of them the flow links.
///
/// Returns the namespaced elements and internal links to add to the block,
/// and the shared slot state for the pad-added path.
pub(super) fn build_video_slots(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<(NamedElements, NamedLinks, Arc<VideoSlots>), BlockBuildError> {
    let num = num_video_tracks(properties);
    let mut elements = Vec::with_capacity(num * 2);
    let mut links = Vec::with_capacity(num);
    let mut slots = Vec::with_capacity(num);
    for slot in 0..num {
        let sel_id = format!("{}:{}", instance_id, selector_id(slot));
        let selector = make_selector(&sel_id).map_err(|e| {
            BlockBuildError::ElementCreation(format!("{}: {}", selector_id(slot), e))
        })?;
        let id = format!("{}:{}", instance_id, tee_id(slot));
        // allow-not-linked: a flow may link the pad to nothing yet, and a
        // consumer can come and go without stopping the stream.
        let tee = gst::ElementFactory::make("tee")
            .name(&id)
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", tee_id(slot), e)))?;
        slots.push(Slot {
            selector: selector.downgrade(),
            tee: tee.downgrade(),
            connected: AtomicBool::new(false),
        });
        links.push((
            ElementPadRef::pad(&sel_id, "src"),
            ElementPadRef::pad(&id, "sink"),
        ));
        elements.push((sel_id, selector));
        elements.push((id, tee));
    }
    let video = VideoSlots::new(
        instance_id,
        slots,
        video_assignment(properties),
        video_stream_timeout_ms(properties),
    );

    if num > 0 {
        // Runs after the flow is linked and before the pipeline leaves NULL,
        // so connectivity is known before any WebRTC stream can arrive.
        let video_for_setup = Arc::clone(&video);
        ctx.register_element_setup(Box::new(move |_flow_id, _events| {
            video_for_setup.record_connected_outputs();
        }));
    }

    Ok((elements, links, video))
}

/// The selector in front of a slot's tee. Its inactive pads drop at once
/// (`sync-streams=false`): a stream that loses its slot to another must not
/// wait for the active one.
pub(super) fn make_selector(name: &str) -> Result<gst::Element, gst::glib::BoolError> {
    gst::ElementFactory::make("input-selector")
        .name(name)
        .property("sync-streams", false)
        .build()
}

impl VideoSlots {
    fn new(instance_id: &str, slots: Vec<Slot>, policy: Assignment, timeout_ms: u64) -> Arc<Self> {
        let num = slots.len();
        Arc::new_cyclic(|me| VideoSlots {
            instance_id: instance_id.to_string(),
            slots,
            epoch: Instant::now(),
            inner: Mutex::new(Inner {
                plan: SlotPlan::new(num, policy, timeout_ms),
                streams: HashMap::new(),
                pipeline: None,
            }),
            ops: Mutex::new(()),
            monitor_started: AtomicBool::new(false),
            generation: AtomicUsize::new(0),
            me: me.clone(),
        })
    }

    /// Slots over `(selector, tee, connected)`, for tests that build the
    /// slot elements themselves.
    #[cfg(test)]
    pub(super) fn for_test(
        slots: &[(&gst::Element, &gst::Element, bool)],
        policy: Assignment,
        timeout_ms: u64,
    ) -> Arc<Self> {
        let slots = slots
            .iter()
            .map(|(selector, tee, connected)| Slot {
                selector: selector.downgrade(),
                tee: tee.downgrade(),
                connected: AtomicBool::new(*connected),
            })
            .collect();
        VideoSlots::new("whep", slots, policy, timeout_ms)
    }

    fn record_connected_outputs(&self) {
        for (index, slot) in self.slots.iter().enumerate() {
            let connected = output_is_connected(&slot.tee);
            slot.connected.store(connected, Ordering::SeqCst);
            if connected {
                info!(
                    "WHEP Input {}: {} is linked, its video will be decoded",
                    self.instance_id,
                    pad_name(index)
                );
            } else {
                debug!(
                    "WHEP Input {}: {} is not linked, its video will be discarded undecoded",
                    self.instance_id,
                    pad_name(index)
                );
            }
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn next_generation(&self) -> usize {
        self.generation.fetch_add(1, Ordering::Relaxed)
    }

    fn is_connected(&self, slot: usize) -> bool {
        self.slots
            .get(slot)
            .is_some_and(|s| s.connected.load(Ordering::SeqCst))
    }

    /// A new video stream on `identity`, whose src pad has its caps. Parks the
    /// stream at once, from the caller's thread, then routes it.
    pub(super) fn register(
        &self,
        identity: &gst::Element,
        pipeline: &gst::Pipeline,
        stream: usize,
    ) -> Result<(), String> {
        let src = identity
            .static_pad("src")
            .ok_or("identity has no src pad")?;
        let fakesink = self.make_fakesink(stream)?;
        pipeline
            .add(&fakesink)
            .map_err(|e| format!("Failed to add fakesink to pipeline: {}", e))?;
        let sink = fakesink
            .static_pad("sink")
            .ok_or("fakesink has no sink pad")?;
        src.link(&sink)
            .map_err(|e| format!("Failed to link to fakesink: {:?}", e))?;
        fakesink
            .sync_state_with_parent()
            .map_err(|e| format!("Failed to sync fakesink state: {}", e))?;

        // Per buffer: one relaxed atomic increment, nothing else. The monitor
        // turns a changed count into "sent since the last look".
        let buffers = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&buffers);
        src.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_, _| {
                count.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            },
        );

        let actions = {
            let mut inner = self.inner.lock().unwrap();
            inner.pipeline.get_or_insert_with(|| pipeline.downgrade());
            inner.streams.insert(
                stream,
                StreamRoute {
                    identity: identity.downgrade(),
                    buffers,
                    buffers_seen: 0,
                    feed: Feed::Parked(fakesink.downgrade()),
                },
            );
            let now = self.now_ms();
            inner.refresh(now);
            inner.plan.arrive(stream, now)
        };
        self.release_when_stream_ends(identity, stream);
        self.start_monitor();
        self.schedule(pipeline, actions);
        Ok(())
    }

    /// Carry out `actions` on the pipeline's async thread.
    fn schedule(&self, pipeline: &gst::Pipeline, actions: Vec<Action>) {
        if actions.is_empty() {
            return;
        }
        let Some(me) = self.me.upgrade() else {
            return;
        };
        pipeline.call_async(move |pipeline| {
            let _ops = me.ops.lock().unwrap();
            me.apply(pipeline, actions);
        });
    }

    /// Carry out `actions`, in order. The caller holds `ops`.
    fn apply(&self, pipeline: &gst::Pipeline, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Release { stream, slot } => self.release(pipeline, stream, slot),
                Action::Assign { stream, slot } => self.assign(pipeline, stream, slot),
            }
        }
    }

    /// Check every stream's activity on a timer, so a silent stream gives up
    /// its slot without waiting for another stream to arrive.
    fn start_monitor(&self) {
        if self.slots.is_empty() || self.monitor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let timeout_ms = self.inner.lock().unwrap().plan.timeout_ms();
        let period_ms = (timeout_ms / 4).clamp(25, 250);
        let clock = gst::SystemClock::obtain();
        let id = clock.new_periodic_id(clock.time(), gst::ClockTime::from_mseconds(period_ms));
        let me = self.me.clone();
        let result = id.wait_async(move |_, _, id| {
            let Some(video) = me.upgrade() else {
                id.unschedule();
                return;
            };
            video.tick();
        });
        if let Err(e) = result {
            warn!(
                "WHEP Input {}: could not start the video stream monitor: {:?}",
                self.instance_id, e
            );
        }
    }

    fn tick(&self) {
        let (pipeline, actions) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(pipeline) = inner.pipeline.as_ref().and_then(|p| p.upgrade()) else {
                return;
            };
            if pipeline.current_state() != gst::State::Playing {
                return;
            }
            let now = self.now_ms();
            inner.refresh(now);
            let mut actions = inner.plan.tick(now);
            actions.extend(self.reconcile(&inner, &actions));
            (pipeline, actions)
        };
        self.schedule(&pipeline, actions);
    }

    /// Actions that bring the routing back in line with the plan where they
    /// have drifted apart (a change dropped while the flow was stopping, a
    /// decoder that failed to start). `pending` is already on its way and
    /// is not repeated.
    fn reconcile(&self, inner: &Inner, pending: &[Action]) -> Vec<Action> {
        let mut fixes = Vec::new();
        for (slot, stream) in inner.plan.holders() {
            let parked = matches!(
                inner.streams.get(&stream).map(|r| &r.feed),
                Some(Feed::Parked(_))
            );
            let action = Action::Assign { stream, slot };
            if parked && self.is_connected(slot) && !pending.contains(&action) {
                fixes.push(action);
            }
        }
        for (&stream, route) in &inner.streams {
            if let Feed::Decoding { slot, .. } = route.feed {
                let action = Action::Release { stream, slot };
                if inner.plan.slot_of(stream) != Some(slot) && !pending.contains(&action) {
                    fixes.push(action);
                }
            }
        }
        // Releases first: an assign may need the slot a release frees.
        fixes.sort_by_key(|a| matches!(a, Action::Assign { .. }));
        fixes
    }

    /// Decode `stream` into `slot`. The caller holds `ops`.
    fn assign(&self, pipeline: &gst::Pipeline, stream: usize, slot: usize) {
        if !self.is_connected(slot) {
            debug!(
                "WHEP: Stream {} holds video slot {}, whose output is not linked; not decoding it",
                stream, slot
            );
            return;
        }
        let Some(selector) = self.slots.get(slot).and_then(|s| s.selector.upgrade()) else {
            return;
        };
        let pads = SharedPads::default();
        let decodebin = match self.make_decodebin(stream, &selector, &pads) {
            Ok(d) => d,
            Err(e) => {
                error!(
                    "WHEP: Stream {} failed to set up its video decoder: {}",
                    stream, e
                );
                return;
            }
        };
        // Claim the route before touching the pipeline. Under `ops` nothing
        // else changes it, but a stream that ended has no route left.
        let (identity, old) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(route) = inner.streams.get_mut(&stream) else {
                return;
            };
            let Feed::Parked(fakesink) = &route.feed else {
                return;
            };
            let old = fakesink.upgrade();
            route.feed = Feed::Decoding {
                decodebin: decodebin.downgrade(),
                pads,
                slot,
            };
            (route.identity.upgrade(), old)
        };
        let Some(identity) = identity else {
            return;
        };
        if let Err(e) = pipeline.add(&decodebin) {
            error!(
                "WHEP: Stream {} failed to add its video decoder: {}",
                stream, e
            );
            return;
        }
        if let Err(e) = decodebin.sync_state_with_parent() {
            error!(
                "WHEP: Stream {} failed to start its video decoder: {}",
                stream, e
            );
        }
        info!(
            "WHEP: Stream {} is video, decoding into video slot {}",
            stream, slot
        );
        let pipeline_weak = pipeline.downgrade();
        let identity_weak = identity.downgrade();
        relink(&identity, &decodebin, move || {
            // Ask the sender for a keyframe: the decoder starts mid-stream.
            if let Some(sink) = identity_weak.upgrade().and_then(|i| i.static_pad("sink")) {
                sink.push_event(
                    gstreamer_video::UpstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build(),
                );
            }
            if let (Some(pipeline), Some(old)) = (pipeline_weak.upgrade(), old) {
                pipeline.call_async(move |pipeline| remove_element(pipeline, &old));
            }
        });
    }

    /// Stop decoding `stream` into `slot`; the stream keeps running, parked.
    /// The caller holds `ops`.
    fn release(&self, pipeline: &gst::Pipeline, stream: usize, slot: usize) {
        let fakesink = match self.make_fakesink(stream) {
            Ok(f) => f,
            Err(e) => {
                error!("WHEP: Stream {}: {}", stream, e);
                return;
            }
        };
        let (identity, decodebin, pads) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(route) = inner.streams.get_mut(&stream) else {
                return;
            };
            // Parked: a slot whose output is not linked; nothing was decoded.
            let Feed::Decoding {
                decodebin, pads, ..
            } = &route.feed
            else {
                return;
            };
            let found = (decodebin.upgrade(), Arc::clone(pads));
            route.feed = Feed::Parked(fakesink.downgrade());
            (route.identity.upgrade(), found.0, found.1)
        };
        let (Some(identity), Some(decodebin)) = (identity, decodebin) else {
            return;
        };
        if pipeline.add(&fakesink).is_err() {
            return;
        }
        let _ = fakesink.sync_state_with_parent();
        info!("WHEP: Stream {} gives up video slot {}", stream, slot);
        let selector = self.slots.get(slot).and_then(|s| s.selector.upgrade());
        let pipeline_weak = pipeline.downgrade();
        relink(&identity, &fakesink, move || {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            pipeline.call_async(move |pipeline| {
                remove_decoder(pipeline, &decodebin, &pads, selector.as_ref());
            });
        });
    }

    /// When a video stream's source pad goes away, free its slot and remove
    /// the elements this block added for it.
    ///
    /// The source unlinks the pad from `identity` when it removes it. The
    /// removal runs from the pipeline's async call thread, not the source's
    /// thread that emits `unlinked`.
    fn release_when_stream_ends(&self, identity: &gst::Element, stream: usize) {
        let Some(sink) = identity.static_pad("sink") else {
            return;
        };
        let me = self.me.clone();
        sink.connect_unlinked(move |_, _| {
            let Some(video) = me.upgrade() else {
                return;
            };
            video.stream_ended(stream);
        });
    }

    fn stream_ended(&self, stream: usize) {
        let pipeline = self
            .inner
            .lock()
            .unwrap()
            .pipeline
            .as_ref()
            .and_then(|p| p.upgrade());
        // The source also unlinks its pads when the flow stops. The pipeline
        // removes everything then, and an async call here would hold it past
        // its drop. Forget the stream; the elements go with the pipeline.
        let stopping = pipeline.as_ref().is_none_or(|p| {
            p.current_state() < gst::State::Paused
                || matches!(p.pending_state(), gst::State::Null | gst::State::Ready)
        });
        if stopping {
            let mut inner = self.inner.lock().unwrap();
            inner.streams.remove(&stream);
            let now = self.now_ms();
            inner.plan.remove(stream, now);
            return;
        }
        let (Some(pipeline), Some(me)) = (pipeline, self.me.upgrade()) else {
            return;
        };
        info!("WHEP: Stream {} ended, freeing its video slot", stream);
        pipeline.call_async(move |pipeline| {
            let _ops = me.ops.lock().unwrap();
            let (route, actions) = {
                let mut inner = me.inner.lock().unwrap();
                let route = inner.streams.remove(&stream);
                let now = me.now_ms();
                inner.refresh(now);
                (route, inner.plan.remove(stream, now))
            };
            if let Some(route) = route {
                match route.feed {
                    Feed::Parked(fakesink) => {
                        if let Some(f) = fakesink.upgrade() {
                            remove_element(pipeline, &f);
                        }
                    }
                    Feed::Decoding {
                        decodebin,
                        pads,
                        slot,
                    } => {
                        if let Some(d) = decodebin.upgrade() {
                            let selector = me.slots.get(slot).and_then(|s| s.selector.upgrade());
                            remove_decoder(pipeline, &d, &pads, selector.as_ref());
                        }
                    }
                }
                if let Some(identity) = route.identity.upgrade() {
                    remove_element(pipeline, &identity);
                }
            }
            me.apply(pipeline, actions);
        });
    }

    fn make_fakesink(&self, stream: usize) -> Result<gst::Element, String> {
        gst::ElementFactory::make("fakesink")
            .name(format!(
                "{}:video_fakesink_{}_{}",
                self.instance_id,
                stream,
                self.next_generation()
            ))
            .property("sync", false)
            .property("async", false)
            .build()
            .map_err(|e| format!("Failed to create fakesink: {}", e))
    }

    /// A decodebin whose video output goes to a new pad on `selector`, which
    /// then becomes the selector's active pad.
    ///
    /// The decoder's output goes as it is, in whatever memory the decoder
    /// produced: the consuming block adapts its own input.
    fn make_decodebin(
        &self,
        stream: usize,
        selector: &gst::Element,
        pads: &SharedPads,
    ) -> Result<gst::Element, String> {
        let decodebin = gst::ElementFactory::make("decodebin")
            .name(format!(
                "{}:video_decodebin_{}_{}",
                self.instance_id,
                stream,
                self.next_generation()
            ))
            // Added to a running pipeline: its async preroll must stay
            // inside it. Posted to the pipeline, it would put the whole flow
            // back to PAUSED until every sink prerolls, and a sink on an
            // output no stream feeds never does.
            .property("async-handling", true)
            .build()
            .map_err(|e| format!("Failed to create video decodebin: {}", e))?;

        // Ask the sender for a keyframe after loss; see configure_keyframe_recovery.
        // The element arrives as a signal argument, so nothing is captured.
        decodebin.connect("deep-element-added", false, |values| {
            if let Ok(element) = values[2].get::<gst::Element>() {
                configure_keyframe_recovery(&element);
            }
            None
        });

        // Weak: the decodebin owns this handler, and the selector is its sibling.
        let selector_weak = selector.downgrade();
        let pads = Arc::clone(pads);
        decodebin.connect_pad_added(move |_decodebin, pad| {
            let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
            let is_video = caps
                .structure(0)
                .is_some_and(|s| s.name().starts_with("video/"));
            if !is_video {
                warn!(
                    "WHEP: Stream {} video decodebin exposed a non-video pad ({}), leaving it unlinked",
                    stream, caps
                );
                return;
            }
            let Some(selector) = selector_weak.upgrade() else {
                return;
            };
            // Held across request, link and record, so removal either sees
            // the pad or stops it from being linked at all.
            let mut pads = pads.lock().unwrap();
            if pads.removed {
                return;
            }
            let Some(sink) = selector.request_pad_simple("sink_%u") else {
                error!("WHEP: Stream {} got no pad on {}", stream, selector.name());
                return;
            };
            match pad.link(&sink) {
                Ok(_) => {
                    selector.set_property("active-pad", &sink);
                    pads.pads.push(sink.downgrade());
                    info!(
                        "WHEP: Stream {} decoded video linked to {} ({})",
                        stream,
                        selector.name(),
                        caps
                    );
                }
                Err(e) => {
                    error!(
                        "WHEP: Stream {} failed to link decoded video to {}: {:?}",
                        stream,
                        selector.name(),
                        e
                    );
                    selector.release_request_pad(&sink);
                }
            }
        });
        Ok(decodebin)
    }
}

/// Point `identity`'s src pad at `target` while no data crosses it, then run
/// `done`. The IDLE probe runs at once when the pad is idle, or on the
/// streaming thread right after the buffer in flight.
fn relink(identity: &gst::Element, target: &gst::Element, done: impl FnOnce() + Send + 'static) {
    let (Some(src), Some(sink)) = (identity.static_pad("src"), target.static_pad("sink")) else {
        return;
    };
    // A probe callback must be Sync; the Mutex makes the FnOnce so.
    let done = Mutex::new(Some(done));
    src.add_probe(gst::PadProbeType::IDLE, move |src, _| {
        if let Some(peer) = src.peer() {
            let _ = src.unlink(&peer);
        }
        if let Err(e) = src.link(&sink) {
            error!("WHEP: failed to relink video stream: {:?}", e);
        }
        if let Some(done) = done.lock().unwrap().take() {
            done();
        }
        gst::PadProbeReturn::Remove
    });
}

fn remove_element(pipeline: &gst::Pipeline, element: &gst::Element) {
    let _ = element.set_state(gst::State::Null);
    let _ = pipeline.remove(element);
}

/// Stop a decoder that no longer receives data and give back its selector
/// pads. Marked removed first, so a pad it exposes meanwhile is not linked;
/// stopped next, so its threads no longer push when it is unlinked.
fn remove_decoder(
    pipeline: &gst::Pipeline,
    decodebin: &gst::Element,
    pads: &Mutex<DecoderPads>,
    selector: Option<&gst::Element>,
) {
    let taken = {
        let mut pads = pads.lock().unwrap();
        pads.removed = true;
        std::mem::take(&mut pads.pads)
    };
    let _ = decodebin.set_state(gst::State::Null);
    if let Some(selector) = selector {
        for pad in taken.iter().filter_map(|p| p.upgrade()) {
            if let Some(peer) = pad.peer() {
                let _ = peer.unlink(&pad);
            }
            selector.release_request_pad(&pad);
        }
    }
    let _ = pipeline.remove(decodebin);
}

/// Configure keyframe recovery on an element the video `decodebin` plugs.
///
/// The decodebin sits in the same pipeline as the WebRTC source, so an
/// upstream force-key-unit event from it reaches `webrtcbin`, whose RTP
/// session turns it into a PLI to the sender. Without a request a sender
/// sends no further keyframe after packet loss, and the picture stays broken
/// until the session ends.
/// - Depayloaders ask on packet loss (`request-keyframe`).
/// - Decoders ask when they output a corrupted frame, at most once a second.
///   They keep decoding meanwhile (`corrupt-output` only, not
///   `discard-input`), so loss shows as a smeared picture that repairs itself,
///   not a freeze.
pub(super) fn configure_keyframe_recovery(element: &gst::Element) {
    if element.has_property("request-keyframe") {
        element.set_property("request-keyframe", true);
    }
    if element.has_property("automatic-request-sync-points") {
        element.set_property("automatic-request-sync-points", true);
        if element.has_property("automatic-request-sync-point-flags") {
            element.set_property_from_str("automatic-request-sync-point-flags", "corrupt-output");
        }
        if element.has_property("min-force-key-unit-interval") {
            element.set_property("min-force-key-unit-interval", 1_000_000_000u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_tracks_default_to_none() {
        assert_eq!(num_video_tracks(&HashMap::new()), 0);
    }

    #[test]
    fn video_tracks_are_clamped() {
        let props: HashMap<String, PropertyValue> =
            [(NUM_VIDEO_TRACKS.to_string(), PropertyValue::UInt(50))].into();
        assert_eq!(num_video_tracks(&props), 8);
    }

    #[test]
    fn first_slot_is_unsuffixed() {
        let pads = external_pads(2);
        assert_eq!(pads[0].name, "video_out");
        assert_eq!(pads[0].internal_element_id, "video_out_tee_0");
        assert_eq!(pads[1].name, "video_out_1");
        assert_eq!(pads[1].internal_element_id, "video_out_tee_1");
    }

    #[test]
    fn assignment_defaults_to_fill() {
        assert_eq!(video_assignment(&HashMap::new()), Assignment::Fill);
        let props: HashMap<String, PropertyValue> = [(
            VIDEO_ASSIGNMENT.to_string(),
            PropertyValue::String("newest".to_string()),
        )]
        .into();
        assert_eq!(video_assignment(&props), Assignment::Newest);
    }

    #[test]
    fn stream_timeout_defaults_and_clamps() {
        assert_eq!(
            video_stream_timeout_ms(&HashMap::new()),
            DEFAULT_WHEP_VIDEO_STREAM_TIMEOUT_MS
        );
        let props: HashMap<String, PropertyValue> =
            [(VIDEO_STREAM_TIMEOUT_MS.to_string(), PropertyValue::Int(5))].into();
        assert_eq!(
            video_stream_timeout_ms(&props),
            MIN_WHEP_VIDEO_STREAM_TIMEOUT_MS
        );
    }
}
