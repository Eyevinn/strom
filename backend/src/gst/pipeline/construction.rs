use super::{PipelineError, PipelineManager, QoSAggregator};
use crate::blocks::BlockRegistry;
use crate::events::EventBroadcaster;
use crate::whip_registry::WhipRegistry;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_net as gst_net;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use strom_types::{BlockDefinition, Element, Flow, PipelineState};
use tracing::{debug, error, info, warn};

impl PipelineManager {
    /// Create a new pipeline from a flow definition.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        flow: &Flow,
        events: EventBroadcaster,
        _block_registry: &BlockRegistry,
        ice_servers: Vec<String>,
        ice_transport_policy: String,
        whip_registry: Option<WhipRegistry>,
        media_path: std::path::PathBuf,
        local_devices: crate::discovery::device::GstDeviceMap,
    ) -> Result<Self, PipelineError> {
        info!("Creating pipeline for flow: {} ({})", flow.name, flow.id);
        info!(
            "Flow has {} elements, {} blocks, {} links",
            flow.elements.len(),
            flow.blocks.len(),
            flow.links.len()
        );

        let pipeline = gst::Pipeline::builder()
            .name(format!("flow-{}", flow.id))
            .build();
        info!("Created GStreamer pipeline object");

        // Create shared storage for dynamically created webrtcbins (from consumer-added callbacks)
        let dynamic_webrtcbins: crate::blocks::DynamicWebrtcbinStore =
            Arc::new(Mutex::new(HashMap::new()));

        // Create shared thread config for session pipelines (populated in start())
        let session_thread_config = crate::gst::SessionThreadConfig::new();

        let probe_manager =
            crate::gst::buffer_age_probe::ProbeManager::new(flow.id, events.clone());

        let mut manager = Self {
            flow_id: flow.id,
            flow_name: flow.name.clone(),
            pipeline,
            elements: HashMap::new(),
            events,
            pending_links: Vec::new(),
            properties: flow.properties.clone(),
            pad_properties: HashMap::new(),
            block_message_handlers: Vec::new(),
            block_message_connect_fns: Vec::new(),
            element_setup_fns: Vec::new(),
            thread_priority_state: None,
            thread_registry: None,
            assigned_cpus: None,
            session_thread_config: session_thread_config.clone(),
            cached_state: std::sync::Arc::new(std::sync::RwLock::new(PipelineState::Null)),
            qos_aggregator: QoSAggregator::new(),
            qos_broadcast_task: None,
            ptp_clock: None,
            ptp_stats: std::sync::Arc::new(std::sync::RwLock::new(None)),
            ntp_clock: None,
            ptp_stats_callback: None,
            dynamic_pad_tees: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
            whep_endpoints: Vec::new(),
            whip_endpoints: Vec::new(),
            whip_endpoint_configs: Vec::new(),
            dynamic_webrtcbins: Arc::clone(&dynamic_webrtcbins),
            thumbnail_taps: crate::gst::new_tap_store(),
            thumbnail_deactivation_task: None,
            probe_manager,
            blocks: flow.blocks.clone(),
            block_definitions: HashMap::new(),
            volume_ramps: crate::gst::volume_ramp::VolumeRampManager::new(),
        };

        // Expand blocks into GStreamer elements
        info!("Starting block expansion (block_in_place)...");
        let flow_id = flow.id;
        let blocks_for_defs = flow.blocks.clone();
        let registry_ref = _block_registry;
        let (expanded, block_definitions) = tokio::task::block_in_place(|| {
            info!("Inside block_in_place, calling block_on...");
            tokio::runtime::Handle::current().block_on(async {
                info!("Inside block_on, calling expand_blocks...");
                let result = super::super::block_expansion::expand_blocks(
                    &blocks_for_defs,
                    &flow.links,
                    &flow_id,
                    &media_path,
                    ice_servers,
                    ice_transport_policy,
                    dynamic_webrtcbins,
                    whip_registry,
                    session_thread_config,
                    local_devices,
                )
                .await;
                info!("expand_blocks completed");

                // Resolve block definitions for automatic probe attachment
                let mut defs: HashMap<String, BlockDefinition> = HashMap::new();
                for block in &blocks_for_defs {
                    if !defs.contains_key(&block.block_definition_id) {
                        if let Some(def) = registry_ref.get_by_id(&block.block_definition_id).await
                        {
                            defs.insert(block.block_definition_id.clone(), def);
                        }
                    }
                }

                result.map(|expanded| (expanded, defs))
            })
        })?;
        info!("Block expansion completed");
        manager.block_definitions = block_definitions;

        // Add regular elements from flow
        debug!(
            "Adding {} regular elements from flow...",
            flow.elements.len()
        );
        for (idx, element) in flow.elements.iter().enumerate() {
            debug!(
                "Adding element {}/{}: {} (type: {})",
                idx + 1,
                flow.elements.len(),
                element.id,
                element.element_type
            );
            manager.add_element(element)?;
            debug!("Successfully added element: {}", element.id);
        }
        debug!("All regular elements added");

        // Add GStreamer elements from expanded blocks
        let block_element_count = expanded.gst_elements.len();
        debug!("Adding {} block elements...", block_element_count);
        let mut idx = 0;
        for (element_id, gst_element) in expanded.gst_elements {
            idx += 1;
            debug!(
                "Adding block element {}/{}: {}",
                idx, block_element_count, element_id
            );
            manager.pipeline.add(&gst_element).map_err(|e| {
                PipelineError::ElementCreation(format!(
                    "Failed to add block element {} to pipeline: {}",
                    element_id, e
                ))
            })?;
            manager.elements.insert(element_id.clone(), gst_element);
            debug!("Successfully added block element: {}", element_id);
        }
        debug!("All block elements added");

        // Store bus message handler connection functions from blocks
        debug!(
            "Storing {} bus message handler(s) from blocks",
            expanded.bus_message_handlers.len()
        );
        manager.block_message_connect_fns = expanded.bus_message_handlers;

        // Store element signal setup functions from blocks
        if !expanded.element_setups.is_empty() {
            debug!(
                "Storing {} element signal setup(s) from blocks",
                expanded.element_setups.len()
            );
        }
        manager.element_setup_fns = expanded.element_setups;

        // Merge pad properties from blocks with existing pad properties
        info!(
            "Merging {} element(s) with pad properties from blocks",
            expanded.pad_properties.len()
        );
        if !expanded.pad_properties.is_empty() {
            for (elem_id, pads) in &expanded.pad_properties {
                info!("Element {}: {} pad(s) with properties", elem_id, pads.len());
            }
        }
        manager.pad_properties.extend(expanded.pad_properties);

        // Store WHEP endpoints from blocks
        if !expanded.whep_endpoints.is_empty() {
            info!(
                "Storing {} WHEP endpoint(s) from blocks",
                expanded.whep_endpoints.len()
            );
        }
        manager.whep_endpoints = expanded.whep_endpoints;

        // Store WHIP endpoints from blocks
        if !expanded.whip_endpoints.is_empty() {
            info!(
                "Storing {} WHIP endpoint(s) from blocks",
                expanded.whip_endpoints.len()
            );
        }
        manager.whip_endpoints = expanded.whip_endpoints;
        manager.whip_endpoint_configs = expanded.whip_endpoint_configs;

        // Analyze links and auto-insert tee elements where needed
        let all_links = expanded.links;
        debug!("Analyzing links and inserting tee elements if needed...");
        let processed_links = Self::insert_tees_if_needed(&all_links);
        info!(
            "Link analysis complete: {} links, {} tees",
            processed_links.links.len(),
            processed_links.tees.len()
        );

        // Create tee elements
        info!("Creating {} tee elements...", processed_links.tees.len());
        for (idx, tee_id) in processed_links.tees.keys().enumerate() {
            info!(
                "Creating tee {}/{}: {}",
                idx + 1,
                processed_links.tees.len(),
                tee_id
            );
            manager.add_tee_element(tee_id)?;
            info!("Successfully created tee: {}", tee_id);
        }
        info!("All tee elements created");

        // Link elements according to processed links
        debug!("Linking {} elements...", processed_links.links.len());
        for (idx, link) in processed_links.links.iter().enumerate() {
            debug!(
                "Linking {}/{}: {} -> {}",
                idx + 1,
                processed_links.links.len(),
                link.from,
                link.to
            );
            if let Err(e) = manager.try_link_elements(link) {
                info!(
                    "Could not link immediately: {} -> {} (error: {}). Will retry when pad becomes available.",
                    link.from, link.to, e
                );
                // Store as pending link
                manager.pending_links.push(link.clone());
            } else {
                info!("Successfully linked: {} -> {}", link.from, link.to);
            }
        }
        debug!(
            "Linking phase complete ({} pending links)",
            manager.pending_links.len()
        );

        // Set up dynamic pad handlers for all elements that might have dynamic pads
        debug!("Setting up dynamic pad handlers...");
        manager.setup_dynamic_pad_handlers();
        debug!("Dynamic pad handlers set up");

        // Note: Pad properties are applied in start() after reaching READY state
        // This ensures aggregator request pads are fully initialized before accessing them

        // Enable QoS on all pads (both with and without user-defined properties)
        debug!("Enabling QoS on all pads...");
        manager.enable_qos_on_all_pads();
        debug!("QoS enabled on all pads");

        // Note: Bus watch is set up when pipeline starts, not here
        info!("Pipeline created successfully for flow: {}", flow.name);
        Ok(manager)
    }

    /// Add an element to the pipeline.
    pub(super) fn add_element(&mut self, element_def: &Element) -> Result<(), PipelineError> {
        debug!(
            "Creating element {} (type: {})",
            element_def.id, element_def.element_type
        );

        // Create the element
        let element = gst::ElementFactory::make(&element_def.element_type)
            .name(&element_def.id)
            .build()
            .map_err(|e| {
                error!("Failed to create element {}: {}", element_def.id, e);
                PipelineError::ElementCreation(format!(
                    "{}: {} - {}",
                    element_def.id, element_def.element_type, e
                ))
            })?;

        // Enable QoS by default if the element supports it (for buffer drop monitoring)
        if element.has_property("qos") {
            element.set_property("qos", true);
            debug!("Enabled QoS on element {}", element_def.id);
        }

        // Default is-live=true for test sources (unless explicitly set by user)
        if (element_def.element_type == "videotestsrc"
            || element_def.element_type == "audiotestsrc")
            && !element_def.properties.contains_key("is-live")
        {
            element.set_property("is-live", true);
            debug!("Enabled is-live on test source {}", element_def.id);
        }

        // Set properties
        if !element_def.properties.is_empty() {
            debug!(
                "Setting {} properties for element {}",
                element_def.properties.len(),
                element_def.id
            );
        }
        for (prop_name, prop_value) in &element_def.properties {
            // Pipeline isn't running yet — no stream-time available — so any
            // volume_ramp routing inside set_property falls back to a direct
            // set. Pass None to make that explicit.
            self.set_property(&element, &element_def.id, prop_name, prop_value, None)?;
        }

        // Store pad properties for later application (after pads are created)
        if !element_def.pad_properties.is_empty() {
            debug!(
                "Storing {} pad properties for element {}",
                element_def.pad_properties.len(),
                element_def.id
            );
            self.pad_properties
                .insert(element_def.id.clone(), element_def.pad_properties.clone());
        }

        // Add to pipeline
        self.pipeline.add(&element).map_err(|e| {
            error!("Failed to add {} to pipeline: {}", element_def.id, e);
            PipelineError::ElementCreation(format!(
                "Failed to add {} to pipeline: {}",
                element_def.id, e
            ))
        })?;

        self.elements.insert(element_def.id.clone(), element);
        Ok(())
    }

    /// Add a tee element to the pipeline.
    pub(super) fn add_tee_element(&mut self, tee_id: &str) -> Result<(), PipelineError> {
        debug!("Creating auto-inserted tee element: {}", tee_id);

        let tee = gst::ElementFactory::make("tee")
            .name(tee_id)
            .build()
            .map_err(|e| {
                PipelineError::ElementCreation(format!("Failed to create tee {}: {}", tee_id, e))
            })?;

        // Configure tee to allow branches to not be linked without affecting other branches
        // This prevents one branch from blocking others when it goes to PAUSED
        tee.set_property("allow-not-linked", true);

        self.pipeline.add(&tee).map_err(|e| {
            PipelineError::ElementCreation(format!(
                "Failed to add tee {} to pipeline: {}",
                tee_id, e
            ))
        })?;

        self.elements.insert(tee_id.to_string(), tee);
        Ok(())
    }

    /// Configure the pipeline clock based on flow properties.
    ///
    /// For direct media timing (AES67), we always set:
    /// - base_time = 0
    /// - start_time = None (GST_CLOCK_TIME_NONE)
    ///
    /// This ensures that RTP timestamps directly correspond to the reference clock,
    /// which is required for `a=mediaclk:direct=0` signaling in SDP (RFC 7273).
    pub(super) fn configure_clock(&mut self) -> Result<(), PipelineError> {
        use strom_types::flow::GStreamerClockType;

        match self.properties.clock_type {
            GStreamerClockType::Ptp => {
                let domain = self.properties.ptp_domain.unwrap_or(0);
                info!(
                    "Configuring PTP clock for pipeline '{}' with domain {}",
                    self.flow_name, domain
                );

                // Initialize PTP clock (on all interfaces)
                // This is a global init, so it's OK if it was already initialized
                if let Err(e) = gst_net::PtpClock::init(None, &[]) {
                    warn!(
                        "PTP clock initialization warning (may already be initialized): {}",
                        e
                    );
                }

                // Create a PTP clock instance for this domain
                let ptp_clock = gst_net::PtpClock::new(None, domain as u32).map_err(|e| {
                    PipelineError::StateChange(format!("Failed to create PTP clock: {}", e))
                })?;

                // Force the pipeline to use the PTP clock (use_clock, not set_clock!)
                // use_clock() forces the pipeline to always use this clock, even if
                // other clock providers are added. set_clock() would allow the pipeline
                // to auto-select a different clock during state changes.
                self.pipeline.use_clock(Some(&ptp_clock));

                // Log initial PTP state
                let synced = ptp_clock.is_synced();
                let gm_id = ptp_clock.grandmaster_clock_id();
                let master_id = ptp_clock.master_clock_id();
                info!(
                    "PTP clock configured: domain={}, synced={}, grandmaster={}, master={}",
                    domain,
                    synced,
                    strom_types::flow::PtpInfo::format_clock_id(gm_id),
                    strom_types::flow::PtpInfo::format_clock_id(master_id)
                );

                // Set up callback for grandmaster changes
                let flow_name = self.flow_name.clone();
                ptp_clock.connect_grandmaster_clock_id_notify(move |clock| {
                    let gm_id = clock.grandmaster_clock_id();
                    let synced = clock.is_synced();
                    info!(
                        "[{}] PTP grandmaster changed: {}, synced={}",
                        flow_name,
                        strom_types::flow::PtpInfo::format_clock_id(gm_id),
                        synced
                    );
                });

                // Store PTP clock reference for later queries
                self.ptp_clock = Some(ptp_clock);

                // Set up PTP statistics callback
                let ptp_stats = self.ptp_stats.clone();
                let stats_flow_name = self.flow_name.clone();
                let stats_domain = domain;
                let stats_callback = gst_net::PtpClock::add_statistics_callback(
                    move |callback_domain, stats| {
                        // Only process stats for our domain
                        if callback_domain != stats_domain {
                            return glib::ControlFlow::Continue;
                        }

                        // Check if this is a TIME_UPDATED event (has the fields we want)
                        let name = stats.name();
                        if name == "GstPtpStatisticsTimeUpdated" {
                            // Extract statistics from the GstStructure
                            let mean_path_delay_ns = stats.get::<u64>("mean-path-delay-avg").ok();
                            let clock_offset_ns = stats.get::<i64>("discontinuity").ok();
                            let r_squared = stats.get::<f64>("r-squared").ok();
                            let clock_rate = stats.get::<f64>("rate").ok();

                            // Update stored stats
                            if let Ok(mut guard) = ptp_stats.write() {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0);

                                *guard = Some(strom_types::flow::PtpStats {
                                    mean_path_delay_ns,
                                    clock_offset_ns,
                                    r_squared,
                                    clock_rate,
                                    last_update: Some(now),
                                });
                            }

                            // Log significant clock corrections (> 100µs)
                            if let Some(offset) = clock_offset_ns {
                                if offset.abs() > 100_000 {
                                    tracing::debug!(
                                        "[{}] PTP clock correction: {}µs, path_delay: {}µs, r²: {:.4}",
                                        stats_flow_name,
                                        offset / 1000,
                                        mean_path_delay_ns.unwrap_or(0) / 1000,
                                        r_squared.unwrap_or(0.0)
                                    );
                                }
                            }
                        }

                        glib::ControlFlow::Continue
                    },
                );
                self.ptp_stats_callback = Some(stats_callback);
                info!("PTP statistics callback registered for domain {}", domain);
            }
            GStreamerClockType::Monotonic => {
                info!("Using Monotonic clock for pipeline '{}'", self.flow_name);
                // SystemClock::obtain() returns the global singleton (default clock-type=MONOTONIC).
                // Safe to reuse here because we never modify clock-type on the singleton —
                // other clock types create fresh SystemClock instances below.
                let clock = gst::SystemClock::obtain();
                self.pipeline.use_clock(Some(&clock));
            }
            GStreamerClockType::Realtime => {
                info!(
                    "Using Realtime (UTC) clock for pipeline '{}'",
                    self.flow_name
                );
                // Create a NEW SystemClock instance instead of using the singleton;
                // setting `clock-type` on the singleton would break every other pipeline
                // that holds a reference to it (Monotonic flows would suddenly see UTC).
                let clock: gst::SystemClock = glib::Object::builder()
                    .property("clock-type", gst::ClockType::Realtime)
                    .build();
                self.pipeline.use_clock(Some(&clock));
            }
            GStreamerClockType::Tai => {
                info!("Using TAI clock for pipeline '{}'", self.flow_name);
                // Create a NEW SystemClock instance; see Realtime branch for rationale.
                let clock: gst::SystemClock = glib::Object::builder()
                    .property("clock-type", gst::ClockType::Tai)
                    .build();
                self.pipeline.use_clock(Some(&clock));
            }
            GStreamerClockType::Ntp => {
                let server = self
                    .properties
                    .ntp_server
                    .clone()
                    .unwrap_or_else(|| "pool.ntp.org".to_string());
                let port = self.properties.ntp_port.unwrap_or(123);
                info!(
                    "Using NTP clock for pipeline '{}' (server={}, port={})",
                    self.flow_name, server, port
                );

                let ntp_clock = gst_net::NtpClock::new(
                    Some(&format!("strom-ntp-{}", self.flow_name)),
                    &server,
                    port as i32,
                    gst::ClockTime::ZERO,
                );
                self.pipeline.use_clock(Some(&ntp_clock));
                self.ntp_clock = Some(ntp_clock);
            }
        }

        // Direct media timing (opt-in per flow). When enabled, the pipeline's
        // base_time is forced to zero and start_time disabled so buffer PTS
        // corresponds directly to the selected pipeline clock — required for
        // AES67 (`mediaclk:direct=0`) whether the reference is PTP or a
        // `phc2sys`-disciplined TAI system clock. Not appropriate for pipelines
        // that include elements assuming `running_time` starts near zero
        // (MPEG-TS demuxers, WHEP session sinks, etc.).
        if self.properties.effective_direct_media_timing() {
            if !self.properties.clock_type.supports_wall_clock_pts() {
                warn!(
                    "Pipeline '{}': direct_media_timing=true but clock_type={:?} does not expose a wall-clock; direct timing is unlikely to be useful here",
                    self.flow_name, self.properties.clock_type
                );
            }
            self.pipeline.set_base_time(gst::ClockTime::ZERO);
            self.pipeline.set_start_time(gst::ClockTime::NONE);
            info!(
                "Pipeline '{}' configured for direct media timing: base_time=0, start_time=None",
                self.flow_name
            );
        }

        Ok(())
    }
}
