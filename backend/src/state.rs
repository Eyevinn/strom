//! Application state management.

use crate::blocks::BlockRegistry;
use crate::events::EventBroadcaster;
use crate::gst::{ElementDiscovery, PipelineError, PipelineManager};
use crate::storage::{JsonFileStorage, Storage};
use crate::system_monitor::SystemMonitor;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use strom_types::element::{ElementInfo, PropertyValue};
use strom_types::{Flow, FlowId, PipelineState, StromEvent};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// All flows, indexed by ID
    flows: RwLock<HashMap<FlowId, Flow>>,
    /// Storage backend
    storage: Arc<dyn Storage>,
    /// GStreamer element discovery
    element_discovery: RwLock<ElementDiscovery>,
    /// Cached discovered elements (populated once at startup)
    cached_elements: RwLock<Vec<ElementInfo>>,
    /// Active pipelines
    pipelines: RwLock<HashMap<FlowId, PipelineManager>>,
    /// Event broadcaster for real-time updates
    events: EventBroadcaster,
    /// Block registry
    block_registry: BlockRegistry,
    /// System monitor for CPU and GPU statistics
    system_monitor: SystemMonitor,
}

impl AppState {
    /// Create new application state with the given storage backend.
    pub fn new(storage: impl Storage + 'static, blocks_path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                flows: RwLock::new(HashMap::new()),
                storage: Arc::new(storage),
                element_discovery: RwLock::new(ElementDiscovery::new()),
                cached_elements: RwLock::new(Vec::new()),
                pipelines: RwLock::new(HashMap::new()),
                events: EventBroadcaster::default(),
                block_registry: BlockRegistry::new(blocks_path),
                system_monitor: SystemMonitor::new(),
            }),
        }
    }

    /// Get the event broadcaster.
    pub fn events(&self) -> &EventBroadcaster {
        &self.inner.events
    }

    /// Get the block registry.
    pub fn blocks(&self) -> &BlockRegistry {
        &self.inner.block_registry
    }

    /// Create new application state with JSON file storage.
    pub fn with_json_storage(
        flows_path: impl AsRef<std::path::Path>,
        blocks_path: impl Into<PathBuf>,
    ) -> Self {
        Self::new(JsonFileStorage::new(flows_path), blocks_path)
    }

    /// Create new application state with PostgreSQL storage.
    ///
    /// This is an async function that returns a Result because it needs to
    /// connect to the database and run migrations.
    pub async fn with_postgres_storage(
        database_url: &str,
        blocks_path: impl Into<PathBuf>,
    ) -> anyhow::Result<Self> {
        use crate::storage::PostgresStorage;

        let storage = PostgresStorage::new(database_url).await?;
        storage.run_migrations().await?;

        Ok(Self::new(storage, blocks_path))
    }

    /// Load flows from storage into memory.
    pub async fn load_from_storage(&self) -> anyhow::Result<()> {
        info!("Loading flows from storage...");
        match self.inner.storage.load_all().await {
            Ok(mut flows) => {
                let count = flows.len();

                // Reset all flow states to None on server restart since pipelines aren't running
                // This prevents showing stale "Playing" states from before the server stopped
                for flow in flows.values_mut() {
                    if flow.state.is_some() {
                        info!(
                            "Resetting state for flow '{}' from {:?} to None (server restart)",
                            flow.name, flow.state
                        );
                        flow.state = None;
                    }
                }

                let mut state_flows = self.inner.flows.write().await;
                *state_flows = flows;
                info!("Loaded {} flows from storage", count);
            }
            Err(e) => {
                error!("Failed to load flows from storage: {}", e);
                return Err(e.into());
            }
        }

        // Load user-defined blocks
        info!("Loading user-defined blocks...");
        if let Err(e) = self.inner.block_registry.load_user_blocks().await {
            error!("Failed to load user blocks: {}", e);
            // Don't fail startup if blocks can't load
        }

        Ok(())
    }

    /// Discover and cache all available GStreamer elements.
    /// This is called lazily on first request to /api/elements.
    /// Element discovery can crash for certain problematic elements,
    /// but lazy loading means the app starts quickly and crashes are isolated.
    pub async fn discover_and_cache_elements(&self) -> anyhow::Result<()> {
        info!("Discovering and caching GStreamer elements...");

        let elements = {
            let mut discovery = self.inner.element_discovery.write().await;
            discovery.discover_all()
        };

        let count = elements.len();

        {
            let mut cached = self.inner.cached_elements.write().await;
            *cached = elements;
        }

        info!("Discovered and cached {} GStreamer elements", count);
        Ok(())
    }

    /// Compute external pads for all blocks in a flow based on their properties.
    /// This is needed for blocks with dynamic pads (e.g., MPEG-TS/SRT with configurable tracks).
    fn compute_flow_external_pads(flow: &mut Flow) {
        for block in &mut flow.blocks {
            if let Some(builder) = crate::blocks::builtin::get_builder(&block.block_definition_id) {
                block.computed_external_pads = builder.get_external_pads(&block.properties);
            }
        }
    }

    /// Get all flows.
    pub async fn get_flows(&self) -> Vec<Flow> {
        let flows = self.inner.flows.read().await;
        let pipelines = self.inner.pipelines.read().await;

        flows
            .values()
            .map(|flow| {
                let mut flow = flow.clone();
                // Update state, clock sync status, PTP info, and thread priority status for running pipelines
                if let Some(pipeline) = pipelines.get(&flow.id) {
                    flow.state = Some(pipeline.get_state());
                    flow.properties.clock_sync_status = Some(pipeline.get_clock_sync_status());
                    // Get PTP info and check if restart is needed (configured domain differs from running)
                    if let Some(mut ptp_info) = pipeline.get_ptp_info() {
                        let configured_domain = flow.properties.ptp_domain.unwrap_or(0);
                        ptp_info.restart_needed = configured_domain != ptp_info.domain;
                        flow.properties.ptp_info = Some(ptp_info);
                    }
                    flow.properties.thread_priority_status = pipeline.get_thread_priority_status();
                }
                // Compute external pads for dynamic blocks
                Self::compute_flow_external_pads(&mut flow);
                flow
            })
            .collect()
    }

    /// Get a specific flow by ID.
    pub async fn get_flow(&self, id: &FlowId) -> Option<Flow> {
        let flows = self.inner.flows.read().await;
        let pipelines = self.inner.pipelines.read().await;

        flows.get(id).map(|flow| {
            let mut flow = flow.clone();
            // Update state, clock sync status, PTP info, and thread priority status for running pipeline
            if let Some(pipeline) = pipelines.get(id) {
                flow.state = Some(pipeline.get_state());
                flow.properties.clock_sync_status = Some(pipeline.get_clock_sync_status());
                // Get PTP info and check if restart is needed (configured domain differs from running)
                if let Some(mut ptp_info) = pipeline.get_ptp_info() {
                    let configured_domain = flow.properties.ptp_domain.unwrap_or(0);
                    ptp_info.restart_needed = configured_domain != ptp_info.domain;
                    flow.properties.ptp_info = Some(ptp_info);
                }
                flow.properties.thread_priority_status = pipeline.get_thread_priority_status();
            }
            // Compute external pads for dynamic blocks
            Self::compute_flow_external_pads(&mut flow);
            flow
        })
    }

    /// Add or update a flow and persist to storage.
    pub async fn upsert_flow(&self, flow: Flow) -> anyhow::Result<()> {
        let is_new = {
            let flows = self.inner.flows.read().await;
            !flows.contains_key(&flow.id)
        };

        // Update in-memory state
        {
            let mut flows = self.inner.flows.write().await;
            flows.insert(flow.id, flow.clone());
        }

        // Persist to storage
        if let Err(e) = self.inner.storage.save_flow(&flow).await {
            error!("Failed to save flow to storage: {}", e);
            return Err(e.into());
        }

        // Broadcast event
        if is_new {
            self.inner
                .events
                .broadcast(StromEvent::FlowCreated { flow_id: flow.id });
        } else {
            self.inner
                .events
                .broadcast(StromEvent::FlowUpdated { flow_id: flow.id });
        }

        Ok(())
    }

    /// Delete a flow and persist to storage.
    pub async fn delete_flow(&self, id: &FlowId) -> anyhow::Result<bool> {
        // Check if flow exists
        let exists = {
            let flows = self.inner.flows.read().await;
            flows.contains_key(id)
        };

        if !exists {
            return Ok(false);
        }

        // Delete from storage first
        if let Err(e) = self.inner.storage.delete_flow(id).await {
            error!("Failed to delete flow from storage: {}", e);
            return Err(e.into());
        }

        // Delete from in-memory state
        {
            let mut flows = self.inner.flows.write().await;
            flows.remove(id);
        }

        // Broadcast event
        self.inner
            .events
            .broadcast(StromEvent::FlowDeleted { flow_id: *id });

        Ok(true)
    }

    /// Get all discovered GStreamer elements from cache.
    /// Elements are discovered lazily on first request.
    pub async fn discover_elements(&self) -> Vec<ElementInfo> {
        // Check if cache is empty
        {
            let cached = self.inner.cached_elements.read().await;
            if !cached.is_empty() {
                return cached.clone();
            }
        }

        // Cache is empty, perform discovery
        info!("Element cache empty, performing lazy discovery...");
        if let Err(e) = self.discover_and_cache_elements().await {
            error!("Failed to discover elements: {}", e);
            return Vec::new();
        }

        // Return the now-populated cache
        let cached = self.inner.cached_elements.read().await;
        cached.clone()
    }

    /// Get information about a specific element from cache.
    /// This returns the lightweight element info without properties.
    /// Use get_element_info_with_properties() for full element info with properties.
    pub async fn get_element_info(&self, name: &str) -> Option<ElementInfo> {
        let cached = self.inner.cached_elements.read().await;
        cached.iter().find(|e| e.name == name).cloned()
    }

    /// Get element information with properties (lazy loading).
    /// If properties are not yet cached, this will introspect them and update the cache.
    /// Both the ElementDiscovery cache and the cached_elements list are updated.
    pub async fn get_element_info_with_properties(&self, name: &str) -> Option<ElementInfo> {
        // First check if we have full properties already
        {
            let cached = self.inner.cached_elements.read().await;
            if let Some(elem) = cached.iter().find(|e| e.name == name) {
                if !elem.properties.is_empty() {
                    return Some(elem.clone());
                }
            }
        }

        // Properties not cached, need to load them
        let info_with_props = {
            let mut discovery = self.inner.element_discovery.write().await;
            discovery.load_element_properties(name)?
        };

        // Update cached_elements with the properties
        {
            let mut cached = self.inner.cached_elements.write().await;
            if let Some(elem) = cached.iter_mut().find(|e| e.name == name) {
                *elem = info_with_props.clone();
            }
        }

        Some(info_with_props)
    }

    /// Get element information with pad properties (on-demand introspection).
    /// This introspects Request pad properties safely for a single element.
    /// Unlike bulk discovery, this can safely request pads for a specific element.
    pub async fn get_element_pad_properties(&self, name: &str) -> Option<ElementInfo> {
        let mut discovery = self.inner.element_discovery.write().await;
        discovery.load_element_pad_properties(name)
    }

    /// Start a flow (create and start its pipeline).
    pub async fn start_flow(&self, id: &FlowId) -> Result<PipelineState, PipelineError> {
        info!("start_flow called for flow ID: {}", id);

        // Get the flow definition
        info!("Acquiring flows read lock...");
        let flow = {
            let flows = self.inner.flows.read().await;
            info!("Flows read lock acquired, looking up flow...");
            flows.get(id).cloned()
        };
        info!("Flows read lock released");

        let Some(mut flow) = flow else {
            error!("Flow not found: {}", id);
            return Err(PipelineError::InvalidFlow(format!(
                "Flow not found: {}",
                id
            )));
        };

        // Check if pipeline is already running
        info!("Checking if pipeline is already running...");
        {
            let pipelines = self.inner.pipelines.read().await;
            if pipelines.contains_key(id) {
                warn!("Pipeline already running for flow: {}", id);
                return Ok(PipelineState::Playing);
            }
        }
        info!("Pipeline not running, proceeding with start");

        info!("Starting flow: {} ({})", flow.name, id);

        // Compute external pads for all block instances based on their properties
        // This is critical for blocks with dynamic pads (e.g., MPEG-TS/SRT output with configurable audio tracks)
        info!(
            "Computing external pads for {} blocks...",
            flow.blocks.len()
        );
        for block in &mut flow.blocks {
            if let Some(builder) = crate::blocks::builtin::get_builder(&block.block_definition_id) {
                block.computed_external_pads = builder.get_external_pads(&block.properties);
                if let Some(ref pads) = block.computed_external_pads {
                    info!(
                        "Block {} ({}) has {} input(s) and {} output(s)",
                        block.id,
                        block.block_definition_id,
                        pads.inputs.len(),
                        pads.outputs.len()
                    );
                }
            }
        }

        // Create pipeline with event broadcaster and block registry
        info!("Creating PipelineManager (this may block)...");
        let mut manager =
            PipelineManager::new(&flow, self.inner.events.clone(), &self.inner.block_registry)?;
        info!("PipelineManager created successfully");

        // Start pipeline
        info!("Calling manager.start() (this may block)...");
        let state = manager.start()?;
        info!("manager.start() returned with state: {:?}", state);

        // Store pipeline manager and keep a reference for SDP generation
        let pipelines_guard = {
            let mut pipelines = self.inner.pipelines.write().await;
            pipelines.insert(*id, manager);
            // Drop write lock and get read lock
            drop(pipelines);
            self.inner.pipelines.read().await
        };

        // Get PTP clock identity from pipeline if available (for SDP generation)
        let ptp_clock_identity = pipelines_guard
            .get(id)
            .and_then(|p| p.get_ptp_info())
            .and_then(|info| info.grandmaster_clock_id)
            .map(|id| crate::blocks::sdp::convert_clock_id_to_sdp_format(&id));

        // Drop the pipelines guard - we don't need it anymore
        drop(pipelines_guard);

        // Generate SDP for AES67 output blocks and store in runtime_data
        for block in &mut flow.blocks {
            if block.block_definition_id == "builtin.aes67_output" {
                info!(
                    "Generating SDP for AES67 output block: {} in flow {}",
                    block.id, id
                );

                // Extract configured sample rate and channels from block properties
                // (can be Int or String from enum)
                let sample_rate = block.properties.get("sample_rate").and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i as i32),
                    PropertyValue::String(s) => s.parse::<i32>().ok(),
                    _ => None,
                });

                let channels = block.properties.get("channels").and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i as i32),
                    PropertyValue::String(s) => s.parse::<i32>().ok(),
                    _ => None,
                });

                info!(
                    "Using configured format for SDP: {} Hz, {} channels",
                    sample_rate.unwrap_or(48000),
                    channels.unwrap_or(2)
                );

                // Generate SDP with flow properties for correct clock signaling (RFC 7273)
                // Include PTP clock identity if available for accurate ts-refclk attribute
                let sdp = crate::blocks::sdp::generate_aes67_output_sdp(
                    block,
                    &flow.name,
                    sample_rate,
                    channels,
                    Some(&flow.properties),
                    ptp_clock_identity.as_deref(),
                );

                // Initialize runtime_data if needed
                if block.runtime_data.is_none() {
                    block.runtime_data = Some(std::collections::HashMap::new());
                }

                // Store SDP
                if let Some(runtime_data) = &mut block.runtime_data {
                    runtime_data.insert("sdp".to_string(), sdp.clone());
                    info!("Stored SDP for block {}: {} bytes", block.id, sdp.len());
                }
            }
        }

        // Update flow state and persist
        // Note: runtime_data is marked with skip_serializing_if in BlockInstance,
        // so it won't be persisted to storage (which is correct - it's runtime-only data)
        flow.state = Some(state);
        flow.properties.auto_restart = true; // Enable auto-restart when flow is started
        {
            let mut flows = self.inner.flows.write().await;
            flows.insert(*id, flow.clone());
        }
        if let Err(e) = self.inner.storage.save_flow(&flow).await {
            error!("Failed to save flow state: {}", e);
        }

        // Broadcast events
        self.inner
            .events
            .broadcast(StromEvent::FlowStarted { flow_id: *id });
        self.inner.events.broadcast(StromEvent::FlowStateChanged {
            flow_id: *id,
            state: format!("{:?}", state),
        });
        // Broadcast FlowUpdated so frontend sees the new runtime_data with SDP
        self.inner
            .events
            .broadcast(StromEvent::FlowUpdated { flow_id: *id });

        Ok(state)
    }

    /// Stop a flow (stop and remove its pipeline).
    pub async fn stop_flow(&self, id: &FlowId) -> Result<PipelineState, PipelineError> {
        info!("Stopping flow: {}", id);

        // Get and remove the pipeline
        let manager = {
            let mut pipelines = self.inner.pipelines.write().await;
            pipelines.remove(id)
        };

        let Some(mut manager) = manager else {
            warn!("No active pipeline for flow: {}", id);
            return Ok(PipelineState::Null);
        };

        // Stop the pipeline
        let state = manager.stop()?;

        // Clear runtime_data from all blocks (SDP is only valid while running)
        let flow = {
            let mut flows = self.inner.flows.write().await;
            if let Some(flow) = flows.get_mut(id) {
                info!("Clearing runtime_data from {} blocks", flow.blocks.len());
                for block in &mut flow.blocks {
                    if block.runtime_data.is_some() {
                        info!(
                            "Clearing runtime_data for block {} (was {} entries)",
                            block.id,
                            block.runtime_data.as_ref().unwrap().len()
                        );
                        block.runtime_data = None;
                    }
                }
                flow.state = Some(state);
                flow.properties.auto_restart = false; // Disable auto-restart when manually stopped
                Some(flow.clone())
            } else {
                None
            }
        };

        if let Some(flow) = flow {
            if let Err(e) = self.inner.storage.save_flow(&flow).await {
                error!("Failed to save flow state: {}", e);
            }
        }

        // Broadcast events
        self.inner
            .events
            .broadcast(StromEvent::FlowStopped { flow_id: *id });
        self.inner.events.broadcast(StromEvent::FlowStateChanged {
            flow_id: *id,
            state: format!("{:?}", state),
        });
        // Broadcast FlowUpdated so frontend sees the cleared runtime_data
        self.inner
            .events
            .broadcast(StromEvent::FlowUpdated { flow_id: *id });

        Ok(state)
    }

    /// Get the state of a flow's pipeline.
    pub async fn get_flow_state(&self, id: &FlowId) -> Option<PipelineState> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines.get(id).map(|p| p.get_state())
    }

    /// Generate a debug DOT graph for a flow's pipeline.
    /// Returns the DOT graph content as a string.
    pub async fn generate_debug_graph(&self, id: &FlowId) -> Option<String> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines.get(id).map(|p| p.generate_dot_graph())
    }

    /// Update a property on a running pipeline element.
    pub async fn update_element_property(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        property_name: &str,
        value: PropertyValue,
    ) -> Result<(), PipelineError> {
        info!(
            "Updating property {}.{} in flow {}",
            element_id, property_name, flow_id
        );

        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.update_element_property(element_id, property_name, &value)?;

        // Broadcast property change event
        self.inner.events.broadcast(StromEvent::PropertyChanged {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
            property_name: property_name.to_string(),
            value,
        });

        Ok(())
    }

    /// Get current property values from a running element.
    pub async fn get_element_properties(
        &self,
        flow_id: &FlowId,
        element_id: &str,
    ) -> Result<HashMap<String, PropertyValue>, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.get_element_properties(element_id)
    }

    /// Get a single property value from a running element.
    pub async fn get_element_property(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        property_name: &str,
    ) -> Result<PropertyValue, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.get_element_property(element_id, property_name)
    }

    /// Update a property on a pad in a running pipeline.
    pub async fn update_pad_property(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        pad_name: &str,
        property_name: &str,
        value: PropertyValue,
    ) -> Result<(), PipelineError> {
        info!(
            "Updating pad property {}:{}:{} in flow {}",
            element_id, pad_name, property_name, flow_id
        );

        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.update_pad_property(element_id, pad_name, property_name, &value)?;

        // Broadcast pad property change event
        self.inner.events.broadcast(StromEvent::PadPropertyChanged {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
            pad_name: pad_name.to_string(),
            property_name: property_name.to_string(),
            value,
        });

        Ok(())
    }

    /// Get current property values from a running pad.
    pub async fn get_pad_properties(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        pad_name: &str,
    ) -> Result<HashMap<String, PropertyValue>, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.get_pad_properties(element_id, pad_name)
    }

    /// Get a single property value from a running pad.
    pub async fn get_pad_property(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        pad_name: &str,
        property_name: &str,
    ) -> Result<PropertyValue, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.get_pad_property(element_id, pad_name, property_name)
    }

    /// Get WebRTC statistics from a running flow's pipeline.
    pub async fn get_webrtc_stats(
        &self,
        flow_id: &FlowId,
    ) -> Result<strom_types::api::WebRtcStats, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        Ok(manager.get_webrtc_stats())
    }

    /// Query the latency of a running pipeline.
    /// Returns (min_latency_ns, max_latency_ns, live) if query succeeds.
    pub async fn get_flow_latency(&self, flow_id: &FlowId) -> Option<(u64, u64, bool)> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines.get(flow_id).and_then(|p| p.query_latency())
    }

    /// Get statistics for a running flow.
    /// Returns statistics from all blocks that support statistics collection.
    pub async fn get_flow_stats(&self, flow_id: &FlowId) -> Option<strom_types::stats::FlowStats> {
        use crate::stats::StatsCollector;

        let pipelines = self.inner.pipelines.read().await;
        let flows = self.inner.flows.read().await;

        let pipeline = pipelines.get(flow_id)?;
        let flow = flows.get(flow_id)?;

        Some(StatsCollector::collect_flow_stats(
            pipeline.pipeline(),
            flow,
        ))
    }

    /// Get current system monitoring statistics (CPU and GPU).
    pub async fn get_system_stats(&self) -> strom_types::SystemStats {
        self.inner.system_monitor.collect_stats().await
    }

    /// Get PTP statistics events for all running pipelines with PTP clocks.
    /// Returns a Vec of StromEvent::PtpStats for each flow with an active PTP clock.
    pub async fn get_ptp_stats_events(&self) -> Vec<StromEvent> {
        let pipelines = self.inner.pipelines.read().await;
        let mut events = Vec::new();

        for (flow_id, pipeline) in pipelines.iter() {
            if let Some(ptp_info) = pipeline.get_ptp_info() {
                events.push(StromEvent::PtpStats {
                    flow_id: *flow_id,
                    domain: ptp_info.domain,
                    synced: ptp_info.synced,
                    mean_path_delay_ns: ptp_info.stats.as_ref().and_then(|s| s.mean_path_delay_ns),
                    clock_offset_ns: ptp_info.stats.as_ref().and_then(|s| s.clock_offset_ns),
                    r_squared: ptp_info.stats.as_ref().and_then(|s| s.r_squared),
                    clock_rate: ptp_info.stats.as_ref().and_then(|s| s.clock_rate),
                });
            }
        }

        events
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::with_json_storage("flows.json", "blocks.json")
    }
}
