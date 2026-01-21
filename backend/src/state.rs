//! Application state management.

use crate::blocks::BlockRegistry;
use crate::discovery::DiscoveryService;
use crate::events::EventBroadcaster;
use crate::gst::{ElementDiscovery, PipelineError, PipelineManager};
use crate::ptp_monitor::PtpMonitor;
use crate::sharing::ChannelRegistry;
use crate::storage::{JsonFileStorage, Storage};
use crate::system_monitor::SystemMonitor;
use crate::whep_registry::WhepRegistry;
use chrono::Local;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use strom_types::element::{ElementInfo, PropertyValue};
use strom_types::{Flow, FlowId, PipelineState, StromEvent};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

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
    /// Channel registry for inter-pipeline sharing
    channel_registry: ChannelRegistry,
    /// AES67 stream discovery service (SAP/mDNS)
    discovery: DiscoveryService,
    /// PTP clock monitoring service
    ptp_monitor: PtpMonitor,
    /// Media files directory path
    media_path: PathBuf,
    /// WHEP endpoint registry (maps endpoint IDs to internal ports)
    whep_registry: WhepRegistry,
    /// ICE servers for WebRTC NAT traversal (STUN/TURN URLs)
    ice_servers: Vec<String>,
}

impl AppState {
    /// Create new application state with the given storage backend.
    pub fn new(
        storage: impl Storage + 'static,
        blocks_path: impl Into<PathBuf>,
        media_path: impl Into<PathBuf>,
        ice_servers: Vec<String>,
        sap_multicast_addresses: Vec<String>,
    ) -> Self {
        let events = EventBroadcaster::default();
        Self {
            inner: Arc::new(AppStateInner {
                flows: RwLock::new(HashMap::new()),
                storage: Arc::new(storage),
                element_discovery: RwLock::new(ElementDiscovery::new()),
                cached_elements: RwLock::new(Vec::new()),
                pipelines: RwLock::new(HashMap::new()),
                events: events.clone(),
                block_registry: BlockRegistry::new(blocks_path),
                system_monitor: SystemMonitor::new(),
                channel_registry: ChannelRegistry::new(),
                discovery: DiscoveryService::new(events, sap_multicast_addresses.clone()),
                ptp_monitor: PtpMonitor::new(),
                media_path: media_path.into(),
                whep_registry: WhepRegistry::new(),
                ice_servers,
            }),
        }
    }

    /// Get the WHEP endpoint registry.
    pub fn whep_registry(&self) -> &WhepRegistry {
        &self.inner.whep_registry
    }

    /// Get the event broadcaster.
    pub fn events(&self) -> &EventBroadcaster {
        &self.inner.events
    }

    /// Get the block registry.
    pub fn blocks(&self) -> &BlockRegistry {
        &self.inner.block_registry
    }

    /// Get the channel registry for inter-pipeline sharing.
    pub fn channels(&self) -> &ChannelRegistry {
        &self.inner.channel_registry
    }

    /// Get the discovery service for AES67 streams.
    pub fn discovery(&self) -> &DiscoveryService {
        &self.inner.discovery
    }

    /// Get the PTP clock monitor.
    pub fn ptp_monitor(&self) -> &PtpMonitor {
        &self.inner.ptp_monitor
    }

    /// Get the media files directory path.
    pub fn media_path(&self) -> &PathBuf {
        &self.inner.media_path
    }

    /// Get the configured ICE servers for WebRTC.
    pub fn ice_servers(&self) -> &[String] {
        &self.inner.ice_servers
    }

    /// Start background services (SAP discovery, etc).
    pub async fn start_services(&self) {
        info!("Starting discovery service (SAP listener and announcer)...");
        if let Err(e) = self.inner.discovery.start().await {
            warn!("Failed to start discovery service: {}", e);
        }
    }

    /// Create new application state with JSON file storage.
    pub fn with_json_storage(
        flows_path: impl AsRef<std::path::Path>,
        blocks_path: impl Into<PathBuf>,
        media_path: impl Into<PathBuf>,
        ice_servers: Vec<String>,
        sap_multicast_addresses: Vec<String>,
    ) -> Self {
        Self::new(
            JsonFileStorage::new(flows_path),
            blocks_path,
            media_path,
            ice_servers,
            sap_multicast_addresses,
        )
    }

    /// Create new application state with PostgreSQL storage.
    ///
    /// This is an async function that returns a Result because it needs to
    /// connect to the database and run migrations.
    pub async fn with_postgres_storage(
        database_url: &str,
        blocks_path: impl Into<PathBuf>,
        media_path: impl Into<PathBuf>,
        ice_servers: Vec<String>,
        sap_multicast_addresses: Vec<String>,
    ) -> anyhow::Result<Self> {
        use crate::storage::PostgresStorage;

        let storage = PostgresStorage::new(database_url).await?;
        storage.run_migrations().await?;

        Ok(Self::new(
            storage,
            blocks_path,
            media_path,
            ice_servers,
            sap_multicast_addresses,
        ))
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
                        debug!(
                            "Resetting state for flow '{}' from {:?} to None (server restart)",
                            flow.name, flow.state
                        );
                        flow.state = None;
                    }
                }

                // Register flows with PTP monitor for those that have PTP configured
                for flow in flows.values() {
                    if flow.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp {
                        let domain = flow.properties.ptp_domain.unwrap_or(0);
                        if let Err(e) = self.inner.ptp_monitor.register_flow(flow.id, domain) {
                            warn!(
                                "Failed to register flow '{}' with PTP monitor: {}",
                                flow.name, e
                            );
                        } else {
                            info!(
                                "Registered flow '{}' with PTP monitor (domain {})",
                                flow.name, domain
                            );
                        }
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
                } else {
                    // Clear runtime-only status when no pipeline is running
                    flow.properties.thread_priority_status = None;
                    flow.properties.clock_sync_status = None;
                    flow.properties.ptp_info = None;
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
            } else {
                // Clear runtime-only status when no pipeline is running
                flow.properties.thread_priority_status = None;
                flow.properties.clock_sync_status = None;
                flow.properties.ptp_info = None;
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

        // Register/unregister with PTP monitor based on clock configuration
        if flow.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp {
            let domain = flow.properties.ptp_domain.unwrap_or(0);
            if let Err(e) = self.inner.ptp_monitor.register_flow(flow.id, domain) {
                warn!("Failed to register flow with PTP monitor: {}", e);
            }
        } else {
            // Flow doesn't use PTP - unregister if it was previously registered
            self.inner.ptp_monitor.unregister_flow(flow.id);
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

        // Unregister from PTP monitor
        self.inner.ptp_monitor.unregister_flow(*id);

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
        let mut manager = PipelineManager::new(
            &flow,
            self.inner.events.clone(),
            &self.inner.block_registry,
            self.inner.ice_servers.clone(),
        )?;
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

        // Collect and register WHEP endpoints from blocks
        let whep_endpoints: Vec<(String, String)> = if let Some(manager) = pipelines_guard.get(id) {
            let mut endpoints = Vec::new();
            for whep_info in manager.whep_endpoints() {
                info!(
                    "Registering WHEP endpoint '{}' (block {}) on port {} mode={:?}",
                    whep_info.endpoint_id,
                    whep_info.block_id,
                    whep_info.internal_port,
                    whep_info.mode
                );
                self.inner
                    .whep_registry
                    .register(
                        whep_info.endpoint_id.clone(),
                        whep_info.internal_port,
                        whep_info.mode,
                    )
                    .await;
                endpoints.push((whep_info.block_id.clone(), whep_info.endpoint_id.clone()));
            }
            endpoints
        } else {
            Vec::new()
        };

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

                // Get the multicast destination address for routing lookup
                let multicast_host = block
                    .properties
                    .get("host")
                    .and_then(|v| {
                        if let PropertyValue::String(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "239.69.1.1".to_string());

                // Determine origin IP:
                // 1. If interface is explicitly set, use that interface's IP
                // 2. Otherwise, ask the kernel which source IP it would use for the multicast address
                //    This respects the routing table and ensures the SDP origin matches actual traffic
                let origin_ip = block
                    .properties
                    .get("interface")
                    .and_then(|v| {
                        if let PropertyValue::String(s) = v {
                            if !s.is_empty() {
                                crate::network::get_interface_ipv4(s).map(|ip| ip.to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // Query kernel for the source IP it would use for this multicast destination
                        crate::network::get_source_ipv4_for_destination(&multicast_host)
                            .map(|ip| ip.to_string())
                    })
                    .or_else(|| crate::network::get_default_ipv4().map(|ip| ip.to_string()));

                // Check if RAVENNA extensions are enabled for this block
                let ravenna_extensions = block
                    .properties
                    .get("ravenna_extensions")
                    .map(|v| matches!(v, PropertyValue::Bool(true)))
                    .unwrap_or(false);

                // Get session name: use custom if set, otherwise fall back to flow name
                let session_name = block
                    .properties
                    .get("session_name")
                    .and_then(|v| match v {
                        PropertyValue::String(s) if !s.trim().is_empty() => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| flow.name.clone());
                let session_name = crate::blocks::sdp::sanitize_session_name(&session_name);

                // Generate SDP with flow properties for correct clock signaling (RFC 7273)
                // Include PTP clock identity if available for accurate ts-refclk attribute
                let sdp = crate::blocks::sdp::generate_aes67_output_sdp(
                    block,
                    &session_name,
                    sample_rate,
                    channels,
                    Some(&flow.properties),
                    ptp_clock_identity.as_deref(),
                    origin_ip.as_deref(),
                    ravenna_extensions,
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

                // Get interface from block properties for SAP announcement filtering
                let announce_interface = block.properties.get("interface").and_then(|v| {
                    if let PropertyValue::String(s) = v {
                        if !s.is_empty() {
                            Some(s.as_str())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

                if let Some(iface) = announce_interface {
                    info!(
                        "AES67 output block {} will announce SAP only on interface {}",
                        block.id, iface
                    );
                }

                // Register stream for SAP announcement
                self.inner
                    .discovery
                    .announce_stream(*id, &block.id, &sdp, announce_interface)
                    .await;
            }

            // Store endpoint_id in runtime_data for WHEP output blocks
            if block.block_definition_id == "builtin.whep_output" {
                if let Some((_, endpoint_id)) =
                    whep_endpoints.iter().find(|(bid, _)| bid == &block.id)
                {
                    info!(
                        "Storing WHEP endpoint_id '{}' for block {} in runtime_data",
                        endpoint_id, block.id
                    );

                    // Initialize runtime_data if needed
                    if block.runtime_data.is_none() {
                        block.runtime_data = Some(std::collections::HashMap::new());
                    }

                    // Store endpoint_id
                    if let Some(runtime_data) = &mut block.runtime_data {
                        runtime_data.insert("whep_endpoint_id".to_string(), endpoint_id.clone());
                    }
                }
            }
        }

        // Register channels for InterOutput blocks
        for block in &flow.blocks {
            if block.block_definition_id == "builtin.inter_output" {
                // Channel name is auto-generated from flow_id + block_id
                let channel_name = format!("strom_{}_{}", id, block.id);

                let media_type = block
                    .properties
                    .get("media_type")
                    .and_then(|v| match v {
                        PropertyValue::String(s) => match s.as_str() {
                            "video" => Some(strom_types::MediaType::Video),
                            "audio" => Some(strom_types::MediaType::Audio),
                            _ => Some(strom_types::MediaType::Generic),
                        },
                        _ => None,
                    })
                    .unwrap_or(strom_types::MediaType::Generic);

                info!(
                    "Registering inter channel '{}' from flow {} block {}",
                    channel_name, id, block.id
                );

                self.inner
                    .channel_registry
                    .register(crate::sharing::ChannelInfo {
                        source_flow_id: *id,
                        output_name: block.id.clone(),
                        channel_name: channel_name.clone(),
                        media_type,
                    })
                    .await;

                // Broadcast event for subscribers
                self.inner
                    .events
                    .broadcast(StromEvent::SourceOutputAvailable {
                        source_flow_id: *id,
                        output_name: block.id.clone(),
                        channel_name: channel_name.clone(),
                    });
            }
        }

        // Update flow state and persist
        // Note: runtime_data is marked with skip_serializing_if in BlockInstance,
        // so it won't be persisted to storage (which is correct - it's runtime-only data)
        flow.state = Some(state);
        flow.properties.auto_restart = true; // Enable auto-restart when flow is started
        flow.properties.started_at = Some(Local::now().to_rfc3339()); // Record when flow started
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

        // Unregister WHEP endpoints before stopping
        for whep_info in manager.whep_endpoints() {
            info!(
                "Unregistering WHEP endpoint '{}' (block {})",
                whep_info.endpoint_id, whep_info.block_id
            );
            self.inner
                .whep_registry
                .unregister(&whep_info.endpoint_id)
                .await;
        }

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
                flow.properties.started_at = None; // Clear started_at when stopped
                Some(flow.clone())
            } else {
                None
            }
        };

        if let Some(ref flow) = flow {
            if let Err(e) = self.inner.storage.save_flow(flow).await {
                error!("Failed to save flow state: {}", e);
            }

            // Unregister inter channels and broadcast events
            for block in &flow.blocks {
                if block.block_definition_id == "builtin.inter_output" {
                    // Channel name is auto-generated from flow_id + block_id
                    let channel_name = format!("strom_{}_{}", id, block.id);

                    info!(
                        "Unregistering inter channel '{}' from flow {} block {}",
                        channel_name, id, block.id
                    );

                    self.inner.channel_registry.unregister(&channel_name).await;

                    // Broadcast event for subscribers
                    self.inner
                        .events
                        .broadcast(StromEvent::SourceOutputUnavailable {
                            source_flow_id: *id,
                            output_name: block.id.clone(),
                        });
                }

                // Remove SAP announcement for AES67 output blocks
                if block.block_definition_id == "builtin.aes67_output" {
                    self.inner
                        .discovery
                        .remove_announcement(*id, &block.id)
                        .await;
                }
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

    /// Get runtime dynamic pads that were auto-linked to tees.
    /// Returns a map of element_id -> {pad_name -> tee_element_name}
    pub async fn get_dynamic_pads(
        &self,
        id: &FlowId,
    ) -> Option<std::collections::HashMap<String, std::collections::HashMap<String, String>>> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines.get(id).map(|p| p.get_dynamic_pads())
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

    /// Get debug information for a running flow.
    /// Returns pipeline timing info (base_time, clock_time, running_time).
    /// Useful for debugging AES67/RFC 7273 RTP timestamp issues.
    pub async fn get_flow_debug_info(
        &self,
        flow_id: &FlowId,
    ) -> Option<strom_types::api::FlowDebugInfo> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines.get(flow_id).map(|p| p.get_debug_info())
    }

    /// Get current system monitoring statistics (CPU and GPU).
    pub async fn get_system_stats(&self) -> strom_types::SystemStats {
        self.inner.system_monitor.collect_stats().await
    }

    /// Get PTP statistics events for all flows with PTP configured.
    ///
    /// This returns stats for all PTP domains being monitored, regardless of
    /// whether the flows are currently running. PTP clocks are shared resources
    /// and sync status is available even when no pipeline is using them.
    pub async fn get_ptp_stats_events(&self) -> Vec<StromEvent> {
        self.inner.ptp_monitor.get_stats_events()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::with_json_storage(
            "flows.json",
            "blocks.json",
            "media",
            vec!["stun:stun.l.google.com:19302".to_string()],
            vec!["239.255.255.255".to_string(), "224.2.127.254".to_string()],
        )
    }
}
