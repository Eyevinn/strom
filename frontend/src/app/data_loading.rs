use super::*;
use crate::api::AuthStatusResponse;
use crate::state::AppMessage;
use egui::Context;
impl StromApp {
    /// Load GStreamer elements from the backend.
    pub(super) fn load_elements(&mut self, ctx: &Context) {
        tracing::info!("Starting to load GStreamer elements...");
        self.status = "Loading elements...".to_string();

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.list_elements().await {
                Ok(elements) => {
                    tracing::info!("Successfully fetched {} elements", elements.len());
                    let _ = tx.send(AppMessage::ElementsLoaded(elements));
                }
                Err(e) => {
                    if e.is_unauthorized() {
                        let _ = tx.send(AppMessage::SessionExpired);
                    } else {
                        tracing::error!("Failed to load elements: {}", e);
                        let _ = tx.send(AppMessage::ElementsError(e.to_string()));
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load blocks from the backend.
    pub(super) fn load_blocks(&mut self, ctx: &Context) {
        tracing::info!("Starting to load blocks...");
        self.status = "Loading blocks...".to_string();

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.list_blocks().await {
                Ok(blocks) => {
                    tracing::info!("Successfully fetched {} blocks", blocks.len());
                    let _ = tx.send(AppMessage::BlocksLoaded(blocks));
                }
                Err(e) => {
                    if e.is_unauthorized() {
                        let _ = tx.send(AppMessage::SessionExpired);
                    } else {
                        tracing::error!("Failed to load blocks: {}", e);
                        let _ = tx.send(AppMessage::BlocksError(e.to_string()));
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load version information from the backend.
    pub(super) fn load_version(&mut self, ctx: egui::Context) {
        tracing::info!("Loading version information from backend...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_version().await {
                Ok(version_info) => {
                    tracing::info!(
                        "Successfully loaded version: v{} ({})",
                        version_info.version,
                        version_info.git_hash
                    );
                    let _ = tx.send(AppMessage::SystemInfoLoaded(version_info));
                }
                Err(e) => {
                    tracing::warn!("Failed to load version info: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load network interfaces from the backend (for network interface property dropdown).
    pub(super) fn load_network_interfaces(&mut self, ctx: egui::Context) {
        if self.network_interfaces_loaded {
            return;
        }
        self.network_interfaces_loaded = true; // Prevent multiple concurrent requests
        tracing::info!("Loading network interfaces from backend...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.list_network_interfaces().await {
                Ok(response) => {
                    tracing::info!(
                        "Successfully loaded {} network interfaces",
                        response.interfaces.len()
                    );
                    let _ = tx.send(AppMessage::NetworkInterfacesLoaded(response.interfaces));
                }
                Err(e) => {
                    tracing::warn!("Failed to load network interfaces: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load log level from the backend.
    pub(super) fn load_log_level(&mut self, ctx: egui::Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_log_level().await {
                Ok(resp) => {
                    let _ = tx.send(AppMessage::LogLevelLoaded {
                        current: resp.current,
                        default: resp.default,
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to load log level: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Set the log level on the backend.
    pub(super) fn set_log_level(&mut self, filter: String, ctx: egui::Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.set_log_level(&filter).await {
                Ok(resp) => {
                    let _ = tx.send(AppMessage::LogLevelLoaded {
                        current: resp.current,
                        default: resp.default,
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to set log level: {}", e);
                    let _ = tx.send(AppMessage::LogLevelError(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load GStreamer debug level from the backend.
    pub(super) fn load_gst_log_level(&mut self, ctx: egui::Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_gst_log_level().await {
                Ok(resp) => {
                    let _ = tx.send(AppMessage::GstLogLevelLoaded {
                        current: resp.current,
                        default: resp.default,
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to load GStreamer debug level: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Set the GStreamer debug level on the backend.
    pub(super) fn set_gst_log_level(&mut self, filter: String, ctx: egui::Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.set_gst_log_level(&filter).await {
                Ok(resp) => {
                    let _ = tx.send(AppMessage::GstLogLevelLoaded {
                        current: resp.current,
                        default: resp.default,
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to set GStreamer debug level: {}", e);
                    let _ = tx.send(AppMessage::GstLogLevelError(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Get cached network interfaces (for property inspector).
    pub fn network_interfaces(&self) -> &[strom_types::NetworkInterfaceInfo] {
        &self.network_interfaces
    }

    /// Load available inter channels from the backend (for InterInput channel dropdown).
    pub(super) fn load_available_channels(&mut self, ctx: egui::Context) {
        if self.available_channels_loaded {
            return;
        }
        self.available_channels_loaded = true; // Prevent multiple concurrent requests
        tracing::info!("Loading available inter channels from backend...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_available_sources().await {
                Ok(response) => {
                    // Flatten all outputs from all source flows
                    let all_channels: Vec<_> = response
                        .sources
                        .into_iter()
                        .flat_map(|source| source.outputs)
                        .collect();
                    tracing::info!(
                        "Successfully loaded {} available inter channels",
                        all_channels.len()
                    );
                    let _ = tx.send(AppMessage::AvailableChannelsLoaded(all_channels));
                }
                Err(e) => {
                    tracing::warn!("Failed to load available channels: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Refresh available channels (called when flow state changes).
    pub fn refresh_available_channels(&mut self) {
        self.available_channels_loaded = false;
    }

    /// Get cached available channels (for property inspector).
    pub fn available_channels(&self) -> &[strom_types::api::AvailableOutput] {
        &self.available_channels
    }

    /// Poll WebRTC stats for the currently selected flow (if running and has WebRTC blocks).
    /// Called periodically (every second).
    pub(super) fn poll_webrtc_stats(&mut self, ctx: &Context) {
        // Only fetch for selected flow if it's running
        let flow_id = match self.selected_flow_id {
            Some(id) => id,
            None => return,
        };

        // Check if the selected flow is running and has WebRTC blocks
        let flow = self.flows.iter().find(|f| f.id == flow_id);
        let is_running = flow.map(|f| f.running).unwrap_or(false);

        if !is_running {
            return;
        }

        // Only fetch WebRTC stats if the flow has WebRTC blocks
        let has_webrtc_blocks = flow
            .map(|f| {
                f.blocks.iter().any(|b| {
                    matches!(
                        b.block_definition_id.as_str(),
                        "builtin.whep_input"
                            | "builtin.whep_output"
                            | "builtin.whip_output"
                            | "builtin.whip_input"
                    )
                })
            })
            .unwrap_or(false);

        if !has_webrtc_blocks {
            return;
        }

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.get_webrtc_stats(flow_id).await {
                Ok(stats) => {
                    tracing::debug!(
                        "Fetched WebRTC stats for flow {}: {} connections",
                        flow_id,
                        stats.connections.len()
                    );
                    let _ = tx.send(AppMessage::WebRtcStatsLoaded { flow_id, stats });
                }
                Err(e) => {
                    // Don't log errors for flows without WebRTC elements
                    tracing::trace!("No WebRTC stats for flow {}: {}", flow_id, e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Check authentication status
    pub(super) fn check_auth_status(&mut self, ctx: egui::Context) {
        if self.checking_auth {
            return;
        }

        self.checking_auth = true;
        tracing::info!("Checking authentication status...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_auth_status().await {
                Ok(status) => {
                    tracing::info!(
                        "Auth status: required={}, authenticated={}",
                        status.auth_required,
                        status.authenticated
                    );
                    let _ = tx.send(AppMessage::AuthStatusLoaded(status));
                }
                Err(e) => {
                    if e.is_unauthorized() {
                        tracing::warn!("Auth status check returned 401 - session expired");
                        let _ = tx.send(AppMessage::SessionExpired);
                    } else {
                        tracing::warn!("Failed to check auth status: {}", e);
                        // Assume auth is not required if check fails (network error, etc.)
                        let _ = tx.send(AppMessage::AuthStatusLoaded(AuthStatusResponse {
                            authenticated: true,
                            auth_required: false,
                            methods: vec![],
                        }));
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    /// Handle logout
    pub(super) fn handle_logout(&mut self, ctx: egui::Context) {
        tracing::info!("Logging out...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.logout().await {
                Ok(_) => {
                    tracing::info!("Logged out successfully");
                    let _ = tx.send(AppMessage::LogoutComplete);
                }
                Err(e) => {
                    tracing::error!("Logout failed: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load element properties from the backend (lazy loading).
    /// Properties are cached after first load.
    pub(super) fn load_element_properties(&mut self, element_type: String, ctx: &Context) {
        tracing::info!("Starting to load properties for element: {}", element_type);

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.get_element_info(&element_type).await {
                Ok(element_info) => {
                    tracing::info!(
                        "Successfully fetched properties for '{}' ({} properties)",
                        element_info.name,
                        element_info.properties.len()
                    );
                    let _ = tx.send(AppMessage::ElementPropertiesLoaded(element_info));
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to load element properties for '{}': {}",
                        element_type,
                        e
                    );
                    let _ = tx.send(AppMessage::ElementPropertiesError(
                        element_type,
                        e.to_string(),
                    ));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load pad properties from the backend (on-demand lazy loading).
    /// Pad properties are cached separately after first load.
    pub(super) fn load_element_pad_properties(&mut self, element_type: String, ctx: &Context) {
        tracing::info!(
            "Starting to load pad properties for element: {}",
            element_type
        );

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.get_element_pad_properties(&element_type).await {
                Ok(element_info) => {
                    tracing::info!(
                        "Successfully fetched pad properties for '{}' (sink_pads: {}, src_pads: {})",
                        element_info.name,
                        element_info.sink_pads.iter().map(|p| p.properties.len()).sum::<usize>(),
                        element_info.src_pads.iter().map(|p| p.properties.len()).sum::<usize>()
                    );
                    let _ = tx.send(AppMessage::ElementPadPropertiesLoaded(element_info));
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to load pad properties for '{}': {}",
                        element_type,
                        e
                    );
                    let _ = tx.send(AppMessage::ElementPadPropertiesError(
                        element_type,
                        e.to_string(),
                    ));
                }
            }
            ctx.request_repaint();
        });
    }
}
