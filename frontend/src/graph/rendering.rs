use egui::{
    pos2, text::LayoutJob, vec2, Color32, FontId, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2,
};
use strom_types::{
    element::{ElementInfo, PropertyValue},
    BlockDefinition, BlockInstance, Element,
};

use super::*;

/// Font size for node type labels (element type / block definition name).
const NODE_TYPE_FONT_SIZE: f32 = 13.0;
/// Font size for custom node name labels (user-assigned names).
const NODE_NAME_FONT_SIZE: f32 = 15.0;
/// Vertical offset for the custom name label below the type label.
const NODE_NAME_Y_OFFSET: f32 = 30.0;
/// Maximum characters displayed for a custom node name.
const NODE_NAME_MAX_CHARS: usize = 20;

/// Draw a custom name label (italic, centered) below the type label on a node.
fn draw_node_name(painter: &egui::Painter, ui: &Ui, name: &str, rect: Rect, zoom: f32) {
    let name_color = if ui.visuals().dark_mode {
        Color32::from_gray(180)
    } else {
        Color32::from_gray(100)
    };
    let display_name = if name.chars().count() > NODE_NAME_MAX_CHARS {
        let truncated: String = name.chars().take(NODE_NAME_MAX_CHARS - 1).collect();
        format!("{truncated}\u{2026}")
    } else {
        name.to_owned()
    };
    let mut job = LayoutJob::simple_singleline(
        display_name,
        FontId::proportional(NODE_NAME_FONT_SIZE * zoom),
        name_color,
    );
    job.sections
        .iter_mut()
        .for_each(|s| s.format.italics = true);
    let galley = painter.layout_job(job);
    let name_x = rect.center().x - galley.size().x / 2.0;
    let name_pos = pos2(name_x, rect.min.y + NODE_NAME_Y_OFFSET * zoom);
    painter.galley(name_pos, galley, Color32::TRANSPARENT);
}

impl GraphEditor {
    /// Center the view on the currently selected element or block.
    pub fn center_on_selected(&mut self) {
        if let Some(ref selected_id) = self.selected {
            // Get the canvas center offset (half of canvas size)
            // If we don't have a stored rect yet, use a reasonable default
            let canvas_center_offset = self
                .last_canvas_rect
                .map(|r| egui::vec2(r.width() / 2.0, r.height() / 2.0))
                .unwrap_or(egui::vec2(400.0, 300.0));

            // Try to find the position of the selected element
            if let Some(element) = self.elements.iter().find(|e| &e.id == selected_id) {
                // Center on element position
                // pan_offset formula: to make world pos appear at screen center
                // screen_center = rect_min + (pos * zoom) + pan_offset
                // pan_offset = screen_center - rect_min - (pos * zoom)
                // Since screen_center - rect_min = canvas_center_offset:
                // pan_offset = canvas_center_offset - (pos * zoom)
                let pos = element.position;
                self.pan_offset =
                    canvas_center_offset - egui::vec2(pos.0 * self.zoom, pos.1 * self.zoom);
            } else if let Some(block) = self.blocks.iter().find(|b| &b.id == selected_id) {
                // Center on block position
                let pos = &block.position;
                self.pan_offset =
                    canvas_center_offset - egui::vec2(pos.x * self.zoom, pos.y * self.zoom);
            }
        }
    }

    /// Calculate the height of an element node based on its pads.
    fn calculate_element_height(&self, element: &Element) -> f32 {
        let element_info = self.element_info_map.get(&element.element_type);
        let (sink_pads, src_pads) = self.get_pads_to_render(element, element_info);
        let pad_count = sink_pads.len().max(src_pads.len()).max(1);
        (80.0 + (pad_count.saturating_sub(1) * 30) as f32).min(400.0)
    }

    /// Calculate the height of a block node based on its pads and content.
    fn calculate_block_height(&self, block: &BlockInstance) -> f32 {
        let block_definition = self.block_definition_map.get(&block.block_definition_id);
        let pad_count = self
            .get_block_external_pads(block, block_definition)
            .map(|pads| pads.inputs.len().max(pads.outputs.len()))
            .unwrap_or(1);
        let base_height = 80.0 + (pad_count.saturating_sub(1) * 30) as f32;
        let content_height = self
            .block_content_map
            .get(&block.id)
            .map(|info| info.additional_height)
            .unwrap_or(0.0);
        // Custom name label takes an extra 20px, pushing content down
        let name_extra = if block.name.as_ref().is_some_and(|n| !n.is_empty()) {
            20.0
        } else {
            0.0
        };
        (base_height + content_height + name_extra).min(400.0)
    }

    /// Calculate the bounding box of all elements and blocks in world coordinates.
    /// Returns None if there are no elements or blocks.
    fn calculate_bounds(&self) -> Option<Rect> {
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;

        // Include all elements
        for element in &self.elements {
            let (x, y) = element.position;
            let height = self.calculate_element_height(element);

            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x + NODE_WIDTH);
            max_y = max_y.max(y + height);
        }

        // Include all blocks
        for block in &self.blocks {
            let x = block.position.x;
            let y = block.position.y;
            let height = self.calculate_block_height(block);

            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x + NODE_WIDTH);
            max_y = max_y.max(y + height);
        }

        if min_x == f32::MAX {
            None // No elements or blocks
        } else {
            Some(Rect::from_min_max(pos2(min_x, min_y), pos2(max_x, max_y)))
        }
    }

    /// Reset the view to the default zoom and pan offset.
    pub fn reset_view(&mut self) {
        self.pan_offset = Vec2::ZERO;
        self.zoom = DEFAULT_ZOOM;
    }

    /// Zoom to fit all elements and blocks in the view.
    /// If there are no elements or blocks, resets to default view.
    pub fn zoom_to_fit(&mut self) {
        let Some(bounds) = self.calculate_bounds() else {
            self.reset_view();
            return;
        };

        // Get canvas size (use stored rect or reasonable default)
        let canvas_size = self
            .last_canvas_rect
            .map(|r| vec2(r.width(), r.height()))
            .unwrap_or(vec2(800.0, 600.0));

        // Calculate available space (canvas minus padding on all sides)
        let available_width = (canvas_size.x - ZOOM_TO_FIT_PADDING * 2.0).max(100.0);
        let available_height = (canvas_size.y - ZOOM_TO_FIT_PADDING * 2.0).max(100.0);

        // Calculate bounds size
        let bounds_width = bounds.width();
        let bounds_height = bounds.height();

        // Calculate zoom to fit both dimensions
        let zoom_x = if bounds_width > 0.0 {
            available_width / bounds_width
        } else {
            MAX_ZOOM_TO_FIT
        };
        let zoom_y = if bounds_height > 0.0 {
            available_height / bounds_height
        } else {
            MAX_ZOOM_TO_FIT
        };

        // Use the smaller zoom to ensure everything fits, clamped to reasonable range
        self.zoom = zoom_x.min(zoom_y).clamp(MIN_ZOOM_TO_FIT, MAX_ZOOM_TO_FIT);

        // Center the view on the bounds center
        let bounds_center = bounds.center();
        let canvas_center = canvas_size / 2.0;

        // pan_offset formula: to make world pos appear at screen center
        // screen_pos = rect_min + (world_pos * zoom) + pan_offset
        // For world_pos to appear at canvas_center (relative to rect_min):
        // canvas_center = (world_pos * zoom) + pan_offset
        // pan_offset = canvas_center - (world_pos * zoom)
        self.pan_offset =
            canvas_center - vec2(bounds_center.x * self.zoom, bounds_center.y * self.zoom);
    }

    /// Render the graph editor.
    pub fn show(&mut self, ui: &mut Ui) -> Response {
        use crate::app::set_local_storage;

        // Reset the QoS marker clicked flag at the start of each frame
        self.qos_marker_clicked.set(false);

        ui.push_id("graph_editor", |ui| {
            let (response, painter) =
                ui.allocate_painter(ui.available_size_before_wrap(), Sense::click_and_drag());

            // Store the canvas rect for centering calculations
            self.last_canvas_rect = Some(response.rect);

            let zoom = self.zoom;
            let pan_offset = self.pan_offset;
            let rect_min = response.rect.min;

            let to_screen = |pos: Pos2| -> Pos2 { rect_min + (pos.to_vec2() * zoom) + pan_offset };

            let from_screen =
                |pos: Pos2| -> Pos2 { ((pos - rect_min - pan_offset) / zoom).to_pos2() };

            // Handle zoom and scroll - use global pointer position so it works even over nodes
            // But don't capture scroll if a window/modal is hovered (prevents scroll bleed-through)
            let pointer_pos = ui.input(|i| i.pointer.hover_pos());
            let pointer_in_canvas = pointer_pos
                .map(|p| response.rect.contains(p))
                .unwrap_or(false);
            let window_hovered = ui.ctx().wants_pointer_input();

            if pointer_in_canvas && !window_hovered {
                let hover_pos = pointer_pos.unwrap();
                let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
                let pinch_zoom = ui.input(|i| i.zoom_delta());
                // Check modifiers from raw scroll events for more accurate detection
                // (i.modifiers may not update without mouse movement)
                let scroll_modifiers = ui.input(|i| {
                    for event in &i.events {
                        if let egui::Event::MouseWheel { modifiers, .. } = event {
                            return *modifiers;
                        }
                    }
                    i.modifiers
                });
                let modifiers = scroll_modifiers;

                // Pinch-to-zoom (trackpad) or Ctrl+Scroll or Alt+Scroll
                if pinch_zoom != 1.0 {
                    let zoom_factor = pinch_zoom;
                    self.zoom = (self.zoom * zoom_factor).clamp(0.1, 3.0);

                    // Adjust pan to zoom towards cursor
                    let world_pos = from_screen(hover_pos);
                    let new_screen_pos = to_screen(world_pos);
                    self.pan_offset += hover_pos - new_screen_pos;
                } else if (modifiers.ctrl || modifiers.alt) && scroll_delta.y != 0.0 {
                    // Ctrl+Scroll or Alt+Scroll: Zoom (using modifiers from raw event)
                    let zoom_delta = scroll_delta.y * 0.001;
                    self.zoom = (self.zoom + zoom_delta).clamp(0.1, 3.0);

                    // Adjust pan to zoom towards cursor
                    let world_pos = from_screen(hover_pos);
                    let new_screen_pos = to_screen(world_pos);
                    self.pan_offset += hover_pos - new_screen_pos;
                }
                // Horizontal scroll (trackpad horizontal swipe)
                else if scroll_delta.x != 0.0 {
                    self.pan_offset.x += scroll_delta.x;
                }
                // Shift+Scroll: Horizontal pan (for mouse wheels)
                else if modifiers.shift && scroll_delta.y != 0.0 {
                    self.pan_offset.x += scroll_delta.y;
                }
                // Plain scroll: Vertical pan
                else if scroll_delta.y != 0.0 {
                    self.pan_offset.y += scroll_delta.y;
                }
            }

            // Draw grid
            self.draw_grid(ui, &painter, response.rect);

            // Draw nodes and handle interaction (must happen before panning)
            let mut elements_to_update = Vec::new();
            let mut pad_interactions = Vec::new();

            for element in &self.elements {
                let pos = element.position;
                let screen_pos = to_screen(pos2(pos.0, pos.1));

                // Calculate height based on number of pads to render (includes dynamic pad expansion)
                let element_info = self.element_info_map.get(&element.element_type);
                let (sink_pads_to_render, src_pads_to_render) =
                    self.get_pads_to_render(element, element_info);
                let pad_count = sink_pads_to_render
                    .len()
                    .max(src_pads_to_render.len())
                    .max(1);
                let node_height = (80.0 + (pad_count.saturating_sub(1) * 30) as f32).min(400.0);

                let node_rect = Rect::from_min_size(
                    screen_pos,
                    vec2(200.0 * self.zoom, node_height * self.zoom),
                );

                let is_selected = self.selected.as_ref() == Some(&element.id);
                let is_hovered = self.hovered_element.as_ref() == Some(&element.id);
                let node_response = self.draw_node(
                    ui,
                    &painter,
                    element,
                    element_info,
                    node_rect,
                    is_selected,
                    is_hovered,
                );

                // Track hover state
                if node_response.hovered() {
                    self.hovered_element = Some(element.id.clone());
                } else if self.hovered_element.as_ref() == Some(&element.id) {
                    self.hovered_element = None;
                }

                // Handle node selection - select on click OR when starting to drag
                if node_response.clicked() || (node_response.dragged() && self.dragging.is_none()) {
                    self.selected = Some(element.id.clone());
                    self.selected_link = None; // Deselect any link
                    self.active_property_tab = PropertyTab::Element; // Switch to Element Properties tab
                    self.focused_pad = None; // Clear pad focus
                }

                // Handle node dragging
                if node_response.dragged() {
                    if self.dragging.is_none() {
                        self.dragging = Some(element.id.clone());
                    }

                    if self.dragging.as_ref() == Some(&element.id) {
                        let delta = node_response.drag_delta() / self.zoom;
                        let new_pos = (pos.0 + delta.x, pos.1 + delta.y);
                        elements_to_update.push((element.id.clone(), new_pos));
                    }
                }

                // Collect pad interactions for later processing
                pad_interactions.push((
                    element.id.clone(),
                    element.element_type.clone(),
                    node_rect,
                ));
            }

            // Handle pad interactions
            for (element_id, element_type, rect) in pad_interactions {
                let element_info = self.element_info_map.get(&element_type).cloned();
                if let Some(element) = self.elements.iter().find(|e| e.id == element_id).cloned() {
                    self.handle_pad_interaction(ui, &element, element_info.as_ref(), rect);
                }
            }

            // Update element positions (no snapping during drag - snap on release)
            for (id, new_pos) in elements_to_update {
                if let Some(elem) = self.elements.iter_mut().find(|e| e.id == id) {
                    elem.position = new_pos;
                }
            }

            // Draw block instances
            let mut blocks_to_update = Vec::new();
            let mut block_pad_interactions = Vec::new();

            for block in &self.blocks {
                let pos = block.position;
                let screen_pos = to_screen(pos2(pos.x, pos.y));

                // Calculate height based on number of external pads (min 80, max 400)
                let block_definition = self.block_definition_map.get(&block.block_definition_id);
                let pad_count = self
                    .get_block_external_pads(block, block_definition)
                    .map(|pads| pads.inputs.len().max(pads.outputs.len()))
                    .unwrap_or(1);

                // Base height for block node
                let base_height = 80.0 + (pad_count.saturating_sub(1) * 30) as f32;

                // Add any dynamic content height (provided by caller)
                let content_height = self
                    .block_content_map
                    .get(&block.id)
                    .map(|info| info.additional_height)
                    .unwrap_or(0.0);

                // Custom name label takes an extra 20px, pushing content down
                let name_extra = if block.name.as_ref().is_some_and(|n| !n.is_empty()) {
                    20.0
                } else {
                    0.0
                };

                let node_height = (base_height + content_height + name_extra).min(400.0);

                let node_rect = Rect::from_min_size(
                    screen_pos,
                    vec2(200.0 * self.zoom, node_height * self.zoom),
                );

                let is_selected = self.selected.as_ref() == Some(&block.id);
                let is_hovered = self.hovered_element.as_ref() == Some(&block.id);

                // Get block definition for UI metadata (icon, color)
                let definition = self.get_block_definition(block);

                let node_response = self.draw_block_node(
                    ui,
                    &painter,
                    block,
                    definition,
                    node_rect,
                    is_selected,
                    is_hovered,
                );

                // Track hover state
                if node_response.hovered() {
                    self.hovered_element = Some(block.id.clone());
                } else if self.hovered_element.as_ref() == Some(&block.id) {
                    self.hovered_element = None;
                }

                // Check if block has interactive content (buttons that need clicks)
                let has_interactive_content = self
                    .block_content_map
                    .get(&block.id)
                    .map(|c| c.render_callback.is_some())
                    .unwrap_or(false);

                // Handle node selection
                // For interactive blocks: only select on drag start (not click) so buttons work
                // For normal blocks: select on click or drag start
                let should_select = if has_interactive_content {
                    node_response.drag_started()
                } else {
                    node_response.clicked() || (node_response.dragged() && self.dragging.is_none())
                };

                if should_select {
                    self.selected = Some(block.id.clone());
                    self.selected_link = None;
                    self.active_property_tab = PropertyTab::Element; // Switch to Element Properties tab
                    self.focused_pad = None; // Clear pad focus
                }

                // Handle double-click to open compositor editor for compositor blocks
                if node_response.double_clicked()
                    && (block.block_definition_id == "builtin.glcompositor"
                        || block.block_definition_id == "builtin.compositor")
                {
                    set_local_storage("open_compositor_editor", &block.id);
                }

                // Handle double-click to open stream picker for AES67 Input blocks
                if node_response.double_clicked()
                    && block.block_definition_id == "builtin.aes67_input"
                {
                    set_local_storage("open_stream_picker", &block.id);
                }

                // Handle double-click to open NDI picker for NDI Input blocks
                if node_response.double_clicked()
                    && block.block_definition_id == "builtin.ndi_input"
                {
                    set_local_storage("open_ndi_picker", &block.id);
                }

                // Handle double-click to open player for WHEP Output blocks
                if node_response.double_clicked()
                    && block.block_definition_id == "builtin.whep_output"
                {
                    set_local_storage("open_whep_player", &block.id);
                }

                // Handle double-click to open ingest page for WHIP Input blocks
                if node_response.double_clicked()
                    && block.block_definition_id == "builtin.whip_input"
                {
                    set_local_storage("open_whip_ingest", &block.id);
                }

                // Handle double-click to open vision mixer control page
                if node_response.double_clicked()
                    && block.block_definition_id == "builtin.vision_mixer"
                {
                    set_local_storage("open_vision_mixer", &block.id);
                }

                // Handle double-click to open routing matrix for Audio Router blocks
                if node_response.double_clicked()
                    && block.block_definition_id == "builtin.audiorouter"
                {
                    set_local_storage("open_routing_editor", &block.id);
                }

                // Handle double-click to open mixer editor for Mixer blocks
                if node_response.double_clicked() && block.block_definition_id == "builtin.mixer" {
                    set_local_storage("open_mixer_editor", &block.id);
                }

                // Note: Playlist editor for media player is opened via the + button in the compact UI

                // Handle node dragging
                if node_response.dragged() {
                    if self.dragging.is_none() {
                        self.dragging = Some(block.id.clone());
                    }

                    if self.dragging.as_ref() == Some(&block.id) {
                        let delta = node_response.drag_delta() / self.zoom;
                        let new_pos = (pos.x + delta.x, pos.y + delta.y);
                        blocks_to_update.push((block.id.clone(), new_pos));
                    }
                }

                // Collect pad interactions for later processing
                block_pad_interactions.push((
                    block.id.clone(),
                    block.block_definition_id.clone(),
                    node_rect,
                ));
            }

            // Handle block pad interactions
            for (block_id, block_def_id, rect) in block_pad_interactions {
                let block_definition = self.block_definition_map.get(&block_def_id).cloned();
                if let Some(block) = self.blocks.iter().find(|b| b.id == block_id).cloned() {
                    self.handle_block_pad_interaction(ui, &block, block_definition.as_ref(), rect);
                }
            }

            // Update block positions (no snapping during drag - snap on release)
            for (id, new_pos) in blocks_to_update {
                if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
                    block.position = strom_types::block::Position {
                        x: new_pos.0,
                        y: new_pos.1,
                    };
                }
            }

            // Handle canvas panning (only if not dragging a node)
            if response.dragged() && self.dragging.is_none() && self.creating_link.is_none() {
                self.pan_offset += response.drag_delta();
            }

            // Deselect when clicking on empty space (not a link, not a node)
            if response.clicked() && self.hovered_link.is_none() && self.hovered_element.is_none() {
                if !self.has_selection() && !response.double_clicked() {
                    // Already deselected and not a double-click — signal to toggle right pane
                    self.request_toggle_right_pane.set(true);
                }
                self.selected = None;
                self.selected_link = None;
            }

            // Ctrl+double-click on background: zoom to fit
            if response.double_clicked()
                && self.hovered_link.is_none()
                && self.hovered_element.is_none()
                && ui.input(|i| i.modifiers.ctrl)
            {
                self.zoom_to_fit();
            }

            // Double-click on background (without Ctrl): request to open palette
            if response.double_clicked()
                && self.hovered_link.is_none()
                && self.hovered_element.is_none()
                && !ui.input(|i| i.modifiers.ctrl)
            {
                self.request_open_palette.set(true);
            }

            // Reset dragging state when mouse is released
            if !ui.input(|i| i.pointer.primary_down()) {
                // Snap to grid when drag ends
                if let Some(ref drag_id) = self.dragging {
                    // Check if it's an element
                    if let Some(elem) = self.elements.iter_mut().find(|e| &e.id == drag_id) {
                        elem.position =
                            (snap_to_grid(elem.position.0), snap_to_grid(elem.position.1));
                    }
                    // Check if it's a block
                    if let Some(block) = self.blocks.iter_mut().find(|b| &b.id == drag_id) {
                        block.position = strom_types::block::Position {
                            x: snap_to_grid(block.position.x),
                            y: snap_to_grid(block.position.y),
                        };
                    }
                }
                self.dragging = None;

                // Finalize link creation
                if let Some((from_id, from_pad)) = self.creating_link.take() {
                    if let Some((to_id, to_pad)) = &self.hovered_pad {
                        if from_id != *to_id {
                            // Determine which pad is output and which is input
                            let from_is_output = self.is_output_pad(&from_id, &from_pad);
                            let to_is_output = self.is_output_pad(to_id, to_pad);

                            // Create link with correct direction (output -> input)
                            // Only create link if one is output and one is input
                            if from_is_output && !to_is_output {
                                // Normal case: dragged from output to input
                                let link = strom_types::Link {
                                    from: format!("{}:{}", from_id, from_pad),
                                    to: format!("{}:{}", to_id, to_pad),
                                };
                                self.links.push(link);
                            } else if !from_is_output && to_is_output {
                                // Reversed: dragged from input to output, swap them
                                let link = strom_types::Link {
                                    from: format!("{}:{}", to_id, to_pad),
                                    to: format!("{}:{}", from_id, from_pad),
                                };
                                self.links.push(link);
                            }
                            // else: Invalid case (both are outputs or both are inputs), don't create link
                        }
                    }
                }
            }

            // Draw links AFTER nodes so they appear on top
            let links_clone = self.links.clone();
            self.hovered_link = None; // Reset hover state

            for (idx, link) in links_clone.iter().enumerate() {
                let is_selected = self.selected_link == Some(idx);

                if let Some(hover_pos) = response.hover_pos() {
                    // Check if mouse is near this link
                    if self.is_point_near_link(link, hover_pos, &to_screen) {
                        self.hovered_link = Some(idx);
                    }
                }

                let is_hovered = self.hovered_link == Some(idx);
                self.draw_link(&painter, link, &to_screen, is_selected, is_hovered);
            }

            // Handle link selection on click
            if response.clicked() && self.hovered_link.is_some() {
                self.selected_link = self.hovered_link;
                self.selected = None; // Deselect any element
            }

            // Draw link being created (on top of everything)
            if let Some((from_id, from_pad)) = &self.creating_link {
                // Determine if we're dragging from an input or output pad
                let from_is_output = self.is_output_pad(from_id, from_pad);
                let from_is_input = !from_is_output;

                // Get the actual position of the source pad
                let from_world_pos = self
                    .get_pad_position(from_id, from_pad, from_is_input)
                    .unwrap_or_else(|| pos2(100.0, 100.0));
                let from_screen_pos = to_screen(from_world_pos);

                let to_pos = ui.input(|i| i.pointer.hover_pos().unwrap_or(from_screen_pos));

                // Draw cubic bezier curve for link being created
                let control_offset = 50.0 * self.zoom;
                let control1 = from_screen_pos + vec2(control_offset, 0.0);
                let control2 = to_pos - vec2(control_offset, 0.0);

                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                    [from_screen_pos, control1, control2, to_pos],
                    false,
                    Color32::TRANSPARENT,
                    Stroke::new(2.0, Color32::from_rgb(100, 150, 255)),
                ));
            }

            // Draw floating view control buttons at top center of canvas
            let button_group_width = 85.0;
            let button_pos = pos2(
                response.rect.center().x - button_group_width / 2.0,
                response.rect.min.y + 8.0,
            );

            egui::Area::new(egui::Id::new("graph_view_controls"))
                .fixed_pos(button_pos)
                .order(egui::Order::Middle)
                .show(ui.ctx(), |ui| {
                    ui.horizontal(|ui| {
                        ui.style_mut().spacing.item_spacing.x = 4.0;

                        if ui
                            .add(egui::Button::new("Fit").min_size(vec2(32.0, 24.0)))
                            .on_hover_text("Zoom to fit all elements (Ctrl+double-click)")
                            .clicked()
                        {
                            self.zoom_to_fit();
                        }

                        if ui
                            .add(egui::Button::new("Reset").min_size(vec2(40.0, 24.0)))
                            .on_hover_text("Reset view to default")
                            .clicked()
                        {
                            self.reset_view();
                        }
                    });
                });

            response
        })
        .inner
    }

    fn draw_grid(&self, ui: &Ui, painter: &egui::Painter, rect: Rect) {
        let grid_spacing = 50.0 * self.zoom;
        let color = if ui.visuals().dark_mode {
            Color32::from_gray(40) // Dark theme: darker grid lines
        } else {
            Color32::from_gray(200) // Light theme: lighter grid lines
        };

        // Grid offset from panning - grid moves with content
        // Use rem_euclid for always-positive remainder
        let offset_x = self.pan_offset.x.rem_euclid(grid_spacing);
        let offset_y = self.pan_offset.y.rem_euclid(grid_spacing);

        let start_x = (rect.min.x / grid_spacing).floor() * grid_spacing + offset_x;
        let start_y = (rect.min.y / grid_spacing).floor() * grid_spacing + offset_y;

        // Vertical lines
        let mut x = start_x;
        while x < rect.max.x {
            painter.line_segment(
                [pos2(x, rect.min.y), pos2(x, rect.max.y)],
                Stroke::new(1.0, color),
            );
            x += grid_spacing;
        }

        // Horizontal lines
        let mut y = start_y;
        while y < rect.max.y {
            painter.line_segment(
                [pos2(rect.min.x, y), pos2(rect.max.x, y)],
                Stroke::new(1.0, color),
            );
            y += grid_spacing;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_node(
        &self,
        ui: &Ui,
        painter: &egui::Painter,
        element: &Element,
        element_info: Option<&ElementInfo>,
        rect: Rect,
        is_selected: bool,
        is_hovered: bool,
    ) -> Response {
        // Check for QoS issues
        let qos_health = self.qos_health_map.get(&element.id.to_string());
        let has_qos_issues = qos_health
            .map(|h| *h != crate::qos_monitor::QoSHealth::Ok)
            .unwrap_or(false);

        let stroke_color = if has_qos_issues {
            // QoS issues - use warning/critical color for border
            qos_health.unwrap().color()
        } else if ui.visuals().dark_mode {
            // Dark theme borders
            if is_selected {
                Color32::from_rgb(100, 220, 220) // Cyan
            } else if is_hovered {
                Color32::from_rgb(120, 180, 180) // Lighter cyan
            } else {
                Color32::from_rgb(80, 160, 160) // Dark cyan
            }
        } else {
            // Light theme borders - vibrant teal
            if is_selected {
                Color32::from_rgb(0, 160, 160) // Vibrant teal
            } else if is_hovered {
                Color32::from_rgb(20, 140, 140) // Medium teal
            } else {
                Color32::from_rgb(40, 120, 120) // Darker teal
            }
        };

        let stroke_width = if has_qos_issues {
            3.0 // Thicker border for QoS issues
        } else if is_selected {
            2.5
        } else if is_hovered {
            1.5
        } else {
            1.0
        };

        let fill_color = if ui.visuals().dark_mode {
            // Dark theme: dark cyan-tinted backgrounds
            if is_selected {
                Color32::from_rgb(40, 60, 60)
            } else if is_hovered {
                Color32::from_rgb(35, 50, 50)
            } else {
                Color32::from_rgb(30, 40, 40)
            }
        } else {
            // Light theme: vibrant cyan/teal backgrounds
            if is_selected {
                Color32::from_rgb(140, 230, 230) // Bright cyan
            } else if is_hovered {
                Color32::from_rgb(160, 240, 240) // Lighter cyan
            } else {
                Color32::from_rgb(180, 245, 245) // Soft cyan
            }
        };

        // Draw QoS glow effect behind the node if there are issues
        if has_qos_issues {
            let glow_color = qos_health.unwrap().color();
            // Draw multiple expanding rectangles for glow effect
            for i in 1..=3 {
                let expand = i as f32 * 2.0 * self.zoom;
                let alpha = 60 - (i * 15) as u8; // Fade out
                let glow_rect = rect.expand(expand);
                painter.rect_filled(
                    glow_rect,
                    5.0 + expand,
                    Color32::from_rgba_unmultiplied(
                        glow_color.r(),
                        glow_color.g(),
                        glow_color.b(),
                        alpha,
                    ),
                );
            }
        }

        // Draw node background
        painter.rect(
            rect,
            5.0,
            fill_color,
            Stroke::new(stroke_width, stroke_color),
            egui::epaint::StrokeKind::Inside,
        );

        // Draw element type
        // Note: multiply offsets by zoom since rect is in screen-space
        let text_pos = rect.min + vec2(10.0 * self.zoom, 10.0 * self.zoom);
        let text_color = if ui.visuals().dark_mode {
            Color32::WHITE
        } else {
            Color32::from_gray(40) // Dark text for light backgrounds
        };
        painter.text(
            text_pos,
            egui::Align2::LEFT_TOP,
            &element.element_type,
            FontId::proportional(NODE_TYPE_FONT_SIZE * self.zoom),
            text_color,
        );

        // Draw custom name (from GStreamer "name" property) below element type
        if let Some(PropertyValue::String(custom_name)) = element.properties.get("name") {
            if !custom_name.is_empty() {
                draw_node_name(painter, ui, custom_name, rect, self.zoom);
            }
        }

        // Draw QoS indicator if there are issues - make it clickable
        if let Some(qos_health) = self.qos_health_map.get(&element.id.to_string()) {
            if *qos_health != crate::qos_monitor::QoSHealth::Ok {
                let qos_icon_pos = rect.right_top() + vec2(-20.0 * self.zoom, 8.0 * self.zoom);
                let icon_size = 16.0 * self.zoom;
                let qos_icon_rect = egui::Rect::from_center_size(
                    qos_icon_pos + vec2(0.0, icon_size / 2.0),
                    vec2(icon_size, icon_size),
                );

                // Check if the QoS icon is clicked
                let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                let clicked = ui.input(|i| i.pointer.primary_clicked());
                if let Some(pos) = pointer_pos {
                    if clicked && qos_icon_rect.contains(pos) {
                        self.qos_marker_clicked.set(true);
                    }
                }

                painter.text(
                    qos_icon_pos,
                    egui::Align2::CENTER_TOP,
                    qos_health.icon(),
                    FontId::proportional(14.0 * self.zoom),
                    qos_health.color(),
                );
            }
        }

        // Draw buffer age indicator if there are issues (clock icon, offset from QoS icon)
        if let Some(ba_health) = self.buffer_age_health_map.get(&element.id.to_string()) {
            if *ba_health != crate::buffer_age::BufferAgeHealth::Ok {
                // Position to the left of the QoS indicator
                let has_qos = self
                    .qos_health_map
                    .get(&element.id.to_string())
                    .map(|h| *h != crate::qos_monitor::QoSHealth::Ok)
                    .unwrap_or(false);
                let x_offset = if has_qos { -38.0 } else { -20.0 };
                let ba_icon_pos = rect.right_top() + vec2(x_offset * self.zoom, 8.0 * self.zoom);

                painter.text(
                    ba_icon_pos,
                    egui::Align2::CENTER_TOP,
                    ba_health.icon(),
                    FontId::proportional(14.0 * self.zoom),
                    ba_health.color(),
                );
            }
        }

        // Draw ports based on element metadata
        let port_size = 16.0 * self.zoom;

        // Get pads to render (expands request pads into actual instances)
        let (sink_pads_to_render, src_pads_to_render) =
            self.get_pads_to_render(element, element_info);

        if !sink_pads_to_render.is_empty() || !src_pads_to_render.is_empty() {
            use strom_types::element::MediaType;

            // Draw sink pads (inputs) on the left
            let sink_count = sink_pads_to_render.len();
            for (idx, pad_to_render) in sink_pads_to_render.iter().enumerate() {
                // Calculate vertical position using tighter spacing
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let pad_count = sink_pads_to_render
                    .len()
                    .max(src_pads_to_render.len())
                    .max(1);
                let node_height = (80.0 + (pad_count.saturating_sub(1) * 30) as f32).min(400.0);
                let y_offset =
                    self.calculate_pad_y_offset(idx, sink_count, node_height) * self.zoom;

                let pad_center = pos2(rect.min.x, rect.min.y + y_offset);
                let pad_rect = Rect::from_center_size(pad_center, vec2(port_size, port_size));

                let is_hovered = self
                    .hovered_pad
                    .as_ref()
                    .map(|(id, pad)| id == &element.id && pad == &pad_to_render.name)
                    .unwrap_or(false);

                // Choose color based on media type
                let (base_color, hover_color, glow_color, label) = match pad_to_render.media_type {
                    MediaType::Audio => (
                        Color32::from_rgb(100, 200, 100), // Green
                        Color32::from_rgb(126, 232, 126),
                        Color32::from_rgba_premultiplied(100, 200, 100, 77),
                        "A",
                    ),
                    MediaType::Video => (
                        Color32::from_rgb(255, 150, 100), // Orange
                        Color32::from_rgb(255, 176, 128),
                        Color32::from_rgba_premultiplied(255, 150, 100, 77),
                        "V",
                    ),
                    MediaType::Generic => (
                        Color32::from_rgb(100, 150, 255), // Blue
                        Color32::from_rgb(126, 176, 255),
                        Color32::from_rgba_premultiplied(100, 150, 255, 77),
                        "",
                    ),
                };

                // Use lighter/transparent color for empty pads
                let (base_color, hover_color) = if pad_to_render.is_empty {
                    (
                        Color32::from_rgba_premultiplied(
                            base_color.r() / 2,
                            base_color.g() / 2,
                            base_color.b() / 2,
                            128,
                        ),
                        Color32::from_rgba_premultiplied(
                            hover_color.r() / 2,
                            hover_color.g() / 2,
                            hover_color.b() / 2,
                            180,
                        ),
                    )
                } else {
                    (base_color, hover_color)
                };

                if is_hovered {
                    // Draw glow effect
                    let glow_rect = Rect::from_center_size(
                        pad_center,
                        vec2(port_size + 10.0 * self.zoom, port_size + 10.0 * self.zoom),
                    );
                    painter.rect(
                        glow_rect,
                        3.0,
                        glow_color,
                        Stroke::NONE,
                        egui::epaint::StrokeKind::Inside,
                    );
                    painter.rect(
                        pad_rect,
                        3.0,
                        hover_color,
                        Stroke::new(1.5 * self.zoom, Color32::from_gray(80)),
                        egui::epaint::StrokeKind::Inside,
                    );
                } else {
                    painter.rect(
                        pad_rect,
                        3.0,
                        base_color,
                        Stroke::new(1.0 * self.zoom, Color32::from_gray(60)),
                        egui::epaint::StrokeKind::Inside,
                    );
                }

                // Draw label inside port (or "+" for empty pads)
                let label_text = if pad_to_render.is_empty {
                    "+"
                } else if !label.is_empty() {
                    label
                } else {
                    ""
                };

                if !label_text.is_empty() {
                    painter.text(
                        pad_center,
                        egui::Align2::CENTER_CENTER,
                        label_text,
                        FontId::proportional(10.0 * self.zoom),
                        if pad_to_render.is_empty {
                            Color32::from_gray(180)
                        } else {
                            Color32::BLACK
                        },
                    );
                }
            }

            // Draw src pads (outputs) on the right
            let src_count = src_pads_to_render.len();
            for (idx, pad_to_render) in src_pads_to_render.iter().enumerate() {
                // Calculate vertical position using tighter spacing
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let pad_count = sink_pads_to_render
                    .len()
                    .max(src_pads_to_render.len())
                    .max(1);
                let node_height = (80.0 + (pad_count.saturating_sub(1) * 30) as f32).min(400.0);
                let y_offset = self.calculate_pad_y_offset(idx, src_count, node_height) * self.zoom;

                let pad_center = pos2(rect.max.x, rect.min.y + y_offset);
                let pad_rect = Rect::from_center_size(pad_center, vec2(port_size, port_size));

                let is_hovered = self
                    .hovered_pad
                    .as_ref()
                    .map(|(id, pad)| id == &element.id && pad == &pad_to_render.name)
                    .unwrap_or(false);

                // Choose color based on media type
                let (base_color, hover_color, glow_color, label) = match pad_to_render.media_type {
                    MediaType::Audio => (
                        Color32::from_rgb(100, 200, 100), // Green
                        Color32::from_rgb(126, 232, 126),
                        Color32::from_rgba_premultiplied(100, 200, 100, 77),
                        "A",
                    ),
                    MediaType::Video => (
                        Color32::from_rgb(255, 150, 100), // Orange
                        Color32::from_rgb(255, 176, 128),
                        Color32::from_rgba_premultiplied(255, 150, 100, 77),
                        "V",
                    ),
                    MediaType::Generic => (
                        Color32::from_rgb(100, 150, 255), // Blue
                        Color32::from_rgb(126, 176, 255),
                        Color32::from_rgba_premultiplied(100, 150, 255, 77),
                        "",
                    ),
                };

                // Use lighter/transparent color for empty pads
                let (base_color, hover_color) = if pad_to_render.is_empty {
                    (
                        Color32::from_rgba_premultiplied(
                            base_color.r() / 2,
                            base_color.g() / 2,
                            base_color.b() / 2,
                            128,
                        ),
                        Color32::from_rgba_premultiplied(
                            hover_color.r() / 2,
                            hover_color.g() / 2,
                            hover_color.b() / 2,
                            180,
                        ),
                    )
                } else {
                    (base_color, hover_color)
                };

                if is_hovered {
                    // Draw glow effect
                    let glow_rect = Rect::from_center_size(
                        pad_center,
                        vec2(port_size + 10.0 * self.zoom, port_size + 10.0 * self.zoom),
                    );
                    painter.rect(
                        glow_rect,
                        3.0,
                        glow_color,
                        Stroke::NONE,
                        egui::epaint::StrokeKind::Inside,
                    );
                    painter.rect(
                        pad_rect,
                        3.0,
                        hover_color,
                        Stroke::new(1.5 * self.zoom, Color32::from_gray(80)),
                        egui::epaint::StrokeKind::Inside,
                    );
                } else {
                    painter.rect(
                        pad_rect,
                        3.0,
                        base_color,
                        Stroke::new(1.0 * self.zoom, Color32::from_gray(60)),
                        egui::epaint::StrokeKind::Inside,
                    );
                }

                // Draw label inside port (or "+" for empty pads)
                let label_text = if pad_to_render.is_empty {
                    "+"
                } else if !label.is_empty() {
                    label
                } else {
                    ""
                };

                if !label_text.is_empty() {
                    painter.text(
                        pad_center,
                        egui::Align2::CENTER_CENTER,
                        label_text,
                        FontId::proportional(10.0 * self.zoom),
                        if pad_to_render.is_empty {
                            Color32::from_gray(180)
                        } else {
                            Color32::BLACK
                        },
                    );
                }
            }
        } else {
            // Fallback: draw generic ports if no metadata available
            let is_source = element.element_type.ends_with("src");
            let is_sink = element.element_type.ends_with("sink");

            // Draw input (generic blue)
            if !is_source {
                let input_center = pos2(rect.min.x, rect.center().y);
                let input_rect = Rect::from_center_size(input_center, vec2(port_size, port_size));
                painter.rect(
                    input_rect,
                    2.0,
                    Color32::from_rgb(100, 150, 255),
                    Stroke::NONE,
                    egui::epaint::StrokeKind::Inside,
                );
            }

            // Draw output (generic blue)
            if !is_sink {
                let output_center = pos2(rect.max.x, rect.center().y);
                let output_rect = Rect::from_center_size(output_center, vec2(port_size, port_size));
                painter.rect(
                    output_rect,
                    2.0,
                    Color32::from_rgb(100, 150, 255),
                    Stroke::NONE,
                    egui::epaint::StrokeKind::Inside,
                );
            }
        }

        ui.interact(rect, ui.id().with(&element.id), Sense::click_and_drag())
    }

    /// Draw a block instance node
    #[allow(clippy::too_many_arguments)]
    fn draw_block_node(
        &self,
        ui: &mut Ui,
        painter: &egui::Painter,
        block: &BlockInstance,
        definition: Option<&BlockDefinition>,
        rect: Rect,
        is_selected: bool,
        is_hovered: bool,
    ) -> Response {
        // Check for QoS issues
        let qos_health = self.qos_health_map.get(&block.id);
        let has_qos_issues = qos_health
            .map(|h| *h != crate::qos_monitor::QoSHealth::Ok)
            .unwrap_or(false);

        // Get custom colors from ui_metadata if available
        let ui_meta = definition.and_then(|d| d.ui_metadata.as_ref());

        let custom_stroke = ui_meta
            .and_then(|m| {
                if ui.visuals().dark_mode {
                    m.dark_stroke_color.as_ref()
                } else {
                    m.light_stroke_color.as_ref()
                }
            })
            .and_then(|c| parse_hex_color(c));

        let custom_fill = ui_meta
            .and_then(|m| {
                if ui.visuals().dark_mode {
                    m.dark_fill_color.as_ref()
                } else {
                    m.light_fill_color.as_ref()
                }
            })
            .and_then(|c| parse_hex_color(c));

        let stroke_color = if has_qos_issues {
            qos_health.unwrap().color()
        } else if let Some(color) = custom_stroke {
            if is_selected {
                brighten_color(color, 30)
            } else if is_hovered {
                brighten_color(color, 15)
            } else {
                color
            }
        } else if ui.visuals().dark_mode {
            if is_selected {
                Color32::from_rgb(180, 120, 220)
            } else if is_hovered {
                Color32::from_rgb(160, 100, 210)
            } else {
                Color32::from_rgb(150, 80, 200)
            }
        } else if is_selected {
            Color32::from_rgb(140, 60, 180)
        } else if is_hovered {
            Color32::from_rgb(130, 50, 170)
        } else {
            Color32::from_rgb(120, 40, 160)
        };

        let stroke_width = if has_qos_issues {
            3.0 // Thicker border for QoS issues
        } else if is_selected {
            2.0
        } else if is_hovered {
            1.5
        } else {
            1.0
        };

        let fill_color = if let Some(color) = custom_fill {
            // Use custom fill color directly
            if is_selected {
                brighten_color(color, 15)
            } else if is_hovered {
                brighten_color(color, 8)
            } else {
                color
            }
        } else if ui.visuals().dark_mode {
            if is_selected {
                Color32::from_rgb(55, 40, 70)
            } else if is_hovered {
                Color32::from_rgb(48, 35, 62)
            } else {
                Color32::from_rgb(40, 30, 55)
            }
        } else if is_selected {
            Color32::from_rgb(230, 210, 245)
        } else if is_hovered {
            Color32::from_rgb(235, 220, 248)
        } else {
            Color32::from_rgb(240, 225, 250)
        };

        // Draw QoS glow effect behind the node if there are issues
        if has_qos_issues {
            let glow_color = qos_health.unwrap().color();
            // Draw multiple expanding rectangles for glow effect
            for i in 1..=3 {
                let expand = i as f32 * 2.0 * self.zoom;
                let alpha = 60 - (i * 15) as u8; // Fade out
                let glow_rect = rect.expand(expand);
                painter.rect_filled(
                    glow_rect,
                    5.0 + expand,
                    Color32::from_rgba_unmultiplied(
                        glow_color.r(),
                        glow_color.g(),
                        glow_color.b(),
                        alpha,
                    ),
                );
            }
        }

        // Draw node background with rounded corners
        painter.rect(
            rect,
            5.0,
            fill_color,
            Stroke::new(stroke_width, stroke_color),
            egui::epaint::StrokeKind::Inside,
        );

        // Add block interaction EARLY - before render_callback adds buttons
        // This way buttons added later will take priority over this interaction
        let content_info = self.block_content_map.get(&block.id);
        let has_interactive_content = content_info
            .map(|c| c.render_callback.is_some())
            .unwrap_or(false);

        let block_response = if has_interactive_content {
            // Drag only - clicks pass through to buttons inside
            ui.interact(rect, ui.id().with(&block.id), Sense::drag())
        } else {
            // Normal blocks: click and drag
            ui.interact(rect, ui.id().with(&block.id), Sense::click_and_drag())
        };

        // Draw block icon (use custom icon from ui_metadata if available)
        // Note: multiply offsets by zoom since rect is in screen-space
        let icon_pos = rect.min + vec2(10.0 * self.zoom, 8.0 * self.zoom);
        let icon_color = if ui.visuals().dark_mode {
            Color32::WHITE
        } else {
            Color32::from_gray(40) // Dark icon for light backgrounds
        };
        let icon = definition
            .and_then(|d| d.ui_metadata.as_ref())
            .and_then(|m| m.icon.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("\u{1f4e6}");
        painter.text(
            icon_pos,
            egui::Align2::LEFT_TOP,
            icon,
            FontId::proportional(16.0 * self.zoom),
            icon_color,
        );

        // Draw block name (use human-readable name from definition if available)
        let block_name = definition.map(|def| def.name.as_str()).unwrap_or_else(|| {
            block
                .block_definition_id
                .strip_prefix("builtin.")
                .unwrap_or(&block.block_definition_id)
        });
        let text_pos = rect.min + vec2(35.0 * self.zoom, 10.0 * self.zoom);
        let custom_text = ui_meta
            .and_then(|m| {
                if ui.visuals().dark_mode {
                    m.dark_text_color.as_ref()
                } else {
                    m.light_text_color.as_ref()
                }
            })
            .and_then(|c| parse_hex_color(c));

        let text_color = custom_text.unwrap_or_else(|| {
            if ui.visuals().dark_mode {
                Color32::from_rgb(220, 180, 255)
            } else {
                Color32::from_rgb(80, 40, 120)
            }
        });
        painter.text(
            text_pos,
            egui::Align2::LEFT_TOP,
            block_name,
            FontId::proportional(NODE_TYPE_FONT_SIZE * self.zoom),
            text_color,
        );

        // Draw custom instance name below block type name
        if let Some(custom_name) = &block.name {
            if !custom_name.is_empty() {
                draw_node_name(painter, ui, custom_name, rect, self.zoom);
            }
        }

        // Draw QoS indicator if there are issues - make it clickable
        if let Some(qos_health) = self.qos_health_map.get(&block.id) {
            if *qos_health != crate::qos_monitor::QoSHealth::Ok {
                let qos_icon_pos = rect.right_top() + vec2(-20.0 * self.zoom, 8.0 * self.zoom);
                let icon_size = 16.0 * self.zoom;
                let qos_icon_rect = egui::Rect::from_center_size(
                    qos_icon_pos + vec2(0.0, icon_size / 2.0),
                    vec2(icon_size, icon_size),
                );

                // Check if the QoS icon is clicked
                let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                let clicked = ui.input(|i| i.pointer.primary_clicked());
                if let Some(pos) = pointer_pos {
                    if clicked && qos_icon_rect.contains(pos) {
                        self.qos_marker_clicked.set(true);
                    }
                }

                painter.text(
                    qos_icon_pos,
                    egui::Align2::CENTER_TOP,
                    qos_health.icon(),
                    FontId::proportional(14.0 * self.zoom),
                    qos_health.color(),
                );
            }
        }

        // Draw buffer age indicator if there are issues (clock icon, offset from QoS icon)
        if let Some(ba_health) = self.buffer_age_health_map.get(&block.id) {
            if *ba_health != crate::buffer_age::BufferAgeHealth::Ok {
                let has_qos = self
                    .qos_health_map
                    .get(&block.id)
                    .map(|h| *h != crate::qos_monitor::QoSHealth::Ok)
                    .unwrap_or(false);
                let x_offset = if has_qos { -38.0 } else { -20.0 };
                let ba_icon_pos = rect.right_top() + vec2(x_offset * self.zoom, 8.0 * self.zoom);

                painter.text(
                    ba_icon_pos,
                    egui::Align2::CENTER_TOP,
                    ba_health.icon(),
                    FontId::proportional(14.0 * self.zoom),
                    ba_health.color(),
                );
            }
        }

        // Render any dynamic content (e.g., meter visualization)
        if let Some(content_info) = self.block_content_map.get(&block.id) {
            if let Some(ref render_callback) = content_info.render_callback {
                // Calculate content area (below the title, above the pads)
                // Clamp content area to fit within the block rect.
                // Use at least 20px (scaled by zoom) so callbacks with
                // additional_height=0 (e.g. 1-2 channel meters) still
                // have space to render within the base block area.
                let has_custom_name = block.name.as_ref().is_some_and(|n| !n.is_empty());
                let content_top = if has_custom_name {
                    55.0 * self.zoom
                } else {
                    35.0 * self.zoom
                };
                let max_content_height = (rect.height() - content_top).max(0.0);
                let render_height =
                    (content_info.additional_height * self.zoom).max(20.0 * self.zoom);
                let content_area = Rect::from_min_size(
                    rect.min + vec2(10.0 * self.zoom, content_top),
                    vec2(180.0 * self.zoom, render_height.min(max_content_height)),
                );

                // Create a child UI for the custom content
                let mut content_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(content_area)
                        .layout(egui::Layout::top_down(egui::Align::LEFT)),
                );
                render_callback(&mut content_ui, content_area);
            }
        }

        // Draw external pads (ports) based on block definition
        let port_size = 16.0 * self.zoom;

        let block_definition = self.block_definition_map.get(&block.block_definition_id);
        if let Some(external_pads) = self.get_block_external_pads(block, block_definition) {
            use strom_types::element::MediaType;

            // Calculate node height (same calculation as in get_pad_position for consistency)
            let pad_count = external_pads.inputs.len().max(external_pads.outputs.len());
            let base_height = 80.0 + (pad_count.saturating_sub(1) * 30) as f32;
            let content_height = self
                .block_content_map
                .get(&block.id)
                .map(|info| info.additional_height)
                .unwrap_or(0.0);
            let name_extra = if block.name.as_ref().is_some_and(|n| !n.is_empty()) {
                20.0
            } else {
                0.0
            };
            let node_height = (base_height + content_height + name_extra).min(400.0);

            // Draw input pads on the left
            let input_count = external_pads.inputs.len();
            for (idx, external_pad) in external_pads.inputs.iter().enumerate() {
                // Calculate vertical position using tighter spacing
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let y_offset =
                    self.calculate_pad_y_offset(idx, input_count, node_height) * self.zoom;

                let pad_center = pos2(rect.min.x, rect.min.y + y_offset);
                let pad_rect = Rect::from_center_size(pad_center, vec2(port_size, port_size));

                let is_hovered = self
                    .hovered_pad
                    .as_ref()
                    .map(|(id, pad)| id == &block.id && pad == &external_pad.name)
                    .unwrap_or(false);

                // Choose color based on media type
                let (base_color, hover_color, glow_color, label) = match external_pad.media_type {
                    MediaType::Audio => (
                        Color32::from_rgb(100, 200, 100), // Green
                        Color32::from_rgb(126, 232, 126),
                        Color32::from_rgba_premultiplied(100, 200, 100, 77),
                        "A",
                    ),
                    MediaType::Video => (
                        Color32::from_rgb(255, 150, 100), // Orange
                        Color32::from_rgb(255, 176, 128),
                        Color32::from_rgba_premultiplied(255, 150, 100, 77),
                        "V",
                    ),
                    MediaType::Generic => (
                        Color32::from_rgb(100, 150, 255), // Blue
                        Color32::from_rgb(126, 176, 255),
                        Color32::from_rgba_premultiplied(100, 150, 255, 77),
                        "",
                    ),
                };

                if is_hovered {
                    // Draw glow effect
                    let glow_rect = Rect::from_center_size(
                        pad_center,
                        vec2(port_size + 10.0 * self.zoom, port_size + 10.0 * self.zoom),
                    );
                    painter.rect(
                        glow_rect,
                        3.0,
                        glow_color,
                        Stroke::NONE,
                        egui::epaint::StrokeKind::Inside,
                    );
                    painter.rect(
                        pad_rect,
                        3.0,
                        hover_color,
                        Stroke::new(1.5 * self.zoom, Color32::from_gray(80)),
                        egui::epaint::StrokeKind::Inside,
                    );
                } else {
                    painter.rect(
                        pad_rect,
                        3.0,
                        base_color,
                        Stroke::new(1.0 * self.zoom, Color32::from_gray(60)),
                        egui::epaint::StrokeKind::Inside,
                    );
                }

                // Draw label inside port
                if !label.is_empty() {
                    painter.text(
                        pad_center,
                        egui::Align2::CENTER_CENTER,
                        label,
                        FontId::proportional(10.0 * self.zoom),
                        Color32::BLACK,
                    );
                }

                // Draw pad label inside block (to the right of input pad)
                if let Some(pad_label) = &external_pad.label {
                    let label_pos = pos2(rect.min.x + port_size * 0.8, pad_center.y);
                    painter.text(
                        label_pos,
                        egui::Align2::LEFT_CENTER,
                        pad_label,
                        FontId::proportional(11.0 * self.zoom),
                        Color32::from_gray(200),
                    );
                }
            }

            // Draw output pads on the right
            let output_count = external_pads.outputs.len();
            for (idx, external_pad) in external_pads.outputs.iter().enumerate() {
                // Calculate vertical position using tighter spacing
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let y_offset =
                    self.calculate_pad_y_offset(idx, output_count, node_height) * self.zoom;

                let pad_center = pos2(rect.max.x, rect.min.y + y_offset);
                let pad_rect = Rect::from_center_size(pad_center, vec2(port_size, port_size));

                let is_hovered = self
                    .hovered_pad
                    .as_ref()
                    .map(|(id, pad)| id == &block.id && pad == &external_pad.name)
                    .unwrap_or(false);

                // Choose color based on media type
                let (base_color, hover_color, glow_color, label) = match external_pad.media_type {
                    MediaType::Audio => (
                        Color32::from_rgb(100, 200, 100), // Green
                        Color32::from_rgb(126, 232, 126),
                        Color32::from_rgba_premultiplied(100, 200, 100, 77),
                        "A",
                    ),
                    MediaType::Video => (
                        Color32::from_rgb(255, 150, 100), // Orange
                        Color32::from_rgb(255, 176, 128),
                        Color32::from_rgba_premultiplied(255, 150, 100, 77),
                        "V",
                    ),
                    MediaType::Generic => (
                        Color32::from_rgb(100, 150, 255), // Blue
                        Color32::from_rgb(126, 176, 255),
                        Color32::from_rgba_premultiplied(100, 150, 255, 77),
                        "",
                    ),
                };

                if is_hovered {
                    // Draw glow effect
                    let glow_rect = Rect::from_center_size(
                        pad_center,
                        vec2(port_size + 10.0 * self.zoom, port_size + 10.0 * self.zoom),
                    );
                    painter.rect(
                        glow_rect,
                        3.0,
                        glow_color,
                        Stroke::NONE,
                        egui::epaint::StrokeKind::Inside,
                    );
                    painter.rect(
                        pad_rect,
                        3.0,
                        hover_color,
                        Stroke::new(1.5 * self.zoom, Color32::from_gray(80)),
                        egui::epaint::StrokeKind::Inside,
                    );
                } else {
                    painter.rect(
                        pad_rect,
                        3.0,
                        base_color,
                        Stroke::new(1.0 * self.zoom, Color32::from_gray(60)),
                        egui::epaint::StrokeKind::Inside,
                    );
                }

                // Draw label inside port
                if !label.is_empty() {
                    painter.text(
                        pad_center,
                        egui::Align2::CENTER_CENTER,
                        label,
                        FontId::proportional(10.0 * self.zoom),
                        Color32::BLACK,
                    );
                }

                // Draw pad label inside block (to the left of output pad)
                if let Some(pad_label) = &external_pad.label {
                    let label_pos = pos2(rect.max.x - port_size * 0.8, pad_center.y);
                    painter.text(
                        label_pos,
                        egui::Align2::RIGHT_CENTER,
                        pad_label,
                        FontId::proportional(11.0 * self.zoom),
                        Color32::from_gray(200),
                    );
                }
            }
        }

        // Return the block response that was created early (before render_callback)
        block_response
    }
}
