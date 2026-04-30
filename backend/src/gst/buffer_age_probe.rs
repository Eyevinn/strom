//! Buffer age pad probes for latency monitoring.
//!
//! Provides both manual (on-demand) and automatic (always-on) probe modes.
//!
//! **Manual probes** are activated via the API. The probe callback writes
//! measurements to a lock-free `ProbeSlot`; a periodic tokio task reads
//! the slot and broadcasts `BufferAgeProbe` events.
//!
//! **Automatic probes** are attached at pipeline start to key measurement points.
//! The periodic task only broadcasts `BufferAgeWarning` when the age exceeds a
//! threshold, keeping WebSocket traffic minimal.
//!
//! This design keeps the GStreamer streaming thread (hot path) free of mutex
//! locks, heap allocations, string clones, and channel sends.

use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::{
    BlockDefinition, BlockInstance, FlowId, StromEvent, BUFFER_AGE_WARNING_THRESHOLD_MS,
};
use tracing::{debug, info};
use uuid::Uuid;

/// A `Cell` wrapper that is `Sync`. Safe to use in GStreamer pad probe
/// callbacks which run on a single streaming thread per pad.
struct SyncCell<T>(std::cell::Cell<T>);

impl<T: Copy> SyncCell<T> {
    fn new(val: T) -> Self {
        Self(std::cell::Cell::new(val))
    }
    fn get(&self) -> T {
        self.0.get()
    }
    fn set(&self, val: T) {
        self.0.set(val);
    }
}

// SAFETY: Pad probe callbacks are invoked on a single GStreamer streaming thread.
unsafe impl<T> Sync for SyncCell<T> {}

/// Sample interval for automatic probes (measure every Nth buffer).
const AUTO_SAMPLE_INTERVAL: u32 = 30;

/// Buffer age threshold for automatic warnings (milliseconds).
const AUTO_WARNING_THRESHOLD_MS: u64 = BUFFER_AGE_WARNING_THRESHOLD_MS;

/// Polling interval for the periodic broadcast task.
const BROADCAST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Lock-free slot for probe measurements. Written by the probe callback
/// (single writer per slot) and read by the periodic broadcast task.
///
/// All orderings are `Relaxed` because there is a single writer (the pad
/// probe on one streaming thread) and the reader (periodic task) only needs
/// eventual visibility — no cross-field consistency is required.
struct ProbeSlot {
    /// Most recently measured buffer age (ms).
    latest_age_ms: AtomicU64,
    /// Maximum age since the last reader reset (ms).
    max_age_ms: AtomicU64,
    /// Total number of samples recorded.
    sample_count: AtomicU64,
}

impl ProbeSlot {
    fn new() -> Self {
        Self {
            latest_age_ms: AtomicU64::new(0),
            max_age_ms: AtomicU64::new(0),
            sample_count: AtomicU64::new(0),
        }
    }

    /// Record a new measurement (called from the probe callback — hot path).
    fn record(&self, age_ms: u64) {
        self.latest_age_ms.store(age_ms, Ordering::Relaxed);
        // Single writer, so a plain load-then-store is sufficient for max tracking.
        let prev_max = self.max_age_ms.load(Ordering::Relaxed);
        if age_ms > prev_max {
            self.max_age_ms.store(age_ms, Ordering::Relaxed);
        }
        self.sample_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the latest values and reset max_age_ms (called from the periodic task).
    fn read_and_reset(&self) -> (u64, u64, u64) {
        let latest = self.latest_age_ms.load(Ordering::Relaxed);
        let max = self.max_age_ms.swap(0, Ordering::Relaxed);
        let count = self.sample_count.load(Ordering::Relaxed);
        (latest, max, count)
    }

    /// Peek at sample_count without resetting (for the list API).
    fn peek_sample_count(&self) -> u64 {
        self.sample_count.load(Ordering::Relaxed)
    }
}

/// Measure the age of a buffer on a pad (pipeline_running_time - buffer_running_time).
///
/// Returns `Some(age_ms)` if the measurement succeeded, `None` otherwise.
/// This runs on the GStreamer streaming thread inside a pad probe callback.
fn measure_buffer_age(
    pad: &gst::Pad,
    info: &gst::PadProbeInfo,
    pipeline: &gst::Pipeline,
) -> Option<u64> {
    let buffer = info.buffer()?;
    let pts = buffer.pts()?;

    let segment = pad.sticky_event::<gst::event::Segment>(0)?;
    let time_segment = segment.segment().downcast_ref::<gst::format::Time>()?;
    let buffer_rt = time_segment.to_running_time(pts)?;

    let clock = pipeline.clock()?;
    let base_time = pipeline.base_time()?;
    let clock_time = clock.time();
    if clock_time < base_time {
        return None;
    }
    let pipeline_rt = clock_time - base_time;

    if pipeline_rt >= buffer_rt {
        let age_ns = (pipeline_rt - buffer_rt).nseconds();
        Some(age_ns / 1_000_000)
    } else {
        None
    }
}

/// Resolve a pad on an element by name, with fallback to first sink pad.
fn resolve_pad(element: &gst::Element, pad_name: &str) -> Result<gst::Pad, String> {
    element
        .static_pad(pad_name)
        .or_else(|| {
            element
                .pads()
                .into_iter()
                .find(|p| p.name().as_str() == pad_name)
        })
        .or_else(|| element.sink_pads().into_iter().next())
        .ok_or_else(|| {
            let available: Vec<String> = element
                .pads()
                .into_iter()
                .map(|p| p.name().to_string())
                .collect();
            format!("No pad '{}' found (available: {:?})", pad_name, available)
        })
}

/// State for a single active probe.
struct ProbeState {
    probe_id: String,
    flow_id: FlowId,
    element_id: String,
    /// Human-friendly pad label sent in events. For block-level probes this is
    /// the external pad's label/name (e.g. "V0", "audio_in_0"). For probes on
    /// standalone elements it falls back to the GStreamer pad name (e.g. "sink").
    pad_name: String,
    /// GStreamer pad probe ID (for removal)
    gst_probe_id: Option<gst::PadProbeId>,
    /// The pad this probe is attached to
    pad: gst::Pad,
    /// Auto-remove timeout handle
    timeout_handle: Option<tokio::task::JoinHandle<()>>,
    /// Whether this is an automatic monitoring probe (excluded from manual API)
    automatic: bool,
    /// Lock-free measurement slot written by the probe callback
    slot: Arc<ProbeSlot>,
}

/// Manages all active buffer age probes for a pipeline.
pub struct ProbeManager {
    probes: Arc<Mutex<HashMap<String, ProbeState>>>,
    events: EventBroadcaster,
    flow_id: FlowId,
    /// Handle for the periodic broadcast task
    broadcast_task: Option<tokio::task::JoinHandle<()>>,
}

impl ProbeManager {
    /// Create a new probe manager for a flow.
    pub fn new(flow_id: FlowId, events: EventBroadcaster) -> Self {
        Self {
            probes: Arc::new(Mutex::new(HashMap::new())),
            events,
            flow_id,
            broadcast_task: None,
        }
    }

    /// Start the periodic broadcast task that reads probe slots and emits events.
    ///
    /// For manual probes: broadcasts `BufferAgeProbe` with latest values.
    /// For automatic probes: broadcasts `BufferAgeWarning` only when age exceeds threshold.
    pub fn start_broadcast_task(&mut self) {
        self.stop_broadcast_task();

        let probes = self.probes.clone();
        let events = self.events.clone();
        let flow_id = self.flow_id;

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(BROADCAST_INTERVAL);
            loop {
                interval.tick().await;

                // Snapshot the probe data under the lock, then release immediately.
                let snapshots: Vec<_> = {
                    let map = probes.lock().unwrap();
                    map.values()
                        .map(|s| {
                            let (latest, _max, count) = s.slot.read_and_reset();
                            (
                                s.probe_id.clone(),
                                s.element_id.clone(),
                                s.pad_name.clone(),
                                s.automatic,
                                latest,
                                count,
                            )
                        })
                        .collect()
                };

                for (probe_id, element_id, pad_name, automatic, latest_age_ms, sample_count) in
                    snapshots
                {
                    if sample_count == 0 {
                        continue;
                    }

                    if automatic {
                        if latest_age_ms > AUTO_WARNING_THRESHOLD_MS {
                            events.broadcast(StromEvent::BufferAgeWarning {
                                flow_id,
                                element_id,
                                pad_name,
                                age_ms: latest_age_ms,
                                threshold_ms: AUTO_WARNING_THRESHOLD_MS,
                            });
                        }
                    } else {
                        events.broadcast(StromEvent::BufferAgeProbe {
                            flow_id,
                            probe_id,
                            element_id,
                            pad_name,
                            age_ms: latest_age_ms,
                            sample_number: sample_count,
                        });
                    }
                }
            }
        });

        self.broadcast_task = Some(task);
    }

    /// Stop the periodic broadcast task.
    pub fn stop_broadcast_task(&mut self) {
        if let Some(task) = self.broadcast_task.take() {
            task.abort();
        }
    }

    /// Activate a new probe on the specified pad.
    ///
    /// - `pipeline`: the GStreamer pipeline (to obtain clock/base_time)
    /// - `element`: the GStreamer element whose pad we probe
    /// - `element_id`: the strom element_id (for events)
    /// - `pad_name`: name of the GStreamer pad to attach to (used for resolution)
    /// - `display_pad_name`: human-friendly pad label sent in events. Pass
    ///   `None` to fall back to the resolved GStreamer pad name (typical for
    ///   standalone elements). For block-level probes, pass the external pad's
    ///   label or name so the UI can distinguish multiple inputs that share
    ///   the same internal pad name (e.g. all "sink").
    /// - `sample_interval`: measure every Nth buffer (default 1)
    /// - `timeout_secs`: auto-remove after this many seconds (default 60)
    #[allow(clippy::too_many_arguments)]
    pub fn activate(
        &self,
        pipeline: &gst::Pipeline,
        element: &gst::Element,
        element_id: String,
        pad_name: String,
        display_pad_name: Option<String>,
        sample_interval: u32,
        timeout_secs: u32,
    ) -> Result<String, String> {
        let pad = resolve_pad(element, &pad_name)?;

        // For events: the caller-supplied display label, or the resolved
        // GStreamer pad name as fallback.
        let pad_name = display_pad_name.unwrap_or_else(|| pad.name().to_string());

        let probe_id = Uuid::new_v4().to_string();
        let sample_interval = sample_interval.max(1);

        let slot = Arc::new(ProbeSlot::new());

        // Capture values for the probe callback (runs on GStreamer streaming thread).
        // Weak pipeline ref to avoid ref cycle (Pipeline → Pad → Closure → Pipeline).
        let pipeline_weak = pipeline.downgrade();
        let slot_cb = slot.clone();
        let count = SyncCell::new(0u64);

        let gst_probe_id = pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let c = count.get() + 1;
            count.set(c);

            if !c.is_multiple_of(sample_interval as u64) {
                return gst::PadProbeReturn::Ok;
            }

            let Some(pipeline) = pipeline_weak.upgrade() else {
                return gst::PadProbeReturn::Remove;
            };

            if let Some(age_ms) = measure_buffer_age(pad, info, &pipeline) {
                slot_cb.record(age_ms);
            }

            gst::PadProbeReturn::Ok
        });

        let gst_probe_id = match gst_probe_id {
            Some(id) => id,
            None => return Err("Failed to attach pad probe".to_string()),
        };

        // Set up auto-removal timeout
        let probes_ref = self.probes.clone();
        let probe_id_timeout = probe_id.clone();
        let events_timeout = self.events.clone();
        let flow_id_timeout = self.flow_id;
        let timeout_handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(timeout_secs as u64)).await;
            let removed = {
                let mut probes = probes_ref.lock().unwrap();
                probes.remove(&probe_id_timeout)
            };
            if let Some(state) = removed {
                state.pad.remove_probe(state.gst_probe_id.unwrap());
                info!(probe_id = %probe_id_timeout, "Buffer age probe auto-removed after timeout");
                events_timeout.broadcast(StromEvent::BufferAgeProbeDeactivated {
                    flow_id: flow_id_timeout,
                    probe_id: probe_id_timeout,
                    reason: "timeout".to_string(),
                });
            }
        });

        // Broadcast activation event
        self.events.broadcast(StromEvent::BufferAgeProbeActivated {
            flow_id: self.flow_id,
            probe_id: probe_id.clone(),
            element_id: element_id.clone(),
            pad_name: pad_name.clone(),
        });

        // Store probe state
        let state = ProbeState {
            probe_id: probe_id.clone(),
            flow_id: self.flow_id,
            element_id,
            pad_name,
            gst_probe_id: Some(gst_probe_id),
            pad,
            timeout_handle: Some(timeout_handle),
            automatic: false,
            slot,
        };

        self.probes.lock().unwrap().insert(probe_id.clone(), state);

        debug!(probe_id = %probe_id, "Buffer age probe activated");
        Ok(probe_id)
    }

    /// Activate probes on all sink pads of an element.
    /// Returns the list of probe IDs created.
    ///
    /// `display_pad_name` provides a human-friendly label that is used in
    /// events for every probe attached here. Pass `Some` for block-level
    /// activations (so the UI can identify the external input by its label
    /// rather than the internal "sink" pad name); pass `None` for standalone
    /// elements to fall back to the GStreamer pad name per pad.
    pub fn activate_all_sinks(
        &self,
        pipeline: &gst::Pipeline,
        element: &gst::Element,
        element_id: String,
        display_pad_name: Option<String>,
        sample_interval: u32,
        timeout_secs: u32,
    ) -> Result<Vec<String>, String> {
        let sink_pads = element.sink_pads();
        if sink_pads.is_empty() {
            let available: Vec<String> = element
                .pads()
                .into_iter()
                .map(|p| p.name().to_string())
                .collect();
            return Err(format!(
                "No sink pads on element (available pads: {:?})",
                available
            ));
        }

        let mut probe_ids = Vec::new();
        for pad in sink_pads {
            let pad_name = pad.name().to_string();
            match self.activate(
                pipeline,
                element,
                element_id.clone(),
                pad_name.clone(),
                display_pad_name.clone(),
                sample_interval,
                timeout_secs,
            ) {
                Ok(id) => probe_ids.push(id),
                Err(e) => {
                    info!(pad = %pad_name, error = %e, "Skipping pad");
                }
            }
        }

        if probe_ids.is_empty() {
            Err("Failed to attach probe to any sink pad".to_string())
        } else {
            Ok(probe_ids)
        }
    }

    /// Attach automatic buffer age monitoring probes to key measurement points.
    ///
    /// For standalone elements: probes are attached to each sink pad.
    /// For blocks: probes are attached to the internal elements that receive external input.
    ///
    /// Automatic probes only emit `BufferAgeWarning` (via the periodic task) when
    /// the buffer age exceeds the threshold.
    pub fn attach_automatic(
        &self,
        pipeline: &gst::Pipeline,
        elements: &HashMap<String, gst::Element>,
        blocks: &[BlockInstance],
        block_definitions: &HashMap<String, BlockDefinition>,
    ) {
        let mut count = 0;

        // Attach to standalone elements (keys without ':' are standalone)
        for (element_id, element) in elements {
            if element_id.contains(':') || element_id.starts_with("auto_tee_") {
                continue;
            }
            let sink_pads = element.sink_pads();
            if sink_pads.is_empty() {
                continue;
            }
            for pad in sink_pads {
                let pad_name = pad.name().to_string();
                match self.attach_automatic_probe(
                    pipeline,
                    element,
                    element_id.clone(),
                    pad_name.clone(),
                    None,
                ) {
                    Ok(_) => count += 1,
                    Err(e) => {
                        debug!(
                            element = %element_id,
                            pad = %pad_name,
                            error = %e,
                            "Skipping automatic probe"
                        );
                    }
                }
            }
        }

        // Attach to block input pads
        for block in blocks {
            let definition = match block_definitions.get(&block.block_definition_id) {
                Some(def) => def,
                None => {
                    debug!(
                        block = %block.id,
                        definition = %block.block_definition_id,
                        "No block definition found, skipping automatic probes"
                    );
                    continue;
                }
            };

            // Use computed_external_pads if available, otherwise fall back to definition
            let inputs = match &block.computed_external_pads {
                Some(pads) => &pads.inputs,
                None => &definition.external_pads.inputs,
            };

            for input_pad in inputs {
                // Resolve to internal element: "block_id:internal_element_id"
                let element_key = format!("{}:{}", block.id, input_pad.internal_element_id);
                let element = match elements.get(&element_key) {
                    Some(el) => el,
                    None => {
                        debug!(
                            block = %block.id,
                            element_key = %element_key,
                            "Internal element not found for automatic probe"
                        );
                        continue;
                    }
                };

                // Probe the specific internal pad that receives external
                // input. The internal pad name (often "sink") is used to
                // resolve the GStreamer pad; the external pad's label or name
                // is what we display in events so the UI can distinguish
                // multiple inputs that all share the same internal pad name.
                let display_pad_name = input_pad
                    .label
                    .clone()
                    .unwrap_or_else(|| input_pad.name.clone());
                match self.attach_automatic_probe(
                    pipeline,
                    element,
                    block.id.clone(),
                    input_pad.internal_pad_name.clone(),
                    Some(display_pad_name),
                ) {
                    Ok(_) => count += 1,
                    Err(e) => {
                        debug!(
                            block = %block.id,
                            element_key = %element_key,
                            pad = %input_pad.internal_pad_name,
                            error = %e,
                            "Skipping automatic probe on block input"
                        );
                    }
                }
            }
        }

        if count > 0 {
            info!(count, "Automatic buffer age probes attached");
        }
    }

    /// Attach a single automatic probe to a pad.
    ///
    /// `display_pad_name` overrides the pad name reported in events. Pass
    /// `None` to fall back to the resolved GStreamer pad name (used for
    /// standalone elements); pass `Some(label)` for block-level probes so
    /// the UI can identify the external input by its user-visible label.
    fn attach_automatic_probe(
        &self,
        pipeline: &gst::Pipeline,
        element: &gst::Element,
        element_id: String,
        pad_name: String,
        display_pad_name: Option<String>,
    ) -> Result<String, String> {
        let pad = resolve_pad(element, &pad_name)?;

        let pad_name = display_pad_name.unwrap_or_else(|| pad.name().to_string());
        let probe_id = format!("auto-{}", Uuid::new_v4());

        let slot = Arc::new(ProbeSlot::new());

        // Weak pipeline ref to avoid ref cycle (Pipeline → Pad → Closure → Pipeline).
        let pipeline_weak = pipeline.downgrade();
        let slot_cb = slot.clone();
        let count = SyncCell::new(0u64);

        let gst_probe_id = pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let c = count.get() + 1;
            count.set(c);

            if !c.is_multiple_of(AUTO_SAMPLE_INTERVAL as u64) {
                return gst::PadProbeReturn::Ok;
            }

            let Some(pipeline) = pipeline_weak.upgrade() else {
                return gst::PadProbeReturn::Remove;
            };

            if let Some(age_ms) = measure_buffer_age(pad, info, &pipeline) {
                slot_cb.record(age_ms);
            }

            gst::PadProbeReturn::Ok
        });

        let gst_probe_id = match gst_probe_id {
            Some(id) => id,
            None => return Err("Failed to attach pad probe".to_string()),
        };

        let state = ProbeState {
            probe_id: probe_id.clone(),
            flow_id: self.flow_id,
            element_id,
            pad_name,
            gst_probe_id: Some(gst_probe_id),
            pad,
            timeout_handle: None,
            automatic: true,
            slot,
        };

        self.probes.lock().unwrap().insert(probe_id.clone(), state);
        Ok(probe_id)
    }

    /// Deactivate a specific probe.
    pub fn deactivate(&self, probe_id: &str) -> Result<(), String> {
        let state = {
            let mut probes = self.probes.lock().unwrap();
            probes
                .remove(probe_id)
                .ok_or_else(|| format!("Probe '{}' not found", probe_id))?
        };

        if let Some(gst_id) = state.gst_probe_id {
            state.pad.remove_probe(gst_id);
        }
        if let Some(handle) = state.timeout_handle {
            handle.abort();
        }

        self.events
            .broadcast(StromEvent::BufferAgeProbeDeactivated {
                flow_id: self.flow_id,
                probe_id: probe_id.to_string(),
                reason: "manual".to_string(),
            });

        info!(probe_id = %probe_id, "Buffer age probe deactivated");
        Ok(())
    }

    /// List all active manual probes (excludes automatic probes).
    pub fn list(&self) -> Vec<strom_types::api::ProbeInfo> {
        let probes = self.probes.lock().unwrap();
        probes
            .values()
            .filter(|s| !s.automatic)
            .map(|s| strom_types::api::ProbeInfo {
                probe_id: s.probe_id.clone(),
                element_id: s.element_id.clone(),
                pad_name: s.pad_name.clone(),
                sample_count: s.slot.peek_sample_count(),
            })
            .collect()
    }

    /// Deactivate all probes (called when flow stops).
    pub fn deactivate_all(&self) {
        let probes: Vec<ProbeState> = {
            let mut map = self.probes.lock().unwrap();
            map.drain().map(|(_, v)| v).collect()
        };

        let manual_count = probes.iter().filter(|s| !s.automatic).count();
        let auto_count = probes.iter().filter(|s| s.automatic).count();

        for state in probes {
            if let Some(gst_id) = state.gst_probe_id {
                state.pad.remove_probe(gst_id);
            }
            if let Some(handle) = state.timeout_handle {
                handle.abort();
            }

            // Only broadcast deactivation events for manual probes
            if !state.automatic {
                self.events
                    .broadcast(StromEvent::BufferAgeProbeDeactivated {
                        flow_id: state.flow_id,
                        probe_id: state.probe_id.clone(),
                        reason: "flow_stopped".to_string(),
                    });
            }
        }

        if manual_count > 0 || auto_count > 0 {
            debug!(
                manual = manual_count,
                auto = auto_count,
                "Deactivated all buffer age probes"
            );
        }
    }
}

impl Drop for ProbeManager {
    fn drop(&mut self) {
        self.stop_broadcast_task();
        self.deactivate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBroadcaster;
    use crate::gst::pipeline::test_utils::poll_until;

    fn init_gst() {
        gst::init().unwrap();
    }

    fn wait_for_pipeline_playing(pipeline: &gst::Pipeline) {
        poll_until(
            || pipeline.state(gst::ClockTime::from_mseconds(0)).1 == gst::State::Playing,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_secs(5),
            "pipeline did not reach Playing",
        );
    }

    fn create_test_pipeline() -> (gst::Pipeline, gst::Element) {
        let pipeline = gst::Pipeline::builder().name("test-pipeline").build();
        let src = gst::ElementFactory::make("videotestsrc")
            .name("src")
            .property("is-live", true)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .name("sink")
            .build()
            .unwrap();
        pipeline.add_many([&src, &sink]).unwrap();
        src.link(&sink).unwrap();
        (pipeline, sink)
    }

    #[test]
    fn test_resolve_pad_static() {
        init_gst();
        let sink = gst::ElementFactory::make("fakesink")
            .name("test-sink")
            .build()
            .unwrap();
        let pad = resolve_pad(&sink, "sink");
        assert!(pad.is_ok());
        assert_eq!(pad.unwrap().name().as_str(), "sink");
    }

    #[test]
    fn test_resolve_pad_fallback() {
        init_gst();
        let sink = gst::ElementFactory::make("fakesink")
            .name("test-sink")
            .build()
            .unwrap();
        // Request a non-existent pad name; should fall back to "sink"
        let pad = resolve_pad(&sink, "nonexistent");
        assert!(pad.is_ok());
        assert_eq!(pad.unwrap().name().as_str(), "sink");
    }

    #[test]
    fn test_resolve_pad_no_sink() {
        init_gst();
        // videotestsrc has only a src pad, no sink pad
        let src = gst::ElementFactory::make("videotestsrc")
            .name("test-src")
            .build()
            .unwrap();
        let pad = resolve_pad(&src, "sink");
        assert!(pad.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_probe_manager_lifecycle() {
        init_gst();
        let (pipeline, sink) = create_test_pipeline();
        pipeline.set_state(gst::State::Playing).unwrap();

        wait_for_pipeline_playing(&pipeline);

        let flow_id = FlowId::from(uuid::Uuid::new_v4());
        let events = EventBroadcaster::default();
        let pm = ProbeManager::new(flow_id, events);

        // Activate a probe
        let result = pm.activate(
            &pipeline,
            &sink,
            "sink".to_string(),
            "sink".to_string(),
            None,
            1,
            60,
        );
        assert!(result.is_ok());
        let probe_id = result.unwrap();

        // List should show one probe
        let list = pm.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].probe_id, probe_id);

        // Deactivate
        assert!(pm.deactivate(&probe_id).is_ok());

        // List should be empty
        assert!(pm.list().is_empty());

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_probe_manager_deactivate_all() {
        init_gst();
        let (pipeline, sink) = create_test_pipeline();
        pipeline.set_state(gst::State::Playing).unwrap();
        wait_for_pipeline_playing(&pipeline);

        let flow_id = FlowId::from(uuid::Uuid::new_v4());
        let events = EventBroadcaster::default();
        let pm = ProbeManager::new(flow_id, events);

        // Activate two probes
        pm.activate(
            &pipeline,
            &sink,
            "s1".to_string(),
            "sink".to_string(),
            None,
            1,
            60,
        )
        .unwrap();
        pm.activate(
            &pipeline,
            &sink,
            "s2".to_string(),
            "sink".to_string(),
            None,
            1,
            60,
        )
        .unwrap();
        assert_eq!(pm.list().len(), 2);

        pm.deactivate_all();
        assert!(pm.list().is_empty());

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_list_excludes_automatic() {
        init_gst();
        let (pipeline, sink) = create_test_pipeline();
        pipeline.set_state(gst::State::Playing).unwrap();
        wait_for_pipeline_playing(&pipeline);

        let flow_id = FlowId::from(uuid::Uuid::new_v4());
        let events = EventBroadcaster::default();
        let pm = ProbeManager::new(flow_id, events);

        // Activate a manual probe
        pm.activate(
            &pipeline,
            &sink,
            "manual".to_string(),
            "sink".to_string(),
            None,
            1,
            60,
        )
        .unwrap();

        // Attach automatic probes (on the sink element)
        let mut elements = HashMap::new();
        elements.insert("sink".to_string(), sink.clone());
        pm.attach_automatic(&pipeline, &elements, &[], &HashMap::new());

        // list() should only return the manual probe
        let list = pm.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].element_id, "manual");

        // But deactivate_all clears everything
        pm.deactivate_all();
        assert!(pm.list().is_empty());

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn test_deactivate_nonexistent() {
        init_gst();
        let flow_id = FlowId::from(uuid::Uuid::new_v4());
        let events = EventBroadcaster::default();
        let pm = ProbeManager::new(flow_id, events);

        assert!(pm.deactivate("nonexistent").is_err());
    }

    #[test]
    fn test_probe_slot_record_and_read() {
        let slot = ProbeSlot::new();

        slot.record(10);
        slot.record(50);
        slot.record(30);

        assert_eq!(slot.peek_sample_count(), 3);

        let (latest, max, count) = slot.read_and_reset();
        assert_eq!(latest, 30);
        assert_eq!(max, 50);
        assert_eq!(count, 3);

        // max should be reset after read_and_reset
        slot.record(5);
        let (latest, max, _) = slot.read_and_reset();
        assert_eq!(latest, 5);
        assert_eq!(max, 5);
    }
}
