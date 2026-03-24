use crate::audiorouter::RoutingMatrixEditor;
use crate::mediaplayer::PlaylistEditor;
use crate::state::AppMessage;
#[cfg(not(target_arch = "wasm32"))]
use crate::state::ConnectionState;
use egui::{CentralPanel, Context};

use super::APP_SETTINGS_KEY;
use super::*;
impl eframe::App for StromApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Check shutdown flag (Ctrl+C handler for native mode)
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ref flag) = self.shutdown_flag {
            use std::sync::atomic::Ordering;
            if flag.load(Ordering::SeqCst) {
                tracing::info!("Shutdown flag set, closing GUI...");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        // Apply theme and zoom in first update frame (iOS workaround - settings during construction may not persist)
        // Apply settings in first update frame (iOS workaround - settings during construction may not persist)
        if self.needs_initial_settings_apply {
            self.needs_initial_settings_apply = false;
            self.apply_theme(ctx.clone());
            if let Some(zoom) = self.settings.zoom {
                ctx.set_pixels_per_point(zoom);
            }
        }

        // Handle pending flow selection (deferred from previous frame to avoid accesskit panic)
        // This MUST happen before any UI is drawn to prevent "Focused ID not in node list" errors
        if let Some(flow_id) = self.pending_flow_selection.take() {
            // Clear any existing focus before changing graph structure
            ctx.memory_mut(|mem| {
                if let Some(focused_id) = mem.focused() {
                    mem.surrender_focus(focused_id);
                }
            });
            // Now safely load the flow
            if let Some(flow) = self.flows.iter().find(|f| f.id == flow_id).cloned() {
                self.selected_flow_id = Some(flow_id);
                self.graph.deselect_all();
                self.graph.clear_runtime_dynamic_pads();
                self.graph.load(flow.elements.clone(), flow.links.clone());
                self.graph.load_blocks(flow.blocks.clone());
                tracing::info!("Loaded deferred flow selection: {}", flow.name);
            }
        }

        // Handle pinch-to-zoom from JavaScript (iOS/mobile)
        // Only apply browser zoom if NOT over the graph editor (graph has its own zoom)
        #[cfg(target_arch = "wasm32")]
        {
            use wasm_bindgen::JsValue;

            if let Some(window) = web_sys::window() {
                if let Ok(pinch_zoom) = js_sys::Reflect::get(&window, &"stromPinchZoom".into()) {
                    if !pinch_zoom.is_undefined() {
                        if let Some(obj) = js_sys::Object::try_from(&pinch_zoom) {
                            let changed = js_sys::Reflect::get(obj, &"changed".into())
                                .ok()
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);

                            // Check if new pinch started - determine if over graph
                            let new_pinch = js_sys::Reflect::get(obj, &"newPinch".into())
                                .ok()
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);

                            if new_pinch {
                                let start_x = js_sys::Reflect::get(obj, &"startX".into())
                                    .ok()
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0)
                                    as f32;
                                let start_y = js_sys::Reflect::get(obj, &"startY".into())
                                    .ok()
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0)
                                    as f32;

                                // JS coordinates are in CSS pixels, egui rect is in screen coords
                                // Don't divide by ppp - egui rects are already in screen space
                                let pinch_pos = egui::pos2(start_x, start_y);

                                let over_graph = self
                                    .graph
                                    .canvas_rect()
                                    .map(|rect: egui::Rect| rect.contains(pinch_pos))
                                    .unwrap_or(false);

                                let _ = js_sys::Reflect::set(
                                    obj,
                                    &"isGraphPinch".into(),
                                    &JsValue::from_bool(over_graph),
                                );
                                let _ =
                                    js_sys::Reflect::set(obj, &"newPinch".into(), &JsValue::FALSE);
                            }

                            if changed {
                                let is_graph_pinch =
                                    js_sys::Reflect::get(obj, &"isGraphPinch".into())
                                        .ok()
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);

                                if !is_graph_pinch {
                                    if let Some(scale) = js_sys::Reflect::get(obj, &"scale".into())
                                        .ok()
                                        .and_then(|v| v.as_f64())
                                    {
                                        let scale_f32 = scale as f32;
                                        if scale_f32.is_finite()
                                            && scale_f32 > 0.1
                                            && scale_f32 < 10.0
                                        {
                                            ctx.set_pixels_per_point(scale_f32);
                                            self.settings.zoom = Some(scale_f32);
                                        }
                                    }
                                }
                                // Reset the changed flag
                                let _ =
                                    js_sys::Reflect::set(obj, &"changed".into(), &JsValue::FALSE);
                            }
                        }
                    }
                }
            }
        }

        // Process all pending channel messages
        while let Ok(msg) = self.channels.rx.try_recv() {
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
                            // Clear any existing focus before changing graph structure
                            // to prevent accesskit panic when focused node is removed
                            ctx.memory_mut(|mem| {
                                if let Some(focused_id) = mem.focused() {
                                    mem.surrender_focus(focused_id);
                                }
                            });
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
                AppMessage::ElementPropertiesError(element_type, error) => {
                    tracing::error!(
                        "Received ElementPropertiesError for '{}': {}",
                        element_type,
                        error
                    );
                    self.palette.mark_element_lookup_failed(element_type);
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
                AppMessage::ElementPadPropertiesError(element_type, error) => {
                    tracing::error!(
                        "Received ElementPadPropertiesError for '{}': {}",
                        element_type,
                        error
                    );
                    self.palette.mark_pad_properties_lookup_failed(element_type);
                    self.error = Some(format!("Pad properties: {}", error));
                }
                AppMessage::Event(event) => {
                    tracing::trace!("Received WebSocket event: {}", event.description());
                    // Handle flow state changes
                    use strom_types::StromEvent;
                    match event {
                        StromEvent::FlowCreated { .. } => {
                            tracing::info!("Flow created, triggering full refresh");
                            self.needs_refresh = true;
                        }
                        StromEvent::FlowDeleted { flow_id } => {
                            tracing::info!("Flow deleted, triggering full refresh");
                            // Clear QoS stats, buffer age, WebRTC stats and start time for deleted flow
                            self.qos_stats.clear_flow(&flow_id);
                            self.buffer_age_data.clear_flow(&flow_id);
                            self.webrtc_stats.clear_flow(&flow_id);
                            self.flow_start_times.remove(&flow_id);
                            self.needs_refresh = true;
                        }
                        StromEvent::FlowStopped { flow_id } => {
                            tracing::info!("Flow {} stopped, clearing QoS stats", flow_id);
                            // Clear QoS stats, buffer age, WebRTC stats, recorder filenames and start time when flow is stopped
                            self.qos_stats.clear_flow(&flow_id);
                            self.buffer_age_data.clear_flow(&flow_id);
                            self.webrtc_stats.clear_flow(&flow_id);
                            self.recorder_filenames
                                .retain(|(fid, _), _| *fid != flow_id);
                            self.recorder_start_times
                                .retain(|(fid, _), _| *fid != flow_id);
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
                                        let _ = tx.send(AppMessage::FlowFetched(Box::new(flow)));
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
                                        let _ = tx.send(AppMessage::FlowFetched(Box::new(flow)));
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
                        StromEvent::FlowStateChanged { flow_id, state } => {
                            tracing::info!(
                                "Flow {} state changed to {}, fetching updated flow",
                                flow_id,
                                state
                            );
                            let api = self.api.clone();
                            let tx = self.channels.sender();
                            let ctx = ctx.clone();

                            spawn_task(async move {
                                match api.get_flow(flow_id).await {
                                    Ok(flow) => {
                                        let _ = tx.send(AppMessage::FlowFetched(Box::new(flow)));
                                        ctx.request_repaint();
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to fetch flow after state change: {}",
                                            e
                                        );
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
                                        let _ = tx.send(AppMessage::FlowFetched(Box::new(flow)));
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
                                "METER DATA RECEIVED: flow={}, element={}, channels={}, rms={:?}, peak={:?}",
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
                            tracing::trace!("Meter data stored for element {}", element_id);
                        }
                        StromEvent::SpectrumData {
                            flow_id,
                            element_id,
                            magnitudes,
                        } => {
                            tracing::trace!(
                                "Spectrum data received: flow={}, element={}, channels={}, bands={}",
                                flow_id,
                                element_id,
                                magnitudes.len(),
                                magnitudes.first().map_or(0, |ch| ch.len())
                            );
                            self.spectrum_data.update(
                                flow_id,
                                element_id.clone(),
                                crate::spectrum::SpectrumData { magnitudes },
                            );
                            tracing::trace!("Spectrum data stored for element {}", element_id);
                        }
                        StromEvent::AudioAnalyzerData {
                            flow_id,
                            element_id,
                            waveform_l_min,
                            waveform_l_max,
                            waveform_r_min,
                            waveform_r_max,
                            vectorscope_l,
                            vectorscope_r,
                        } => {
                            tracing::trace!(
                                "AudioAnalyzer data received: flow={}, element={}, columns={}, pairs={}",
                                flow_id,
                                element_id,
                                waveform_l_min.len(),
                                vectorscope_l.len()
                            );
                            self.audioanalyzer_data.update(
                                flow_id,
                                element_id,
                                crate::audioanalyzer::AudioAnalyzerData::from_base64(
                                    &waveform_l_min,
                                    &waveform_l_max,
                                    &waveform_r_min,
                                    &waveform_r_max,
                                    &vectorscope_l,
                                    &vectorscope_r,
                                ),
                            );
                        }
                        StromEvent::LoudnessData {
                            flow_id,
                            element_id,
                            momentary,
                            shortterm,
                            integrated,
                            loudness_range,
                            true_peak,
                        } => {
                            tracing::trace!(
                                "Loudness data received: flow={}, element={}, M={:.1}, S={:?}, I={:?}",
                                flow_id,
                                element_id,
                                momentary,
                                shortterm,
                                integrated
                            );
                            self.loudness_data.update(
                                flow_id,
                                element_id,
                                crate::loudness::LoudnessData {
                                    momentary,
                                    shortterm,
                                    integrated,
                                    loudness_range,
                                    true_peak,
                                },
                            );
                        }
                        StromEvent::LatencyData {
                            flow_id,
                            element_id,
                            last_latency_us,
                            average_latency_us,
                        } => {
                            tracing::trace!(
                                "Latency data received: flow={}, element={}, last={}us, avg={}us",
                                flow_id,
                                element_id,
                                last_latency_us,
                                average_latency_us
                            );
                            // Store latency data for visualization
                            self.latency_data.update(
                                flow_id,
                                element_id.clone(),
                                crate::latency::LatencyData {
                                    last_latency_us,
                                    average_latency_us,
                                },
                            );
                            tracing::trace!("Latency data stored for element {}", element_id);
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
                            self.mediaplayer_data.update_state(
                                flow_id,
                                block_id,
                                state,
                                current_file,
                            );
                        }
                        StromEvent::SystemStats(stats) => {
                            self.system_monitor.update(stats);
                        }
                        StromEvent::ThreadStats(stats) => {
                            self.thread_monitor.update(stats);
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
                                    flow.properties.ptp_info =
                                        Some(strom_types::flow::PtpInfo::default());
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
                                    start.elapsed()
                                        < std::time::Duration::from_secs(QOS_GRACE_PERIOD_SECS)
                                })
                                .unwrap_or(false);

                            if in_grace_period {
                                // Skip QoS processing during grace period
                                continue;
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
                                let display_name = if let Some(ref internal) = internal_element_type
                                {
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
                        StromEvent::RecorderFileChanged {
                            flow_id,
                            block_id,
                            filename,
                        } => {
                            let key = (flow_id, block_id);
                            self.recorder_start_times
                                .entry(key.clone())
                                .or_insert_with(instant::Instant::now);
                            self.recorder_filenames.insert(key, filename);
                        }
                        StromEvent::RecorderAutoStop { flow_id, block_id } => {
                            tracing::info!(
                                "Recorder {} requested auto-stop for flow {}",
                                block_id,
                                flow_id
                            );
                            self.stop_flow_by_id(flow_id, ctx);
                        }
                        StromEvent::BufferAgeWarning {
                            flow_id,
                            element_id,
                            pad_name,
                            age_ms,
                            threshold_ms,
                        } => {
                            self.buffer_age_data.update_warning(
                                flow_id,
                                element_id.clone(),
                                age_ms,
                                threshold_ms,
                            );
                            self.log_entries.push(LogEntry::new(
                                LogLevel::Warning,
                                format!(
                                    "Buffer age {}ms on {}:{} (threshold {}ms)",
                                    age_ms, element_id, pad_name, threshold_ms
                                ),
                                Some(element_id),
                                Some(flow_id),
                            ));
                        }
                        StromEvent::BufferAgeProbe {
                            probe_id,
                            element_id,
                            pad_name,
                            age_ms,
                            ..
                        } => {
                            self.buffer_age_data
                                .update_probe(probe_id, element_id, pad_name, age_ms);
                        }
                        StromEvent::BufferAgeProbeActivated {
                            probe_id,
                            element_id,
                            pad_name,
                            ..
                        } => {
                            self.buffer_age_data
                                .probe_activated(probe_id, element_id, pad_name);
                        }
                        StromEvent::BufferAgeProbeDeactivated { probe_id, .. } => {
                            self.buffer_age_data.probe_deactivated(&probe_id);
                        }
                        _ => {}
                    }
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

                        // Check if backend has been rebuilt - this will trigger a reload if build_id changed
                        self.load_version(ctx.clone());
                    }

                    self.connection_state = state;
                }
                AppMessage::FlowFetched(flow) => {
                    let flow = *flow; // Unbox
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

                            tracing::info!(
                                "Graph editor updated with {} blocks",
                                flow.blocks.len()
                            );
                        } else {
                            tracing::info!("Not the selected flow - skipping graph editor update");
                        }
                    } else {
                        tracing::warn!("Flow not found in list, adding it");
                        self.flows.push(flow);
                    }
                }
                AppMessage::RefreshNeeded => {
                    tracing::info!("Refresh requested due to flow fetch failure");
                    self.needs_refresh = true;
                }
                AppMessage::SystemInfoLoaded(system_info) => {
                    tracing::info!(
                        "System info loaded: v{} ({}) build_id={}",
                        system_info.version,
                        system_info.git_hash,
                        system_info.build_id
                    );

                    // Check if backend build_id differs from the one we got on initial load
                    // If so, the backend has been rebuilt and we need to reload the frontend
                    if let Some(ref existing_info) = self.system_info {
                        if !system_info.build_id.is_empty()
                            && !existing_info.build_id.is_empty()
                            && system_info.build_id != existing_info.build_id
                        {
                            tracing::warn!(
                                "Build ID mismatch! Previous: {}, Current: {} - reloading frontend",
                                existing_info.build_id,
                                system_info.build_id
                            );

                            // Force a hard reload to get the new frontend from the backend
                            #[cfg(target_arch = "wasm32")]
                            {
                                if let Some(window) = web_sys::window() {
                                    if let Err(e) = window.location().reload() {
                                        tracing::error!("Failed to reload page: {:?}", e);
                                    }
                                }
                            }
                            return;
                        }
                    }

                    self.system_info = Some(system_info);
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
                        self.connection_state = ConnectionState::Disconnected;
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
                AppMessage::RtpStatsLoaded { flow_id, rtp_stats } => {
                    tracing::debug!(
                        "RTP stats loaded for flow {}: {} blocks",
                        flow_id,
                        rtp_stats.blocks.len()
                    );
                    self.rtp_stats_cache.insert(flow_id, rtp_stats);
                }
                AppMessage::RtpStatsNotAvailable(flow_id) => {
                    tracing::debug!("RTP stats not available for flow {}", flow_id);
                    self.rtp_stats_cache.remove(&flow_id);
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
                    crate::clipboard::copy_text_with_ctx(ctx, &pipeline);
                    self.status =
                        format!("Flow '{}' exported to clipboard as gst-launch", flow_name);
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
                AppMessage::NdiSourcesLoaded { available, sources } => {
                    tracing::debug!(
                        "NDI sources loaded: available={}, {} sources",
                        available,
                        sources.len()
                    );
                    self.discovery_page.set_ndi_sources(available, sources);
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
            }
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

        // Repaint every second while something needs periodic updates
        // (thumbnails, recorder duration counter, etc.)
        let has_running_flow = self.flows.iter().any(|f| f.running);
        if has_running_flow || !self.recorder_start_times.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
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

        // Only poll flow-specific stats when on the Flows page
        if self.current_page == AppPage::Flows {
            // Poll WebRTC stats every second for running flows
            {
                let poll_interval = std::time::Duration::from_secs(1);
                if self.last_webrtc_poll.elapsed() >= poll_interval {
                    self.poll_webrtc_stats(ctx);
                    self.webrtc_stats
                        .evict_stale(std::time::Duration::from_secs(3));
                    self.last_webrtc_poll = instant::Instant::now();
                }
            }

            // Periodically fetch latency for selected flow (every 3 seconds)
            if self.last_latency_fetch.elapsed() > std::time::Duration::from_secs(3) {
                self.last_latency_fetch = instant::Instant::now();
                self.fetch_latency_for_running_flows(ctx);
            }

            // Periodically fetch RTP stats for selected flow (every second)
            if self.last_rtp_stats_fetch.elapsed() > std::time::Duration::from_secs(1) {
                self.last_rtp_stats_fetch = instant::Instant::now();
                self.fetch_rtp_stats_for_selected_flow(ctx);
            }

            // Poll thumbnail blocks in running flows
            self.poll_block_thumbnails(ctx);
            self.check_block_thumbnails(ctx);
        }

        // Handle keyboard shortcuts
        self.handle_keyboard_shortcuts(ctx);

        // Check for compositor editor open signal - enters Live mode
        if let Some(block_id) = get_local_storage("open_compositor_editor") {
            remove_local_storage("open_compositor_editor");

            // Get current flow
            if let Some(flow) = self.current_flow() {
                // Find the block
                if let Some(_block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Enter Live mode for this compositor
                    self.enter_live_mode(flow.id, block_id, ctx);
                }
            }
        }

        // Check for mixer editor open signal - enters Live Audio mode
        if let Some(block_id) = get_local_storage("open_mixer_editor") {
            remove_local_storage("open_mixer_editor");

            // Extract data from flow/block to avoid borrow issues
            let mixer_data = self.current_flow().and_then(|flow| {
                flow.blocks.iter().find(|b| b.id == block_id).map(|block| {
                    let num_channels = block
                        .properties
                        .get("num_channels")
                        .and_then(|v| match v {
                            strom_types::PropertyValue::String(s) => s.parse::<usize>().ok(),
                            strom_types::PropertyValue::Int(i) => Some(*i as usize),
                            _ => None,
                        })
                        .unwrap_or(8);
                    (flow.id, block.properties.clone(), num_channels)
                })
            });

            if let Some((flow_id, properties, num_channels)) = mixer_data {
                // Create mixer editor
                let mut editor = crate::mixer::MixerEditor::new(
                    flow_id,
                    block_id.clone(),
                    num_channels,
                    self.api.clone(),
                );
                editor.load_from_properties(&properties);
                self.mixer_editor = Some(editor);

                // Enter Live Audio mode
                self.app_mode = AppMode::Live { flow_id, block_id };
                tracing::info!("Entered Live Audio mode for mixer");
            }
        }

        // Check for playlist editor open signal
        // Check for stream picker open signal (double-click on AES67 Input)
        if let Some(block_id) = get_local_storage("open_stream_picker") {
            remove_local_storage("open_stream_picker");
            self.show_stream_picker_for_block = Some(block_id);
        }

        // Check for NDI picker open signal (double-click on NDI Input)
        if let Some(block_id) = get_local_storage("open_ndi_picker") {
            remove_local_storage("open_ndi_picker");
            self.show_ndi_picker_for_block = Some(block_id);
        }

        // Check for WHEP player open signal (double-click on WHEP Output)
        if let Some(block_id) = get_local_storage("open_whep_player") {
            remove_local_storage("open_whep_player");

            if let Some(flow) = self.current_flow() {
                if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Get endpoint_id from runtime_data or properties
                    let endpoint_id = block
                        .runtime_data
                        .as_ref()
                        .and_then(|rd| rd.get("whep_endpoint_id").cloned())
                        .or_else(|| {
                            block.properties.get("endpoint_id").and_then(|v| {
                                if let strom_types::PropertyValue::String(s) = v {
                                    if !s.is_empty() {
                                        Some(s.clone())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                        });

                    if let Some(endpoint_id) = endpoint_id {
                        let player_url = self.api.get_whep_player_url(&endpoint_id);
                        ctx.open_url(egui::OpenUrl::new_tab(&player_url));
                    }
                }
            }
        }

        // Check for vision mixer control page open signal (double-click on Vision Mixer)
        if let Some(_block_id) = get_local_storage("open_vision_mixer") {
            remove_local_storage("open_vision_mixer");

            if let Some(flow) = self.current_flow() {
                let url = self.api.get_vision_mixer_url(&flow.id);
                ctx.open_url(egui::OpenUrl::new_tab(&url));
            }
        }

        // Check for WHIP ingest open signal (double-click on WHIP Input)
        if let Some(block_id) = get_local_storage("open_whip_ingest") {
            remove_local_storage("open_whip_ingest");

            if let Some(flow) = self.current_flow() {
                if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Get endpoint_id from runtime_data or properties
                    let endpoint_id = block
                        .runtime_data
                        .as_ref()
                        .and_then(|rd| rd.get("whip_endpoint_id").cloned())
                        .or_else(|| {
                            block.properties.get("endpoint_id").and_then(|v| {
                                if let strom_types::PropertyValue::String(s) = v {
                                    if !s.is_empty() {
                                        Some(s.clone())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                        });

                    if let Some(endpoint_id) = endpoint_id {
                        let ingest_url = self.api.get_whip_ingest_url(&endpoint_id);
                        ctx.open_url(egui::OpenUrl::new_tab(&ingest_url));
                    }
                }
            }
        }

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

        // Check for routing matrix editor open signal
        if let Some(block_id) = get_local_storage("open_routing_editor") {
            remove_local_storage("open_routing_editor");

            // Get current flow and block definitions
            if let Some(flow) = self.current_flow() {
                if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Find block definition using graph's lookup
                    if let Some(definition) = self
                        .graph
                        .get_block_definition_by_id(&block.block_definition_id)
                    {
                        let mut editor = RoutingMatrixEditor::new(flow.id, block_id.clone());
                        editor.load_from_block(block, definition);
                        self.routing_matrix_editor = Some(editor);
                    }
                }
            }
        }

        // Show routing matrix editor if open
        let mut routing_save_pending: Option<(strom_types::FlowId, String, String)> = None;
        if let Some(ref mut editor) = self.routing_matrix_editor {
            if let Some(routing_json) = editor.show(ctx) {
                // Mark that we need to save
                routing_save_pending =
                    Some((editor.flow_id, editor.block_id.clone(), routing_json));
            }

            if !editor.open {
                self.routing_matrix_editor = None;
            }
        }

        // Process routing save outside the borrow
        if let Some((flow_id, block_id, routing_json)) = routing_save_pending {
            tracing::debug!(
                "Processing routing save for block {} in flow {}",
                block_id,
                flow_id
            );
            tracing::debug!("Routing JSON to save: {}", routing_json);

            // Update the block property in BOTH self.graph.blocks AND self.flows
            // (save_current_flow copies from graph.blocks, so we must update there)
            if let Some(block) = self.graph.blocks.iter_mut().find(|b| b.id == block_id) {
                block.properties.insert(
                    "routing_matrix".to_string(),
                    strom_types::PropertyValue::String(routing_json.clone()),
                );
                tracing::debug!("Updated routing_matrix in graph.blocks");
            }

            // Also update in self.flows for consistency
            if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
                if let Some(block) = flow.blocks.iter_mut().find(|b| b.id == block_id) {
                    block.properties.insert(
                        "routing_matrix".to_string(),
                        strom_types::PropertyValue::String(routing_json),
                    );
                }
            }

            // Save the flow
            self.save_current_flow(ctx);
        }

        // Check for player action signals (from compact UI controls)
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

        // Render based on app mode
        match &self.app_mode {
            AppMode::Admin => {
                self.render_toolbar(ctx);

                // Render page-specific content
                match self.current_page {
                    AppPage::Flows => {
                        self.render_flow_list(ctx);

                        // Only show palette when a flow is selected
                        if self.current_flow().is_some() {
                            self.render_palette(ctx);
                        }

                        self.render_log_panel(ctx);
                        self.render_canvas(ctx);
                        self.render_new_flow_dialog(ctx);
                        self.render_delete_confirmation_dialog(ctx);
                        self.render_flow_properties_dialog(ctx);
                        self.render_import_dialog(ctx);
                        self.render_stream_picker_modal(ctx);
                        self.render_ndi_picker_modal(ctx);
                    }
                    AppPage::Discovery => {
                        CentralPanel::default().show(ctx, |ui| {
                            self.discovery_page
                                .render(ui, &self.api, ctx, &self.channels.tx);
                        });

                        // Handle pending create flow from discovery
                        if let Some((sdp, interface)) =
                            self.discovery_page.take_pending_create_flow()
                        {
                            self.create_flow_from_sdp(sdp, interface, ctx);
                        }

                        // Handle pending go to flow from discovery
                        if let Some(flow_id_str) = self.discovery_page.take_pending_go_to_flow() {
                            if let Ok(uuid) = uuid::Uuid::parse_str(&flow_id_str) {
                                let flow_id = strom_types::FlowId::from(uuid);
                                // Clear any existing focus before changing graph structure
                                // to prevent accesskit panic when focused node is removed
                                ctx.memory_mut(|mem| {
                                    if let Some(focused_id) = mem.focused() {
                                        mem.surrender_focus(focused_id);
                                    }
                                });
                                self.select_flow(flow_id);
                                self.current_page = AppPage::Flows;
                            }
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
                                self.system_info.as_ref(),
                                &self.system_monitor,
                                &self.network_interfaces,
                                &self.flows,
                                &self.renderer_info,
                            );
                        });
                    }
                    AppPage::Links => {
                        CentralPanel::default().show(ctx, |ui| {
                            let server_hostname =
                                self.system_info.as_ref().map(|s| s.hostname.as_str());
                            self.links_page.render(
                                ui,
                                &self.api,
                                ctx,
                                &self.flows,
                                server_hostname,
                            );
                        });
                    }
                }

                self.render_status_bar(ctx);
                self.render_system_monitor_window(ctx);
            }
            AppMode::Live { flow_id, block_id } => {
                let flow_id = *flow_id;
                let block_id = block_id.clone();
                self.render_live_ui(ctx, flow_id, &block_id);
            }
        }

        // Process pending flow copy (after render to avoid borrow checker issues)
        if let Some(flow) = self.flow_pending_copy.take() {
            self.copy_flow(&flow, ctx);
        }

        // Render interactive overlay on top of everything when active
        if let Some(ref mut overlay) = self.interactive_overlay {
            if overlay.update(ctx) {
                self.interactive_overlay = None;
            }
        }
    }

    /// Save persistent state (called by eframe on shutdown and periodically)
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, APP_SETTINGS_KEY, &self.settings);
    }

    /// Auto-save interval - save every second to ensure persistence on iOS PWA
    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
}
