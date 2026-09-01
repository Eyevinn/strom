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
    /// Input streams the operator has folded away, by index. A large router is
    /// mostly rows you are not looking at; folding a stream hides its channel
    /// rows and leaves the group header, so the view can be shrunk to the
    /// streams in play without changing any routing.
    folded_inputs: std::collections::HashSet<usize>,
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
            folded_inputs: std::collections::HashSet::new(),
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
        // The same rule the builder uses: a matrix that was never configured
        // gets the straight-through default, so the grid shows what the flow
        // will actually run. Gated on live routing, so `builtin.audiorouter` —
        // whose builder has no such default — keeps showing an empty grid.
        self.routing = match block.properties.get("routing_matrix") {
            None if self.live_capable => {
                routing::default_routing(&self.input_channels, &self.output_channels)
            }
            _ => {
                let (gains, skipped) = routing::parse_routing_gains(&routing_json);
                for key in &skipped {
                    tracing::warn!("Routing matrix: unusable entry {key}");
                }
                gains
            }
        };
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
        let mut folded_inputs = self.folded_inputs.clone();
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
                        ui.add_space(8.0);
                        if ui
                            .small_button("Maximize")
                            .on_hover_text("Show every input's channels")
                            .clicked()
                        {
                            folded_inputs.clear();
                        }
                        if ui
                            .small_button("Minimize")
                            .on_hover_text("Fold every input down to its group header")
                            .clicked()
                        {
                            folded_inputs.extend(0..num_inputs);
                        }
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
                            &mut folded_inputs,
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
        self.folded_inputs = folded_inputs;

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
        folded_inputs: &mut std::collections::HashSet<usize>,
        num_inputs: usize,
        out_idx: usize,
        input_channels: &[usize],
        out_ch_count: usize,
        flip: bool,
    ) {
        const CELL_SIZE: f32 = 20.0;
        const ROW_LABEL_WIDTH: f32 = 50.0;

        if flip {
            // Flipped: rows = output channels, columns = input channels
            Self::show_matrix_flipped(
                ui,
                routing,
                dirty,
                supports_gain,
                folded_inputs,
                num_inputs,
                out_idx,
                input_channels,
                out_ch_count,
                CELL_SIZE,
                ROW_LABEL_WIDTH,
            );
        } else {
            // Normal: rows = input channels, columns = output channels
            Self::show_matrix_normal(
                ui,
                routing,
                dirty,
                supports_gain,
                folded_inputs,
                num_inputs,
                out_idx,
                input_channels,
                out_ch_count,
                CELL_SIZE,
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
        folded_inputs: &mut std::collections::HashSet<usize>,
        num_inputs: usize,
        out_idx: usize,
        input_channels: &[usize],
        out_ch_count: usize,
        cell_size: f32,
        _row_label_width: f32,
    ) {
        egui::Grid::new(format!("routing_matrix_normal_{}", out_idx))
            .min_col_width(cell_size)
            .spacing([2.0, 4.0])
            .show(ui, |ui| {
                // Header row: empty corner + output channel numbers
                ui.label(""); // Empty corner cell
                for out_ch in 0..out_ch_count {
                    ui.allocate_ui_with_layout(
                        egui::vec2(cell_size, 14.0),
                        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                        |ui| {
                            ui.label(egui::RichText::new(format!("{}", out_ch)).small().strong());
                        },
                    );
                }
                ui.end_row();

                // Data rows - grouped by input
                for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
                    // Input group header, which folds the stream away
                    let folded = folded_inputs.contains(&in_idx);
                    if ui
                        .selectable_label(
                            false,
                            egui::RichText::new(format!(
                                "{} In {}",
                                if folded { "\u{25B8}" } else { "\u{25BE}" },
                                in_idx
                            ))
                            .small()
                            .strong(),
                        )
                        .on_hover_text(if folded {
                            "Show this input's channels"
                        } else {
                            "Hide this input's channels"
                        })
                        .clicked()
                    {
                        if folded {
                            folded_inputs.remove(&in_idx);
                        } else {
                            folded_inputs.insert(in_idx);
                        }
                    }
                    for _ in 0..out_ch_count {
                        // A folded stream still shows whether it is routed at
                        // all, so nothing is hidden without a trace.
                        ui.label("");
                    }
                    ui.end_row();

                    // Channel rows
                    for in_ch in 0..(if folded { 0 } else { in_ch_count }) {
                        ui.label(egui::RichText::new(format!("  {}", in_ch)).small());

                        for out_ch in 0..out_ch_count {
                            Self::show_crosspoint_grid(
                                ui,
                                routing,
                                dirty,
                                supports_gain,
                                Crosspoint::new(in_idx, in_ch, out_idx, out_ch),
                                cell_size,
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
        folded_inputs: &mut std::collections::HashSet<usize>,
        num_inputs: usize,
        out_idx: usize,
        input_channels: &[usize],
        out_ch_count: usize,
        cell_size: f32,
        _row_label_width: f32,
    ) {
        egui::Grid::new(format!("routing_matrix_flipped_{}", out_idx))
            .min_col_width(cell_size)
            .spacing([2.0, 4.0])
            .show(ui, |ui| {
                // Header row 1: Input group labels at start of each group
                ui.label(""); // Empty corner cell
                for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
                    let folded = folded_inputs.contains(&in_idx);
                    if ui
                        .selectable_label(
                            false,
                            egui::RichText::new(format!(
                                "{} In {}",
                                if folded { "\u{25B8}" } else { "\u{25BE}" },
                                in_idx
                            ))
                            .small()
                            .strong(),
                        )
                        .on_hover_text(if folded {
                            "Show this input's channels"
                        } else {
                            "Hide this input's channels"
                        })
                        .clicked()
                    {
                        if folded {
                            folded_inputs.remove(&in_idx);
                        } else {
                            folded_inputs.insert(in_idx);
                        }
                    }
                    for _ in 1..(if folded { 1 } else { in_ch_count }) {
                        ui.label("");
                    }
                    // Separator column between input groups
                    if in_idx < num_inputs - 1 {
                        ui.label("");
                    }
                }
                ui.end_row();

                // Header row 2: Channel numbers, minus the folded streams
                ui.label(""); // Empty corner cell
                for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
                    let visible = if folded_inputs.contains(&in_idx) {
                        0
                    } else {
                        in_ch_count
                    };
                    for in_ch in 0..visible {
                        ui.allocate_ui_with_layout(
                            egui::vec2(cell_size, 14.0),
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

                    // A dot for each input channel, minus the folded streams
                    for (in_idx, &in_ch_count) in input_channels.iter().enumerate().take(num_inputs)
                    {
                        let visible = if folded_inputs.contains(&in_idx) {
                            0
                        } else {
                            in_ch_count
                        };
                        for in_ch in 0..visible {
                            Self::show_crosspoint_grid(
                                ui,
                                routing,
                                dirty,
                                supports_gain,
                                Crosspoint::new(in_idx, in_ch, out_idx, out_ch),
                                cell_size,
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
    /// Every crosspoint shows a small dot whether or not it is connected, so
    /// the lattice is always visible and a routing reads as a pattern against
    /// it — a straight-through routing being a line of dots on the diagonal.
    /// A connected crosspoint grows into a knob whose pointer shows the gain.
    ///
    /// Click connects and disconnects, drag turns the knob, and the context
    /// menu sets an exact value. A block without gain support gets the dot and
    /// the click, and nothing else.
    fn show_crosspoint_grid(
        ui: &mut Ui,
        routing: &mut RoutingGains,
        dirty: &mut bool,
        supports_gain: bool,
        crosspoint: Crosspoint,
        cell_size: f32,
    ) {
        let gain = routing.get(&crosspoint).copied();
        let sense = if supports_gain {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::click()
        };
        let (rect, response) = ui.allocate_exact_size(egui::vec2(cell_size, cell_size), sense);

        // Dragging turns the knob; a drag is not also a click, so the two do
        // not fight. Vertical, because that is how every other fader in this
        // application is dragged.
        let dragged = supports_gain && gain.is_some() && response.dragged();
        if dragged {
            let delta = -response.drag_delta().y as f64;
            if delta != 0.0 {
                // Full travel over roughly one cell-height of drag would be
                // unusably twitchy; 120 px for the whole range is close to how
                // a DragValue behaves.
                let step = delta * (-routing::GAIN_FLOOR_DB) / 120.0;
                let db = (routing::gain_to_db(gain.unwrap_or(1.0)) + step)
                    .clamp(routing::GAIN_FLOOR_DB, routing::MAX_CROSSPOINT_GAIN_DB);
                *dirty = true;
                routing.insert(crosspoint, routing::db_to_gain(db));
            }
        } else if response.clicked() {
            *dirty = true;
            if gain.is_some() {
                routing.remove(&crosspoint);
            } else {
                routing.insert(crosspoint, 1.0);
            }
        }

        let gain = routing.get(&crosspoint).copied();
        if ui.is_rect_visible(rect) {
            let visuals = ui.style().interact(&response);
            let painter = ui.painter();
            let centre = rect.center();
            let radius = (cell_size * 0.32).max(2.5);
            match gain {
                Some(gain) => {
                    painter.circle_filled(centre, radius, visuals.fg_stroke.color);
                    if supports_gain {
                        // Pointer from the centre to just past the rim, drawn
                        // as a broad dark stroke with a thin light one on top.
                        // Fixed black and white rather than theme colours: the
                        // pointer sits on the dot's own fill, so it has to read
                        // against that whatever the theme does to it.
                        let angle = routing::knob_angle(gain) as f32;
                        let dir = egui::vec2(angle.sin(), -angle.cos());
                        let tip = centre + dir * (radius * 1.45);
                        painter.line_segment(
                            [centre, tip],
                            egui::Stroke::new(3.0_f32, Color32::BLACK),
                        );
                        painter.line_segment(
                            [centre, tip],
                            egui::Stroke::new(1.0_f32, Color32::WHITE),
                        );
                    }
                }
                None => {
                    // The unconnected crosspoint, still visible so the grid is.
                    painter.circle_filled(centre, 1.5_f32, visuals.weak_bg_fill);
                }
            }
        }

        // While the knob is being turned, put the value under the pointer —
        // the hover text is not shown during a drag, and a knob with no
        // readout is guesswork. Painted on the tooltip layer so the grid's
        // clip rect does not cut it off.
        if dragged {
            if let Some(gain) = gain {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new(("crosspoint_drag_readout", crosspoint)),
                ));
                let galley = painter.layout_no_wrap(
                    format!("{:.1} dB", routing::gain_to_db(gain)),
                    egui::FontId::monospace(12.0),
                    Color32::WHITE,
                );
                let box_rect = egui::Rect::from_center_size(
                    egui::pos2(rect.center().x, rect.top() - 12.0),
                    galley.size() + egui::vec2(10.0, 6.0),
                );
                painter.rect_filled(box_rect, 3.0, Color32::from_black_alpha(230));
                painter.galley(
                    box_rect.center() - galley.size() * 0.5,
                    galley,
                    Color32::WHITE,
                );
            }
        }

        let label = format!(
            "{} -> {}",
            crosspoint.source_key(),
            crosspoint.destination_key()
        );
        let response = match gain {
            Some(g) if supports_gain => response.on_hover_text(format!(
                "{label}: {:.1} dB\nDrag to trim, click to disconnect, right-click for an exact value",
                routing::gain_to_db(g)
            )),
            Some(_) => response.on_hover_text(format!("{label}\nClick to disconnect")),
            None => response.on_hover_text(format!("{label}\nClick to connect")),
        };

        if !supports_gain || gain.is_none() {
            return;
        }
        response.context_menu(|ui| {
            ui.label(&label);
            let mut db = routing::gain_to_db(routing[&crosspoint]);
            if ui
                .add(
                    egui::DragValue::new(&mut db)
                        .speed(0.25)
                        .range(routing::GAIN_FLOOR_DB..=routing::MAX_CROSSPOINT_GAIN_DB)
                        .fixed_decimals(1)
                        .suffix(" dB"),
                )
                .changed()
            {
                *dirty = true;
                routing.insert(crosspoint, routing::db_to_gain(db));
            }
            if ui.button("Unity (0.0 dB)").clicked() {
                *dirty = true;
                routing.insert(crosspoint, 1.0);
                ui.close();
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
