use crate::state::AppMessage;
use egui::Context;
use strom_types::Flow;

use super::*;
impl StromApp {
    /// Load flows from the backend.
    pub(super) fn load_flows(&mut self, ctx: &Context) {
        if self.loading {
            return;
        }

        tracing::info!("Starting to load flows...");
        self.loading = true;
        self.status = "Loading flows...".to_string();
        self.error = None;

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.list_flows().await {
                Ok(flows) => {
                    tracing::info!("Successfully fetched {} flows", flows.len());
                    let _ = tx.send(AppMessage::FlowsLoaded(flows));
                }
                Err(e) => {
                    if e.is_unauthorized() {
                        let _ = tx.send(AppMessage::SessionExpired);
                    } else {
                        tracing::error!("Failed to load flows: {}", e);
                        let _ = tx.send(AppMessage::FlowsError(e.to_string()));
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    /// Fetch latency for the currently selected flow (if running).
    pub(super) fn fetch_latency_for_running_flows(&self, ctx: &Context) {
        // Only fetch for selected flow if it's running
        let flow_id = match self.selected_flow_id {
            Some(id) => id,
            None => return,
        };

        // Check if the selected flow is running
        let is_running = self
            .flows
            .iter()
            .find(|f| f.id == flow_id)
            .map(|f| f.running)
            .unwrap_or(false);

        if !is_running {
            return;
        }

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();
        let flow_id_str = flow_id.to_string();

        spawn_task(async move {
            match api.get_flow_latency(flow_id).await {
                Ok(latency) => {
                    let _ = tx.send(AppMessage::LatencyLoaded {
                        flow_id: flow_id_str,
                        latency,
                    });
                }
                Err(_) => {
                    // Flow not running or latency not available - silently ignore
                    let _ = tx.send(AppMessage::LatencyNotAvailable(flow_id_str));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Fetch system clock synchronization state from the backend.
    pub(super) fn fetch_system_clock_info(&self, ctx: &Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.get_system_clock().await {
                Ok(info) => {
                    let _ = tx.send(AppMessage::SystemClockLoaded(info));
                }
                Err(crate::api::ApiError::Http(501, _)) => {
                    let _ = tx.send(AppMessage::SystemClockUnsupported);
                }
                Err(_) => {
                    // Transient network/decode error — leave previous state intact.
                }
            }
            ctx.request_repaint();
        });
    }

    /// Fetch RTP statistics and dynamic pads for the currently selected flow (if running).
    /// RTP stats are only fetched if the flow has blocks that produce them (e.g., AES67 Input).
    pub(super) fn fetch_rtp_stats_for_selected_flow(&self, ctx: &Context) {
        // Only fetch for selected flow if it's running
        let flow_id = match self.selected_flow_id {
            Some(id) => id,
            None => return,
        };

        // Check if the selected flow is running
        let flow = self.flows.iter().find(|f| f.id == flow_id);
        let is_running = flow.map(|f| f.running).unwrap_or(false);

        if !is_running {
            return;
        }

        // Check if the flow has blocks that produce RTP stats
        let has_rtp_stats_blocks = flow
            .map(|f| {
                f.blocks
                    .iter()
                    .any(|b| b.block_definition_id == "builtin.aes67_input")
            })
            .unwrap_or(false);

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();
        let flow_id_str = flow_id.to_string();

        spawn_task(async move {
            // Only fetch RTP stats if the flow has blocks that produce them
            if has_rtp_stats_blocks {
                match api.get_flow_rtp_stats(flow_id).await {
                    Ok(rtp_stats) => {
                        let _ = tx.send(AppMessage::RtpStatsLoaded {
                            flow_id: flow_id_str.clone(),
                            rtp_stats,
                        });
                    }
                    Err(_) => {
                        // Flow not running or RTP stats not available - silently ignore
                        let _ = tx.send(AppMessage::RtpStatsNotAvailable(flow_id_str.clone()));
                    }
                }
            }

            // Always fetch dynamic pads for the selected flow
            if let Ok(pads) = api.get_dynamic_pads(flow_id).await {
                let _ = tx.send(AppMessage::DynamicPadsLoaded {
                    flow_id: flow_id_str,
                    pads,
                });
            }

            ctx.request_repaint();
        });
    }

    /// Save the current flow to the backend.
    pub(super) fn save_current_flow(&mut self, ctx: &Context) {
        tracing::info!(
            "save_current_flow called, selected_flow_id: {:?}",
            self.selected_flow_id
        );

        if let Some(flow_id) = self.selected_flow_id {
            // Update flow with current graph state
            if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.elements = self.graph.elements.clone();
                flow.blocks = self.graph.blocks.clone();
                flow.links = self.graph.links.clone();

                tracing::info!(
                    "Preparing to save flow: id={}, name='{}', elements={}, links={}",
                    flow.id,
                    flow.name,
                    flow.elements.len(),
                    flow.links.len()
                );

                let flow_clone = flow.clone();
                let api = self.api.clone();
                let tx = self.channels.sender();
                let ctx = ctx.clone();

                self.status = "Saving flow...".to_string();

                spawn_task(async move {
                    tracing::info!("Starting async save operation for flow {}", flow_clone.id);
                    match api.update_flow(&flow_clone).await {
                        Ok(_) => {
                            tracing::info!(
                                "Flow saved successfully - WebSocket event will trigger refresh"
                            );
                            let _ =
                                tx.send(AppMessage::FlowOperationSuccess("Flow saved".to_string()));
                        }
                        Err(e) => {
                            tracing::error!("Failed to save flow: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to save flow: {}",
                                e
                            )));
                        }
                    }
                    ctx.request_repaint();
                });
            } else {
                tracing::warn!("save_current_flow: No flow found with id {}", flow_id);
            }
        } else {
            tracing::warn!("save_current_flow: No flow selected");
        }
    }

    /// Create a new flow.
    pub(super) fn create_flow(&mut self, ctx: &Context) {
        if self.new_flow_name.is_empty() {
            self.error = Some("Flow name cannot be empty".to_string());
            return;
        }

        let new_flow = Flow::new(self.new_flow_name.clone());
        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Creating flow...".to_string();
        self.show_new_flow_dialog = false;
        self.new_flow_name.clear();

        spawn_task(async move {
            match api.create_flow(&new_flow).await {
                Ok(created_flow) => {
                    tracing::info!(
                        "Flow created successfully: {} - WebSocket event will trigger refresh",
                        created_flow.name
                    );
                    let flow_id = created_flow.id;
                    let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                        "Flow '{}' created",
                        created_flow.name
                    )));
                    // Send flow ID so we can navigate to it after refresh
                    let _ = tx.send(AppMessage::FlowCreated(flow_id));
                }
                Err(e) => {
                    tracing::error!("Failed to create flow: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to create flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Create a new flow from an SDP (from discovered stream).
    /// If interface is provided, it will be set on the AES67 Input block.
    pub(super) fn create_flow_from_sdp(
        &mut self,
        sdp: String,
        interface: Option<String>,
        ctx: &Context,
    ) {
        use strom_types::{block::Position, BlockInstance, PropertyValue};

        // Parse stream name from SDP (before moving sdp)
        let stream_name = sdp
            .lines()
            .find(|l| l.starts_with("s="))
            .map(|l| l.trim_start_matches("s=").trim().to_string())
            .unwrap_or_else(|| "Discovered Stream".to_string());

        let flow_name = format!("AES67 - {}", stream_name);

        // Create flow with AES67 Input block
        let mut new_flow = Flow::new(flow_name.clone());

        // Build properties - SDP and optionally interface
        let mut properties =
            std::collections::HashMap::from([("SDP".to_string(), PropertyValue::String(sdp))]);

        // Add interface if discovered
        if let Some(iface) = interface {
            tracing::info!(
                "Setting interface '{}' on AES67 Input block (discovered from SAP)",
                iface
            );
            properties.insert("interface".to_string(), PropertyValue::String(iface));
        }

        // Create AES67 Input block instance
        let block = BlockInstance {
            id: uuid::Uuid::new_v4().to_string(),
            block_definition_id: "builtin.aes67_input".to_string(),
            name: Some(stream_name.clone()),
            properties,
            position: Position { x: 100.0, y: 100.0 },
            runtime_data: None,
            computed_external_pads: None,
        };

        new_flow.blocks.push(block);

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Creating flow from SDP...".to_string();
        // Switch to Flows page
        self.current_page = AppPage::Flows;

        spawn_task(async move {
            // First create the empty flow to get an ID
            match api.create_flow(&new_flow).await {
                Ok(created_flow) => {
                    tracing::info!("Flow created from SDP: {}", created_flow.name);
                    let flow_id = created_flow.id;
                    let flow_name = created_flow.name.clone();

                    // Now update the flow with the blocks
                    let mut full_flow = new_flow;
                    full_flow.id = flow_id;

                    match api.update_flow(&full_flow).await {
                        Ok(_) => {
                            tracing::info!("Flow updated with AES67 Input block: {}", flow_name);
                            let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                "Flow '{}' created from discovered stream",
                                flow_name
                            )));
                            let _ = tx.send(AppMessage::FlowCreated(flow_id));
                        }
                        Err(e) => {
                            tracing::error!("Failed to update flow with block: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to add block to flow: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create flow from SDP: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to create flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Start the current flow.
    pub(super) fn start_flow(&mut self, ctx: &Context) {
        if let Some(flow) = self.current_flow() {
            let flow_id = flow.id;
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            self.status = "Starting flow...".to_string();

            spawn_task(async move {
                match api.start_flow(flow_id).await {
                    Ok(_) => {
                        tracing::info!(
                            "Flow started successfully - WebSocket event will trigger refresh"
                        );
                        let _ =
                            tx.send(AppMessage::FlowOperationSuccess("Flow started".to_string()));
                    }
                    Err(e) => {
                        tracing::error!("Failed to start flow: {}", e);
                        let _ = tx.send(AppMessage::FlowOperationError(format!(
                            "Failed to start flow: {}",
                            e
                        )));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Stop the current flow.
    pub(super) fn stop_flow(&mut self, ctx: &Context) {
        if let Some(flow) = self.current_flow() {
            let flow_id = flow.id;
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            self.status = "Stopping flow...".to_string();

            spawn_task(async move {
                match api.stop_flow(flow_id).await {
                    Ok(_) => {
                        tracing::info!(
                            "Flow stopped successfully - WebSocket event will trigger refresh"
                        );
                        let _ =
                            tx.send(AppMessage::FlowOperationSuccess("Flow stopped".to_string()));
                    }
                    Err(e) => {
                        tracing::error!("Failed to stop flow: {}", e);
                        let _ = tx.send(AppMessage::FlowOperationError(format!(
                            "Failed to stop flow: {}",
                            e
                        )));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Stop a specific flow by ID (used for auto-stop triggered by recorder or other blocks).
    pub(super) fn stop_flow_by_id(&mut self, flow_id: strom_types::FlowId, ctx: &Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Stopping flow...".to_string();

        spawn_task(async move {
            match api.stop_flow(flow_id).await {
                Ok(_) => {
                    tracing::info!("Flow {} stopped by auto-stop", flow_id);
                    let _ = tx.send(AppMessage::FlowOperationSuccess("Flow stopped".to_string()));
                }
                Err(e) => {
                    tracing::error!("Failed to stop flow {}: {}", flow_id, e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to stop flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Delete a flow.
    pub(super) fn delete_flow(&mut self, flow_id: strom_types::FlowId, ctx: &Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Deleting flow...".to_string();

        spawn_task(async move {
            match api.delete_flow(flow_id).await {
                Ok(_) => {
                    tracing::info!(
                        "Flow deleted successfully - WebSocket event will trigger refresh"
                    );
                    let _ = tx.send(AppMessage::FlowOperationSuccess("Flow deleted".to_string()));
                }
                Err(e) => {
                    tracing::error!("Failed to delete flow: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to delete flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }
}
