//! Audio Router routing matrix editor.

use egui::{Color32, ScrollArea, Ui};
use strom_types::routing::{self, Crosspoint, RoutingGains};
use strom_types::{BlockDefinition, BlockInstance, FlowId, PropertyValue};

/// Whether a block should get the routing matrix editor.
///
/// Keyed on the `routing_matrix` property rather than on block ids, so a new
/// router block does not have to touch the call sites that gate the editor.
pub fn has_routing_matrix(definition: &BlockDefinition) -> bool {
    definition
        .exposed_properties
        .iter()
        .any(|p| p.name == "routing_matrix")
}

/// One routing change leaving the editor.
pub struct RoutingUpdate {
    /// The routing matrix as JSON.
    pub json: String,
    /// Whether the caller should also write the flow to storage. False in live
    /// mode: the block-property endpoint persists the value itself, and saving
    /// the whole flow on every drag would be a lot of write traffic.
    pub persist: bool,
    /// Whether this block's routing can be written to a running pipeline.
    /// False for `builtin.audiorouter`, whose routing is topology: sending it
    /// would be a wasted round-trip that the backend rejects.
    pub live: bool,
}

/// Whether this block's routing can be changed on a running flow.
///
/// Read off the property's own `live` flag rather than a block id, so
/// `builtin.audiorouter` — whose routing is topology and needs a restart —
/// simply never offers live mode.
pub fn has_live_routing(definition: &BlockDefinition) -> bool {
    definition
        .exposed_properties
        .iter()
        .find(|p| p.name == "routing_matrix")
        .map(|p| p.live)
        .unwrap_or(false)
}

/// Whether a block's crosspoints carry a gain rather than just on/off.
///
/// Keyed on the fade property the gain-capable router exposes, not on a block
/// id, for the same reason `has_routing_matrix` is.
pub fn has_crosspoint_gain(definition: &BlockDefinition) -> bool {
    definition
        .exposed_properties
        .iter()
        .any(|p| p.name == "crosspoint_fade_ms")
}

/// Routing matrix editor for Audio Router blocks.
pub struct RoutingMatrixEditor {
    /// Flow ID this editor is for
    pub flow_id: FlowId,
    /// Block ID this editor is for
    pub block_id: String,
    /// Whether the editor window is open
    pub open: bool,
    /// Current routing matrix (source -> destinations)
    /// Crosspoint gains. The wire format lives in `strom_types::routing`,
    /// shared with the backend blocks that read the same JSON.
    pub routing: RoutingGains,
    /// Whether the block's crosspoints carry a gain rather than just on/off.
    supports_gain: bool,
    /// Whether this block's routing can be changed on a running flow.
    live_capable: bool,
    /// Send every change straight to the running flow instead of waiting for
    /// Save. The write persists as well, so in this mode Save is redundant.
    live_apply: bool,
    /// A change has been made that live mode has not sent yet.
    live_pending: bool,
    /// Whether we need to save changes
    pub dirty: bool,
    /// Cached config
    pub num_inputs: usize,
    pub num_outputs: usize,
    pub input_channels: Vec<usize>,
    pub output_channels: Vec<usize>,
    /// Currently selected output tab
    pub selected_output: usize,
    /// Flip rows/columns in matrix display
    pub flip_layout: bool,
}

impl RoutingMatrixEditor {
    pub fn new(flow_id: FlowId, block_id: String) -> Self {
        Self {
            flow_id,
            block_id,
            open: true,
            routing: RoutingGains::new(),
            supports_gain: false,
            live_capable: false,
            live_apply: true,
            live_pending: false,
            dirty: false,
            num_inputs: 2,
            num_outputs: 2,
            input_channels: vec![2, 2],
            output_channels: vec![2, 2],
            selected_output: 0,
            flip_layout: false,
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

        self.supports_gain = has_crosspoint_gain(definition);
        self.live_capable = has_live_routing(definition);
        let (gains, skipped) = routing::parse_routing_gains(&routing_json);
        for key in &skipped {
            tracing::warn!("Routing matrix: unusable entry {key}");
        }
        self.routing = gains;
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
        for (crosspoint, gain) in &self.routing {
            tracing::debug!(
                "  {} -> {} @ {gain}",
                crosspoint.source_key(),
                crosspoint.destination_key()
            );
        }

        self.dirty = false;
        self.selected_output = 0;
    }

    /// Drop crosspoints that reference channels the block does not have.
    /// Keyed by `Crosspoint`, this is one bounds check rather than two key sets.
    fn cleanup_routing(&mut self) {
        let inputs = self.input_channels.clone();
        let outputs = self.output_channels.clone();
        let before = self.routing.len();
        self.routing.retain(|c, _| {
            inputs.get(c.in_stream).is_some_and(|ch| c.in_channel < *ch)
                && outputs
                    .get(c.out_stream)
                    .is_some_and(|ch| c.out_channel < *ch)
        });
        if self.routing.len() != before {
            tracing::debug!(
                "cleanup_routing: dropped {} crosspoint(s) outside the configured channels",
                before - self.routing.len()
            );
        }
    }

    /// Show the routing matrix editor window.
    /// Returns Some(routing_json) if the user clicked Save.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<RoutingUpdate> {
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
        let routing_before = routing::serialize_routing_gains(&self.routing);
        let supports_gain = self.supports_gain;
        let live_capable = self.live_capable;
        let mut live_apply = self.live_apply;
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
                // Header with info and action buttons
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} inputs ({} ch) -> {} outputs ({} ch)",
                        num_inputs, total_in_ch, num_outputs, total_out_ch
                    ));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if live_capable {
                            ui.checkbox(&mut live_apply, "Live").on_hover_text(
                                "Apply every change to the running flow as you make it, \
                                     fading each crosspoint. The change is saved too, so \
                                     Save is not needed.",
                            );
                            ui.add_space(8.0);
                        }
                        if dirty && !(live_capable && live_apply) {
                            ui.colored_label(Color32::from_rgb(255, 200, 100), "• Unsaved");
                            ui.add_space(8.0);
                        }
                        if ui
                            .small_button(format!("Clear Out {}", selected_output))
                            .on_hover_text("Remove routing for this output only")
                            .clicked()
                        {
                            clear_output = true;
                        }
                        if ui
                            .small_button("Clear All")
                            .on_hover_text("Remove all routing")
                            .clicked()
                        {
                            clear_all = true;
                        }
                        if ui
                            .small_button("1:1 Diagonal")
                            .on_hover_text("Route input channels 1:1 to all outputs")
                            .clicked()
                        {
                            set_diagonal = true;
                        }
                    });
                });

                ui.separator();

                // Output tabs and layout toggle
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

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.checkbox(&mut self.flip_layout, "Inputs as columns")
                            .on_hover_text(
                                "Swap rows and columns: inputs become columns, outputs become rows",
                            );
                    });
                });

                ui.separator();

                // Matrix for selected output
                let out_ch_count = output_channels[selected_output];
                let flip = self.flip_layout;

                ScrollArea::both()
                    .id_salt(format!("routing_matrix_scroll_{}", selected_output))
                    .show(ui, |ui| {
                        Self::show_output_matrix(
                            ui,
                            &mut self.routing,
                            &mut self.dirty,
                            supports_gain,
                            num_inputs,
                            selected_output,
                            &input_channels,
                            out_ch_count,
                            flip,
                        );
                    });

                ui.add_space(4.0);
                ui.separator();

                // Save/Cancel buttons
                ui.horizontal(|ui| {
                    if live_capable && live_apply {
                        ui.label(
                            egui::RichText::new("Changes apply and save as you make them")
                                .weak()
                                .small(),
                        );
                    } else if ui
                        .button(format!("{} Save", egui_phosphor::regular::FLOPPY_DISK))
                        .clicked()
                    {
                        let json = routing::serialize_routing_gains(&self.routing);
                        tracing::debug!("Saving routing matrix: {}", json);
                        result = Some(RoutingUpdate {
                            json,
                            persist: true,
                            live: live_capable,
                        });
                        self.dirty = false;
                    }
                    if ui.button("Cancel").clicked() {
                        should_close = true;
                    }
                });
            });

        self.open = open;
        self.selected_output = new_selected_output;

        self.live_apply = live_apply;

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

        // In live mode every change goes straight out. The write persists too
        // (block properties persist by default), so Save has nothing left to
        // do and `dirty` is cleared here rather than by a button.
        //
        // Change is detected by value: the grid, the bulk buttons and the gain
        // drags all mutate `self.routing` through different paths, and one
        // comparison covers every one of them.
        let after = routing::serialize_routing_gains(&self.routing);
        if after != routing_before {
            self.live_pending = true;
            self.dirty = true;
        }
        if self.live_capable && self.live_apply && self.live_pending {
            self.live_pending = false;
            self.dirty = false;
            result = Some(RoutingUpdate {
                json: after,
                persist: false,
                live: true,
            });
        }

        result
    }

    /// Show the matrix for a single output with compact checkboxes.
    /// When flip=false: rows=inputs, columns=outputs
    /// When flip=true: rows=outputs, columns=inputs
    #[allow(clippy::too_many_arguments)]
    fn show_output_matrix(
        ui: &mut Ui,
        routing: &mut RoutingGains,
        dirty: &mut bool,
        supports_gain: bool,
        num_inputs: usize,
        out_idx: usize,
        input_channels: &[usize],
        out_ch_count: usize,
        flip: bool,
    ) {
        const CHECKBOX_SIZE: f32 = 16.0;
        const ROW_LABEL_WIDTH: f32 = 50.0;

        if flip {
            // Flipped: rows = output channels, columns = input channels
            Self::show_matrix_flipped(
                ui,
                routing,
                dirty,
                supports_gain,
                num_inputs,
                out_idx,
                input_channels,
                out_ch_count,
                CHECKBOX_SIZE,
                ROW_LABEL_WIDTH,
            );
        } else {
            // Normal: rows = input channels, columns = output channels
            Self::show_matrix_normal(
                ui,
                routing,
                dirty,
                supports_gain,
                num_inputs,
                out_idx,
                input_channels,
                out_ch_count,
                CHECKBOX_SIZE,
                ROW_LABEL_WIDTH,
            );
        }
    }

    /// Normal layout: rows = inputs, columns = outputs (using Grid for proper alignment)
    #[allow(clippy::too_many_arguments)]
    fn show_matrix_normal(
        ui: &mut Ui,
        routing: &mut RoutingGains,
        dirty: &mut bool,
        supports_gain: bool,
        num_inputs: usize,
        out_idx: usize,
        input_channels: &[usize],
        out_ch_count: usize,
        checkbox_size: f32,
        _row_label_width: f32,
    ) {
        egui::Grid::new(format!("routing_matrix_normal_{}", out_idx))
            .min_col_width(checkbox_size)
            .spacing([2.0, 4.0])
            .show(ui, |ui| {
                // Header row: empty corner + output channel numbers
                ui.label(""); // Empty corner cell
                for out_ch in 0..out_ch_count {
                    ui.allocate_ui_with_layout(
                        egui::vec2(checkbox_size, 14.0),
                        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                        |ui| {
                            ui.label(egui::RichText::new(format!("{}", out_ch)).small().strong());
                        },
                    );
                }
                ui.end_row();

                // Data rows - grouped by input
                for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
                    // Input group header row
                    ui.label(
                        egui::RichText::new(format!("In {}", in_idx))
                            .small()
                            .strong(),
                    );
                    for _ in 0..out_ch_count {
                        ui.label("");
                    }
                    ui.end_row();

                    // Channel rows
                    for in_ch in 0..in_ch_count {
                        ui.label(egui::RichText::new(format!("  {}", in_ch)).small());

                        for out_ch in 0..out_ch_count {
                            Self::show_crosspoint_grid(
                                ui,
                                routing,
                                dirty,
                                supports_gain,
                                Crosspoint::new(in_idx, in_ch, out_idx, out_ch),
                                checkbox_size,
                            );
                        }
                        ui.end_row();
                    }

                    // Separator row between input groups
                    if in_idx < num_inputs - 1 {
                        ui.label("");
                        ui.end_row();
                    }
                }
            });
    }

    /// Flipped layout: rows = outputs, columns = inputs (using Grid for proper alignment)
    #[allow(clippy::too_many_arguments)]
    fn show_matrix_flipped(
        ui: &mut Ui,
        routing: &mut RoutingGains,
        dirty: &mut bool,
        supports_gain: bool,
        num_inputs: usize,
        out_idx: usize,
        input_channels: &[usize],
        out_ch_count: usize,
        checkbox_size: f32,
        _row_label_width: f32,
    ) {
        egui::Grid::new(format!("routing_matrix_flipped_{}", out_idx))
            .min_col_width(checkbox_size)
            .spacing([2.0, 4.0])
            .show(ui, |ui| {
                // Header row 1: Input group labels at start of each group
                ui.label(""); // Empty corner cell
                for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
                    ui.label(
                        egui::RichText::new(format!("In {}", in_idx))
                            .small()
                            .strong(),
                    );
                    for _ in 1..in_ch_count {
                        ui.label("");
                    }
                    // Separator column between input groups
                    if in_idx < num_inputs - 1 {
                        ui.label("");
                    }
                }
                ui.end_row();

                // Header row 2: Channel numbers
                ui.label(""); // Empty corner cell
                for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
                    for in_ch in 0..in_ch_count {
                        ui.allocate_ui_with_layout(
                            egui::vec2(checkbox_size, 14.0),
                            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                            |ui| {
                                ui.label(egui::RichText::new(format!("{}", in_ch)).small());
                            },
                        );
                    }
                    // Separator column between input groups
                    if in_idx < num_inputs - 1 {
                        ui.label("");
                    }
                }
                ui.end_row();

                // Data rows - one per output channel
                for out_ch in 0..out_ch_count {
                    // Row label
                    ui.label(egui::RichText::new(format!("Out {}", out_ch)).small());

                    // Checkboxes for each input channel
                    for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs)
                    {
                        for in_ch in 0..in_ch_count {
                            Self::show_crosspoint_grid(
                                ui,
                                routing,
                                dirty,
                                supports_gain,
                                Crosspoint::new(in_idx, in_ch, out_idx, out_ch),
                                checkbox_size,
                            );
                        }
                        // Separator column between input groups
                        if in_idx < num_inputs - 1 {
                            ui.label("");
                        }
                    }
                    ui.end_row();
                }
            });
    }

    /// One crosspoint cell.
    ///
    /// The checkbox opens and closes the crosspoint. Where the block supports a
    /// gain, an open crosspoint also gets a compact dB control — dB rather than
    /// a raw coefficient because that is what the level meters next to it read
    /// in. Toggling a crosspoint off and on again returns it to unity; the gain
    /// is only remembered while it stays open.
    #[allow(clippy::too_many_arguments)]
    fn show_crosspoint_grid(
        ui: &mut Ui,
        routing: &mut RoutingGains,
        dirty: &mut bool,
        supports_gain: bool,
        crosspoint: Crosspoint,
        checkbox_size: f32,
    ) {
        let current = routing.get(&crosspoint).copied();
        let mut checked = current.is_some();

        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(checkbox_size, checkbox_size),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    if ui.checkbox(&mut checked, "").changed() {
                        *dirty = true;
                        if checked {
                            routing.insert(crosspoint, 1.0);
                        } else {
                            routing.remove(&crosspoint);
                        }
                    }
                },
            );

            if !supports_gain {
                return;
            }

            match current {
                Some(gain) => {
                    let mut db = routing::gain_to_db(gain);
                    let response = ui.add(
                        egui::DragValue::new(&mut db)
                            .speed(0.25)
                            .range(routing::GAIN_FLOOR_DB..=0.0)
                            .fixed_decimals(1)
                            .suffix(" dB"),
                    );
                    if response.changed() {
                        *dirty = true;
                        routing.insert(crosspoint, routing::db_to_gain(db));
                    }
                    response.on_hover_text("Crosspoint gain. Drag to trim this route.");
                }
                None => {
                    // Keep the columns aligned with the open crosspoints.
                    ui.add_enabled(false, egui::Label::new(egui::RichText::new("  –  ").weak()));
                }
            }
        });
    }

    /// Route input channels 1:1 onto every output, by global channel index.
    fn set_diagonal_routing(&mut self) {
        self.routing.clear();
        let mut in_global = 0;
        for in_idx in 0..self.num_inputs {
            for in_ch in 0..self.input_channels[in_idx] {
                let mut out_global = 0;
                for out_idx in 0..self.num_outputs {
                    for out_ch in 0..self.output_channels[out_idx] {
                        if in_global == out_global {
                            self.routing
                                .insert(Crosspoint::new(in_idx, in_ch, out_idx, out_ch), 1.0);
                        }
                        out_global += 1;
                    }
                }
                in_global += 1;
            }
        }
    }

    /// Close every crosspoint feeding one output stream.
    fn clear_output_routing(&mut self, out_idx: usize) {
        self.routing.retain(|c, _| c.out_stream != out_idx);
    }
}
