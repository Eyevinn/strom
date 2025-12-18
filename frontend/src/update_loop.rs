//! Main update loop for the Strom application.
//!
//! Contains the message processing and UI rendering logic extracted from the eframe update function.

use egui::{CentralPanel, Context, SidePanel};

use crate::app::{AppPage, StromApp};
use crate::compositor_editor::CompositorEditor;
use crate::logging::{LogEntry, LogLevel};
use crate::mediaplayer::PlaylistEditor;
use crate::state::AppMessage;
use crate::utils::{get_local_storage, remove_local_storage, set_local_storage, spawn_task};

impl StromApp {
    /// Main update loop - processes messages and renders UI.
    /// Called from eframe::App::update after shutdown check.
    pub(crate) fn run_update_loop(&mut self, ctx: &Context) {
        // Process all pending channel messages
        while let Ok(msg) = self.channels.rx.try_recv() {
            self.process_message(msg, ctx);
        }

        // Process pending gst-launch export
        if let Some((elements, links, flow_name)) = self.pending_gst_launch_export.take() {
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            spawn_task(async move {
                match api.export_gst_launch(&elements, &links).await {
                    Ok(pipeline) => {
                        let _ = tx.send(AppMessage::GstLaunchExported {
                            pipeline,
                            flow_name,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(AppMessage::GstLaunchExportError(e.to_string()));
                    }
                }
                ctx.request_repaint();
            });
        }

        // Check authentication - if required and not authenticated, don't render
        // The HTML login form (in index.html) handles authentication
        // WASM should just stay quiet until authentication is complete
        if let Some(ref status) = self.auth_status {
            if status.auth_required && !status.authenticated {
                // Don't render anything - HTML login form is handling auth
                return;
            }
        }

        // Check if we're disconnected - if so, show blocking overlay and don't render normal UI
        if !self.connection_state.is_connected() {
            self.render_disconnect_overlay(ctx);
            return;
        }

        // Load elements on first frame
        if !self.elements_loaded {
            self.load_elements(ctx);
            self.elements_loaded = true;
        }

        // Load blocks on first frame
        if !self.blocks_loaded {
            self.load_blocks(ctx);
            self.blocks_loaded = true;
        }

        // Load flows on first frame or when refresh is needed
        if self.needs_refresh {
            self.load_flows(ctx);
            self.needs_refresh = false;
        }

        // Poll WebRTC stats every second for running flows
        {
            let poll_interval = std::time::Duration::from_secs(1);
            if self.last_webrtc_poll.elapsed() >= poll_interval {
                self.poll_webrtc_stats(ctx);
                self.last_webrtc_poll = instant::Instant::now();
            }
        }

        // Periodically fetch latency for running flows (every 2 seconds)
        if self.last_latency_fetch.elapsed() > std::time::Duration::from_secs(2) {
            self.last_latency_fetch = instant::Instant::now();
            self.fetch_latency_for_running_flows(ctx);
        }

        // Periodically fetch stats for running flows (every 2 seconds)
        if self.last_stats_fetch.elapsed() > std::time::Duration::from_secs(2) {
            self.last_stats_fetch = instant::Instant::now();
            self.fetch_stats_for_running_flows(ctx);
        }

        // Handle keyboard shortcuts
        self.handle_keyboard_shortcuts(ctx);

        // Handle compositor editor
        self.handle_compositor_editor(ctx);

        // Handle playlist editor
        self.handle_playlist_editor(ctx);

        // Handle player action signals
        self.handle_player_actions(ctx);

        // Render UI
        self.render_toolbar(ctx);
        self.render_page_content(ctx);
        self.render_status_bar(ctx);
        self.render_system_monitor_window(ctx);

        // Process pending flow copy (after render to avoid borrow checker issues)
        if let Some(flow) = self.flow_pending_copy.take() {
            self.copy_flow(&flow, ctx);
        }
    }

    /// Process a single message from the channel.
    fn process_message(&mut self, msg: AppMessage, ctx: &Context) {
        match msg {
            AppMessage::FlowsLoaded(flows) => {
                tracing::info!("Received FlowsLoaded: {} flows", flows.len());

                // Remember the previously selected flow ID (using ID, not index!)
                let previously_selected_id = self.selected_flow_id;

                self.flows = flows;
                self.status = format!("Loaded {} flows", self.flows.len());
                self.loading = false;

                // Check if there's a pending flow navigation (takes priority)
                if let Some(pending_flow_id) = self.pending_flow_navigation.take() {
                    tracing::info!(
                        "Processing pending navigation to flow ID: {}",
                        pending_flow_id
                    );
                    if let Some(flow) = self.flows.iter().find(|f| f.id == pending_flow_id) {
                        self.selected_flow_id = Some(pending_flow_id);
                        // Clear graph selection and load the new flow
                        self.graph.deselect_all();
                        self.graph.load(flow.elements.clone(), flow.links.clone());
                        self.graph.load_blocks(flow.blocks.clone());
                        tracing::info!("Navigated to flow: {}", flow.name);
                    } else {
                        tracing::warn!(
                            "Pending flow ID {} not found in refreshed flow list",
                            pending_flow_id
                        );
                    }
                } else if let Some(prev_id) = previously_selected_id {
                    // No pending navigation - check if previously selected flow still exists
                    if !self.flows.iter().any(|f| f.id == prev_id) {
                        // Flow was deleted - clear selection and graph
                        tracing::info!(
                            "Previously selected flow {} was deleted, clearing selection",
                            prev_id
                        );
                        self.clear_flow_selection();
                    }
                    // If flow still exists, selection is automatically valid (ID-based!)
                }
            }
            AppMessage::FlowsError(error) => {
                tracing::error!("Received FlowsError: {}", error);
                self.error = Some(format!("Flows: {}", error));
                self.loading = false;
                self.status = "Error loading flows".to_string();
            }
            AppMessage::ElementsLoaded(elements) => {
                let count = elements.len();
                tracing::info!("Received ElementsLoaded: {} elements", count);
                self.palette.load_elements(elements.clone());
                self.graph.set_all_element_info(elements);
                self.status = format!("Loaded {} elements", count);
            }
            AppMessage::ElementsError(error) => {
                tracing::error!("Received ElementsError: {}", error);
                self.error = Some(format!("Elements: {}", error));
            }
            AppMessage::BlocksLoaded(blocks) => {
                let count = blocks.len();
                tracing::info!("Received BlocksLoaded: {} blocks", count);
                self.palette.load_blocks(blocks.clone());
                self.graph.set_all_block_definitions(blocks);
                self.status = format!("Loaded {} blocks", count);
            }
            AppMessage::BlocksError(error) => {
                tracing::error!("Received BlocksError: {}", error);
                self.error = Some(format!("Blocks: {}", error));
            }
            AppMessage::ElementPropertiesLoaded(info) => {
                tracing::info!(
                    "Received ElementPropertiesLoaded: {} ({} properties)",
                    info.name,
                    info.properties.len()
                );
                self.palette.cache_element_properties(info);
            }
            AppMessage::ElementPropertiesError(error) => {
                tracing::error!("Received ElementPropertiesError: {}", error);
                self.error = Some(format!("Element properties: {}", error));
            }
            AppMessage::ElementPadPropertiesLoaded(info) => {
                tracing::info!(
                    "Received ElementPadPropertiesLoaded: {} (sink: {} pads, src: {} pads)",
                    info.name,
                    info.sink_pads.len(),
                    info.src_pads.len()
                );
                // Update graph's element info map so pads render correctly
                self.graph.set_element_info(info.name.clone(), info.clone());
                self.palette.cache_element_pad_properties(info);
            }
            AppMessage::ElementPadPropertiesError(error) => {
                tracing::error!("Received ElementPadPropertiesError: {}", error);
                self.error = Some(format!("Pad properties: {}", error));
            }
            AppMessage::Event(event) => {
                self.process_strom_event(event, ctx);
            }
            AppMessage::ConnectionStateChanged(state) => {
                tracing::info!("Connection state changed: {:?}", state);

                // If we're transitioning to Connected state, invalidate all cached data
                let was_disconnected = !self.connection_state.is_connected();
                let now_connected = state.is_connected();

                if was_disconnected && now_connected {
                    tracing::info!("Reconnected to backend - invalidating all cached state");
                    // Trigger reload of all data from backend
                    self.needs_refresh = true;
                    self.elements_loaded = false;
                    self.blocks_loaded = false;
                }

                self.connection_state = state;
            }
            AppMessage::FlowFetched(flow) => {
                self.process_flow_fetched(flow);
            }
            AppMessage::RefreshNeeded => {
                tracing::info!("Refresh requested due to flow fetch failure");
                self.needs_refresh = true;
            }
            AppMessage::VersionLoaded(version_info) => {
                tracing::info!(
                    "Version info loaded: v{} ({})",
                    version_info.version,
                    version_info.git_hash
                );
                self.version_info = Some(version_info);
            }
            AppMessage::AuthStatusLoaded(status) => {
                tracing::info!(
                    "Auth status loaded: required={}, authenticated={}",
                    status.auth_required,
                    status.authenticated
                );
                self.auth_status = Some(status.clone());
                self.checking_auth = false;

                // If authenticated or auth not required, set up connections
                if !status.auth_required || status.authenticated {
                    self.setup_websocket_connection(ctx.clone());
                    self.load_version(ctx.clone());
                }
            }
            AppMessage::LoginResult(response) => {
                tracing::info!("Login result: success={}", response.success);
                self.login_screen.set_logging_in(false);

                if response.success {
                    // Clear login form
                    self.login_screen.username.clear();
                    self.login_screen.password.clear();
                    self.login_screen.clear_error();

                    // Recheck auth status to update UI
                    self.check_auth_status(ctx.clone());
                } else {
                    self.login_screen.set_error(response.message);
                }
            }
            AppMessage::LogoutComplete => {
                tracing::info!("Logout complete, reloading page to show login form");

                // Reload the page so the HTML login form can re-initialize
                // The session cookie has been cleared by the logout API call
                #[cfg(target_arch = "wasm32")]
                {
                    if let Some(window) = web_sys::window() {
                        if let Err(e) = window.location().reload() {
                            tracing::error!("Failed to reload page: {:?}", e);
                        }
                    }
                }

                // For native mode, just reset state and recheck auth
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.flows.clear();
                    self.ws_client = None;
                    self.connection_state = crate::state::ConnectionState::Disconnected;
                    self.check_auth_status(ctx.clone());
                }
            }
            AppMessage::WebRtcStatsLoaded { flow_id, stats } => {
                tracing::debug!(
                    "WebRTC stats loaded for flow {}: {} connections",
                    flow_id,
                    stats.connections.len()
                );
                self.webrtc_stats.update(flow_id, stats);
            }
            AppMessage::FlowOperationSuccess(message) => {
                tracing::info!("Flow operation succeeded: {}", message);
                self.status = message;
                self.error = None;
            }
            AppMessage::FlowOperationError(message) => {
                tracing::error!("Flow operation failed: {}", message);
                self.status = "Ready".to_string();
                self.error = Some(message.clone());
                // Add to log entries
                let flow_id = self.current_flow().map(|f| f.id);
                self.add_log_entry(LogEntry::new(LogLevel::Error, message, None, flow_id));
                // Auto-show log panel on errors
                self.show_log_panel = true;
            }
            AppMessage::FlowCreated(flow_id) => {
                tracing::info!(
                    "Flow created, will navigate to flow ID after next refresh: {}",
                    flow_id
                );
                // Store the flow ID to navigate to after the next refresh
                self.pending_flow_navigation = Some(flow_id);
            }
            AppMessage::LatencyLoaded { flow_id, latency } => {
                tracing::debug!(
                    "Latency loaded for flow {}: {}",
                    flow_id,
                    latency.min_latency_formatted
                );
                self.latency_cache.insert(flow_id, latency);
            }
            AppMessage::LatencyNotAvailable(flow_id) => {
                tracing::debug!("Latency not available for flow {}", flow_id);
                self.latency_cache.remove(&flow_id);
            }
            AppMessage::WebRtcStatsError(error) => {
                tracing::trace!("WebRTC stats error: {}", error);
            }
            AppMessage::StatsLoaded { flow_id, stats } => {
                tracing::debug!(
                    "Stats loaded for flow {}: {} blocks",
                    flow_id,
                    stats.blocks.len()
                );
                self.stats_cache.insert(flow_id, stats);
            }
            AppMessage::StatsNotAvailable(flow_id) => {
                tracing::debug!("Stats not available for flow {}", flow_id);
                self.stats_cache.remove(&flow_id);
            }
            AppMessage::DynamicPadsLoaded { flow_id, pads } => {
                tracing::debug!(
                    "Dynamic pads loaded for flow {}: {} elements",
                    flow_id,
                    pads.len()
                );
                // Update graph editor if this is the currently selected flow
                if let Some(current_flow) = self.current_flow() {
                    if current_flow.id.to_string() == flow_id {
                        self.graph.set_runtime_dynamic_pads(pads);
                    }
                }
            }
            AppMessage::GstLaunchExported {
                pipeline,
                flow_name,
            } => {
                ctx.copy_text(pipeline);
                self.status = format!("Flow '{}' exported to clipboard as gst-launch", flow_name);
            }
            AppMessage::GstLaunchExportError(e) => {
                self.error = Some(format!("Failed to export as gst-launch: {}", e));
            }
            AppMessage::NetworkInterfacesLoaded(interfaces) => {
                tracing::info!("Network interfaces loaded: {} interfaces", interfaces.len());
                self.network_interfaces = interfaces;
            }
            AppMessage::AvailableChannelsLoaded(mut channels) => {
                // Sort by flow name, then by description/name
                channels.sort_by(|a, b| {
                    let flow_cmp = a.flow_name.cmp(&b.flow_name);
                    if flow_cmp != std::cmp::Ordering::Equal {
                        return flow_cmp;
                    }
                    // Then by description or block name
                    let a_label = a.description.as_ref().unwrap_or(&a.name);
                    let b_label = b.description.as_ref().unwrap_or(&b.name);
                    a_label.cmp(b_label)
                });
                tracing::info!("Available channels loaded: {} channels", channels.len());
                self.available_channels = channels;
            }
            AppMessage::DiscoveredStreamsLoaded(streams) => {
                tracing::debug!("Discovered streams loaded: {} streams", streams.len());
                self.discovery_page.set_discovered_streams(streams);
            }
            AppMessage::AnnouncedStreamsLoaded(streams) => {
                tracing::debug!("Announced streams loaded: {} streams", streams.len());
                self.discovery_page.set_announced_streams(streams);
            }
            AppMessage::StreamSdpLoaded { stream_id, sdp } => {
                tracing::info!("Stream SDP loaded for: {}", stream_id);
                self.discovery_page.set_stream_sdp(stream_id, sdp);
            }
            AppMessage::StreamPickerSdpLoaded { block_id, sdp } => {
                tracing::info!(
                    "Stream picker SDP loaded for block: {}, SDP length: {}",
                    block_id,
                    sdp.len()
                );
                // Find the block and update its SDP property
                if let Some(block) = self.graph.get_block_by_id_mut(&block_id) {
                    block
                        .properties
                        .insert("SDP".to_string(), strom_types::PropertyValue::String(sdp));
                    self.status = "SDP applied to block".to_string();
                    tracing::info!("SDP property updated for block {}", block_id);
                } else {
                    tracing::warn!("Block {} not found in graph when applying SDP", block_id);
                    self.error = Some(format!("Block not found: {}", block_id));
                }
            }
            AppMessage::MediaListLoaded(response) => {
                tracing::debug!(
                    "Media list loaded: {} entries in {}",
                    response.entries.len(),
                    response.current_path
                );
                self.media_page.set_entries(response);
            }
            AppMessage::MediaSuccess(message) => {
                tracing::info!("Media operation success: {}", message);
                self.status = message;
            }
            AppMessage::MediaError(message) => {
                tracing::error!("Media operation error: {}", message);
                self.error = Some(message);
            }
            AppMessage::MediaRefresh => {
                tracing::debug!("Media refresh requested");
                self.media_page
                    .refresh(&self.api, ctx, &self.channels.sender());
            }
            // SDP messages are handled elsewhere
            AppMessage::SdpLoaded { .. } | AppMessage::SdpError(_) => {}
        }
    }

    /// Process a Strom event from the WebSocket.
    fn process_strom_event(&mut self, event: strom_types::StromEvent, ctx: &Context) {
        use strom_types::StromEvent;

        tracing::trace!("Received WebSocket event: {}", event.description());

        match event {
            StromEvent::FlowCreated { .. } => {
                tracing::info!("Flow created, triggering full refresh");
                self.needs_refresh = true;
            }
            StromEvent::FlowDeleted { flow_id } => {
                tracing::info!("Flow deleted, triggering full refresh");
                // Clear QoS stats and start time for deleted flow
                self.qos_stats.clear_flow(&flow_id);
                self.flow_start_times.remove(&flow_id);
                self.needs_refresh = true;
            }
            StromEvent::FlowStopped { flow_id } => {
                tracing::info!("Flow {} stopped, clearing QoS stats", flow_id);
                // Clear QoS stats and start time when flow is stopped
                self.qos_stats.clear_flow(&flow_id);
                // Refresh available channels (channels may have been removed)
                self.refresh_available_channels();
                self.flow_start_times.remove(&flow_id);

                // Fetch updated flow state
                let api = self.api.clone();
                let tx = self.channels.sender();
                let ctx = ctx.clone();

                spawn_task(async move {
                    match api.get_flow(flow_id).await {
                        Ok(flow) => {
                            tracing::info!("Fetched updated flow: {}", flow.name);
                            let _ = tx.send(AppMessage::FlowFetched(flow));
                            ctx.request_repaint();
                        }
                        Err(e) => {
                            tracing::error!("Failed to fetch updated flow: {}", e);
                            let _ = tx.send(AppMessage::RefreshNeeded);
                            ctx.request_repaint();
                        }
                    }
                });
            }
            StromEvent::FlowStarted { flow_id } => {
                // Record when the flow started (for QoS grace period)
                self.flow_start_times
                    .insert(flow_id, instant::Instant::now());
                // Refresh available channels (new channels may be available)
                self.refresh_available_channels();

                // Fetch the updated flow state
                tracing::info!("Flow {} started, fetching updated flow", flow_id);
                let api = self.api.clone();
                let tx = self.channels.sender();
                let ctx = ctx.clone();

                spawn_task(async move {
                    match api.get_flow(flow_id).await {
                        Ok(flow) => {
                            tracing::info!("Fetched started flow: {}", flow.name);
                            let _ = tx.send(AppMessage::FlowFetched(flow));
                            ctx.request_repaint();
                        }
                        Err(e) => {
                            tracing::error!("Failed to fetch started flow: {}", e);
                            let _ = tx.send(AppMessage::RefreshNeeded);
                            ctx.request_repaint();
                        }
                    }
                });
            }
            StromEvent::FlowUpdated { flow_id } => {
                // For updates, fetch the specific flow to update it in-place
                tracing::info!("Flow {} updated, fetching updated flow", flow_id);
                // Refresh available channels (flow name may have changed)
                self.refresh_available_channels();
                let api = self.api.clone();
                let tx = self.channels.sender();
                let ctx = ctx.clone();

                spawn_task(async move {
                    match api.get_flow(flow_id).await {
                        Ok(flow) => {
                            tracing::info!("Fetched updated flow: {}", flow.name);
                            let _ = tx.send(AppMessage::FlowFetched(flow));
                            ctx.request_repaint();
                        }
                        Err(e) => {
                            tracing::error!("Failed to fetch updated flow: {}", e);
                            // Fall back to full refresh
                            let _ = tx.send(AppMessage::RefreshNeeded);
                            ctx.request_repaint();
                        }
                    }
                });
            }
            StromEvent::PipelineError {
                flow_id,
                error,
                source,
            } => {
                tracing::error!(
                    "Pipeline error in flow {}: {} (source: {:?})",
                    flow_id,
                    error,
                    source
                );
                // Add to log entries
                self.add_log_entry(LogEntry::new(
                    LogLevel::Error,
                    error.clone(),
                    source.clone(),
                    Some(flow_id),
                ));
                // Also set the legacy error field for status bar
                let error_msg = if let Some(ref src) = source {
                    format!("{}: {}", src, error)
                } else {
                    error
                };
                self.error = Some(error_msg);
                // Auto-show log panel on errors
                self.show_log_panel = true;
            }
            StromEvent::PipelineWarning {
                flow_id,
                warning,
                source,
            } => {
                tracing::warn!(
                    "Pipeline warning in flow {}: {} (source: {:?})",
                    flow_id,
                    warning,
                    source
                );
                self.add_log_entry(LogEntry::new(
                    LogLevel::Warning,
                    warning,
                    source,
                    Some(flow_id),
                ));
            }
            StromEvent::PipelineInfo {
                flow_id,
                message,
                source,
            } => {
                tracing::info!(
                    "Pipeline info in flow {}: {} (source: {:?})",
                    flow_id,
                    message,
                    source
                );
                self.add_log_entry(LogEntry::new(
                    LogLevel::Info,
                    message,
                    source,
                    Some(flow_id),
                ));
            }
            StromEvent::MeterData {
                flow_id,
                element_id,
                rms,
                peak,
                decay,
            } => {
                tracing::trace!(
                    "📊 METER DATA RECEIVED: flow={}, element={}, channels={}, rms={:?}, peak={:?}",
                    flow_id,
                    element_id,
                    rms.len(),
                    rms,
                    peak
                );
                // Store meter data for visualization
                self.meter_data.update(
                    flow_id,
                    element_id.clone(),
                    crate::meter::MeterData { rms, peak, decay },
                );
                tracing::trace!("📊 Meter data stored for element {}", element_id);
            }
            StromEvent::MediaPlayerPosition {
                flow_id,
                block_id,
                position_ns,
                duration_ns,
                current_file_index,
                total_files,
            } => {
                tracing::trace!(
                    "Media player position: flow={}, block={}, pos={}ns, dur={}ns",
                    flow_id,
                    block_id,
                    position_ns,
                    duration_ns
                );
                self.mediaplayer_data.update_position(
                    flow_id,
                    block_id,
                    position_ns,
                    duration_ns,
                    current_file_index,
                    total_files,
                );
            }
            StromEvent::MediaPlayerStateChanged {
                flow_id,
                block_id,
                state,
                current_file,
            } => {
                tracing::debug!(
                    "Media player state changed: flow={}, block={}, state={}",
                    flow_id,
                    block_id,
                    state
                );
                self.mediaplayer_data
                    .update_state(flow_id, block_id, state, current_file);
            }
            StromEvent::SystemStats(stats) => {
                self.system_monitor.update(stats);
            }
            StromEvent::PtpStats {
                flow_id,
                domain,
                synced,
                mean_path_delay_ns,
                clock_offset_ns,
                r_squared,
                clock_rate,
                grandmaster_id,
                master_id,
            } => {
                // Update PTP stats in the corresponding flow for real-time display
                if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
                    // Update clock_sync_status (used by the UI for status display)
                    flow.properties.clock_sync_status = Some(if synced {
                        strom_types::flow::ClockSyncStatus::Synced
                    } else {
                        strom_types::flow::ClockSyncStatus::NotSynced
                    });

                    // Ensure ptp_info exists
                    if flow.properties.ptp_info.is_none() {
                        flow.properties.ptp_info = Some(strom_types::flow::PtpInfo::default());
                    }
                    if let Some(ref mut ptp_info) = flow.properties.ptp_info {
                        ptp_info.domain = domain;
                        ptp_info.synced = synced;
                        // Update stats
                        let stats = strom_types::flow::PtpStats {
                            mean_path_delay_ns,
                            clock_offset_ns,
                            r_squared,
                            clock_rate,
                            last_update: None,
                        };
                        ptp_info.stats = Some(stats);
                    }
                }

                // Also update the PTP stats store for history tracking
                self.ptp_stats.update(
                    flow_id,
                    crate::ptp_monitor::PtpStatsData {
                        domain,
                        synced,
                        mean_path_delay_ns,
                        clock_offset_ns,
                        r_squared,
                        clock_rate,
                        grandmaster_id,
                        master_id,
                    },
                );
            }
            StromEvent::QoSStats {
                flow_id,
                block_id,
                element_id,
                element_name,
                internal_element_type,
                event_count,
                avg_proportion,
                min_proportion,
                max_proportion,
                avg_jitter,
                total_processed,
                is_falling_behind,
            } => {
                // Grace period: ignore QoS events in first 3 seconds after flow start
                // (transient issues during startup are common and not indicative of real problems)
                const QOS_GRACE_PERIOD_SECS: u64 = 3;
                let in_grace_period = self
                    .flow_start_times
                    .get(&flow_id)
                    .map(|start| {
                        start.elapsed() < std::time::Duration::from_secs(QOS_GRACE_PERIOD_SECS)
                    })
                    .unwrap_or(false);

                if in_grace_period {
                    // Skip QoS processing during grace period
                    return;
                }

                // Update QoS store
                self.qos_stats.update(
                    flow_id,
                    crate::qos_monitor::QoSElementData {
                        element_id: element_id.clone(),
                        block_id: block_id.clone(),
                        element_name: element_name.clone(),
                        internal_element_type: internal_element_type.clone(),
                        avg_proportion,
                        min_proportion,
                        max_proportion,
                        avg_jitter_ns: avg_jitter,
                        event_count,
                        total_processed,
                        is_falling_behind,
                        last_update: instant::Instant::now(),
                    },
                );

                // Log QoS issues (only when falling behind or recovering)
                if is_falling_behind {
                    let display_name = if let Some(ref internal) = internal_element_type {
                        format!("{} ({})", element_name, internal)
                    } else {
                        element_name.clone()
                    };
                    let message = format!(
                        "QoS: {} falling behind ({:.1}%, {} events)",
                        display_name,
                        avg_proportion * 100.0,
                        event_count
                    );
                    self.add_log_entry(LogEntry::new(
                        if avg_proportion < 0.8 {
                            LogLevel::Error
                        } else {
                            LogLevel::Warning
                        },
                        message,
                        Some(element_id.clone()),
                        Some(flow_id),
                    ));
                }
            }
            _ => {}
        }
    }

    /// Process a fetched flow update.
    fn process_flow_fetched(&mut self, flow: strom_types::Flow) {
        tracing::info!("Received updated flow: {} (id={})", flow.name, flow.id);

        // Check if this is the currently selected flow BEFORE updating
        let current_flow_id = self.current_flow().map(|f| f.id);
        let is_selected_flow = current_flow_id == Some(flow.id);

        tracing::info!(
            "Current selected flow: {:?}, Fetched flow: {}, Is selected: {}",
            current_flow_id,
            flow.id,
            is_selected_flow
        );

        // Log runtime_data for AES67 blocks
        for block in &flow.blocks {
            if block.block_definition_id == "builtin.aes67_output" {
                let has_sdp = block
                    .runtime_data
                    .as_ref()
                    .and_then(|data| data.get("sdp"))
                    .is_some();
                tracing::info!("AES67 block {} has SDP: {}", block.id, has_sdp);
            }
        }

        // Update the specific flow in-place
        if let Some(existing_flow) = self.flows.iter_mut().find(|f| f.id == flow.id) {
            *existing_flow = flow.clone();
            tracing::info!("Updated flow in self.flows");

            // If this is the currently selected flow, update the graph editor in-place
            if is_selected_flow {
                tracing::info!("This is the selected flow - updating graph editor");

                // Selectively update graph editor data without overwriting positions
                // This ensures property inspector sees latest runtime_data while preserving
                // local position changes that may have occurred after save

                // Update element properties (but preserve positions)
                for updated_elem in &flow.elements {
                    if let Some(local_elem) = self
                        .graph
                        .elements
                        .iter_mut()
                        .find(|e| e.id == updated_elem.id)
                    {
                        // Preserve local position
                        let saved_position = local_elem.position;
                        // Update properties from backend
                        local_elem.properties = updated_elem.properties.clone();
                        local_elem.pad_properties = updated_elem.pad_properties.clone();
                        // Restore local position
                        local_elem.position = saved_position;
                    }
                }

                // Update block runtime_data and properties (but preserve positions)
                for updated_block in &flow.blocks {
                    if let Some(local_block) = self
                        .graph
                        .blocks
                        .iter_mut()
                        .find(|b| b.id == updated_block.id)
                    {
                        // Preserve local position
                        let saved_position = local_block.position;
                        // Update runtime_data, properties, and computed_external_pads from backend
                        local_block.runtime_data = updated_block.runtime_data.clone();
                        local_block.properties = updated_block.properties.clone();
                        local_block.computed_external_pads =
                            updated_block.computed_external_pads.clone();
                        // Restore local position
                        local_block.position = saved_position;
                    }
                }

                // Update links (links don't have positions)
                self.graph.links = flow.links.clone();

                tracing::info!("Graph editor updated with {} blocks", flow.blocks.len());
            } else {
                tracing::info!("Not the selected flow - skipping graph editor update");
            }
        } else {
            tracing::warn!("Flow not found in list, adding it");
            self.flows.push(flow);
        }
    }

    /// Handle compositor editor open/update.
    fn handle_compositor_editor(&mut self, ctx: &Context) {
        // Check for compositor editor open signal
        if let Some(block_id) = get_local_storage("open_compositor_editor") {
            remove_local_storage("open_compositor_editor");

            // Get current flow
            if let Some(flow) = self.current_flow() {
                // Find the block
                if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Extract resolution from output_resolution property
                    // Default to 1920x1080 (Full HD) if not set or can't be parsed
                    let (output_width, output_height) = block
                        .properties
                        .get("output_resolution")
                        .and_then(|v| match v {
                            strom_types::PropertyValue::String(s) if !s.is_empty() => {
                                strom_types::parse_resolution_string(s)
                            }
                            _ => None,
                        })
                        .unwrap_or((1920, 1080));

                    let num_inputs = block
                        .properties
                        .get("num_inputs")
                        .and_then(|v| match v {
                            strom_types::PropertyValue::UInt(u) => Some(*u as usize),
                            strom_types::PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
                            _ => None,
                        })
                        .unwrap_or(2);

                    // Create editor
                    let mut editor = CompositorEditor::new(
                        flow.id,
                        block_id.clone(),
                        output_width,
                        output_height,
                        num_inputs,
                        self.api.clone(),
                    );

                    // Load current properties from backend
                    editor.load_properties(ctx);

                    self.compositor_editor = Some(editor);
                }
            }
        }

        // Show compositor editor if open (as a window, doesn't block main UI)
        if let Some(ref mut editor) = self.compositor_editor {
            let is_open = editor.show(ctx);
            if !is_open {
                self.compositor_editor = None;
            }
        }
    }

    /// Handle playlist editor open/update.
    fn handle_playlist_editor(&mut self, ctx: &Context) {
        // Check for playlist editor open signal
        if let Some(block_id) = get_local_storage("open_playlist_editor") {
            remove_local_storage("open_playlist_editor");

            // Get current flow
            if let Some(flow) = self.current_flow() {
                // Find the block
                if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Create playlist editor
                    let mut editor = PlaylistEditor::new(flow.id, block_id.clone());

                    // Load current playlist from block properties
                    if let Some(strom_types::PropertyValue::String(playlist_json)) =
                        block.properties.get("playlist")
                    {
                        if let Ok(playlist) = serde_json::from_str::<Vec<String>>(playlist_json) {
                            editor.set_playlist(playlist);
                        }
                    }

                    self.playlist_editor = Some(editor);
                }
            }
        }

        // Show playlist editor if open (as a window, doesn't block main UI)
        if let Some(ref mut editor) = self.playlist_editor {
            // Check if browser needs to load files
            if let Some(path) = editor.get_browser_path_to_load() {
                let api = self.api.clone();
                // Use local storage to pass results back
                #[cfg(target_arch = "wasm32")]
                {
                    wasm_bindgen_futures::spawn_local(async move {
                        match api.list_media(&path).await {
                            Ok(result) => {
                                // Serialize result to local storage
                                if let Ok(json) = serde_json::to_string(&result) {
                                    set_local_storage("media_browser_result", &json);
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to list media files: {}", e);
                                set_local_storage("media_browser_result", "error");
                            }
                        }
                    });
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let rt = tokio::runtime::Handle::try_current();
                    if let Ok(handle) = rt {
                        handle.spawn(async move {
                            match api.list_media(&path).await {
                                Ok(result) => {
                                    if let Ok(json) = serde_json::to_string(&result) {
                                        set_local_storage("media_browser_result", &json);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to list media files: {}", e);
                                    set_local_storage("media_browser_result", "error");
                                }
                            }
                        });
                    }
                }
            }

            // Check for media browser results
            if let Some(result_json) = get_local_storage("media_browser_result") {
                remove_local_storage("media_browser_result");
                if result_json != "error" {
                    if let Ok(result) =
                        serde_json::from_str::<strom_types::api::ListMediaResponse>(&result_json)
                    {
                        let entries: Vec<crate::mediaplayer::MediaEntry> = result
                            .entries
                            .into_iter()
                            .map(|e| crate::mediaplayer::MediaEntry {
                                name: e.name,
                                path: e.path,
                                is_dir: e.is_directory,
                                size: e.size,
                            })
                            .collect();
                        editor.set_browser_entries(
                            result.current_path,
                            result.parent_path,
                            entries,
                        );
                    }
                } else {
                    // Clear loading state on error
                    editor.browser_loading = false;
                }
            }

            // Update current playing index from player data
            if let Some(player_data) = self.mediaplayer_data.get(&editor.flow_id, &editor.block_id)
            {
                editor.current_playing_index = Some(player_data.current_file_index);
            }

            if let Some(playlist) = editor.show(ctx) {
                // User clicked Save - send playlist to API
                let flow_id = editor.flow_id;
                let block_id = editor.block_id.clone();
                let api = self.api.clone();

                #[cfg(target_arch = "wasm32")]
                {
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(e) = api.set_player_playlist(flow_id, &block_id, playlist).await
                        {
                            tracing::error!("Failed to set playlist: {}", e);
                        }
                    });
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let rt = tokio::runtime::Handle::try_current();
                    if let Ok(handle) = rt {
                        handle.spawn(async move {
                            if let Err(e) =
                                api.set_player_playlist(flow_id, &block_id, playlist).await
                            {
                                tracing::error!("Failed to set playlist: {}", e);
                            }
                        });
                    }
                }
            }

            if !editor.open {
                self.playlist_editor = None;
            }
        }
    }

    /// Handle player action signals from compact UI controls.
    fn handle_player_actions(&mut self, ctx: &Context) {
        let _ = ctx; // Used in spawn_task calls

        if let Some(action_data) = get_local_storage("player_action") {
            remove_local_storage("player_action");
            tracing::info!("Received player action: {}", action_data);

            // Parse action data: "block_id:action" or "block_id:action:position"
            let parts: Vec<&str> = action_data.split(':').collect();
            if parts.len() >= 2 {
                let block_id = parts[0].to_string();
                let action = parts[1];
                tracing::info!("Parsed action: block={}, action={}", block_id, action);

                if let Some(flow) = self.current_flow() {
                    let flow_id = flow.id;
                    let api = self.api.clone();
                    tracing::info!("Sending action to flow {}", flow_id);

                    match action {
                        "play" | "pause" | "next" | "previous" => {
                            let action_str = action.to_string();
                            #[cfg(target_arch = "wasm32")]
                            {
                                wasm_bindgen_futures::spawn_local(async move {
                                    if let Err(e) =
                                        api.control_player(flow_id, &block_id, &action_str).await
                                    {
                                        tracing::error!("Failed to control player: {}", e);
                                    }
                                });
                            }

                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                let rt = tokio::runtime::Handle::try_current();
                                if let Ok(handle) = rt {
                                    handle.spawn(async move {
                                        if let Err(e) = api
                                            .control_player(flow_id, &block_id, &action_str)
                                            .await
                                        {
                                            tracing::error!("Failed to control player: {}", e);
                                        }
                                    });
                                }
                            }
                        }
                        "seek" if parts.len() >= 3 => {
                            if let Ok(position_ns) = parts[2].parse::<u64>() {
                                #[cfg(target_arch = "wasm32")]
                                {
                                    wasm_bindgen_futures::spawn_local(async move {
                                        if let Err(e) =
                                            api.seek_player(flow_id, &block_id, position_ns).await
                                        {
                                            tracing::error!("Failed to seek player: {}", e);
                                        }
                                    });
                                }

                                #[cfg(not(target_arch = "wasm32"))]
                                {
                                    let rt = tokio::runtime::Handle::try_current();
                                    if let Ok(handle) = rt {
                                        handle.spawn(async move {
                                            if let Err(e) = api
                                                .seek_player(flow_id, &block_id, position_ns)
                                                .await
                                            {
                                                tracing::error!("Failed to seek player: {}", e);
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        "playlist" => {
                            // Open playlist editor for this block
                            let mut editor = PlaylistEditor::new(flow_id, block_id.clone());

                            // Load current playlist from block properties
                            if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                                if let Some(strom_types::PropertyValue::String(playlist_json)) =
                                    block.properties.get("playlist")
                                {
                                    if let Ok(playlist) =
                                        serde_json::from_str::<Vec<String>>(playlist_json)
                                    {
                                        editor.set_playlist(playlist);
                                    }
                                }
                            }

                            self.playlist_editor = Some(editor);
                        }
                        _ => {
                            tracing::warn!("Unknown player action: {}", action);
                        }
                    }
                }
            }
        }
    }

    /// Render page-specific content.
    fn render_page_content(&mut self, ctx: &Context) {
        match self.current_page {
            AppPage::Flows => {
                self.render_flow_list(ctx);

                // Always show palette, even if no flow selected
                if self.current_flow().is_some() {
                    self.render_palette(ctx);
                } else {
                    // Show simplified palette when no flow is selected
                    SidePanel::right("palette")
                        .default_width(250.0)
                        .resizable(true)
                        .show(ctx, |ui| {
                            ui.heading("Elements");
                            ui.separator();
                            ui.label("Select or create a flow to see the element palette");
                        });
                }

                self.render_canvas(ctx);
                self.render_log_panel(ctx);
                self.render_new_flow_dialog(ctx);
                self.render_delete_confirmation_dialog(ctx);
                self.render_flow_properties_dialog(ctx);
                self.render_import_dialog(ctx);
                self.render_stream_picker_modal(ctx);
            }
            AppPage::Discovery => {
                CentralPanel::default().show(ctx, |ui| {
                    self.discovery_page
                        .render(ui, &self.api, ctx, &self.channels.tx);
                });

                // Handle pending create flow from discovery
                if let Some(sdp) = self.discovery_page.take_pending_create_flow_sdp() {
                    self.create_flow_from_sdp(sdp, ctx);
                }
            }
            AppPage::Clocks => {
                CentralPanel::default().show(ctx, |ui| {
                    self.clocks_page.render(ui, &self.ptp_stats, &self.flows);
                });
            }
            AppPage::Media => {
                CentralPanel::default().show(ctx, |ui| {
                    self.media_page
                        .render(ui, &self.api, ctx, &self.channels.sender());
                });
            }
            AppPage::Info => {
                // Auto-load network interfaces when Info page is shown
                if self.info_page.should_load_network() {
                    self.network_interfaces_loaded = false;
                    self.load_network_interfaces(ctx.clone());
                }

                CentralPanel::default().show(ctx, |ui| {
                    self.info_page.render(
                        ui,
                        self.version_info.as_ref(),
                        &self.system_monitor,
                        &self.network_interfaces,
                        &self.flows,
                    );
                });
            }
        }
    }
}
