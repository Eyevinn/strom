//! Application state management.

use crate::affinity_manager::AffinityManager;
use crate::blocks::BlockRegistry;
use crate::discovery::DiscoveryService;
use crate::events::EventBroadcaster;
use crate::gst::{ElementDiscovery, PipelineError, PipelineManager};
use crate::ptp_monitor::PtpMonitor;
use crate::sharing::ChannelRegistry;
use crate::storage::{JsonFileStorage, Storage};
use crate::system_monitor::{SystemMonitor, ThreadCpuSampler};
use crate::thread_registry::ThreadRegistry;
use crate::whep_registry::WhepRegistry;
use crate::whip_registry::WhipRegistry;
use crate::whip_session_manager::WhipSessionManager;
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use strom_types::element::{ElementInfo, PropertyValue};
use strom_types::{Flow, FlowId, PipelineState, StromEvent};
use tokio::sync::RwLock;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::reload;
use tracing_subscriber::EnvFilter;

/// Handle for reloading the log filter at runtime.
pub type LogReloadHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// The endpoint registrations a teardown is allowed to remove.
///
/// Only ever narrower than "everything the flow declares", and only for a
/// caller that failed part-way through registering: an endpoint-id conflict
/// means the id already belongs to another flow, so unregistering it would
/// tear down that flow's endpoint instead of ours.
#[derive(Default)]
struct RegisteredEndpoints {
    whep: Vec<String>,
    whip: Vec<String>,
}

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
    /// Thread registry for tracking GStreamer streaming threads
    thread_registry: ThreadRegistry,
    /// CPU affinity manager for smart core allocation
    affinity_manager: AffinityManager,
    /// Thread CPU sampler for measuring per-thread CPU usage
    thread_cpu_sampler: parking_lot::Mutex<ThreadCpuSampler>,
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
    /// WHIP endpoint registry (maps endpoint IDs to internal ports)
    whip_registry: WhipRegistry,
    /// WHIP session manager (creates per-client whipserversrc elements)
    whip_session_manager: Arc<WhipSessionManager>,
    /// ICE servers for WebRTC NAT traversal (STUN/TURN URLs)
    ice_servers: Vec<String>,
    /// ICE transport policy for WebRTC connections ("all" or "relay")
    ice_transport_policy: String,
    /// Flows pending save (debounced to avoid excessive disk writes)
    pending_saves: RwLock<HashSet<FlowId>>,
    /// Handle for reloading the tracing EnvFilter at runtime
    log_reload_handle: parking_lot::Mutex<Option<LogReloadHandle>>,
    /// The log filter string the server was started with
    default_log_filter: parking_lot::Mutex<String>,
    /// The currently active GStreamer debug filter string (tracked by us)
    gst_debug_filter: parking_lot::Mutex<String>,
    /// The GStreamer debug filter the server started with
    default_gst_debug_filter: parking_lot::Mutex<String>,
    /// Authoritative solo intent for each mixer block instance: the set of
    /// `chN_pfl` / `chN_afl` exposed property names that the user has currently
    /// engaged. Updated as a side effect of every `update_block_properties`
    /// call that touches PFL/AFL bools, and consulted by the monitor-gate
    /// refresh — never derived from element values (which would race with the
    /// per-channel volume ramp). PFL/AFL bools are `persist: false`, so the
    /// per-flow entry is cleared on `stop_flow`; a fresh start sees an empty
    /// set, which matches the build-time element defaults (gates closed).
    mixer_solo_state: RwLock<HashMap<FlowId, HashMap<String, HashSet<String>>>>,
}

/// Pick the ramp_ms that should apply to a single property in a batched
/// `update_block_properties` call. A per-name entry in `overrides` wins over
/// the batch-level `global` default; absent both, returns `None` so the
/// pipeline layer falls back to its own per-route default.
fn resolve_ramp_ms(
    name: &str,
    overrides: Option<&HashMap<String, u32>>,
    global: Option<u32>,
) -> Option<u32> {
    overrides.and_then(|m| m.get(name).copied()).or(global)
}

impl AppState {
    /// Create new application state with the given storage backend.
    pub fn new(
        storage: impl Storage + 'static,
        blocks_path: impl Into<PathBuf>,
        media_path: impl Into<PathBuf>,
        ice_servers: Vec<String>,
        ice_transport_policy: String,
        sap_multicast_addresses: Vec<String>,
    ) -> Self {
        let events = EventBroadcaster::default();
        let affinity_manager = AffinityManager::new();
        let num_cores = affinity_manager.num_cores();
        Self {
            inner: Arc::new(AppStateInner {
                flows: RwLock::new(HashMap::new()),
                storage: Arc::new(storage),
                element_discovery: RwLock::new(ElementDiscovery::new()),
                cached_elements: RwLock::new(Vec::new()),
                pipelines: RwLock::new(HashMap::new()),
                events: events.clone(),
                block_registry: BlockRegistry::new(blocks_path),
                system_monitor: SystemMonitor::new(num_cores),
                thread_registry: ThreadRegistry::new(),
                affinity_manager,
                thread_cpu_sampler: parking_lot::Mutex::new(ThreadCpuSampler::new()),
                channel_registry: ChannelRegistry::new(),
                discovery: DiscoveryService::new(events, sap_multicast_addresses.clone()),
                ptp_monitor: PtpMonitor::new(),
                media_path: media_path.into(),
                whep_registry: WhepRegistry::new(),
                whip_registry: WhipRegistry::new(),
                whip_session_manager: Arc::new(WhipSessionManager::new()),
                ice_servers,
                ice_transport_policy,
                pending_saves: RwLock::new(HashSet::new()),
                log_reload_handle: parking_lot::Mutex::new(None),
                default_log_filter: parking_lot::Mutex::new("info".to_string()),
                gst_debug_filter: parking_lot::Mutex::new(String::new()),
                default_gst_debug_filter: parking_lot::Mutex::new(String::new()),
                mixer_solo_state: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Set the log reload handle and default filter (called once from main after init_logging).
    pub fn set_log_reload_handle(&self, handle: LogReloadHandle, default_filter: String) {
        *self.inner.default_log_filter.lock() = default_filter;
        *self.inner.log_reload_handle.lock() = Some(handle);
    }

    /// Get the current log filter string.
    pub fn current_log_filter(&self) -> String {
        let guard = self.inner.log_reload_handle.lock();
        if let Some(handle) = guard.as_ref() {
            handle
                .with_current(|f| format!("{}", f))
                .unwrap_or_else(|_| "unknown".to_string())
        } else {
            "unknown".to_string()
        }
    }

    /// Get the default log filter string.
    pub fn default_log_filter(&self) -> String {
        self.inner.default_log_filter.lock().clone()
    }

    /// Reload the log filter at runtime. Returns an error if the filter string is invalid.
    pub fn reload_log_filter(&self, filter: &str) -> Result<(), String> {
        let new_filter = EnvFilter::try_new(filter)
            .map_err(|e| format!("Invalid filter '{}': {}", filter, e))?;
        let guard = self.inner.log_reload_handle.lock();
        if let Some(handle) = guard.as_ref() {
            handle
                .reload(new_filter)
                .map_err(|e| format!("Failed to reload filter: {}", e))
        } else {
            Err("Log reload handle not initialized".to_string())
        }
    }

    /// Initialize GStreamer debug level tracking (called once after gst::init).
    pub fn init_gst_debug_filter(&self) {
        let initial = std::env::var("GST_DEBUG").unwrap_or_default();
        let filter = if initial.is_empty() {
            let level = gst_level_to_int(gstreamer::log::get_default_threshold());
            format!("*:{}", level)
        } else {
            initial
        };
        *self.inner.default_gst_debug_filter.lock() = filter.clone();
        *self.inner.gst_debug_filter.lock() = filter;
    }

    /// Get the current GStreamer debug filter string.
    pub fn current_gst_debug_filter(&self) -> String {
        self.inner.gst_debug_filter.lock().clone()
    }

    /// Get the default GStreamer debug filter string.
    pub fn default_gst_debug_filter(&self) -> String {
        self.inner.default_gst_debug_filter.lock().clone()
    }

    /// Apply a new GStreamer debug filter at runtime.
    pub fn set_gst_debug_filter(&self, filter: &str) -> Result<(), String> {
        let filter = filter.trim();
        if filter.is_empty() {
            return Err("Filter string must not be empty".to_string());
        }

        // First reset default threshold to none, then apply the new filter.
        // This ensures previously set per-category overrides from a prior
        // filter string don't linger when switching to a simpler filter.
        gstreamer::log::set_default_threshold(gstreamer::DebugLevel::None);

        for part in filter.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((cat, level_str)) = part.split_once(':') {
                let level = parse_gst_level(level_str.trim())?;
                if cat.trim() == "*" {
                    gstreamer::log::set_default_threshold(level);
                } else {
                    gstreamer::log::set_threshold_for_name(cat.trim(), level);
                }
            } else {
                let level = parse_gst_level(part)?;
                gstreamer::log::set_default_threshold(level);
            }
        }

        *self.inner.gst_debug_filter.lock() = filter.to_string();
        Ok(())
    }

    /// Get the WHEP endpoint registry.
    pub fn whep_registry(&self) -> &WhepRegistry {
        &self.inner.whep_registry
    }

    /// Get the WHIP endpoint registry.
    pub fn whip_registry(&self) -> &WhipRegistry {
        &self.inner.whip_registry
    }

    /// Get the WHIP session manager.
    pub fn whip_session_manager(&self) -> &Arc<WhipSessionManager> {
        &self.inner.whip_session_manager
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

    /// Get the configured ICE transport policy for WebRTC.
    pub fn ice_transport_policy(&self) -> &str {
        &self.inner.ice_transport_policy
    }

    /// Get the thread registry for tracking GStreamer streaming threads.
    pub fn thread_registry(&self) -> &ThreadRegistry {
        &self.inner.thread_registry
    }

    /// Get current thread CPU statistics.
    ///
    /// Samples CPU usage for all registered GStreamer streaming threads.
    pub fn get_thread_stats(&self) -> strom_types::ThreadStats {
        let mut sampler = self.inner.thread_cpu_sampler.lock();
        sampler.sample(&self.inner.thread_registry)
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
        ice_transport_policy: String,
        sap_multicast_addresses: Vec<String>,
    ) -> Self {
        Self::new(
            JsonFileStorage::new(flows_path),
            blocks_path,
            media_path,
            ice_servers,
            ice_transport_policy,
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
        ice_transport_policy: String,
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
            ice_transport_policy,
            sap_multicast_addresses,
        ))
    }

    /// Strip block properties marked `persist: false` from a flow.
    ///
    /// Transient state (currently `chN_pfl` / `chN_afl` on the audio mixer)
    /// should never reach the in-memory flow definition or disk — otherwise an
    /// explicit flow save would re-engage solo on the next restart, and the
    /// runtime-only intent of `persist: false` would silently leak.
    ///
    /// Called at two boundaries:
    ///   * `load_from_storage` — cleans up legacy flow JSON that may carry
    ///     transient values from before this guard existed.
    ///   * `upsert_flow` — filters everything coming in from the flow PATCH /
    ///     PUT handlers and the create-flow path.
    ///
    /// Live PATCHes via `update_block_properties` already skip `persist: false`
    /// at write time, so no additional filtering is needed on that path.
    async fn strip_transient_properties(&self, flow: &mut Flow) {
        for block in &mut flow.blocks {
            let Some(def) = self
                .inner
                .block_registry
                .get_by_id(&block.block_definition_id)
                .await
            else {
                continue;
            };
            for prop in &def.exposed_properties {
                if !prop.persist() {
                    block.properties.remove(&prop.name);
                }
            }
        }
    }

    /// Load flows from storage into memory.
    pub async fn load_from_storage(&self) -> anyhow::Result<()> {
        info!("Loading flows from storage...");
        match self.inner.storage.load_all().await {
            Ok(mut flows) => {
                let count = flows.len();

                // Strip transient (persist: false) properties from any legacy
                // flow JSON that stored them before this guard existed.
                for flow in flows.values_mut() {
                    self.strip_transient_properties(flow).await;
                }

                // Migrate legacy `mode` on WHEP Output blocks to explicit track
                // counts so the property panel and the computed pads agree.
                for flow in flows.values_mut() {
                    for block in &mut flow.blocks {
                        if block.block_definition_id == "builtin.whep_output" {
                            crate::blocks::builtin::whep::migrate_legacy_mode(
                                &mut block.properties,
                            );
                        }
                    }
                }

                // Reset all flow states to None on server restart since pipelines aren't running
                // This prevents showing stale "Playing" states from before the server stopped
                for flow in flows.values_mut() {
                    if flow.gst_state.is_some() {
                        debug!(
                            "Resetting state for flow '{}' from {:?} to None (server restart)",
                            flow.name, flow.gst_state
                        );
                        flow.set_gst_state(None);
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

    /// Mark a flow as needing to be saved (debounced).
    /// The actual save will happen after a short delay to batch multiple changes.
    pub async fn mark_flow_dirty(&self, flow_id: FlowId) {
        let mut pending = self.inner.pending_saves.write().await;
        pending.insert(flow_id);
        trace!("Marked flow {} as dirty for save", flow_id);
    }

    /// Read-only access to active pipelines (for monitoring).
    pub async fn pipelines_read(
        &self,
    ) -> tokio::sync::RwLockReadGuard<'_, HashMap<FlowId, PipelineManager>> {
        self.inner.pipelines.read().await
    }

    /// Write access to active pipelines (for probe management).
    pub async fn pipelines_write(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, HashMap<FlowId, PipelineManager>> {
        self.inner.pipelines.write().await
    }

    /// Start the background task that periodically saves dirty flows.
    /// Should be called once at startup.
    pub fn start_debounced_save_task(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(1500));
            loop {
                interval.tick().await;
                state.flush_pending_saves().await;
            }
        });
        info!("Started debounced flow save task (1.5s interval)");
    }

    /// Flush all pending saves to storage.
    async fn flush_pending_saves(&self) {
        // Get and clear pending saves
        let to_save: Vec<FlowId> = {
            let mut pending = self.inner.pending_saves.write().await;
            if pending.is_empty() {
                return;
            }
            let ids: Vec<FlowId> = pending.drain().collect();
            ids
        };

        // Save each dirty flow
        let flows = self.inner.flows.read().await;
        for flow_id in to_save {
            if let Some(flow) = flows.get(&flow_id) {
                if flow.properties.ephemeral {
                    continue;
                }
                if let Err(e) = self.inner.storage.save_flow(flow).await {
                    error!("Failed to save flow {} to storage: {}", flow_id, e);
                    // Re-add to pending saves to retry later
                    let mut pending = self.inner.pending_saves.write().await;
                    pending.insert(flow_id);
                } else {
                    debug!("Saved flow {} to storage (debounced)", flow_id);
                }
            }
        }
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
                    flow.set_gst_state(Some(pipeline.get_state()));
                    flow.properties.clock_sync_status = Some(pipeline.get_clock_sync_status());
                    // Get PTP info and check if restart is needed (configured domain differs from running)
                    if let Some(mut ptp_info) = pipeline.get_ptp_info() {
                        let configured_domain = flow.properties.ptp_domain.unwrap_or(0);
                        ptp_info.restart_needed = configured_domain != ptp_info.domain;
                        flow.properties.ptp_info = Some(ptp_info);
                    }
                    flow.properties.ntp_info = pipeline.get_ntp_info();
                    flow.properties.thread_priority_status = pipeline.get_thread_priority_status();
                } else {
                    // Clear runtime-only status when no pipeline is running
                    flow.set_gst_state(None);
                    flow.properties.thread_priority_status = None;
                    flow.properties.clock_sync_status = None;
                    flow.properties.ptp_info = None;
                    flow.properties.ntp_info = None;
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
                flow.set_gst_state(Some(pipeline.get_state()));
                flow.properties.clock_sync_status = Some(pipeline.get_clock_sync_status());
                // Get PTP info and check if restart is needed (configured domain differs from running)
                if let Some(mut ptp_info) = pipeline.get_ptp_info() {
                    let configured_domain = flow.properties.ptp_domain.unwrap_or(0);
                    ptp_info.restart_needed = configured_domain != ptp_info.domain;
                    flow.properties.ptp_info = Some(ptp_info);
                }
                flow.properties.ntp_info = pipeline.get_ntp_info();
                flow.properties.thread_priority_status = pipeline.get_thread_priority_status();
            } else {
                // Clear runtime-only status when no pipeline is running
                flow.set_gst_state(None);
                flow.properties.thread_priority_status = None;
                flow.properties.clock_sync_status = None;
                flow.properties.ptp_info = None;
                flow.properties.ntp_info = None;
            }
            // Compute external pads for dynamic blocks
            Self::compute_flow_external_pads(&mut flow);
            flow
        })
    }

    /// Add or update a flow and persist to storage.
    pub async fn upsert_flow(&self, mut flow: Flow) -> anyhow::Result<()> {
        // Filter out transient (persist: false) block properties before this
        // flow definition reaches either the in-memory map or disk. Without
        // this, an explicit save from the frontend would re-engage transient
        // state (e.g. PFL/AFL solo) on the next restart.
        self.strip_transient_properties(&mut flow).await;

        // Update in-memory state. `insert` reports whether the id was already
        // taken, so newness is decided under the same lock that writes it.
        let is_new = {
            let mut flows = self.inner.flows.write().await;
            flows.insert(flow.id, flow.clone()).is_none()
        };

        self.persist_flow(&flow, is_new).await
    }

    /// Insert a flow only if its id is not already taken.
    ///
    /// Returns `Ok(false)`, leaving the stored flow untouched, when a flow with
    /// the same id already exists. The check and the insert happen under a
    /// single write lock, so two concurrent creates supplying the same id
    /// cannot both succeed and overwrite one another.
    pub async fn insert_flow_if_absent(&self, mut flow: Flow) -> anyhow::Result<bool> {
        self.strip_transient_properties(&mut flow).await;

        {
            let mut flows = self.inner.flows.write().await;
            if flows.contains_key(&flow.id) {
                return Ok(false);
            }
            flows.insert(flow.id, flow.clone());
        }

        self.persist_flow(&flow, true).await?;
        Ok(true)
    }

    /// Persist a flow that is already in the in-memory map, update its PTP
    /// registration, and broadcast the created/updated event.
    async fn persist_flow(&self, flow: &Flow, is_new: bool) -> anyhow::Result<()> {
        // Persist to storage (skip ephemeral flows)
        if flow.properties.ephemeral {
            // Remove any previously persisted copy so it doesn't reappear on restart
            let _ = self.inner.storage.delete_flow(&flow.id).await;
        } else if let Err(e) = self.inner.storage.save_flow(flow).await {
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

        // Stop the pipeline first if it is still running. Without this, the
        // PipelineManager stays in self.inner.pipelines after the flow record
        // is gone, with no API path left to reach it.
        let pipeline_active = {
            let pipelines = self.inner.pipelines.read().await;
            pipelines.contains_key(id)
        };
        if pipeline_active {
            if let Err(e) = self.stop_flow(id).await {
                error!(
                    "Failed to stop flow {} before delete: {} — pipeline resources may leak",
                    id, e
                );
            }
        }

        // Delete from storage first (skip for ephemeral flows)
        let is_ephemeral = {
            let flows = self.inner.flows.read().await;
            flows.get(id).is_some_and(|f| f.properties.ephemeral)
        };
        if !is_ephemeral {
            if let Err(e) = self.inner.storage.delete_flow(id).await {
                error!("Failed to delete flow from storage: {}", e);
                return Err(e.into());
            }
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
            return Err(PipelineError::FlowNotFound(id.to_string()));
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

        // Snapshot the live local-device map so the Local Input block can
        // resolve a chosen device id without starting a transient
        // DeviceMonitor inside its build() (which crashes inside
        // gst_device_provider_stop on macOS).
        let local_devices = self.inner.discovery.local_device_map().await;

        // Create pipeline with event broadcaster and block registry
        info!("Creating PipelineManager (this may block)...");
        let mut manager = match PipelineManager::new(
            &flow,
            self.inner.events.clone(),
            &self.inner.block_registry,
            self.inner.ice_servers.clone(),
            self.inner.ice_transport_policy.clone(),
            Some(self.inner.whip_registry.clone()),
            self.inner.media_path.clone(),
            local_devices,
        ) {
            Ok(manager) => manager,
            Err(e) => {
                // There is no pipeline to stop, but blocks built before the
                // failing one may already have registered themselves.
                error!("Failed to build pipeline for flow {}: {}", id, e);
                let _ = self.teardown_flow(id, None, None).await;
                return Err(e);
            }
        };
        info!("PipelineManager created successfully");

        // Set thread registry for CPU monitoring
        manager.set_thread_registry(self.inner.thread_registry.clone());

        // Allocate CPU core for SingleCore affinity
        if matches!(
            flow.properties.cpu_affinity,
            strom_types::flow::CpuAffinity::SingleCore
        ) {
            let assigned_cpus = self.inner.affinity_manager.allocate(*id);
            manager.set_assigned_cpus(assigned_cpus);
        }

        // Start pipeline
        info!("Calling manager.start() (this may block)...");
        let state = match manager.start() {
            Ok(state) => state,
            Err(e) => {
                error!("Failed to start flow {}: {}", id, e);
                // No endpoints are registered yet, so the flow owns none.
                if let Err(teardown_err) = self
                    .teardown_flow(id, Some(manager), Some(RegisteredEndpoints::default()))
                    .await
                {
                    warn!(
                        "Cleanup after failed start of flow {} could not stop the pipeline: {}",
                        id, teardown_err
                    );
                }
                return Err(e);
            }
        };
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
            let mut endpoints: Vec<(String, String)> = Vec::new();
            for whep_info in manager.whep_endpoints() {
                info!(
                    "Registering WHEP endpoint '{}' (block {}) on port {} audio_tracks={} video_tracks={}",
                    whep_info.endpoint_id,
                    whep_info.block_id,
                    whep_info.internal_port,
                    whep_info.num_audio_tracks,
                    whep_info.num_video_tracks,
                );
                if let Err(e) = self
                    .inner
                    .whep_registry
                    .register(
                        whep_info.endpoint_id.clone(),
                        whep_info.internal_port,
                        whep_info.num_audio_tracks,
                        whep_info.num_video_tracks,
                    )
                    .await
                {
                    error!("Endpoint conflict registering WHEP endpoint: {}", e);
                    // Only the ids we registered. The one that conflicted
                    // belongs to another flow — unregistering it here would
                    // take that flow's endpoint down instead of ours.
                    let owned = RegisteredEndpoints {
                        whep: endpoints.iter().map(|(_, ep)| ep.clone()).collect(),
                        whip: Vec::new(),
                    };
                    drop(pipelines_guard);
                    let manager = self.inner.pipelines.write().await.remove(id);
                    if let Err(teardown_err) = self.teardown_flow(id, manager, Some(owned)).await {
                        warn!(
                            "Cleanup after WHEP endpoint conflict on flow {} could not stop the pipeline: {}",
                            id, teardown_err
                        );
                    }
                    return Err(PipelineError::EndpointConflict(e));
                }
                endpoints.push((whep_info.block_id.clone(), whep_info.endpoint_id.clone()));
            }
            endpoints
        } else {
            Vec::new()
        };

        // Collect and register WHIP endpoints from blocks
        let whip_endpoints: Vec<(String, String)> = if let Some(manager) = pipelines_guard.get(id) {
            let mut endpoints: Vec<(String, String)> = Vec::new();
            for whip_info in manager.whip_endpoints() {
                info!(
                    "Registering WHIP endpoint '{}' (block {}) mode={:?} (port assigned per-session)",
                    whip_info.endpoint_id,
                    whip_info.block_id,
                    whip_info.mode
                );
                // Register with port=0 placeholder; actual ports are per-session
                if let Err(e) = self
                    .inner
                    .whip_registry
                    .register(whip_info.endpoint_id.clone(), 0, whip_info.mode)
                    .await
                {
                    error!("Endpoint conflict registering WHIP endpoint: {}", e);
                    // Every WHEP endpoint did register, so the flow owns all of
                    // them; of the WHIP ones it owns only those it got in
                    // before the conflict.
                    let owned = RegisteredEndpoints {
                        whep: whep_endpoints.iter().map(|(_, ep)| ep.clone()).collect(),
                        whip: endpoints.iter().map(|(_, ep)| ep.clone()).collect(),
                    };
                    drop(pipelines_guard);
                    let manager = self.inner.pipelines.write().await.remove(id);
                    if let Err(teardown_err) = self.teardown_flow(id, manager, Some(owned)).await {
                        warn!(
                            "Cleanup after WHIP endpoint conflict on flow {} could not stop the pipeline: {}",
                            id, teardown_err
                        );
                    }
                    return Err(PipelineError::EndpointConflict(e));
                }
                endpoints.push((whip_info.block_id.clone(), whip_info.endpoint_id.clone()));
            }
            endpoints
        } else {
            Vec::new()
        };

        // Register WHIP endpoint configs with the session manager
        // We need mutable access to take configs
        drop(pipelines_guard);
        {
            let mut pipelines = self.inner.pipelines.write().await;
            if let Some(manager) = pipelines.get_mut(id) {
                let configs = manager.take_whip_endpoint_configs();
                if !configs.is_empty() {
                    for (endpoint_id, mut config) in configs {
                        config.pipeline_weak = manager.pipeline_weak();
                        config.endpoint_id = endpoint_id.clone();
                        info!(
                            "Registering WHIP endpoint config '{}' with session manager",
                            endpoint_id
                        );
                        self.inner
                            .whip_session_manager
                            .register_endpoint(endpoint_id, config);
                    }
                }
            }
        }
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

                    if block.runtime_data.is_none() {
                        block.runtime_data = Some(std::collections::HashMap::new());
                    }

                    if let Some(runtime_data) = &mut block.runtime_data {
                        runtime_data.insert("whep_endpoint_id".to_string(), endpoint_id.clone());
                    }
                }
            }

            // Store endpoint_id in runtime_data for WHIP input blocks
            if block.block_definition_id == "builtin.whip_input" {
                if let Some((_, endpoint_id)) =
                    whip_endpoints.iter().find(|(bid, _)| bid == &block.id)
                {
                    info!(
                        "Storing WHIP endpoint_id '{}' for block {} in runtime_data",
                        endpoint_id, block.id
                    );

                    if block.runtime_data.is_none() {
                        block.runtime_data = Some(std::collections::HashMap::new());
                    }

                    if let Some(runtime_data) = &mut block.runtime_data {
                        runtime_data.insert("whip_endpoint_id".to_string(), endpoint_id.clone());
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
        flow.set_gst_state(Some(state));
        flow.properties.auto_restart = true; // Enable auto-restart when flow is started
        flow.properties.started_at = Some(Local::now().to_rfc3339()); // Record when flow started
        {
            let mut flows = self.inner.flows.write().await;
            flows.insert(*id, flow.clone());
        }
        if !flow.properties.ephemeral {
            if let Err(e) = self.inner.storage.save_flow(&flow).await {
                error!("Failed to save flow state: {}", e);
            }
        }

        // Broadcast events
        // Note: FlowStateChanged is now broadcast from the bus watch on actual GStreamer
        // state transitions, so we don't need to broadcast it here.
        self.inner
            .events
            .broadcast(StromEvent::FlowStarted { flow_id: *id });
        // Broadcast FlowUpdated so frontend sees the new runtime_data with SDP
        self.inner
            .events
            .broadcast(StromEvent::FlowUpdated { flow_id: *id });

        Ok(state)
    }

    /// Stop a flow (stop and remove its pipeline).
    /// Release everything a flow's pipeline holds. The single way it is done.
    ///
    /// `start_flow()` can fail in four places and `stop_flow()` is a fifth
    /// caller. Each has to release the same set of things, and each used to do
    /// its own subset — so a flow that failed to start leaked a bus watch, its
    /// Media Player pipelines, or its CPU allocation, depending on which line
    /// it failed on. Anything that must be released when a flow goes away
    /// belongs here, not at a call site.
    ///
    /// Order matters: endpoints stop accepting traffic before the pipeline they
    /// point at goes to NULL.
    ///
    /// `endpoints` says which registrations this teardown owns. `None` means
    /// every endpoint the manager declares, which is right whenever the flow
    /// finished registering them. A caller that failed *during* registration
    /// must pass the ids it actually registered: an endpoint-id conflict means
    /// the id belongs to another flow, and unregistering it here would tear
    /// down that flow's endpoint instead.
    ///
    /// Returns the state the pipeline reached, or the error `stop()` gave. The
    /// rest of the teardown runs either way — a pipeline that refuses to stop
    /// is exactly when leaking its cores and its registry entries hurts most.
    async fn teardown_flow(
        &self,
        id: &FlowId,
        manager: Option<PipelineManager>,
        endpoints: Option<RegisteredEndpoints>,
    ) -> Result<PipelineState, PipelineError> {
        let Some(mut manager) = manager else {
            // No pipeline was ever built. Blocks constructed before the failing
            // one can still have registered themselves, and the CPU allocation
            // may already be held; deallocate() ignores an id it does not know.
            crate::blocks::builtin::mediaplayer::MEDIA_PLAYER_REGISTRY.unregister_flow(id);
            self.inner.affinity_manager.deallocate(id);
            return Ok(PipelineState::Null);
        };

        let owned = endpoints.unwrap_or_else(|| RegisteredEndpoints {
            whep: manager
                .whep_endpoints()
                .iter()
                .map(|e| e.endpoint_id.clone())
                .collect(),
            whip: manager
                .whip_endpoints()
                .iter()
                .map(|e| e.endpoint_id.clone())
                .collect(),
        });

        for endpoint_id in &owned.whep {
            info!("Unregistering WHEP endpoint '{}'", endpoint_id);
            self.inner.whep_registry.unregister(endpoint_id).await;
        }

        for endpoint_id in &owned.whip {
            info!(
                "Tearing down WHIP sessions and unregistering endpoint '{}'",
                endpoint_id
            );
            let session_entries = self
                .inner
                .whip_session_manager
                .remove_all_sessions(endpoint_id);
            let count = session_entries.len();
            if count > 0 {
                // Session pipelines go to NULL on the blocking pool: that can
                // take seconds and this is awaited from an HTTP handler.
                let endpoint_id_log = endpoint_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    for (pipeline, element) in session_entries {
                        WhipSessionManager::teardown_session_pipeline(&pipeline);
                        drop(element);
                    }
                    info!(
                        "Torn down {} active WHIP session(s) for endpoint '{}'",
                        count, endpoint_id_log
                    );
                })
                .await;
            }
            self.inner
                .whip_session_manager
                .unregister_endpoint(endpoint_id);
            self.inner.whip_registry.unregister(endpoint_id).await;
        }

        // The registry holds an Arc per Media Player instance, and that Arc's
        // Drop is the only thing that takes the block's *internal* pipeline to
        // NULL. Skip this and a Media Player flow leaves a decoding pipeline
        // and its file descriptors running for the life of the process.
        crate::blocks::builtin::mediaplayer::MEDIA_PLAYER_REGISTRY.unregister_flow(id);

        // stop() — not just dropping the manager. Drop aborts the thumbnail
        // task, stops probes and sets NULL; stop() is also what removes the bus
        // watch, the thread-priority handler and the flow's threads from the
        // CPU-monitor registry. Dropping alone leaks one bus watch GSource per
        // teardown and leaves dead TIDs in the registry, where a reused TID is
        // then attributed to a flow that no longer exists.
        //
        // On the blocking pool: stop() joins a thread around set_state(Null),
        // which can take seconds, and this is awaited from an HTTP handler.
        let stopped = tokio::task::spawn_blocking(move || {
            let result = manager.stop();

            // Weak refs taken before the drop. Anything still alive afterwards
            // is held by a leaked strong reference — a signal handler closure,
            // a probe — and its OS resources (sockets, threads) never come
            // back. See the GStreamer reference rules in CLAUDE.md.
            let pipeline_weak = manager.pipeline_weak();
            let element_weak_refs = manager.element_weak_refs();
            let flow_name = manager.flow_name().to_string();
            drop(manager);

            let leaked_elements: Vec<String> = if pipeline_weak.upgrade().is_some() {
                element_weak_refs
                    .iter()
                    .filter(|(_, weak)| weak.upgrade().is_some())
                    .map(|(name, _)| name.clone())
                    .collect()
            } else {
                Vec::new()
            };
            let pipeline_survived = pipeline_weak.upgrade().is_some();

            (result, flow_name, pipeline_survived, leaked_elements)
        })
        .await;

        // Release the cores before returning, whatever stop() said. A flow that
        // fails to stop would otherwise hold them until the process restarts.
        self.inner.affinity_manager.deallocate(id);

        match stopped {
            Ok((result, flow_name, pipeline_survived, leaked_elements)) => {
                if pipeline_survived {
                    error!(
                        "Pipeline '{}': GStreamer pipeline still alive after drop — OS resources will leak",
                        flow_name
                    );
                    for name in &leaked_elements {
                        error!("Pipeline '{}': leaked element '{}'", flow_name, name);
                    }
                }
                result
            }
            Err(join_err) => Err(PipelineError::StateChange(format!(
                "Teardown task panicked: {}",
                join_err
            ))),
        }
    }

    pub async fn stop_flow(&self, id: &FlowId) -> Result<PipelineState, PipelineError> {
        info!("Stopping flow: {}", id);

        // Drop any cached mixer solo intent — PFL/AFL bools are `persist: false`
        // and the rebuilt pipeline will start with the no-solo defaults.
        {
            let mut state = self.inner.mixer_solo_state.write().await;
            state.remove(id);
        }

        // Get and remove the pipeline
        let manager = {
            let mut pipelines = self.inner.pipelines.write().await;
            pipelines.remove(id)
        };

        let Some(manager) = manager else {
            warn!("No active pipeline for flow: {}", id);
            // Clear persisted state so the flow no longer appears as running
            let mut flows = self.inner.flows.write().await;
            if let Some(flow) = flows.get_mut(id) {
                flow.set_gst_state(Some(PipelineState::Null));
                flow.properties.auto_restart = false;
                flow.properties.started_at = None;
                let flow_clone = flow.clone();
                drop(flows);
                if !flow_clone.properties.ephemeral {
                    if let Err(e) = self.inner.storage.save_flow(&flow_clone).await {
                        error!("Failed to save flow state: {}", e);
                    }
                }
                self.inner.events.broadcast(StromEvent::FlowStateChanged {
                    flow_id: *id,
                    state: format!("{:?}", PipelineState::Null),
                });
            }
            return Ok(PipelineState::Null);
        };

        // Endpoints, Media Player registry, pipeline, leak check, CPU cores.
        let state = self.teardown_flow(id, Some(manager), None).await?;

        // Clear runtime_data from all blocks (SDP is only valid while running)
        let flow = {
            let mut flows = self.inner.flows.write().await;
            if let Some(flow) = flows.get_mut(id) {
                info!("Clearing runtime_data from {} blocks", flow.blocks.len());
                for block in &mut flow.blocks {
                    if let Some(runtime_data) = &block.runtime_data {
                        info!(
                            "Clearing runtime_data for block {} (was {} entries)",
                            block.id,
                            runtime_data.len()
                        );
                        block.runtime_data = None;
                    }
                }
                flow.set_gst_state(Some(state));
                flow.properties.auto_restart = false; // Disable auto-restart when manually stopped
                flow.properties.started_at = None; // Clear started_at when stopped
                Some(flow.clone())
            } else {
                None
            }
        };

        if let Some(ref flow) = flow {
            if !flow.properties.ephemeral {
                if let Err(e) = self.inner.storage.save_flow(flow).await {
                    error!("Failed to save flow state: {}", e);
                }
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

                // Clean up vision mixer overlay state
                if block.block_definition_id == "builtin.vision_mixer" {
                    crate::blocks::builtin::vision_mixer::overlay::unregister_overlay_state(
                        &block.id,
                    );
                    // Without this, the overlay-timer-* thread keeps polling the
                    // renderer registry, holds a strong AppSrc ref, and prevents
                    // the pipeline (and its NiceAgent) from finalizing.
                    crate::blocks::builtin::vision_mixer::overlay::unregister_overlay_renderer(
                        &block.id,
                    );
                }
            }
        }

        // Broadcast events
        // Note: FlowStateChanged is now broadcast from the bus watch on actual GStreamer
        // state transitions, so we don't need to broadcast it here.
        self.inner
            .events
            .broadcast(StromEvent::FlowStopped { flow_id: *id });
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
    ///
    /// `ramp_ms` is honored for properties that support smooth interpolation
    /// (currently audio `volume`-element `volume`); ignored for others.
    pub async fn update_element_property(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        property_name: &str,
        value: PropertyValue,
        ramp_ms: Option<u32>,
    ) -> Result<(), PipelineError> {
        info!(
            "Updating property {}.{} in flow {} (ramp_ms={:?})",
            element_id, property_name, flow_id, ramp_ms
        );

        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.update_element_property(element_id, property_name, &value, ramp_ms)?;

        // Broadcast property change event
        self.inner.events.broadcast(StromEvent::PropertyChanged {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
            property_name: property_name.to_string(),
            value,
        });

        Ok(())
    }

    /// Apply a batch of exposed block-level properties to the running pipeline live.
    ///
    /// Each requested property is looked up in the block's `ExposedProperty` list, its
    /// declared `transform` (e.g. `bool_to_volume`) is applied, and the result is
    /// written to the resolved underlying element via [`Self::update_element_property`]
    /// — so all the existing anti-click / ramp behaviour is inherited.
    ///
    /// Block-specific derived state: for the audio mixer, any chN_pfl /
    /// chN_afl / auxN_afl / groupN_afl write in the batch triggers a
    /// post-step that recomputes "any solo active" and writes the two
    /// monitor-source gates (`solo_to_mon` / `main_to_mon`) with the same
    /// `ramp_ms`. Those gates are pure derived state — clients must never
    /// touch them directly.
    ///
    /// Returns `(current_values, rejected)`:
    /// - `current_values`: block-level (inverse-transformed) values after the writes.
    /// - `rejected`: per-property reason strings for entries that could not be applied
    ///   (unknown name, non-live, transform mismatch). The overall call still succeeds —
    ///   only flow/block-not-found is a hard `Err`.
    pub async fn update_block_properties(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        properties: HashMap<String, PropertyValue>,
        ramp_ms: Option<u32>,
        ramp_ms_overrides: Option<HashMap<String, u32>>,
    ) -> Result<(HashMap<String, PropertyValue>, HashMap<String, String>), PipelineError> {
        // Resolve block instance → definition_id → BlockDefinition.
        let definition_id = {
            let flows = self.inner.flows.read().await;
            let flow = flows.get(flow_id).ok_or_else(|| {
                PipelineError::InvalidFlow(format!("Flow not found: {}", flow_id))
            })?;
            flow.blocks
                .iter()
                .find(|b| b.id == block_instance_id)
                .map(|b| b.block_definition_id.clone())
                .ok_or_else(|| {
                    PipelineError::InvalidFlow(format!(
                        "Block instance not found in flow: {}",
                        block_instance_id
                    ))
                })?
        };
        let definition = self
            .inner
            .block_registry
            .get_by_id(&definition_id)
            .await
            .ok_or_else(|| {
                PipelineError::InvalidFlow(format!("Block definition not found: {}", definition_id))
            })?;

        let mut rejected: HashMap<String, String> = HashMap::new();
        let mut to_persist: Vec<(String, PropertyValue)> = Vec::new();
        // True iff this batch successfully applied at least one solo write
        // (channel chN_pfl / chN_afl, aux auxN_afl, or group groupN_afl).
        // We update the per-block solo-intent cache below as those writes
        // succeed, then run the monitor-gate refresh once.
        let mut mixer_solo_changed = false;

        for (name, value) in properties {
            let Some(exposed) = definition
                .exposed_properties
                .iter()
                .find(|p| p.name == name)
            else {
                rejected.insert(name, "unknown exposed property".to_string());
                continue;
            };

            if !exposed.live {
                rejected.insert(
                    name,
                    "property is not live (requires flow restart)".to_string(),
                );
                continue;
            }

            // The `_block` element_id marker is a virtual element for properties that
            // get baked into the block at build time — they have no underlying element
            // to write to live.
            if exposed.mapping.element_id == "_block" {
                rejected.insert(name, "property has no underlying live element".to_string());
                continue;
            }

            let transform = crate::blocks::transforms::lookup(exposed.mapping.transform.as_deref());
            let Some(transformed) = (transform.forward)(value.clone()) else {
                rejected.insert(name, "value type does not match transform".to_string());
                continue;
            };

            // Element IDs in block definitions are relative to the instance — prepend.
            let full_element_id = format!("{}:{}", block_instance_id, exposed.mapping.element_id);

            let effective_ramp_ms = resolve_ramp_ms(&name, ramp_ms_overrides.as_ref(), ramp_ms);

            if let Err(e) = self
                .update_element_property(
                    flow_id,
                    &full_element_id,
                    &exposed.mapping.property_name,
                    transformed,
                    effective_ramp_ms,
                )
                .await
            {
                rejected.insert(name, format!("pipeline write failed: {}", e));
                continue;
            }

            if definition.id == crate::blocks::builtin::mixer::MIXER_BLOCK_ID
                && crate::blocks::builtin::mixer::is_solo_property_name(&name)
            {
                if let PropertyValue::Bool(b) = &value {
                    self.set_mixer_solo_intent(flow_id, block_instance_id, &name, *b)
                        .await;
                    mixer_solo_changed = true;
                }
            }

            if exposed.persist() {
                to_persist.push((name, value));
            }
        }

        // Mixer: PFL/AFL bools are the only public API for solo. The two
        // monitor-source gates (solo_to_mon / main_to_mon) are pure derived
        // state — any chN_pfl / chN_afl / auxN_afl / groupN_afl currently
        // engaged → solo bus to monitor; otherwise main bus to monitor. We
        // apply this exactly once per batch so the two gates are atomic
        // relative to the bool writes that triggered them.
        if mixer_solo_changed {
            self.refresh_mixer_monitor_gates(flow_id, block_instance_id, ramp_ms)
                .await;
        }

        // Sync persisted values back to the block instance so they survive a
        // pipeline restart. Done after the pipeline writes so we don't store
        // values that failed to apply.
        if !to_persist.is_empty() {
            {
                let mut flows = self.inner.flows.write().await;
                if let Some(flow) = flows.get_mut(flow_id) {
                    if let Some(block) = flow.blocks.iter_mut().find(|b| b.id == block_instance_id)
                    {
                        for (name, value) in to_persist {
                            block.properties.insert(name, value);
                        }
                    }
                }
            }
            self.mark_flow_dirty(*flow_id).await;
        }

        let current = self
            .get_block_properties_inner(flow_id, block_instance_id, &definition)
            .await?;
        Ok((current, rejected))
    }

    /// Read the current block-level (user-facing) values of all live exposed properties
    /// from the running pipeline. Non-live properties and those without a transform
    /// match are silently skipped.
    pub async fn get_block_properties(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
    ) -> Result<HashMap<String, PropertyValue>, PipelineError> {
        let definition_id = {
            let flows = self.inner.flows.read().await;
            let flow = flows.get(flow_id).ok_or_else(|| {
                PipelineError::InvalidFlow(format!("Flow not found: {}", flow_id))
            })?;
            flow.blocks
                .iter()
                .find(|b| b.id == block_instance_id)
                .map(|b| b.block_definition_id.clone())
                .ok_or_else(|| {
                    PipelineError::InvalidFlow(format!(
                        "Block instance not found in flow: {}",
                        block_instance_id
                    ))
                })?
        };
        let definition = self
            .inner
            .block_registry
            .get_by_id(&definition_id)
            .await
            .ok_or_else(|| {
                PipelineError::InvalidFlow(format!("Block definition not found: {}", definition_id))
            })?;
        self.get_block_properties_inner(flow_id, block_instance_id, &definition)
            .await
    }

    /// Record a single solo-affecting write (chN_pfl / chN_afl / auxN_afl /
    /// groupN_afl) in the per-block solo-intent cache. `true` adds the
    /// property name to the set, `false` removes it. The empty-set case is
    /// the no-solo state and matches the build-time gate defaults, so we
    /// clean up empty inner / outer maps.
    async fn set_mixer_solo_intent(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        property_name: &str,
        value: bool,
    ) {
        let mut state = self.inner.mixer_solo_state.write().await;
        let by_block = state.entry(*flow_id).or_default();
        let set = by_block.entry(block_instance_id.to_string()).or_default();
        if value {
            set.insert(property_name.to_string());
        } else {
            set.remove(property_name);
        }
        if set.is_empty() {
            by_block.remove(block_instance_id);
        }
        if by_block.is_empty() {
            state.remove(flow_id);
        }
    }

    /// True iff at least one channel / aux / group PFL or AFL is currently
    /// engaged on the given mixer block instance. Reads only the in-memory
    /// intent cache — never touches the running pipeline, so it is immune
    /// to mid-ramp races.
    async fn mixer_any_solo_active(&self, flow_id: &FlowId, block_instance_id: &str) -> bool {
        let state = self.inner.mixer_solo_state.read().await;
        state
            .get(flow_id)
            .and_then(|by_block| by_block.get(block_instance_id))
            .map(|set| !set.is_empty())
            .unwrap_or(false)
    }

    /// Mixer-specific derived state: refresh the monitor-source gates after a
    /// batch that contained one or more chN_pfl / chN_afl / auxN_afl /
    /// groupN_afl writes.
    ///
    /// "Any solo active" is computed purely from the in-memory solo-intent
    /// cache (see [`Self::set_mixer_solo_intent`]) — the running element
    /// values are not consulted, so a long volume ramp on one channel cannot
    /// race with a release on another. Both gates are written with the
    /// caller's `ramp_ms` so they stay in sync with the PFL/AFL ramps that
    /// just kicked off.
    ///
    /// Gate-write failures are logged but never bubble up — a transient
    /// element-not-found shouldn't fail the user's solo toggle. Note that a
    /// partial failure (one gate written, the other not) leaves both buses
    /// audible on the monitor until the next solo write retries — an
    /// acceptable degraded mode but worth knowing about when debugging.
    async fn refresh_mixer_monitor_gates(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        ramp_ms: Option<u32>,
    ) {
        let any_solo = self.mixer_any_solo_active(flow_id, block_instance_id).await;
        let (solo_vol, main_vol) = if any_solo { (1.0, 0.0) } else { (0.0, 1.0) };
        let solo_id = format!(
            "{}:{}",
            block_instance_id,
            crate::blocks::builtin::mixer::SOLO_TO_MON_ELEMENT
        );
        let main_id = format!(
            "{}:{}",
            block_instance_id,
            crate::blocks::builtin::mixer::MAIN_TO_MON_ELEMENT
        );
        if let Err(e) = self
            .update_element_property(
                flow_id,
                &solo_id,
                "volume",
                PropertyValue::Float(solo_vol),
                ramp_ms,
            )
            .await
        {
            warn!(
                "Mixer {} solo_to_mon gate update failed (any_solo={}): {}",
                block_instance_id, any_solo, e
            );
        }
        if let Err(e) = self
            .update_element_property(
                flow_id,
                &main_id,
                "volume",
                PropertyValue::Float(main_vol),
                ramp_ms,
            )
            .await
        {
            warn!(
                "Mixer {} main_to_mon gate update failed (any_solo={}): {}",
                block_instance_id, any_solo, e
            );
        }
    }

    /// Read the current block-level values of all live exposed properties from
    /// the running pipeline.
    ///
    /// A block definition is generated for the block type's MAX sizing (e.g. the
    /// mixer's 128 channels / 32 aux / 32 groups), so a smaller instance has no
    /// backing element for the vast majority of its exposed properties. We take
    /// the pipelines lock once and resolve which elements the instance actually
    /// built, then skip every property whose element is absent before touching
    /// GStreamer — instead of re-acquiring the lock and walking the
    /// element-not-found path thousands of times per call. The output is
    /// identical to reading each property individually (a property is only
    /// readable when its element exists), this just avoids the per-property
    /// lock churn and error-path allocations. Generic: any block that builds
    /// elements conditionally (mixer, audiorouter, …) benefits.
    async fn get_block_properties_inner(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        definition: &strom_types::BlockDefinition,
    ) -> Result<HashMap<String, PropertyValue>, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;
        let Some(manager) = pipelines.get(flow_id) else {
            // Pipeline not running: every read would fail and be skipped, so the
            // historical behaviour here is an empty map rather than an error.
            return Ok(HashMap::new());
        };

        // Element IDs that exist for this instance, with the "{instance}:" prefix
        // stripped so the definition's bare `element_id` can be tested without
        // allocating a full id per property.
        let prefix_len = block_instance_id.len() + 1;
        let existing: std::collections::HashSet<&str> = manager
            .find_block_elements(block_instance_id)
            .into_iter()
            .map(|(id, _)| &id[prefix_len..])
            .collect();

        let mut out = HashMap::new();
        for exposed in &definition.exposed_properties {
            if !exposed.live || exposed.mapping.element_id == "_block" {
                continue;
            }
            if !existing.contains(exposed.mapping.element_id.as_str()) {
                continue;
            }
            let full_element_id = format!("{}:{}", block_instance_id, exposed.mapping.element_id);
            let raw = match manager
                .get_element_property(&full_element_id, &exposed.mapping.property_name)
            {
                Ok(v) => v,
                Err(e) => {
                    trace!(
                        "Skipping {} (could not read {}.{}): {}",
                        exposed.name,
                        full_element_id,
                        exposed.mapping.property_name,
                        e
                    );
                    continue;
                }
            };
            let transform = crate::blocks::transforms::lookup(exposed.mapping.transform.as_deref());
            if let Some(v) = (transform.inverse)(raw) {
                out.insert(exposed.name.clone(), v);
            }
        }
        Ok(out)
    }

    /// Trigger a transition on a compositor/mixer block.
    pub async fn trigger_transition(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        from_input: usize,
        to_input: usize,
        transition_type: &str,
        duration_ms: u64,
    ) -> Result<String, PipelineError> {
        debug!(
            "Triggering {} transition on block {} in flow {} ({} -> {}, {}ms)",
            transition_type, block_instance_id, flow_id, from_input, to_input, duration_ms
        );

        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        let (ftb_cancelled, old_pgm, new_pgm, actual_kind) = manager.trigger_transition(
            block_instance_id,
            from_input,
            to_input,
            transition_type,
            duration_ms,
        )?;

        drop(pipelines);

        // Broadcast FTB cancelled event so clients update their UI
        if ftb_cancelled {
            self.inner
                .events
                .broadcast(StromEvent::VisionMixerFtbChanged {
                    flow_id: *flow_id,
                    block_id: block_instance_id.to_string(),
                    active: false,
                });
        }

        // Sync final alpha values back to flow definition for persistence
        // Clear ALL input alphas to 0.0, then set the active input to 1.0
        if let Some(block_id) = block_instance_id.split(':').next() {
            let mut flows = self.inner.flows.write().await;
            if let Some(flow) = flows.get_mut(flow_id) {
                if let Some(block) = flow.blocks.iter_mut().find(|b| b.id == block_id) {
                    // Clear all existing alpha properties
                    let alpha_keys: Vec<String> = block
                        .properties
                        .keys()
                        .filter(|k| k.starts_with("input_") && k.ends_with("_alpha"))
                        .cloned()
                        .collect();
                    for key in alpha_keys {
                        block.properties.insert(key, PropertyValue::Float(0.0));
                    }
                    // Set the active input (new PGM, if any)
                    if let Some(idx) = new_pgm {
                        block
                            .properties
                            .insert(format!("input_{}_alpha", idx), PropertyValue::Float(1.0));
                    }
                    trace!(
                        "Synced transition alpha values: all -> 0.0, input {:?} -> 1.0",
                        new_pgm
                    );
                }
            }
            drop(flows);

            // Mark flow for debounced save
            self.mark_flow_dirty(*flow_id).await;
        }

        // Check if this is a vision mixer block and update multiview accordingly
        // After take: new PGM = old PVW, new PVW = old PGM (swap)
        if let Some(block_id) = block_instance_id.split(':').next() {
            let flows = self.inner.flows.read().await;
            let is_vision_mixer = flows
                .get(flow_id)
                .and_then(|flow| flow.blocks.iter().find(|b| b.id == block_id))
                .map(|b| b.block_definition_id == "builtin.vision_mixer")
                .unwrap_or(false);
            drop(flows);

            if is_vision_mixer {
                let num_inputs = self.get_vision_mixer_num_inputs(flow_id, block_id).await;
                let new_pvw = old_pgm;
                let pipelines = self.inner.pipelines.read().await;
                if let Some(manager) = pipelines.get(flow_id) {
                    let _ = manager
                        .update_vision_mixer_after_take(block_id, new_pgm, new_pvw, num_inputs);
                }
                drop(pipelines);

                // Broadcast vision mixer state change. Reads authoritative
                // post-take state from the overlay so PiP-aware takes are
                // reflected (the local new_pgm/new_pvw are input-centric and
                // don't carry PiP info).
                let overlay = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(
                    block_instance_id,
                );
                let preview_input = overlay.as_ref().and_then(|s| s.pvw_input());
                let program_input = overlay.as_ref().and_then(|s| s.pgm_input());
                let preview_pip = overlay.as_ref().and_then(|s| s.pvw_pip());
                let program_pip = overlay.as_ref().and_then(|s| s.pgm_pip());
                self.inner
                    .events
                    .broadcast(StromEvent::VisionMixerStateChanged {
                        flow_id: *flow_id,
                        block_id: block_id.to_string(),
                        preview_input,
                        program_input,
                        preview_pip,
                        program_pip,
                    });
            }
        }

        // Broadcast transition event
        self.inner
            .events
            .broadcast(StromEvent::TransitionTriggered {
                flow_id: *flow_id,
                block_instance_id: block_instance_id.to_string(),
                from_input,
                to_input,
                transition_type: transition_type.to_string(),
                duration_ms,
            });

        Ok(actual_kind)
    }

    /// Select a preview input on a vision mixer block.
    ///
    /// Replaces the PVW source with `input` (clearing any PiP-on-PVW mode).
    ///
    /// Returns `(new_pvw, current_pgm)`. Either is `None` when the bus is on
    /// a PiP source.
    pub async fn select_vision_mixer_preview(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        input: usize,
    ) -> Result<(Option<usize>, Option<usize>), PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        // Get num_inputs from block properties
        let num_inputs = self
            .get_vision_mixer_num_inputs(flow_id, block_instance_id)
            .await;

        let (new_pvw, pgm) =
            manager.select_vision_mixer_preview(block_instance_id, input, num_inputs)?;

        drop(pipelines);

        // Broadcast state change event. Reads authoritative state from the
        // overlay so PiP visibility is reflected alongside the inputs.
        let overlay =
            crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(block_instance_id);
        let preview_pip = overlay.as_ref().and_then(|s| s.pvw_pip());
        let program_pip = overlay.as_ref().and_then(|s| s.pgm_pip());
        self.inner
            .events
            .broadcast(StromEvent::VisionMixerStateChanged {
                flow_id: *flow_id,
                block_id: block_instance_id.to_string(),
                preview_input: new_pvw,
                program_input: pgm,
                preview_pip,
                program_pip,
            });

        Ok((new_pvw, pgm))
    }

    /// Select a PiP composition as the preview source on a vision mixer block.
    pub async fn select_vision_mixer_pip_for_preview(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        pip_idx: usize,
    ) -> Result<(), PipelineError> {
        let pipelines = self.inner.pipelines.read().await;
        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;
        manager.select_vision_mixer_pip_for_preview(block_instance_id, pip_idx)?;
        Ok(())
    }

    /// Update a PiP composition (bg + overlays) on a vision mixer block at runtime.
    pub async fn apply_vision_mixer_pip_config(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        pip_idx: usize,
        bg: Option<usize>,
        zones: Vec<strom_types::vision_mixer::Zone>,
        transforms: strom_types::vision_mixer::PipTransforms,
    ) -> Result<(), PipelineError> {
        let pipelines = self.inner.pipelines.read().await;
        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;
        manager.apply_vision_mixer_pip_config(block_instance_id, pip_idx, bg, zones, transforms)?;
        Ok(())
    }

    /// Read the negotiated input resolutions for a vision mixer block.
    /// Returns all-`None` when the pipeline isn't running.
    pub async fn vision_mixer_input_resolutions(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        num_inputs: usize,
    ) -> Vec<Option<strom_types::vision_mixer::InputResolution>> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines
            .get(flow_id)
            .map(|m| m.vision_mixer_input_resolutions(block_instance_id, num_inputs))
            .unwrap_or_else(|| vec![None; num_inputs])
    }

    /// Get num_inputs for a vision mixer block from the flow definition.
    async fn get_vision_mixer_num_inputs(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
    ) -> usize {
        use crate::blocks::builtin::vision_mixer::properties as vm_props;
        let flows = self.inner.flows.read().await;
        flows
            .get(flow_id)
            .and_then(|flow| flow.blocks.iter().find(|b| b.id == block_instance_id))
            .map(|block| vm_props::parse_num_inputs(&block.properties))
            .unwrap_or(4)
    }

    /// Toggle a DSK (Downstream Keyer) layer on a vision mixer block.
    pub async fn set_dsk_enabled(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        dsk_index: usize,
        enabled: bool,
    ) -> Result<(), PipelineError> {
        let num_inputs = self
            .get_vision_mixer_num_inputs(flow_id, block_instance_id)
            .await;
        let pipelines = self.inner.pipelines.read().await;
        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;
        manager.set_dsk_enabled(block_instance_id, dsk_index, num_inputs, enabled)?;
        drop(pipelines);

        // Broadcast DSK state change (1-based dsk number)
        self.inner
            .events
            .broadcast(StromEvent::VisionMixerDskChanged {
                flow_id: *flow_id,
                block_id: block_instance_id.to_string(),
                dsk: dsk_index + 1,
                enabled,
            });

        Ok(())
    }

    /// Set the multiview overlay alpha on a vision mixer block.
    pub async fn set_overlay_alpha(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        alpha: f64,
    ) -> Result<(), PipelineError> {
        let num_inputs = self
            .get_vision_mixer_num_inputs(flow_id, block_instance_id)
            .await;
        let pipelines = self.inner.pipelines.read().await;
        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;
        manager.set_overlay_alpha(block_instance_id, num_inputs, alpha)?;
        drop(pipelines);

        self.inner
            .events
            .broadcast(StromEvent::VisionMixerOverlayAlphaChanged {
                flow_id: *flow_id,
                block_id: block_instance_id.to_string(),
                alpha,
            });

        Ok(())
    }

    /// Toggle Fade to Black on a vision mixer block.
    pub async fn fade_to_black(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        duration_ms: u64,
    ) -> Result<bool, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;
        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;
        let active = manager.fade_to_black(block_instance_id, duration_ms)?;
        drop(pipelines);

        self.inner
            .events
            .broadcast(StromEvent::VisionMixerFtbChanged {
                flow_id: *flow_id,
                block_id: block_instance_id.to_string(),
                active,
            });

        Ok(active)
    }

    /// Set a shader video effect on a vision mixer block (input look or PGM
    /// master). GPU FX engine only.
    pub async fn set_vision_mixer_effect(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        target: strom_types::effects::EffectTarget,
        effect: &strom_types::effects::VideoEffect,
    ) -> Result<strom_types::effects::VideoEffect, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;
        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;
        let applied = manager.set_vision_mixer_effect(block_instance_id, target, effect)?;
        drop(pipelines);

        self.inner
            .events
            .broadcast(StromEvent::VisionMixerEffectChanged {
                flow_id: *flow_id,
                block_id: block_instance_id.to_string(),
                target,
                effect: applied.clone(),
            });

        Ok(applied)
    }

    /// Whether the shader FX engine is built into a vision mixer block's
    /// running pipeline.
    pub async fn vision_mixer_fx_available(&self, flow_id: &FlowId, block_id: &str) -> bool {
        let pipelines = self.inner.pipelines.read().await;
        pipelines
            .get(flow_id)
            .map(|m| m.vision_mixer_fx_available(block_id))
            .unwrap_or(false)
    }

    /// Reset accumulated loudness measurements on an EBU R128 meter block.
    pub async fn reset_loudness(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<(), PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.reset_loudness(block_id)?;

        Ok(())
    }

    pub async fn recorder_split_now(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Result<(), PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.recorder_split_now(block_id)?;

        Ok(())
    }

    /// Animate a single input's position/size on a compositor block.
    #[allow(clippy::too_many_arguments)]
    pub async fn animate_input(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        input_index: usize,
        target_xpos: Option<i32>,
        target_ypos: Option<i32>,
        target_width: Option<i32>,
        target_height: Option<i32>,
        duration_ms: u64,
    ) -> Result<(), PipelineError> {
        info!(
            "Animating input {} on block {} in flow {}",
            input_index, block_instance_id, flow_id
        );

        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.animate_input(
            block_instance_id,
            input_index,
            target_xpos,
            target_ypos,
            target_width,
            target_height,
            duration_ms,
        )?;

        drop(pipelines);

        // Sync final values back to flow definition for persistence
        // block_instance_id format: "block_id:element_name" (e.g., "b0:mixer")
        if let Some(block_id) = block_instance_id.split(':').next() {
            let mut flows = self.inner.flows.write().await;
            if let Some(flow) = flows.get_mut(flow_id) {
                if let Some(block) = flow.blocks.iter_mut().find(|b| b.id == block_id) {
                    // Update the block properties with target values
                    if let Some(x) = target_xpos {
                        block.properties.insert(
                            format!("input_{}_xpos", input_index),
                            PropertyValue::Int(x as i64),
                        );
                    }
                    if let Some(y) = target_ypos {
                        block.properties.insert(
                            format!("input_{}_ypos", input_index),
                            PropertyValue::Int(y as i64),
                        );
                    }
                    if let Some(w) = target_width {
                        block.properties.insert(
                            format!("input_{}_width", input_index),
                            PropertyValue::Int(w as i64),
                        );
                    }
                    if let Some(h) = target_height {
                        block.properties.insert(
                            format!("input_{}_height", input_index),
                            PropertyValue::Int(h as i64),
                        );
                    }
                    trace!(
                        "Synced animated input {} properties to block {}",
                        input_index,
                        block_id
                    );
                }
            }
            drop(flows);

            // Mark flow for debounced save
            self.mark_flow_dirty(*flow_id).await;
        }

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
    /// Also syncs the change back to the flow definition for persistence.
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

        // Update the running pipeline
        {
            let pipelines = self.inner.pipelines.read().await;
            let manager = pipelines.get(flow_id).ok_or_else(|| {
                PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
            })?;
            manager.update_pad_property(element_id, pad_name, property_name, &value)?;
        }

        // Sync change back to flow definition for persistence
        // Element ID format: "block_id:element_name" (e.g., "b0:mixer")
        // Pad name format: "sink_N" (e.g., "sink_0")
        if let Some(block_id) = element_id.split(':').next() {
            if let Some(input_index) = pad_name
                .strip_prefix("sink_")
                .and_then(|s| s.parse::<usize>().ok())
            {
                // Map pad property to block property name
                // Note: GStreamer uses hyphens (sizing-policy) but block properties use underscores (sizing_policy)
                let property_name_normalized = property_name.replace('-', "_");
                let block_property_name =
                    format!("input_{}_{}", input_index, property_name_normalized);

                let mut flows = self.inner.flows.write().await;
                if let Some(flow) = flows.get_mut(flow_id) {
                    if let Some(block) = flow.blocks.iter_mut().find(|b| b.id == block_id) {
                        // Update the block property
                        block
                            .properties
                            .insert(block_property_name.clone(), value.clone());
                        trace!(
                            "Synced pad property to block: {} -> {}={:?}",
                            pad_name,
                            block_property_name,
                            value
                        );
                    }
                }
                drop(flows);

                // Mark flow for debounced save
                self.mark_flow_dirty(*flow_id).await;
            }
        }

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
    ///
    /// Uses block_in_place so the synchronous GStreamer promise.wait() calls
    /// don't prevent tokio from scheduling other tasks on other threads.
    pub async fn get_webrtc_stats(
        &self,
        flow_id: &FlowId,
    ) -> Result<strom_types::api::WebRtcStats, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        let stats = tokio::task::block_in_place(|| manager.get_webrtc_stats());
        Ok(stats)
    }

    /// Get SRT statistics from a running flow's pipeline.
    ///
    /// Returns curated stats for every `srtsink`/`srtsrc` element in the pipeline.
    pub async fn get_srt_stats(
        &self,
        flow_id: &FlowId,
    ) -> Result<strom_types::api::SrtStats, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        let stats = manager.get_srt_stats();
        Ok(stats)
    }

    /// Query the latency of a running pipeline.
    /// Returns (min_latency_ns, max_latency_ns, live) if query succeeds.
    pub async fn get_flow_latency(&self, flow_id: &FlowId) -> Option<(u64, u64, bool)> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines.get(flow_id).and_then(|p| p.query_latency())
    }

    /// Get RTP statistics for a running flow.
    /// Returns jitterbuffer statistics from RTP-based blocks like AES67 Input.
    pub async fn get_flow_rtp_stats(
        &self,
        flow_id: &FlowId,
    ) -> Option<strom_types::stats::FlowStats> {
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

    /// Get negotiated caps for all pads in a running flow's pipeline.
    pub async fn get_flow_pad_caps(
        &self,
        flow_id: &FlowId,
    ) -> Option<std::collections::HashMap<String, Vec<(String, String, String)>>> {
        let pipelines = self.inner.pipelines.read().await;
        pipelines.get(flow_id).map(|p| p.get_all_pad_caps())
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

    /// Capture a thumbnail from a block's tee element at the given index.
    ///
    /// Works with any block that exposes thumbnail tee elements, including
    /// `builtin.thumbnail` (index 0) and compositor blocks (one per input).
    pub async fn capture_block_thumbnail(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        index: usize,
    ) -> Result<Vec<u8>, PipelineError> {
        let pipelines = self.inner.pipelines.read().await;

        let manager = pipelines.get(flow_id).ok_or_else(|| {
            PipelineError::InvalidFlow(format!("Pipeline not running for flow: {}", flow_id))
        })?;

        manager.capture_block_thumbnail(block_id, index)
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::with_json_storage(
            "flows.json",
            "blocks.json",
            "media",
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            vec!["239.255.255.255".to_string(), "224.2.127.254".to_string()],
        )
    }
}

/// Convert a GStreamer debug level to its numeric value.
fn gst_level_to_int(level: gstreamer::DebugLevel) -> u32 {
    match level {
        gstreamer::DebugLevel::None => 0,
        gstreamer::DebugLevel::Error => 1,
        gstreamer::DebugLevel::Warning => 2,
        gstreamer::DebugLevel::Fixme => 3,
        gstreamer::DebugLevel::Info => 4,
        gstreamer::DebugLevel::Debug => 5,
        gstreamer::DebugLevel::Log => 6,
        gstreamer::DebugLevel::Trace => 7,
        gstreamer::DebugLevel::Memdump => 9,
        _ => 0,
    }
}

/// Parse a GStreamer debug level from a string (number 0-9).
fn parse_gst_level(s: &str) -> Result<gstreamer::DebugLevel, String> {
    let n: u32 = s
        .parse()
        .map_err(|_| format!("Invalid GStreamer debug level '{}': expected 0-9", s))?;
    match n {
        0 => Ok(gstreamer::DebugLevel::None),
        1 => Ok(gstreamer::DebugLevel::Error),
        2 => Ok(gstreamer::DebugLevel::Warning),
        3 => Ok(gstreamer::DebugLevel::Fixme),
        4 => Ok(gstreamer::DebugLevel::Info),
        5 => Ok(gstreamer::DebugLevel::Debug),
        6 => Ok(gstreamer::DebugLevel::Log),
        7 => Ok(gstreamer::DebugLevel::Trace),
        9 => Ok(gstreamer::DebugLevel::Memdump),
        _ => Err(format!(
            "Invalid GStreamer debug level '{}': expected 0-7 or 9",
            n
        )),
    }
}

#[cfg(test)]
mod mixer_solo_intent_tests {
    use super::*;
    use crate::storage::JsonFileStorage;
    use std::sync::Once;
    use tempfile::NamedTempFile;

    static GST_INIT: Once = Once::new();

    fn new_state() -> AppState {
        // AppState construction touches GStreamer registries; init once.
        GST_INIT.call_once(|| {
            gstreamer::init().expect("gstreamer init failed in test");
        });
        let storage_file = NamedTempFile::new().unwrap();
        let blocks_file = NamedTempFile::new().unwrap();
        let storage = JsonFileStorage::new(storage_file.path());
        AppState::new(
            storage,
            blocks_file.path(),
            std::env::temp_dir(),
            vec![],
            "all".to_string(),
            vec![],
        )
    }

    #[tokio::test]
    async fn solo_intent_starts_empty_and_records_writes() {
        let state = new_state();
        let flow = FlowId::new_v4();
        assert!(!state.mixer_any_solo_active(&flow, "mix1").await);

        state
            .set_mixer_solo_intent(&flow, "mix1", "ch1_pfl", true)
            .await;
        assert!(state.mixer_any_solo_active(&flow, "mix1").await);

        state
            .set_mixer_solo_intent(&flow, "mix1", "ch1_pfl", false)
            .await;
        assert!(!state.mixer_any_solo_active(&flow, "mix1").await);
    }

    #[tokio::test]
    async fn solo_intent_tracks_multiple_channels_independently() {
        let state = new_state();
        let flow = FlowId::new_v4();
        // Two channels engaged on the same block.
        state
            .set_mixer_solo_intent(&flow, "mix1", "ch1_pfl", true)
            .await;
        state
            .set_mixer_solo_intent(&flow, "mix1", "ch2_afl", true)
            .await;
        assert!(state.mixer_any_solo_active(&flow, "mix1").await);

        // Releasing only one channel must not flip the gate.
        state
            .set_mixer_solo_intent(&flow, "mix1", "ch1_pfl", false)
            .await;
        assert!(
            state.mixer_any_solo_active(&flow, "mix1").await,
            "monitor should stay on solo while ch2_afl is still engaged"
        );

        // Releasing the last engaged channel returns to no-solo.
        state
            .set_mixer_solo_intent(&flow, "mix1", "ch2_afl", false)
            .await;
        assert!(!state.mixer_any_solo_active(&flow, "mix1").await);
    }

    #[tokio::test]
    async fn solo_intent_is_scoped_per_block_and_per_flow() {
        let state = new_state();
        let flow_a = FlowId::new_v4();
        let flow_b = FlowId::new_v4();
        state
            .set_mixer_solo_intent(&flow_a, "mixA", "ch1_pfl", true)
            .await;
        assert!(state.mixer_any_solo_active(&flow_a, "mixA").await);
        assert!(!state.mixer_any_solo_active(&flow_a, "mixB").await);
        assert!(!state.mixer_any_solo_active(&flow_b, "mixA").await);
    }

    #[tokio::test]
    async fn solo_intent_is_cleared_on_stop_flow() {
        let state = new_state();
        let flow = FlowId::new_v4();
        state
            .set_mixer_solo_intent(&flow, "mix1", "ch1_pfl", true)
            .await;
        assert!(state.mixer_any_solo_active(&flow, "mix1").await);

        // stop_flow on an unknown flow is a no-op for the pipeline map but
        // still drops the solo intent — matches the `persist: false`
        // semantics of the underlying PFL/AFL bools.
        let _ = state.stop_flow(&flow).await;
        assert!(
            !state.mixer_any_solo_active(&flow, "mix1").await,
            "stop_flow must purge cached solo intent so a restart starts clean"
        );
    }

    #[tokio::test]
    async fn solo_intent_set_false_is_idempotent_when_empty() {
        // Writing false to a channel that was never engaged should leave
        // the cache empty (no spurious entry).
        let state = new_state();
        let flow = FlowId::new_v4();
        state
            .set_mixer_solo_intent(&flow, "mix1", "ch1_pfl", false)
            .await;
        assert!(!state.mixer_any_solo_active(&flow, "mix1").await);
        let map = state.inner.mixer_solo_state.read().await;
        assert!(map.is_empty(), "no flow entry should be created for false");
    }
}

#[cfg(test)]
mod ramp_ms_resolution_tests {
    use super::resolve_ramp_ms;
    use std::collections::HashMap;

    #[test]
    fn override_wins_over_global() {
        let mut overrides = HashMap::new();
        overrides.insert("ch1_fader_db".to_string(), 500);
        assert_eq!(
            resolve_ramp_ms("ch1_fader_db", Some(&overrides), Some(50)),
            Some(500)
        );
    }

    #[test]
    fn falls_back_to_global_when_no_override_for_name() {
        let mut overrides = HashMap::new();
        overrides.insert("ch2_fader_db".to_string(), 500);
        assert_eq!(
            resolve_ramp_ms("ch1_fader_db", Some(&overrides), Some(50)),
            Some(50)
        );
    }

    #[test]
    fn falls_back_to_global_when_overrides_absent() {
        assert_eq!(resolve_ramp_ms("ch1_fader_db", None, Some(50)), Some(50));
    }

    #[test]
    fn returns_none_when_neither_set() {
        assert_eq!(resolve_ramp_ms("ch1_fader_db", None, None), None);
    }

    #[test]
    fn override_used_even_when_global_is_none() {
        let mut overrides = HashMap::new();
        overrides.insert("ch1_fader_db".to_string(), 500);
        assert_eq!(
            resolve_ramp_ms("ch1_fader_db", Some(&overrides), None),
            Some(500)
        );
    }
}
