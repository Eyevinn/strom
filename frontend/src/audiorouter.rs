//! Audio Router routing matrix editor.

use egui::{Color32, ScrollArea, Ui};
use std::collections::HashMap;
use strom_types::{BlockDefinition, BlockInstance, FlowId, PropertyValue};

/// Routing matrix editor for Audio Router blocks.
pub struct RoutingMatrixEditor {
    /// Flow ID this editor is for
    pub flow_id: FlowId,
    /// Block ID this editor is for
    pub block_id: String,
    /// Whether the editor window is open
    pub open: bool,
    /// Current routing matrix (source -> destinations)
    pub routing: HashMap<String, Vec<String>>,
    /// Whether we need to save changes
    pub dirty: bool,
    /// Cached config
    pub num_inputs: usize,
    pub num_outputs: usize,
    pub input_channels: Vec<usize>,
    pub output_channels: Vec<usize>,
    /// Currently selected output tab
    pub selected_output: usize,
}

impl RoutingMatrixEditor {
    pub fn new(flow_id: FlowId, block_id: String) -> Self {
        Self {
            flow_id,
            block_id,
            open: true,
            routing: HashMap::new(),
            dirty: false,
            num_inputs: 2,
            num_outputs: 2,
            input_channels: vec![2, 2],
            output_channels: vec![2, 2],
            selected_output: 0,
        }
    }

    /// Load configuration from block instance.
    pub fn load_from_block(&mut self, block: &BlockInstance, definition: &BlockDefinition) {
        // Helper to get property value
        let get_uint = |name: &str| -> usize {
            block
                .properties
                .get(name)
                .and_then(|v| match v {
                    PropertyValue::UInt(u) => Some(*u as usize),
                    PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
                    _ => None,
                })
                .or_else(|| {
                    definition
                        .exposed_properties
                        .iter()
                        .find(|p| p.name == name)
                        .and_then(|p| p.default_value.as_ref())
                        .and_then(|v| match v {
                            PropertyValue::UInt(u) => Some(*u as usize),
                            PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
                            _ => None,
                        })
                })
                .unwrap_or(2)
        };

        self.num_inputs = get_uint("num_inputs").clamp(1, 8);
        self.num_outputs = get_uint("num_outputs").clamp(1, 8);

        self.input_channels = (0..self.num_inputs)
            .map(|i| get_uint(&format!("input_{}_channels", i)).clamp(1, 64))
            .collect();

        self.output_channels = (0..self.num_outputs)
            .map(|i| get_uint(&format!("output_{}_channels", i)).clamp(1, 64))
            .collect();

        // Parse routing matrix
        let routing_json = block
            .properties
            .get("routing_matrix")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "{}".to_string());

        tracing::debug!("load_from_block: routing_json = {}", routing_json);
        tracing::debug!(
            "load_from_block: num_inputs={}, num_outputs={}",
            self.num_inputs,
            self.num_outputs
        );
        tracing::debug!(
            "load_from_block: input_channels={:?}, output_channels={:?}",
            self.input_channels,
            self.output_channels
        );

        self.routing = serde_json::from_str(&routing_json).unwrap_or_default();
        tracing::debug!(
            "load_from_block: parsed {} routing entries",
            self.routing.len()
        );

        // Clean up invalid entries
        self.cleanup_routing();
        tracing::debug!(
            "load_from_block: after cleanup {} routing entries",
            self.routing.len()
        );
        for (src, dests) in &self.routing {
            tracing::debug!("  {} -> {:?}", src, dests);
        }

        self.dirty = false;
        self.selected_output = 0;
    }

    /// Remove routing entries that reference non-existent inputs/outputs.
    fn cleanup_routing(&mut self) {
        // Build set of valid source keys
        let valid_src_keys: std::collections::HashSet<String> = (0..self.num_inputs)
            .flat_map(|in_idx| {
                (0..self.input_channels[in_idx]).map(move |in_ch| format!("i{}c{}", in_idx, in_ch))
            })
            .collect();

        // Build set of valid destination keys
        let valid_dest_keys: std::collections::HashSet<String> = (0..self.num_outputs)
            .flat_map(|out_idx| {
                (0..self.output_channels[out_idx])
                    .map(move |out_ch| format!("o{}c{}", out_idx, out_ch))
            })
            .collect();

        tracing::debug!("cleanup_routing: valid_src_keys = {:?}", valid_src_keys);
        tracing::debug!("cleanup_routing: valid_dest_keys = {:?}", valid_dest_keys);

        // Remove invalid source keys
        let src_keys: Vec<String> = self.routing.keys().cloned().collect();
        for src_key in src_keys {
            if !valid_src_keys.contains(&src_key) {
                self.routing.remove(&src_key);
            } else if let Some(dests) = self.routing.get_mut(&src_key) {
                // Remove invalid destination keys
                dests.retain(|d| valid_dest_keys.contains(d));
                if dests.is_empty() {
                    self.routing.remove(&src_key);
                }
            }
        }
    }

    /// Get the routing matrix as JSON string.
    pub fn get_routing_json(&self) -> String {
        serde_json::to_string(&self.routing).unwrap_or_else(|_| "{}".to_string())
    }

    /// Show the routing matrix editor window.
    /// Returns Some(routing_json) if the user clicked Save.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<String> {
        if !self.open {
            return None;
        }

        let mut result = None;
        let mut should_close = false;
        let mut set_diagonal = false;
        let mut clear_all = false;
        let mut clear_output = false;

        let total_in_ch: usize = self.input_channels.iter().sum();
        let total_out_ch: usize = self.output_channels.iter().sum();

        // Create window ID before creating window
        let window_id = egui::Id::new(format!(
            "routing_matrix_editor_{}_{}",
            self.flow_id, self.block_id
        ));

        // Cache values needed in closure
        let num_inputs = self.num_inputs;
        let num_outputs = self.num_outputs;
        let input_channels = self.input_channels.clone();
        let output_channels = self.output_channels.clone();
        let dirty = self.dirty;
        let selected_output = self.selected_output;

        let mut open = self.open;
        let mut new_selected_output = selected_output;

        egui::Window::new("🔀 Routing Matrix")
            .id(window_id)
            .open(&mut open)
            .default_width(500.0)
            .default_height(450.0)
            .min_width(250.0)
            .min_height(200.0)
            .resizable(true)
            .show(ctx, |ui| {
                // Header with info
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} inputs ({} ch) → {} outputs ({} ch)",
                        num_inputs, total_in_ch, num_outputs, total_out_ch
                    ));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if dirty {
                            ui.colored_label(Color32::from_rgb(255, 200, 100), "• Unsaved");
                        }
                    });
                });

                ui.separator();

                // Output tabs
                ui.horizontal(|ui| {
                    for (out_idx, &num_ch) in output_channels.iter().enumerate().take(num_outputs) {
                        let label = format!("Out {} ({} ch)", out_idx, num_ch);
                        if ui
                            .selectable_label(selected_output == out_idx, label)
                            .clicked()
                        {
                            new_selected_output = out_idx;
                        }
                    }
                });

                ui.separator();

                // Quick action buttons
                ui.horizontal(|ui| {
                    if ui
                        .button("1:1 Diagonal")
                        .on_hover_text("Route input channels 1:1 to all outputs")
                        .clicked()
                    {
                        set_diagonal = true;
                    }
                    if ui
                        .button("Clear All")
                        .on_hover_text("Remove all routing")
                        .clicked()
                    {
                        clear_all = true;
                    }
                    if ui
                        .button(format!("Clear Out {}", selected_output))
                        .on_hover_text("Remove routing for this output only")
                        .clicked()
                    {
                        clear_output = true;
                    }
                });

                ui.add_space(4.0);

                // Matrix for selected output
                let out_ch_count = output_channels[selected_output];

                ScrollArea::both()
                    .id_salt(format!("routing_matrix_scroll_{}", selected_output))
                    .show(ui, |ui| {
                        Self::show_output_matrix(
                            ui,
                            &mut self.routing,
                            &mut self.dirty,
                            num_inputs,
                            selected_output,
                            &input_channels,
                            out_ch_count,
                        );
                    });

                ui.add_space(4.0);
                ui.separator();

                // Save/Cancel buttons
                ui.horizontal(|ui| {
                    if ui.button("💾 Save").clicked() {
                        let json = serde_json::to_string(&self.routing)
                            .unwrap_or_else(|_| "{}".to_string());
                        tracing::debug!("Saving routing matrix: {}", json);
                        result = Some(json);
                        self.dirty = false;
                    }
                    if ui.button("Cancel").clicked() {
                        should_close = true;
                    }
                });
            });

        self.open = open;
        self.selected_output = new_selected_output;

        // Apply deferred actions
        if set_diagonal {
            self.set_diagonal_routing();
            self.dirty = true;
        }
        if clear_all {
            self.routing.clear();
            self.dirty = true;
        }
        if clear_output {
            self.clear_output_routing(selected_output);
            self.dirty = true;
        }
        if should_close {
            self.open = false;
        }

        result
    }

    /// Show the matrix for a single output with compact checkboxes.
    fn show_output_matrix(
        ui: &mut Ui,
        routing: &mut HashMap<String, Vec<String>>,
        dirty: &mut bool,
        num_inputs: usize,
        out_idx: usize,
        input_channels: &[usize],
        out_ch_count: usize,
    ) {
        const CHECKBOX_SIZE: f32 = 18.0;
        const ROW_LABEL_WIDTH: f32 = 50.0;

        // Channel number headers
        ui.horizontal(|ui| {
            ui.add_space(ROW_LABEL_WIDTH);
            for out_ch in 0..out_ch_count {
                ui.allocate_ui_with_layout(
                    egui::vec2(CHECKBOX_SIZE, 14.0),
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        ui.label(egui::RichText::new(format!("{}", out_ch)).small().strong());
                    },
                );
            }
        });

        ui.add_space(2.0);

        // Data rows - grouped by input
        for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
            // Input group header
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("In {} ({} ch)", in_idx, in_ch_count))
                        .small()
                        .strong(),
                );
            });

            for in_ch in 0..in_ch_count {
                ui.horizontal(|ui| {
                    // Row label
                    ui.add_sized(
                        [ROW_LABEL_WIDTH, CHECKBOX_SIZE],
                        egui::Label::new(egui::RichText::new(format!(" Ch {}", in_ch)).small()),
                    );

                    let src_key = format!("i{}c{}", in_idx, in_ch);

                    // Checkboxes for each output channel
                    for out_ch in 0..out_ch_count {
                        let dest_key = format!("o{}c{}", out_idx, out_ch);

                        let is_routed = routing
                            .get(&src_key)
                            .map(|dests| dests.contains(&dest_key))
                            .unwrap_or(false);

                        let mut checked = is_routed;

                        // Use unique ID for each checkbox to prevent state confusion
                        let checkbox_id = format!("cb_{}_{}", src_key, dest_key);
                        ui.push_id(&checkbox_id, |ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(CHECKBOX_SIZE, CHECKBOX_SIZE),
                                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                                |ui| {
                                    if ui.checkbox(&mut checked, "").changed() {
                                        *dirty = true;
                                        tracing::debug!(
                                            "Checkbox changed: {} -> {} = {}",
                                            src_key,
                                            dest_key,
                                            checked
                                        );
                                        if checked {
                                            routing
                                                .entry(src_key.clone())
                                                .or_default()
                                                .push(dest_key.clone());
                                        } else if let Some(dests) = routing.get_mut(&src_key) {
                                            dests.retain(|d| d != &dest_key);
                                            if dests.is_empty() {
                                                routing.remove(&src_key);
                                            }
                                        }
                                    }
                                },
                            );
                        });
                    }
                });
            }

            // Space between input groups
            if in_idx < num_inputs - 1 {
                ui.add_space(4.0);
            }
        }
    }

    /// Set 1:1 diagonal routing.
    fn set_diagonal_routing(&mut self) {
        self.routing.clear();
        let mut in_ch_global = 0;
        for in_idx in 0..self.num_inputs {
            for in_ch in 0..self.input_channels[in_idx] {
                let mut out_ch_global = 0;
                for out_idx in 0..self.num_outputs {
                    for out_ch in 0..self.output_channels[out_idx] {
                        if in_ch_global == out_ch_global {
                            let src_key = format!("i{}c{}", in_idx, in_ch);
                            let dest_key = format!("o{}c{}", out_idx, out_ch);
                            tracing::debug!("Diagonal routing: {} -> {}", src_key, dest_key);
                            self.routing.entry(src_key).or_default().push(dest_key);
                        }
                        out_ch_global += 1;
                    }
                }
                in_ch_global += 1;
            }
        }
        tracing::debug!(
            "Diagonal routing complete. Total entries: {}",
            self.routing.len()
        );
        for (src, dests) in &self.routing {
            tracing::debug!("  {} -> {:?}", src, dests);
        }
    }

    /// Clear routing for a specific output only.
    fn clear_output_routing(&mut self, out_idx: usize) {
        let out_ch_count = self.output_channels[out_idx];

        // Build list of dest_keys to remove
        let dest_keys_to_remove: Vec<String> = (0..out_ch_count)
            .map(|out_ch| format!("o{}c{}", out_idx, out_ch))
            .collect();

        // Remove these destinations from all source entries
        let src_keys: Vec<String> = self.routing.keys().cloned().collect();
        for src_key in src_keys {
            if let Some(dests) = self.routing.get_mut(&src_key) {
                dests.retain(|d| !dest_keys_to_remove.contains(d));
                if dests.is_empty() {
                    self.routing.remove(&src_key);
                }
            }
        }
    }
}
