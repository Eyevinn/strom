use crate::state::{AppMessage, ConnectionState};
use egui::{CentralPanel, Color32, Context};
use strom_types::{Flow, PipelineState};

use super::ImportFormat;
use super::*;
impl StromApp {
    /// Render the import flow dialog.
    pub(super) fn render_import_dialog(&mut self, ctx: &Context) {
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
                    if ui.button(format!("{} Import", egui_phosphor::regular::DOWNLOAD_SIMPLE)).clicked() {
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
    pub(super) fn import_flow_from_json(&mut self, ctx: &Context) {
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
    pub(super) fn import_flow_from_gst_launch(&mut self, ctx: &Context) {
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
    pub(super) fn regenerate_flow_ids(mut flow: Flow) -> Flow {
        use std::collections::HashMap;

        // Generate new flow ID
        flow.id = uuid::Uuid::new_v4();

        // Reset state to Null
        flow.set_gst_state(Some(PipelineState::Null));

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
    pub(super) fn copy_flow(&mut self, flow: &Flow, ctx: &Context) {
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
    pub(super) fn render_disconnect_overlay(&mut self, ctx: &Context) {
        CentralPanel::default().show(ctx, |ui| {
            // Center everything vertically and horizontally
            ui.vertical_centered(|ui| {
                // Add vertical spacing to center content
                let available_height = ui.available_height();
                ui.add_space(available_height * 0.35);

                // Show large icon and status based on connection state
                match self.connection_state {
                    ConnectionState::Disconnected => {
                        ui.heading(
                            egui::RichText::new(egui_phosphor::regular::WARNING)
                                .size(80.0)
                                .color(Color32::from_rgb(255, 165, 0))
                        );
                        ui.add_space(20.0);
                        ui.heading(
                            egui::RichText::new("Disconnected from Backend")
                                .size(32.0)
                                .color(Color32::from_rgb(200, 200, 200))
                        );
                    }
                    ConnectionState::Reconnecting { attempt } => {
                        // Animated spinner
                        ui.add(egui::Spinner::new().size(80.0));
                        ui.add_space(20.0);
                        ui.heading(
                            egui::RichText::new(format!("Reconnecting (Attempt {})", attempt))
                                .size(32.0)
                                .color(Color32::from_rgb(200, 200, 200))
                        );
                    }
                    ConnectionState::Connected => {
                        // Should not reach here, but just in case
                        ui.heading(
                            egui::RichText::new(egui_phosphor::regular::CHECK)
                                .size(80.0)
                                .color(Color32::from_rgb(0, 200, 0))
                        );
                        ui.add_space(20.0);
                        ui.heading(
                            egui::RichText::new("Connected")
                                .size(32.0)
                                .color(Color32::from_rgb(200, 200, 200))
                        );
                    }
                }

                ui.add_space(15.0);
                ui.label(
                    egui::RichText::new("Please wait while we attempt to reconnect to the Strom backend...")
                        .size(16.0)
                        .color(Color32::from_rgb(150, 150, 150))
                );

                ui.add_space(20.0);

                // Reconnect now button - reloads the page to force fresh WebSocket connection
                if ui.button(egui::RichText::new("Reconnect Now").size(18.0)).clicked() {
                    #[cfg(target_arch = "wasm32")]
                    {
                        if let Some(window) = web_sys::window() {
                            let _ = window.location().reload();
                        }
                    }
                }

                ui.add_space(20.0);
                ui.separator();
                ui.add_space(10.0);

                // Show connection details
                ui.label(
                    egui::RichText::new("The application will automatically reconnect when the backend is available.")
                        .size(14.0)
                        .color(Color32::from_rgb(120, 120, 120))
                );
            });
        });
    }
}
