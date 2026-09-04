//! GStreamer pipeline management.

mod bus;
mod construction;
pub(crate) mod effects;
mod lifecycle;
mod linking;
mod properties;
mod srt;
mod state;
mod webrtc;

use crate::events::EventBroadcaster;
use crate::gst::thread_priority::ThreadPriorityState;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_net as gst_net;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use strom_types::{BlockDefinition, BlockInstance, FlowId, Link, PipelineState, PropertyValue};
use thiserror::Error;
use tracing::debug;

/// Result of processing links with automatic tee insertion.
struct ProcessedLinks {
    /// Final list of links (including links to/from tees)
    links: Vec<Link>,
    /// Map of tee element IDs to their source spec (element:pad they're connected to)
    tees: HashMap<String, String>,
}

/// Aggregated QoS statistics for a single element.
#[derive(Debug, Clone)]
struct ElementQoSStats {
    event_count: u64,
    sum_proportion: f64,
    min_proportion: f64,
    max_proportion: f64,
    sum_jitter: i64,
    total_processed: u64,
}

impl ElementQoSStats {
    fn new() -> Self {
        Self {
            event_count: 0,
            sum_proportion: 0.0,
            min_proportion: f64::MAX,
            max_proportion: f64::MIN,
            sum_jitter: 0,
            total_processed: 0,
        }
    }

    fn add_event(&mut self, proportion: f64, jitter: i64, processed: u64) {
        self.event_count = self.event_count.saturating_add(1);
        self.sum_proportion += proportion;
        self.min_proportion = self.min_proportion.min(proportion);
        self.max_proportion = self.max_proportion.max(proportion);
        // Saturating add: QoS jitter values can be huge when the pipeline clock is
        // NTP/TAI (time since 1900/1970 in ns), and the running sum may overflow i64.
        self.sum_jitter = self.sum_jitter.saturating_add(jitter);
        self.total_processed = processed; // Keep the latest value
    }

    fn avg_proportion(&self) -> f64 {
        if self.event_count > 0 {
            self.sum_proportion / self.event_count as f64
        } else {
            0.0
        }
    }

    fn avg_jitter(&self) -> i64 {
        if self.event_count > 0 {
            self.sum_jitter / self.event_count as i64
        } else {
            0
        }
    }
}

/// QoS statistics aggregator (collects QoS events and broadcasts periodically).
#[derive(Debug, Clone)]
struct QoSAggregator {
    stats: Arc<Mutex<HashMap<String, ElementQoSStats>>>,
}

impl QoSAggregator {
    fn new() -> Self {
        Self {
            stats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn add_event(&self, element_name: String, proportion: f64, jitter: i64, processed: u64) {
        let mut stats = self.stats.lock().unwrap();
        stats
            .entry(element_name)
            .or_insert_with(ElementQoSStats::new)
            .add_event(proportion, jitter, processed);
    }

    fn extract_and_reset(&self) -> HashMap<String, ElementQoSStats> {
        let mut stats = self.stats.lock().unwrap();
        std::mem::take(&mut *stats)
    }
}

#[derive(Error, Debug)]
pub enum PipelineError {
    #[error("GStreamer error: {0}")]
    GStreamer(#[from] gst::glib::Error),

    #[error("GStreamer boolean error: {0}")]
    BoolError(#[from] gst::glib::BoolError),

    #[error("Element not found: {0}")]
    ElementNotFound(String),

    #[error("Failed to create element: {0}")]
    ElementCreation(String),

    #[error("Failed to link elements: {0} -> {1}")]
    LinkError(String, String),

    #[error("Invalid property value for {element}.{property}: {reason}")]
    InvalidProperty {
        element: String,
        property: String,
        reason: String,
    },

    #[error("Pipeline state change failed: {0}")]
    StateChange(String),

    #[error("Invalid flow: {0}")]
    InvalidFlow(String),

    #[error("Flow not found: {0}")]
    FlowNotFound(String),

    #[error("Property {property} on element {element} cannot be changed in {state:?} state")]
    PropertyNotMutable {
        element: String,
        property: String,
        state: PipelineState,
    },

    #[error("Pad not found: {element}:{pad}")]
    PadNotFound { element: String, pad: String },

    #[error("Transition error: {0}")]
    TransitionError(String),

    #[error("Thumbnail capture error: {0}")]
    ThumbnailCapture(String),

    #[error("Endpoint conflict: {0}")]
    EndpointConflict(String),
}

/// Manages a single GStreamer pipeline for a flow.
pub struct PipelineManager {
    flow_id: FlowId,
    flow_name: String,
    pipeline: gst::Pipeline,
    elements: HashMap<String, gst::Element>,
    events: EventBroadcaster,
    /// Pending links that couldn't be made because source pads don't exist yet (dynamic pads)
    pending_links: Vec<Link>,
    /// Flow properties (clock configuration, etc.)
    properties: strom_types::flow::FlowProperties,
    /// Pad properties to apply after pads are created (element_id -> (pad_name -> properties))
    pad_properties: HashMap<String, HashMap<String, HashMap<String, PropertyValue>>>,
    /// Block-specific bus message handler IDs (allows blocks to register their own bus message handlers)
    block_message_handlers: Vec<gst::glib::SignalHandlerId>,
    /// Bus message handler connection functions from blocks (called when pipeline starts)
    block_message_connect_fns: Vec<crate::blocks::BusMessageConnectFn>,
    /// Element signal setup functions from blocks (called when pipeline starts)
    element_setup_fns: Vec<crate::blocks::ElementSetupFn>,
    /// Thread priority state tracker (tracks whether priority was successfully set)
    thread_priority_state: Option<ThreadPriorityState>,
    /// Thread registry for tracking streaming threads (optional, for CPU monitoring)
    thread_registry: Option<crate::thread_registry::ThreadRegistry>,
    /// CPU set assigned by AffinityManager for SingleCore affinity (None for Off).
    /// Contains all logical CPUs (hyperthreads) of the allocated physical core.
    assigned_cpus: Option<Vec<usize>>,
    /// Shared thread config for dynamic session pipelines (WHEP/WebRTC).
    /// Populated in start() so consumer-added callbacks can install sync handlers.
    session_thread_config: crate::gst::SessionThreadConfig,
    /// Cached pipeline state to avoid querying async sinks during initialization
    cached_state: std::sync::Arc<std::sync::RwLock<PipelineState>>,
    /// QoS statistics aggregator (collects and periodically broadcasts QoS events)
    qos_aggregator: QoSAggregator,
    /// Handle for the periodic QoS stats broadcast task
    qos_broadcast_task: Option<tokio::task::JoinHandle<()>>,
    /// PTP clock reference (stored for querying grandmaster/master info)
    ptp_clock: Option<gst_net::PtpClock>,
    /// PTP statistics (updated by statistics callback)
    ptp_stats: std::sync::Arc<std::sync::RwLock<Option<strom_types::flow::PtpStats>>>,
    /// PTP statistics callback handle (must be kept alive)
    #[allow(dead_code)]
    ptp_stats_callback: Option<gst_net::PtpStatisticsCallback>,
    /// NTP clock reference (stored for querying calibration and sync status)
    ntp_clock: Option<gst_net::NtpClock>,
    /// Dynamic pads that were auto-linked to tees because no link was defined
    /// Maps element_id -> {pad_name -> tee_element_name}
    /// These tees have allow-not-linked=true so unlinked streams don't block the pipeline
    dynamic_pad_tees: std::sync::Arc<std::sync::RwLock<HashMap<String, HashMap<String, String>>>>,
    /// WHEP endpoints registered by blocks
    whep_endpoints: Vec<crate::blocks::WhepEndpointInfo>,
    /// WHIP endpoints registered by blocks
    whip_endpoints: Vec<crate::blocks::WhipEndpointInfo>,
    /// WHIP endpoint configs for session manager (collected from block expansion)
    whip_endpoint_configs: Vec<(String, crate::whip_session_manager::WhipEndpointConfig)>,
    /// Dynamically created webrtcbins (from webrtcsink/whepserversink consumer-added callbacks).
    /// Maps block_id to list of (consumer_id, webrtcbin) pairs.
    dynamic_webrtcbins: crate::blocks::DynamicWebrtcbinStore,
    /// Thumbnail taps for compositor inputs (block_id → per-input taps)
    thumbnail_taps: crate::gst::ThumbnailTapStore,
    /// Handle for the periodic thumbnail deactivation task
    thumbnail_deactivation_task: Option<tokio::task::JoinHandle<()>>,
    /// Buffer age probe manager for on-demand pad probing
    probe_manager: crate::gst::buffer_age_probe::ProbeManager,
    /// Block instances from the flow (needed for automatic probe attachment at start)
    blocks: Vec<BlockInstance>,
    /// Block definitions for blocks used in this flow (resolved at construction time)
    block_definitions: HashMap<String, BlockDefinition>,
    /// Per-element control sources for smooth volume/mute transitions on
    /// audio `volume` elements. Eliminates zipper noise on fader drags and
    /// click artifacts on mute toggles.
    volume_ramps: crate::gst::volume_ramp::VolumeRampManager,
}

impl Drop for PipelineManager {
    fn drop(&mut self) {
        debug!("Dropping pipeline for flow: {}", self.flow_name);
        // Stop thumbnail deactivation task
        if let Some(task) = self.thumbnail_deactivation_task.take() {
            task.abort();
        }
        // Stop broadcast task and deactivate probes before pipeline goes to Null
        self.probe_manager.stop_broadcast_task();
        self.probe_manager.deactivate_all();
        self.stop_qos_broadcast_task();

        // Ensure pipeline is in Null state before releasing references.
        // If stop() was already called this is a no-op.
        let pipeline = self.pipeline.clone();
        let _ = std::thread::spawn(move || pipeline.set_state(gst::State::Null)).join();
    }
}

#[cfg(test)]
pub(crate) mod test_utils {
    use std::time::{Duration, Instant};

    /// Poll `condition` every `interval` until it returns `true` or `timeout` expires.
    /// Panics with `msg` on timeout. Tests that pass complete as fast as the
    /// condition is met instead of sleeping a fixed duration.
    pub fn poll_until(
        condition: impl Fn() -> bool,
        interval: Duration,
        timeout: Duration,
        msg: &str,
    ) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if condition() {
                return;
            }
            std::thread::sleep(interval);
        }
        panic!("timed out after {:?}: {}", timeout, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::BlockRegistry;
    use strom_types::{Element, Flow};

    fn create_test_flow() -> Flow {
        let mut flow = Flow::new("Test Pipeline");
        flow.elements = vec![
            Element {
                id: "src".to_string(),
                element_type: "videotestsrc".to_string(),
                properties: HashMap::from([("is-live".to_string(), PropertyValue::Bool(true))]),
                pad_properties: HashMap::new(),
                position: (0.0, 0.0),
            },
            Element {
                id: "sink".to_string(),
                element_type: "fakesink".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (100.0, 0.0),
            },
        ];
        flow.links = vec![Link {
            from: "src".to_string(),
            to: "sink".to_string(),
        }];
        flow
    }

    fn default_test_ice_servers() -> Vec<String> {
        vec!["stun:stun.l.google.com:19302".to_string()]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_create_pipeline() {
        gst::init().unwrap();
        let flow = create_test_flow();
        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(
            &flow,
            events,
            &registry,
            default_test_ice_servers(),
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        );
        assert!(manager.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_start_stop_pipeline() {
        gst::init().unwrap();
        let flow = create_test_flow();
        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let mut manager = PipelineManager::new(
            &flow,
            events,
            &registry,
            default_test_ice_servers(),
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .unwrap();

        // Start pipeline
        let state = manager.start();
        assert!(state.is_ok());
        assert!(
            state.unwrap().is_active(),
            "expected active state after start()"
        );

        // Verify ongoing state
        assert!(manager.get_state().is_active());

        // Stop pipeline
        let state = manager.stop();
        assert!(state.is_ok());
        assert_eq!(state.unwrap(), PipelineState::Null);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_invalid_element() {
        gst::init().unwrap();
        let mut flow = create_test_flow();
        flow.elements[0].element_type = "nonexistentelement".to_string();

        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(
            &flow,
            events,
            &registry,
            default_test_ice_servers(),
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        );
        assert!(manager.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_auto_tee_insertion() {
        gst::init().unwrap();

        // Create a flow with one source and two sinks (should auto-insert a tee)
        let mut flow = Flow::new("Auto-Tee Test");
        flow.elements = vec![
            Element {
                id: "src".to_string(),
                element_type: "videotestsrc".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (0.0, 0.0),
            },
            Element {
                id: "sink1".to_string(),
                element_type: "fakesink".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (100.0, 0.0),
            },
            Element {
                id: "sink2".to_string(),
                element_type: "fakesink".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (100.0, 100.0),
            },
        ];
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "sink1".to_string(),
            },
            Link {
                from: "src".to_string(),
                to: "sink2".to_string(),
            },
        ];

        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(
            &flow,
            events,
            &registry,
            default_test_ice_servers(),
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        );
        assert!(manager.is_ok());

        let manager = manager.unwrap();
        // Should have 3 original elements + 1 auto-inserted tee
        assert_eq!(manager.elements.len(), 4);
        // Check that tee element was created
        assert!(manager.elements.contains_key("auto_tee_src"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_tee_insertion_when_not_needed() {
        gst::init().unwrap();

        let flow = create_test_flow(); // Simple 1-to-1 connection

        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(
            &flow,
            events,
            &registry,
            default_test_ice_servers(),
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .unwrap();

        // Should have only 2 original elements, no tee
        assert_eq!(manager.elements.len(), 2);
        assert!(!manager.elements.contains_key("auto_tee_src"));
    }
}
