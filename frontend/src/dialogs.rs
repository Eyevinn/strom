//! Dialog rendering for the Strom application.
//!
//! Contains all modal dialogs and overlays:
//! - New flow dialog
//! - Delete confirmation dialog
//! - Flow properties dialog
//! - Import dialog (JSON and gst-launch)
//! - Stream picker modal
//! - System monitor window
//! - Disconnect overlay

use egui::{CentralPanel, Color32, Context};
use strom_types::{Flow, PipelineState};

use crate::app::{AppPage, ImportFormat, StromApp};
use crate::state::AppMessage;
use crate::utils::spawn_task;

impl StromApp {
    /// Render the new flow dialog.
    pub(crate) fn render_new_flow_dialog(&mut self, ctx: &Context) {
        if !self.show_new_flow_dialog {
            return;
        }

        egui::Window::new("New Flow")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut self.new_flow_name);
                });

                // Check for Enter key to create flow
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.new_flow_name.is_empty() {
                    self.create_flow(ctx);
                }

                ui.horizontal(|ui| {
                    if ui.button("Create").clicked() {
                        self.create_flow(ctx);
                    }

                    if ui.button("Cancel").clicked() {
                        self.show_new_flow_dialog = false;
                        self.new_flow_name.clear();
                    }
                });
            });
    }

    /// Render the delete confirmation dialog.
    pub(crate) fn render_delete_confirmation_dialog(&mut self, ctx: &Context) {
        if self.flow_pending_deletion.is_none() {
            return;
        }

        let (flow_id, flow_name) = self.flow_pending_deletion.as_ref().unwrap().clone();

        egui::Window::new("Delete Flow")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Are you sure you want to delete this flow?");
                ui.add_space(5.0);
                ui.colored_label(Color32::YELLOW, format!("Flow: {}", flow_name));
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if ui.button("❌ Delete").clicked() {
                        self.delete_flow(flow_id, ctx);
                        self.flow_pending_deletion = None;
                    }

                    if ui.button("Cancel").clicked() {
                        self.flow_pending_deletion = None;
                    }
                });
            });
    }

    /// Render the system monitor window.
    pub(crate) fn render_system_monitor_window(&mut self, ctx: &Context) {
        if !self.show_system_monitor {
            return;
        }

        egui::Window::new("System Monitoring")
            .collapsible(true)
            .resizable(true)
            .default_width(700.0)
            .default_height(500.0)
            .open(&mut self.show_system_monitor)
            .show(ctx, |ui| {
                crate::system_monitor::DetailedSystemMonitor::new(&self.system_monitor).show(ui);
            });
    }

    /// Render the flow properties dialog.
    pub(crate) fn render_flow_properties_dialog(&mut self, ctx: &Context) {
        let flow_id = match self.editing_properties_flow_id {
            Some(id) => id,
            None => return,
        };

        let flow = match self.flows.iter().find(|f| f.id == flow_id) {
            Some(f) => f,
            None => {
                self.editing_properties_flow_id = None;
                return;
            }
        };

        let flow_name = flow.name.clone();

        egui::Window::new(format!("⚙ {} - Properties", flow_name))
            .collapsible(false)
            .resizable(true)
            .default_width(400.0)
            .default_height(500.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 50.0) // Leave room for buttons
                    .show(ui, |ui| {
                ui.heading("Flow Properties");
                ui.add_space(5.0);

                // Name
                ui.label("Name:");
                ui.text_edit_singleline(&mut self.properties_name_buffer);
                ui.add_space(10.0);

                // Description
                ui.label("Description:");
                ui.add(
                    egui::TextEdit::multiline(&mut self.properties_description_buffer)
                        .desired_width(f32::INFINITY)
                        .desired_rows(5)
                        .hint_text("Optional description for this flow..."),
                );

                ui.add_space(10.0);

                // Clock Type
                ui.label("Clock Type:");
                ui.horizontal(|ui| {
                    use strom_types::flow::GStreamerClockType;

                    egui::ComboBox::from_id_salt("clock_type_selector")
                        .selected_text(self.properties_clock_type_buffer.label())
                        .show_ui(ui, |ui| {
                            for clock_type in GStreamerClockType::all() {
                                let label = if *clock_type == GStreamerClockType::Monotonic {
                                    format!("{} (recommended)", clock_type.label())
                                } else {
                                    clock_type.label().to_string()
                                };
                                ui.selectable_value(
                                    &mut self.properties_clock_type_buffer,
                                    *clock_type,
                                    label,
                                );
                            }
                        });
                });

                // Show description of selected clock type
                ui.label(self.properties_clock_type_buffer.description());

                // Show PTP domain field only when PTP is selected
                if matches!(
                    self.properties_clock_type_buffer,
                    strom_types::flow::GStreamerClockType::Ptp
                ) {
                    ui.add_space(10.0);
                    ui.label("PTP Domain (0-255):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.properties_ptp_domain_buffer)
                            .desired_width(100.0)
                            .hint_text("0"),
                    );
                    ui.label("The PTP domain for clock synchronization");
                }

                // Show clock sync status for PTP/NTP clocks
                if matches!(
                    self.properties_clock_type_buffer,
                    strom_types::flow::GStreamerClockType::Ptp
                        | strom_types::flow::GStreamerClockType::Ntp
                ) {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.label("Clock Status:");
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                            if let Some(sync_status) = flow.properties.clock_sync_status {
                                use strom_types::flow::ClockSyncStatus;
                                match sync_status {
                                    ClockSyncStatus::Synced => {
                                        ui.colored_label(Color32::from_rgb(0, 200, 0), "[OK] Synced");
                                    }
                                    ClockSyncStatus::NotSynced => {
                                        ui.colored_label(
                                            Color32::from_rgb(200, 0, 0),
                                            "[!] Not Synced",
                                        );
                                    }
                                    ClockSyncStatus::Unknown => {
                                        ui.colored_label(Color32::GRAY, "[-] Unknown");
                                    }
                                }
                            } else {
                                ui.colored_label(Color32::GRAY, "[-] Unknown");
                            }
                        }
                    });

                    // Show PTP-specific options and link to Clocks page
                    if matches!(
                        self.properties_clock_type_buffer,
                        strom_types::flow::GStreamerClockType::Ptp
                    ) {
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                            ui.add_space(5.0);

                            // Show warning if restart needed - compare buffer with running domain
                            if let Some(ref ptp_info) = flow.properties.ptp_info {
                                let buffer_domain: u8 = self
                                    .properties_ptp_domain_buffer
                                    .parse()
                                    .unwrap_or(0);
                                let domain_changed = buffer_domain != ptp_info.domain;
                                if domain_changed {
                                    ui.colored_label(
                                        Color32::from_rgb(255, 165, 0),
                                        "! Restart needed - domain changed",
                                    );
                                }
                            }

                            // Button to open Clocks page for detailed stats
                            ui.add_space(5.0);
                            if ui
                                .button("View PTP Statistics")
                                .on_hover_text("Open Clocks page for detailed PTP statistics")
                                .clicked()
                            {
                                self.current_page = AppPage::Clocks;
                            }
                        }
                    }
                }

                ui.add_space(15.0);
                ui.separator();
                ui.add_space(10.0);

                // Thread Priority
                ui.label("Thread Priority:");
                ui.horizontal(|ui| {
                    use strom_types::flow::ThreadPriority;

                    egui::ComboBox::from_id_salt("thread_priority_selector")
                        .selected_text(format!("{:?}", self.properties_thread_priority_buffer))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::Normal,
                                "Normal",
                            );
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::High,
                                "High (recommended)",
                            );
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::Realtime,
                                "Realtime (requires privileges)",
                            );
                        });
                });

                // Show description of selected thread priority
                ui.label(self.properties_thread_priority_buffer.description());

                // Show thread priority status for running pipelines
                if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                    if let Some(ref status) = flow.properties.thread_priority_status {
                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.label("Status:");
                            if status.achieved {
                                ui.colored_label(
                                    Color32::from_rgb(0, 200, 0),
                                    format!("[OK] Achieved ({} threads)", status.threads_configured),
                                );
                            } else if let Some(ref err) = status.error {
                                ui.colored_label(Color32::from_rgb(255, 165, 0), "[!] Warning");
                                ui.label(format!("- {}", err));
                            } else {
                                ui.colored_label(Color32::GRAY, "[-] Not set");
                            }
                        });
                    }
                }

                }); // End ScrollArea

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(5.0);

                // Buttons (outside scroll area)
                ui.horizontal(|ui| {
                    if ui.button("💾 Save").clicked() {
                        // Update flow properties
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter_mut().find(|f| f.id == id)) {
                            // Update flow name
                            flow.name = self.properties_name_buffer.clone();

                            flow.properties.description =
                                if self.properties_description_buffer.is_empty() {
                                    None
                                } else {
                                    Some(self.properties_description_buffer.clone())
                                };
                            flow.properties.clock_type = self.properties_clock_type_buffer;

                            // Parse and set PTP domain if PTP clock is selected
                            flow.properties.ptp_domain = if matches!(
                                self.properties_clock_type_buffer,
                                strom_types::flow::GStreamerClockType::Ptp
                            ) {
                                self.properties_ptp_domain_buffer.parse::<u8>().ok()
                            } else {
                                None
                            };

                            // Set thread priority
                            flow.properties.thread_priority =
                                self.properties_thread_priority_buffer;

                            let flow_clone = flow.clone();
                            let api = self.api.clone();
                            let ctx_clone = ctx.clone();

                            spawn_task(async move {
                                match api.update_flow(&flow_clone).await {
                                    Ok(_) => {
                                        tracing::info!("Flow properties updated successfully - WebSocket event will trigger refresh");
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to update flow properties: {}", e);
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        self.editing_properties_flow_id = None;
                    }

                    if ui.button("Cancel").clicked() {
                        self.editing_properties_flow_id = None;
                    }
                });
            });
    }

    /// Render the stream picker modal for selecting discovered streams.
    pub(crate) fn render_stream_picker_modal(&mut self, ctx: &Context) {
        let Some(block_id) = self.show_stream_picker_for_block.clone() else {
            return;
        };

        let mut close_modal = false;
        let mut selected_sdp: Option<String> = None;

        egui::Window::new("Select Discovered Stream")
            .collapsible(false)
            .resizable(true)
            .default_width(500.0)
            .default_height(400.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Select a stream to use its SDP:");
                ui.add_space(8.0);

                let streams = &self.discovery_page.discovered_streams;
                let is_loading = self.discovery_page.loading;

                if is_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading discovered streams...");
                    });
                } else if streams.is_empty() {
                    ui.label("No discovered streams available.");
                    ui.label("Make sure SAP discovery is running and streams are being announced on the network.");
                    ui.add_space(8.0);
                    if ui.button("🔄 Refresh").clicked() {
                        self.discovery_page.refresh(&self.api, ctx, &self.channels.tx);
                    }
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for stream in streams {
                                let text = format!(
                                    "{} - {}:{} ({}ch {}Hz)",
                                    stream.name,
                                    stream.multicast_address,
                                    stream.port,
                                    stream.channels,
                                    stream.sample_rate
                                );

                                if ui.selectable_label(false, &text).clicked() {
                                    // Fetch SDP for this stream
                                    // For now, we'll construct it from the stream info
                                    // In a real implementation, we'd fetch the actual SDP
                                    selected_sdp = Some(stream.id.clone());
                                }
                            }
                        });
                }

                ui.add_space(8.0);
                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close_modal = true;
                    }
                });
            });

        if close_modal {
            self.show_stream_picker_for_block = None;
        }

        // If a stream was selected, fetch its SDP and update the block
        if let Some(stream_id) = selected_sdp {
            self.show_stream_picker_for_block = None;

            // Fetch the SDP and update the block
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            spawn_task(async move {
                match api.get_stream_sdp(&stream_id).await {
                    Ok(sdp) => {
                        tracing::info!(
                            "Fetched SDP for stream {}, sending to block {}",
                            stream_id,
                            block_id
                        );
                        let _ = tx.send(AppMessage::StreamPickerSdpLoaded { block_id, sdp });
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch stream SDP for {}: {}", stream_id, e);
                        let _ = tx.send(AppMessage::FlowOperationError(format!(
                            "Failed to fetch stream SDP: {}",
                            e
                        )));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Render the import flow dialog.
    pub(crate) fn render_import_dialog(&mut self, ctx: &Context) {
        if !self.show_import_dialog {
            return;
        }

        egui::Window::new("Import Flow")
            .collapsible(false)
            .resizable(true)
            .default_width(550.0)
            .default_height(450.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                // Format selection tabs
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    ui.add_space(10.0);
                    if ui
                        .selectable_label(self.import_format == ImportFormat::Json, "JSON")
                        .clicked()
                    {
                        self.import_format = ImportFormat::Json;
                        self.import_error = None;
                    }
                    if ui
                        .selectable_label(self.import_format == ImportFormat::GstLaunch, "gst-launch")
                        .clicked()
                    {
                        self.import_format = ImportFormat::GstLaunch;
                        self.import_error = None;
                    }
                });

                ui.add_space(5.0);
                ui.separator();
                ui.add_space(5.0);

                // Format-specific instructions
                match self.import_format {
                    ImportFormat::Json => {
                        ui.label("Paste flow JSON below:");
                    }
                    ImportFormat::GstLaunch => {
                        ui.label("Paste gst-launch-1.0 pipeline below, or click an example:");
                        ui.add_space(5.0);

                        // Example pipelines in a collapsible section
                        egui::CollapsingHeader::new("Examples")
                            .default_open(true)
                            .show(ui, |ui| {
                                let examples = [
                                    ("Test Video", "videotestsrc pattern=ball is-live=true ! videoconvert ! autovideosink"),
                                    ("Test Audio", "audiotestsrc wave=sine freq=440 is-live=true ! audioconvert ! autoaudiosink"),
                                    ("Video + Overlay", "videotestsrc is-live=true ! clockoverlay ! videoconvert ! autovideosink"),
                                    ("Record Video", "videotestsrc num-buffers=300 is-live=true ! x264enc ! mp4mux ! filesink location=test.mp4"),
                                    ("RTP Stream Send", "videotestsrc is-live=true ! x264enc tune=zerolatency bitrate=500 ! rtph264pay ! udpsink port=5000 host=127.0.0.1"),
                                    ("RTP Stream Receive", "udpsrc ! application/x-rtp,payload=96 ! rtph264depay ! avdec_h264 ! videoconvert ! autovideosink"),
                                    ("Record + Display", "videotestsrc is-live=true ! tee name=t t. ! queue ! x264enc ! mp4mux ! filesink location=output.mp4 t. ! queue ! autovideosink"),
                                    ("AV Mux", "videotestsrc is-live=true ! x264enc ! mp4mux name=mux ! filesink location=av.mp4 audiotestsrc is-live=true ! lamemp3enc ! mux."),
                                    ("File Playback", "filesrc location=video.mp4 ! decodebin ! videoconvert ! autovideosink"),
                                    ("Camera", "v4l2src ! videoconvert ! autovideosink"),
                                ];

                                ui.horizontal_wrapped(|ui| {
                                    for (name, pipeline) in examples {
                                        if ui.small_button(name).on_hover_text(pipeline).clicked() {
                                            self.import_json_buffer = pipeline.to_string();
                                        }
                                    }
                                });
                            });
                    }
                }
                ui.add_space(5.0);

                // Large text area for input
                let hint_text = match self.import_format {
                    ImportFormat::Json => "Paste flow JSON here...",
                    ImportFormat::GstLaunch => "videotestsrc ! videoconvert ! autovideosink",
                };

                egui::ScrollArea::vertical()
                    .max_height(280.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.import_json_buffer)
                                .desired_width(f32::INFINITY)
                                .desired_rows(12)
                                .font(egui::TextStyle::Monospace)
                                .hint_text(hint_text),
                        );
                    });

                // Show error if any
                if let Some(ref error) = self.import_error {
                    ui.add_space(5.0);
                    ui.colored_label(Color32::RED, error);
                }

                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if ui.button("📥 Import").clicked() {
                        match self.import_format {
                            ImportFormat::Json => self.import_flow_from_json(ctx),
                            ImportFormat::GstLaunch => self.import_flow_from_gst_launch(ctx),
                        }
                    }

                    if ui.button("Cancel").clicked() {
                        self.show_import_dialog = false;
                        self.import_json_buffer.clear();
                        self.import_error = None;
                    }
                });
            });
    }

    /// Import a flow from the JSON buffer.
    /// Note: The backend's create_flow only takes a name, so we create first then update.
    pub(crate) fn import_flow_from_json(&mut self, ctx: &Context) {
        if self.import_json_buffer.trim().is_empty() {
            self.import_error = Some("Please paste flow JSON first".to_string());
            return;
        }

        // Try to parse the JSON as a Flow
        match serde_json::from_str::<Flow>(&self.import_json_buffer) {
            Ok(flow) => {
                // Regenerate all IDs to avoid conflicts
                let flow = Self::regenerate_flow_ids(flow);

                let api = self.api.clone();
                let tx = self.channels.sender();
                let ctx = ctx.clone();
                let flow_name = flow.name.clone();

                self.status = format!("Importing flow '{}'...", flow_name);
                self.show_import_dialog = false;
                self.import_json_buffer.clear();
                self.import_error = None;

                spawn_task(async move {
                    // Step 1: Create an empty flow with the name
                    match api.create_flow(&flow).await {
                        Ok(created_flow) => {
                            tracing::info!(
                                "Empty flow created: {} ({}), now updating with content...",
                                created_flow.name,
                                created_flow.id
                            );

                            // Step 2: Update the created flow with the full content
                            let mut full_flow = flow.clone();
                            full_flow.id = created_flow.id;
                            let flow_id = created_flow.id;

                            match api.update_flow(&full_flow).await {
                                Ok(_) => {
                                    tracing::info!(
                                        "Flow imported successfully: {} - WebSocket event will trigger refresh",
                                        flow_name
                                    );
                                    let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                        "Flow '{}' imported",
                                        flow_name
                                    )));
                                    // Navigate to imported flow
                                    let _ = tx.send(AppMessage::FlowCreated(flow_id));
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to update imported flow with content: {}",
                                        e
                                    );
                                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                                        "Failed to import flow: {}",
                                        e
                                    )));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to create flow for import: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to import flow: {}",
                                e
                            )));
                        }
                    }
                    ctx.request_repaint();
                });
            }
            Err(e) => {
                self.import_error = Some(format!("Invalid JSON: {}", e));
            }
        }
    }

    /// Import a flow from gst-launch-1.0 syntax.
    /// Parses the pipeline using the backend's GStreamer parser and creates a new flow.
    pub(crate) fn import_flow_from_gst_launch(&mut self, ctx: &Context) {
        let pipeline = self.import_json_buffer.trim();
        if pipeline.is_empty() {
            self.import_error = Some("Please enter a gst-launch pipeline".to_string());
            return;
        }

        // Strip leading "gst-launch-1.0 " if present
        let pipeline = pipeline
            .strip_prefix("gst-launch-1.0 ")
            .or_else(|| pipeline.strip_prefix("gst-launch "))
            .unwrap_or(pipeline)
            .to_string();

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Parsing gst-launch pipeline...".to_string();
        self.show_import_dialog = false;
        self.import_json_buffer.clear();
        self.import_error = None;

        spawn_task(async move {
            // Step 1: Parse the pipeline using the backend
            match api.parse_gst_launch(&pipeline).await {
                Ok(parsed) => {
                    if parsed.elements.is_empty() {
                        let _ = tx.send(AppMessage::FlowOperationError(
                            "No elements found in pipeline".to_string(),
                        ));
                        ctx.request_repaint();
                        return;
                    }

                    // Step 2: Create a new flow with a name based on first element
                    // Add random suffix to make each import unique
                    let unique_id = &uuid::Uuid::new_v4().to_string()[..8];
                    let flow_name = format!(
                        "Imported: {} ({})",
                        parsed
                            .elements
                            .first()
                            .map(|e| e.element_type.as_str())
                            .unwrap_or("pipeline"),
                        unique_id
                    );

                    let mut new_flow = Flow::new(&flow_name);
                    new_flow.elements = parsed.elements;
                    new_flow.links = parsed.links;

                    // Save the original gst-launch syntax in the description
                    new_flow.properties.description = Some(format!(
                        "Imported from gst-launch-1.0:\n\n```\n{}\n```",
                        pipeline
                    ));

                    // Step 3: Create the flow via API
                    match api.create_flow(&new_flow).await {
                        Ok(created_flow) => {
                            tracing::info!(
                                "Flow created from gst-launch: {} ({})",
                                created_flow.name,
                                created_flow.id
                            );

                            // Step 4: Update with the parsed content
                            let mut full_flow = new_flow.clone();
                            full_flow.id = created_flow.id;
                            let flow_id = created_flow.id;

                            match api.update_flow(&full_flow).await {
                                Ok(_) => {
                                    tracing::info!(
                                        "Flow imported from gst-launch successfully: {}",
                                        flow_name
                                    );
                                    let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                        "Flow '{}' imported from gst-launch",
                                        flow_name
                                    )));
                                    let _ = tx.send(AppMessage::FlowCreated(flow_id));
                                }
                                Err(e) => {
                                    tracing::error!("Failed to update imported flow: {}", e);
                                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                                        "Failed to import flow: {}",
                                        e
                                    )));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to create flow from gst-launch: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to create flow: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to parse gst-launch pipeline: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to parse pipeline: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Regenerate all IDs in a flow (flow ID, element IDs, block IDs) and update links.
    /// This is used for both import and copy operations to avoid ID conflicts.
    pub(crate) fn regenerate_flow_ids(mut flow: Flow) -> Flow {
        use std::collections::HashMap;

        // Generate new flow ID
        flow.id = uuid::Uuid::new_v4();

        // Reset state to Null
        flow.state = Some(PipelineState::Null);

        // Clear auto_restart flag
        flow.properties.auto_restart = false;

        // Clear runtime data (e.g., SDP for AES67 blocks)
        for block in &mut flow.blocks {
            block.runtime_data = None;
        }

        // Build mapping of old IDs to new IDs for elements
        let mut element_id_map: HashMap<String, String> = HashMap::new();
        for element in &mut flow.elements {
            let old_id = element.id.clone();
            let new_id = format!("e{}", uuid::Uuid::new_v4().simple());
            element_id_map.insert(old_id, new_id.clone());
            element.id = new_id;
        }

        // Build mapping of old IDs to new IDs for blocks
        let mut block_id_map: HashMap<String, String> = HashMap::new();
        for block in &mut flow.blocks {
            let old_id = block.id.clone();
            let new_id = format!("b{}", uuid::Uuid::new_v4().simple());
            block_id_map.insert(old_id, new_id.clone());
            block.id = new_id;
        }

        // Update links to use new IDs
        for link in &mut flow.links {
            // Update 'from' reference (format: "element_id:pad_name")
            if let Some((old_id, pad_name)) = link.from.split_once(':') {
                if let Some(new_id) = element_id_map.get(old_id) {
                    link.from = format!("{}:{}", new_id, pad_name);
                } else if let Some(new_id) = block_id_map.get(old_id) {
                    link.from = format!("{}:{}", new_id, pad_name);
                }
            }

            // Update 'to' reference (format: "element_id:pad_name")
            if let Some((old_id, pad_name)) = link.to.split_once(':') {
                if let Some(new_id) = element_id_map.get(old_id) {
                    link.to = format!("{}:{}", new_id, pad_name);
                } else if let Some(new_id) = block_id_map.get(old_id) {
                    link.to = format!("{}:{}", new_id, pad_name);
                }
            }
        }

        flow
    }

    /// Copy a flow with regenerated IDs and create it on the backend.
    /// Note: The backend's create_flow only takes a name, so we create first then update.
    pub(crate) fn copy_flow(&mut self, flow: &Flow, ctx: &Context) {
        let mut flow_copy = flow.clone();

        // Add " (copy)" suffix to the name
        flow_copy.name = format!("{} (copy)", flow.name);

        // Regenerate all IDs
        let flow_copy = Self::regenerate_flow_ids(flow_copy);

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();
        let flow_name = flow_copy.name.clone();

        self.status = format!("Copying flow '{}'...", flow.name);

        spawn_task(async move {
            // Step 1: Create an empty flow with the name
            match api.create_flow(&flow_copy).await {
                Ok(created_flow) => {
                    tracing::info!(
                        "Empty flow created: {} ({}), now updating with content...",
                        created_flow.name,
                        created_flow.id
                    );

                    // Step 2: Update the created flow with the full content
                    // Use the ID from the created flow
                    let mut full_flow = flow_copy.clone();
                    full_flow.id = created_flow.id;
                    let flow_id = created_flow.id;

                    match api.update_flow(&full_flow).await {
                        Ok(_) => {
                            tracing::info!(
                                "Flow copied successfully: {} - WebSocket event will trigger refresh",
                                flow_name
                            );
                            let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                "Flow '{}' created",
                                flow_name
                            )));
                            // Navigate to copied flow
                            let _ = tx.send(AppMessage::FlowCreated(flow_id));
                        }
                        Err(e) => {
                            tracing::error!("Failed to update copied flow with content: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to copy flow: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create flow for copy: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to copy flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Render the full-screen disconnect overlay when WebSocket is not connected.
    pub(crate) fn render_disconnect_overlay(&mut self, ctx: &Context) {
        use crate::state::ConnectionState;

        CentralPanel::default().show(ctx, |ui| {
            // Center everything vertically and horizontally
            ui.vertical_centered(|ui| {
                // Add vertical spacing to center content
                let available_height = ui.available_height();
                ui.add_space(available_height * 0.35);

                // Show large icon and status based on connection state
                match &self.connection_state {
                    ConnectionState::Disconnected => {
                        ui.heading("⚠️");
                        ui.add_space(10.0);
                        ui.heading("Connection Lost");
                        ui.add_space(10.0);
                        ui.label("Attempting to reconnect to the backend...");
                        ui.add_space(20.0);
                        ui.spinner();
                    }
                    ConnectionState::Reconnecting { attempt } => {
                        ui.heading("⏳");
                        ui.add_space(10.0);
                        ui.heading("Reconnecting...");
                        ui.add_space(10.0);
                        ui.label(format!(
                            "Attempt {} to reconnect to the backend...",
                            attempt
                        ));
                        ui.add_space(20.0);
                        ui.spinner();
                    }
                    ConnectionState::Connected => {
                        // This shouldn't happen, but handle it gracefully
                        ui.heading("✓ Connected");
                    }
                }

                // Show connection details
                ui.add_space(30.0);
                ui.label(format!("Backend: {}", self.api.base_url()));

                // Show error if any
                if let Some(ref error) = self.error {
                    ui.add_space(10.0);
                    ui.colored_label(Color32::from_rgb(255, 100, 100), error);
                }
            });
        });
    }
}
