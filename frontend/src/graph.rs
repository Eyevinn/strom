//! Node-based graph editor for GStreamer pipelines.

use egui::{pos2, vec2, Color32, FontId, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};
use std::collections::HashMap;
use strom_types::{
    element::{ElementInfo, PadInfo},
    BlockDefinition, BlockInstance, Element, ElementId, Link, MediaType,
};
use uuid::Uuid;

use crate::app::set_local_storage;

/// Grid size for snapping (in world coordinates)
const GRID_SIZE: f32 = 50.0;

/// Snap a value to the grid
fn snap_to_grid(value: f32) -> f32 {
    (value / GRID_SIZE).round() * GRID_SIZE
}

/// Represents the state of the graph editor.
pub struct GraphEditor {
    /// Elements (nodes) in the graph
    pub elements: Vec<Element>,
    /// Block instances in the graph
    pub blocks: Vec<BlockInstance>,
    /// Links (edges) between elements
    pub links: Vec<Link>,
    /// Element metadata (type -> info) for rendering ports
    element_info_map: HashMap<String, ElementInfo>,
    /// Block definitions (id -> definition) for rendering ports and properties
    block_definition_map: HashMap<String, BlockDefinition>,
    /// Dynamic content info per block (block_id -> content info)
    block_content_map: HashMap<String, BlockContentInfo>,
    /// Runtime dynamic pads from the backend (element_id -> pad_name -> tee_name)
    /// These are pads created at runtime (e.g., by decodebin) that weren't in the original flow.
    runtime_dynamic_pads: HashMap<String, HashMap<String, String>>,
    /// Currently selected element ID
    pub selected: Option<ElementId>,
    /// Element being dragged
    dragging: Option<ElementId>,
    /// Offset for panning the canvas
    pub pan_offset: Vec2,
    /// Zoom level
    pub zoom: f32,
    /// Link being created (source element and pad)
    creating_link: Option<(ElementId, String)>,
    /// Hover state for pads (element_id, pad_name)
    hovered_pad: Option<(ElementId, String)>,
    /// Hover state for elements
    hovered_element: Option<ElementId>,
    /// Currently selected link index
    selected_link: Option<usize>,
    /// Hovered link index
    hovered_link: Option<usize>,
    /// Active property tab and focused pad
    pub active_property_tab: PropertyTab,
    /// Pad to focus/highlight in the active tab
    pub focused_pad: Option<String>,
    /// QoS health status per element (for rendering indicators)
    qos_health_map: HashMap<String, crate::qos_monitor::QoSHealth>,
    /// Last known canvas rect (for centering calculations)
    last_canvas_rect: Option<egui::Rect>,
    /// Flag indicating a QoS marker was clicked (to signal log panel should open)
    /// Uses Cell for interior mutability since draw_* functions take &self
    qos_marker_clicked: std::cell::Cell<bool>,
}

/// Property panel tab selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyTab {
    Element,
    InputPads,
    OutputPads,
}

/// Represents a pad to render in the graph, either a static pad or a dynamic pad instance.
#[derive(Debug, Clone)]
struct PadToRender {
    /// The actual pad name (e.g., "sink_0" for a request pad, or "sink" for a static pad)
    name: String,
    /// The template name (e.g., "sink_%u" for request pads, or "sink" for static pads)
    template_name: String,
    /// Media type for coloring
    media_type: strom_types::element::MediaType,
    /// Whether this is the "empty" pad (always unconnected, for creating new links)
    is_empty: bool,
}

/// Callback type for rendering custom block content
pub type BlockRenderCallback = Box<dyn Fn(&mut egui::Ui, egui::Rect) + 'static>;

/// Dynamic content information for a block (e.g., meter visualization).
/// This allows the graph editor to remain generic while supporting blocks with custom content.
pub struct BlockContentInfo {
    /// Additional height for dynamic content (beyond base node height)
    pub additional_height: f32,
    /// Optional render callback for custom content within the block node
    pub render_callback: Option<BlockRenderCallback>,
}

impl Default for GraphEditor {
    fn default() -> Self {
        Self {
            elements: Vec::new(),
            blocks: Vec::new(),
            links: Vec::new(),
            element_info_map: HashMap::new(),
            block_definition_map: HashMap::new(),
            block_content_map: HashMap::new(),
            runtime_dynamic_pads: HashMap::new(),
            selected: None,
            dragging: None,
            pan_offset: Vec2::ZERO,
            zoom: 1.0,
            creating_link: None,
            hovered_pad: None,
            hovered_element: None,
            selected_link: None,
            hovered_link: None,
            active_property_tab: PropertyTab::Element,
            focused_pad: None,
            qos_health_map: HashMap::new(),
            last_canvas_rect: None,
            qos_marker_clicked: std::cell::Cell::new(false),
        }
    }
}

impl GraphEditor {
    /// Create a new graph editor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the QoS health map for rendering indicators on nodes
    pub fn set_qos_health_map(
        &mut self,
        health_map: HashMap<String, crate::qos_monitor::QoSHealth>,
    ) {
        self.qos_health_map = health_map;
    }

    /// Calculate the vertical offset for a pad given its index and total count.
    /// Uses tighter spacing (20 pixels between pads) instead of spreading across the full height.
    fn calculate_pad_y_offset(&self, idx: usize, count: usize, node_height: f32) -> f32 {
        if count == 1 {
            // Single pad: center it
            node_height / 2.0
        } else {
            // Multiple pads: use fixed spacing
            const PAD_SPACING: f32 = 20.0;
            const TOP_MARGIN: f32 = 60.0; // Start below the element label

            TOP_MARGIN + (idx as f32 * PAD_SPACING)
        }
    }

    /// Load elements, blocks, and links into the editor.
    pub fn load(&mut self, elements: Vec<Element>, links: Vec<Link>) {
        self.elements = elements;
        self.links = links;
        self.selected = None;
        self.dragging = None;
        self.creating_link = None;
        self.hovered_element = None;
    }

    /// Load blocks into the editor (used when loading from backend).
    pub fn load_blocks(&mut self, blocks: Vec<BlockInstance>) {
        self.blocks = blocks;
    }

    /// Update runtime data for blocks without replacing the entire array.
    /// This preserves UI state while updating runtime fields like SDP.
    pub fn update_blocks_runtime_data(&mut self, updated_blocks: &[BlockInstance]) {
        for updated_block in updated_blocks {
            if let Some(existing_block) = self.blocks.iter_mut().find(|b| b.id == updated_block.id)
            {
                // Only update runtime_data, preserve other fields
                existing_block.runtime_data = updated_block.runtime_data.clone();
            }
        }
    }

    /// Add a new element to the graph at the given position.
    pub fn add_element(&mut self, element_type: String, pos: Pos2) {
        let id = format!("e{}", Uuid::new_v4().simple());
        let element = Element {
            id: id.clone(),
            element_type,
            properties: HashMap::new(),
            pad_properties: HashMap::new(),
            position: (pos.x, pos.y),
        };
        self.elements.push(element);
    }

    /// Add a new block instance to the graph at the given position.
    pub fn add_block(&mut self, block_definition_id: String, pos: Pos2) {
        let id = format!("b{}", Uuid::new_v4().simple());
        let block = BlockInstance {
            id: id.clone(),
            block_definition_id,
            name: None,
            properties: HashMap::new(),
            position: strom_types::block::Position { x: pos.x, y: pos.y },
            runtime_data: None,
            computed_external_pads: None,
        };
        self.blocks.push(block);
    }

    /// Remove the currently selected element or block.
    pub fn remove_selected(&mut self) {
        if let Some(id) = &self.selected {
            // Check if it's an element or block by looking for it in the respective lists
            // (Note: we can't just check id.starts_with('e') because gst-launch imports
            // create IDs like "filesrc_0", "decodebin_1" that don't start with 'e')
            let is_element = self.elements.iter().any(|e| &e.id == id);
            let is_block = self.blocks.iter().any(|b| &b.id == id);

            if is_element {
                // Remove element
                self.elements.retain(|e| &e.id != id);
            } else if is_block {
                // Remove block
                self.blocks.retain(|b| &b.id != id);
            }

            // Remove links connected to this element or block
            let id_clone = id.clone();
            self.links.retain(|link| {
                !link.from.starts_with(&id_clone) && !link.to.starts_with(&id_clone)
            });
            self.selected = None;
        }
    }

    /// Remove the currently selected link.
    pub fn remove_selected_link(&mut self) {
        if let Some(idx) = self.selected_link {
            if idx < self.links.len() {
                self.links.remove(idx);
                self.selected_link = None;
            }
        }
    }

    /// Deselect all elements and links.
    pub fn deselect_all(&mut self) {
        self.selected = None;
        self.selected_link = None;
    }

    /// Check if anything is selected in the graph (element, block, or link).
    pub fn has_selection(&self) -> bool {
        self.selected.is_some() || self.selected_link.is_some()
    }

    /// Check if a QoS marker was clicked during the last frame.
    pub fn was_qos_marker_clicked(&self) -> bool {
        self.qos_marker_clicked.get()
    }

    /// Select a node (element) by its ID.
    pub fn select_node(&mut self, id: ElementId) {
        self.selected = Some(id);
        self.selected_link = None;
    }

    /// Select a block by its ID.
    pub fn select_block(&mut self, id: &str) {
        self.selected = Some(id.to_string());
        self.selected_link = None;
    }

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

    /// Set element metadata for rendering ports.
    pub fn set_element_info(&mut self, element_type: String, info: ElementInfo) {
        self.element_info_map.insert(element_type, info);
    }

    /// Set all element metadata at once.
    pub fn set_all_element_info(&mut self, infos: Vec<ElementInfo>) {
        self.element_info_map.clear();
        for info in infos {
            self.element_info_map.insert(info.name.clone(), info);
        }
    }

    /// Set all block definitions at once.
    pub fn set_all_block_definitions(&mut self, definitions: Vec<BlockDefinition>) {
        self.block_definition_map.clear();
        for def in definitions {
            self.block_definition_map.insert(def.id.clone(), def);
        }
    }

    /// Set dynamic content info for a specific block.
    /// This allows blocks to have custom rendering and dynamic height.
    pub fn set_block_content(&mut self, block_id: String, content_info: BlockContentInfo) {
        self.block_content_map.insert(block_id, content_info);
    }

    /// Clear all block content info (typically called before re-rendering).
    pub fn clear_block_content(&mut self) {
        self.block_content_map.clear();
    }

    /// Set runtime dynamic pads for the current flow.
    /// These are pads created at runtime by elements like decodebin that aren't in the flow definition.
    /// The map is element_id -> (pad_name -> tee_name).
    pub fn set_runtime_dynamic_pads(&mut self, pads: HashMap<String, HashMap<String, String>>) {
        self.runtime_dynamic_pads = pads;
    }

    /// Clear runtime dynamic pads (e.g., when flow stops or changes).
    pub fn clear_runtime_dynamic_pads(&mut self) {
        self.runtime_dynamic_pads.clear();
    }

    /// Get the currently selected block instance.
    pub fn get_selected_block(&self) -> Option<&BlockInstance> {
        self.selected
            .as_ref()
            .and_then(|id| self.blocks.iter().find(|b| &b.id == id))
    }

    /// Get a mutable reference to the currently selected block.
    pub fn get_selected_block_mut(&mut self) -> Option<&mut BlockInstance> {
        let selected_id = self.selected.clone()?;
        self.blocks.iter_mut().find(|b| b.id == selected_id)
    }

    /// Get the block definition for a block instance.
    pub fn get_block_definition(&self, block: &BlockInstance) -> Option<&BlockDefinition> {
        self.block_definition_map.get(&block.block_definition_id)
    }

    /// Get a block definition by ID.
    pub fn get_block_definition_by_id(&self, definition_id: &str) -> Option<&BlockDefinition> {
        self.block_definition_map.get(definition_id)
    }

    /// Get the effective external pads for a block instance.
    /// Uses computed_external_pads if available, otherwise falls back to the static definition pads.
    fn get_block_external_pads<'a>(
        &'a self,
        block: &'a BlockInstance,
        definition: Option<&'a BlockDefinition>,
    ) -> Option<&'a strom_types::ExternalPads> {
        // First try to use computed pads from the block instance
        if let Some(ref computed_pads) = block.computed_external_pads {
            return Some(computed_pads);
        }

        // Fall back to definition's static pads
        definition.map(|def| &def.external_pads)
    }

    /// Get the list of pads to render for an element, expanding request pads into actual instances.
    /// For request pads, this returns all connected instances plus one empty pad.
    /// Also renders pads from links when element_info doesn't have pad information
    /// (e.g., for elements with Sometimes pads like decodebin).
    fn get_pads_to_render(
        &self,
        element: &Element,
        element_info: Option<&ElementInfo>,
    ) -> (Vec<PadToRender>, Vec<PadToRender>) {
        let Some(info) = element_info else {
            // No element info - fall back to rendering pads from links
            return self.get_pads_from_links(element);
        };

        // If element info has no pads defined, fall back to rendering pads from links
        // This handles elements with Sometimes pads (like decodebin) where discovery
        // returns empty pad lists because the pads don't exist until runtime.
        if info.sink_pads.is_empty() && info.src_pads.is_empty() {
            return self.get_pads_from_links(element);
        }

        let mut sink_pads_to_render = Vec::new();
        let mut src_pads_to_render = Vec::new();

        // Process sink pads
        for pad_info in &info.sink_pads {
            if is_request_pad(pad_info) {
                // Get all connected instances
                let connected =
                    get_connected_request_pad_names(&element.id, &pad_info.name, &self.links, true);

                // Add all connected instances
                for actual_name in &connected {
                    sink_pads_to_render.push(PadToRender {
                        name: actual_name.clone(),
                        template_name: pad_info.name.clone(),
                        media_type: pad_info.media_type,
                        is_empty: false,
                    });
                }

                // Add one empty pad
                let next_name =
                    allocate_next_pad_name(&element.id, &pad_info.name, &self.links, true);
                sink_pads_to_render.push(PadToRender {
                    name: next_name,
                    template_name: pad_info.name.clone(),
                    media_type: pad_info.media_type,
                    is_empty: true,
                });
            } else {
                // Static pad - render as-is
                sink_pads_to_render.push(PadToRender {
                    name: pad_info.name.clone(),
                    template_name: pad_info.name.clone(),
                    media_type: pad_info.media_type,
                    is_empty: false,
                });
            }
        }

        // Process src pads
        for pad_info in &info.src_pads {
            if is_request_pad(pad_info) {
                // Get all connected instances
                let connected = get_connected_request_pad_names(
                    &element.id,
                    &pad_info.name,
                    &self.links,
                    false,
                );

                // Add all connected instances
                for actual_name in &connected {
                    src_pads_to_render.push(PadToRender {
                        name: actual_name.clone(),
                        template_name: pad_info.name.clone(),
                        media_type: pad_info.media_type,
                        is_empty: false,
                    });
                }

                // Add one empty pad
                let next_name =
                    allocate_next_pad_name(&element.id, &pad_info.name, &self.links, false);
                src_pads_to_render.push(PadToRender {
                    name: next_name,
                    template_name: pad_info.name.clone(),
                    media_type: pad_info.media_type,
                    is_empty: true,
                });
            } else {
                // Static pad - render as-is
                src_pads_to_render.push(PadToRender {
                    name: pad_info.name.clone(),
                    template_name: pad_info.name.clone(),
                    media_type: pad_info.media_type,
                    is_empty: false,
                });
            }
        }

        (sink_pads_to_render, src_pads_to_render)
    }

    /// Render the graph editor.
    pub fn show(&mut self, ui: &mut Ui) -> Response {
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
            let pointer_pos = ui.input(|i| i.pointer.hover_pos());
            let pointer_in_canvas = pointer_pos
                .map(|p| response.rect.contains(p))
                .unwrap_or(false);

            if pointer_in_canvas {
                let hover_pos = pointer_pos.unwrap();
                let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
                let pinch_zoom = ui.input(|i| i.zoom_delta());
                let modifiers = ui.input(|i| i.modifiers);

                // Pinch-to-zoom (trackpad) or Ctrl+Scroll or Alt+Scroll
                if pinch_zoom != 1.0 {
                    let zoom_factor = pinch_zoom;
                    self.zoom = (self.zoom * zoom_factor).clamp(0.1, 3.0);

                    // Adjust pan to zoom towards cursor
                    let world_pos = from_screen(hover_pos);
                    let new_screen_pos = to_screen(world_pos);
                    self.pan_offset += hover_pos - new_screen_pos;
                } else if (modifiers.ctrl || modifiers.alt) && scroll_delta.y != 0.0 {
                    // Ctrl+Scroll or Alt+Scroll: Zoom
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

                let node_height = (base_height + content_height).min(400.0);

                let node_rect = Rect::from_min_size(
                    screen_pos,
                    vec2(200.0 * self.zoom, node_height * self.zoom),
                );

                let is_selected = self.selected.as_ref() == Some(&block.id);
                let is_hovered = self.hovered_element.as_ref() == Some(&block.id);

                let node_response =
                    self.draw_block_node(ui, &painter, block, node_rect, is_selected, is_hovered);

                // Track hover state
                if node_response.hovered() {
                    self.hovered_element = Some(block.id.clone());
                } else if self.hovered_element.as_ref() == Some(&block.id) {
                    self.hovered_element = None;
                }

                // Handle node selection
                if node_response.clicked() || (node_response.dragged() && self.dragging.is_none()) {
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
                self.selected = None;
                self.selected_link = None;
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
                                let link = Link {
                                    from: format!("{}:{}", from_id, from_pad),
                                    to: format!("{}:{}", to_id, to_pad),
                                };
                                self.links.push(link);
                            } else if !from_is_output && to_is_output {
                                // Reversed: dragged from input to output, swap them
                                let link = Link {
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
            FontId::proportional(14.0 * self.zoom),
            text_color,
        );

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
    fn draw_block_node(
        &self,
        ui: &mut Ui,
        painter: &egui::Painter,
        block: &BlockInstance,
        rect: Rect,
        is_selected: bool,
        is_hovered: bool,
    ) -> Response {
        // Check for QoS issues
        let qos_health = self.qos_health_map.get(&block.id);
        let has_qos_issues = qos_health
            .map(|h| *h != crate::qos_monitor::QoSHealth::Ok)
            .unwrap_or(false);

        let stroke_color = if has_qos_issues {
            // QoS issues - use warning/critical color for border
            qos_health.unwrap().color()
        } else if ui.visuals().dark_mode {
            // Dark theme borders
            if is_selected {
                Color32::from_rgb(200, 100, 255) // Purple for blocks
            } else if is_hovered {
                Color32::from_gray(154)
            } else {
                Color32::from_rgb(150, 80, 200) // Darker purple
            }
        } else {
            // Light theme borders - vibrant purple/magenta
            if is_selected {
                Color32::from_rgb(160, 0, 200) // Vibrant magenta
            } else if is_hovered {
                Color32::from_rgb(140, 40, 180) // Medium purple
            } else {
                Color32::from_rgb(120, 60, 160) // Darker purple
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
            // Dark theme: dark purple backgrounds
            if is_selected {
                Color32::from_rgb(60, 40, 80)
            } else if is_hovered {
                Color32::from_rgb(50, 35, 65)
            } else {
                Color32::from_rgb(40, 30, 50)
            }
        } else {
            // Light theme: vibrant purple/lavender backgrounds
            if is_selected {
                Color32::from_rgb(220, 180, 255) // Bright lavender
            } else if is_hovered {
                Color32::from_rgb(230, 200, 255) // Lighter lavender
            } else {
                Color32::from_rgb(235, 215, 255) // Soft lavender
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

        // Draw node background with rounded corners
        painter.rect(
            rect,
            5.0,
            fill_color,
            Stroke::new(stroke_width, stroke_color),
            egui::epaint::StrokeKind::Inside,
        );

        // Get the block definition to show the human-readable name
        let block_definition = self.block_definition_map.get(&block.block_definition_id);

        // Draw block icon
        // Note: multiply offsets by zoom since rect is in screen-space
        let icon_pos = rect.min + vec2(10.0 * self.zoom, 8.0 * self.zoom);
        let icon_color = if ui.visuals().dark_mode {
            Color32::WHITE
        } else {
            Color32::from_gray(40) // Dark icon for light backgrounds
        };
        painter.text(
            icon_pos,
            egui::Align2::LEFT_TOP,
            "📦",
            FontId::proportional(16.0 * self.zoom),
            icon_color,
        );

        // Draw block name (use human-readable name from definition if available)
        let block_name = block_definition
            .map(|def| def.name.as_str())
            .unwrap_or_else(|| {
                block
                    .block_definition_id
                    .strip_prefix("builtin.")
                    .unwrap_or(&block.block_definition_id)
            });
        let text_pos = rect.min + vec2(35.0 * self.zoom, 10.0 * self.zoom);
        let text_color = if ui.visuals().dark_mode {
            Color32::from_rgb(220, 180, 255) // Light purple for dark backgrounds
        } else {
            Color32::from_rgb(80, 40, 120) // Dark purple for light backgrounds
        };
        painter.text(
            text_pos,
            egui::Align2::LEFT_TOP,
            block_name,
            FontId::proportional(14.0 * self.zoom),
            text_color,
        );

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

        // Render any dynamic content (e.g., meter visualization)
        if let Some(content_info) = self.block_content_map.get(&block.id) {
            if let Some(ref render_callback) = content_info.render_callback {
                // Calculate content area (below the title, above the pads)
                let content_area = Rect::from_min_size(
                    rect.min + vec2(10.0 * self.zoom, 35.0 * self.zoom),
                    vec2(
                        180.0 * self.zoom,
                        content_info.additional_height * self.zoom,
                    ),
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
            let node_height = (base_height + content_height).min(400.0);

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
            }
        }

        ui.interact(rect, ui.id().with(&block.id), Sense::click_and_drag())
    }

    fn draw_link(
        &self,
        painter: &egui::Painter,
        link: &Link,
        to_screen: &impl Fn(Pos2) -> Pos2,
        is_selected: bool,
        is_hovered: bool,
    ) {
        // Parse IDs and pad names from link
        let (from_id, from_pad) = parse_pad_ref(&link.from).unwrap_or_default();
        let (to_id, to_pad) = parse_pad_ref(&link.to).unwrap_or_default();

        // Get from pad position
        let from_pos = self.get_pad_position(&from_id, &from_pad, false);

        // Get to pad position
        let to_pos = self.get_pad_position(&to_id, &to_pad, true);

        if let (Some(from), Some(to)) = (from_pos, to_pos) {
            let from_screen = to_screen(from);
            let to_screen_pos = to_screen(to);

            // Draw cubic bezier curve
            let control_offset = 50.0 * self.zoom;
            let control1 = from_screen + vec2(control_offset, 0.0);
            let control2 = to_screen_pos - vec2(control_offset, 0.0);

            // Determine color and width based on state
            let (color, width) = if is_selected {
                (Color32::from_rgb(100, 150, 255), 3.0) // Blue and thicker when selected
            } else if is_hovered {
                (Color32::from_rgb(200, 200, 200), 2.5) // Brighter when hovered
            } else {
                (Color32::from_rgb(150, 150, 150), 2.0) // Default gray
            };

            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                [from_screen, control1, control2, to_screen_pos],
                false,
                Color32::TRANSPARENT,
                Stroke::new(width, color),
            ));
        }
    }

    /// Get the world position of a specific pad on an element or block.
    /// Returns position on the right edge for output pads, left edge for input pads.
    fn get_pad_position(&self, element_id: &str, pad_name: &str, is_input: bool) -> Option<Pos2> {
        // Try to find as element first
        if let Some(element) = self.elements.iter().find(|e| e.id == element_id) {
            let base_pos = pos2(element.position.0, element.position.1);
            let element_info = self.element_info_map.get(&element.element_type);

            // Get pads to render (same as in draw_node)
            let (sink_pads_to_render, src_pads_to_render) =
                self.get_pads_to_render(element, element_info);

            // Calculate node height (same as in show method)
            let pad_count = sink_pads_to_render
                .len()
                .max(src_pads_to_render.len())
                .max(1);
            let node_height = (80.0 + (pad_count.saturating_sub(1) * 30) as f32).min(400.0);

            if is_input {
                // Find the pad in sink_pads_to_render
                if let Some(idx) = sink_pads_to_render.iter().position(|p| p.name == pad_name) {
                    let sink_count = sink_pads_to_render.len();
                    let y_offset = self.calculate_pad_y_offset(idx, sink_count, node_height);
                    return Some(pos2(base_pos.x, base_pos.y + y_offset));
                }
            } else {
                // Find the pad in src_pads_to_render
                if let Some(idx) = src_pads_to_render.iter().position(|p| p.name == pad_name) {
                    let src_count = src_pads_to_render.len();
                    let y_offset = self.calculate_pad_y_offset(idx, src_count, node_height);
                    return Some(pos2(base_pos.x + 200.0, base_pos.y + y_offset));
                }
            }

            // Fallback to center if pad not found
            return Some(pos2(
                base_pos.x + if is_input { 0.0 } else { 200.0 },
                base_pos.y + node_height / 2.0,
            ));
        }

        // Try to find as block
        if let Some(block) = self.blocks.iter().find(|b| b.id == element_id) {
            let base_pos = pos2(block.position.x, block.position.y);
            let block_definition = self.block_definition_map.get(&block.block_definition_id);

            if let Some(external_pads) = self.get_block_external_pads(block, block_definition) {
                // Calculate node height (same as in show method)
                let pad_count = external_pads.inputs.len().max(external_pads.outputs.len());

                // Base height for block node
                let base_height = 80.0 + (pad_count.saturating_sub(1) * 30) as f32;

                // Add any dynamic content height (provided by caller)
                let content_height = self
                    .block_content_map
                    .get(&block.id)
                    .map(|info| info.additional_height)
                    .unwrap_or(0.0);

                let node_height = (base_height + content_height).min(400.0);

                if is_input {
                    // Find the pad in inputs
                    if let Some(idx) = external_pads.inputs.iter().position(|p| p.name == pad_name)
                    {
                        let input_count = external_pads.inputs.len();
                        let y_offset = self.calculate_pad_y_offset(idx, input_count, node_height);
                        return Some(pos2(base_pos.x, base_pos.y + y_offset));
                    }
                } else {
                    // Find the pad in outputs
                    if let Some(idx) = external_pads
                        .outputs
                        .iter()
                        .position(|p| p.name == pad_name)
                    {
                        let output_count = external_pads.outputs.len();
                        let y_offset = self.calculate_pad_y_offset(idx, output_count, node_height);
                        return Some(pos2(base_pos.x + 200.0, base_pos.y + y_offset));
                    }
                }

                // Fallback to center if pad not found
                return Some(pos2(
                    base_pos.x + if is_input { 0.0 } else { 200.0 },
                    base_pos.y + node_height / 2.0,
                ));
            }
        }

        None
    }

    /// Check if a point is near a bezier curve (for click detection)
    fn is_point_near_link(
        &self,
        link: &Link,
        point: Pos2,
        to_screen: &impl Fn(Pos2) -> Pos2,
    ) -> bool {
        // Parse IDs and pad names from link
        let (from_id, from_pad) = parse_pad_ref(&link.from).unwrap_or_default();
        let (to_id, to_pad) = parse_pad_ref(&link.to).unwrap_or_default();

        // Get from pad position
        let from_pos = self.get_pad_position(&from_id, &from_pad, false);

        // Get to pad position
        let to_pos = self.get_pad_position(&to_id, &to_pad, true);

        if let (Some(from), Some(to)) = (from_pos, to_pos) {
            let from_screen = to_screen(from);
            let to_screen_pos = to_screen(to);

            let control_offset = 50.0 * self.zoom;
            let control1 = from_screen + vec2(control_offset, 0.0);
            let control2 = to_screen_pos - vec2(control_offset, 0.0);

            // Sample points along the bezier curve and check distance
            let threshold = 10.0; // Click detection threshold in pixels
            let samples = 20;

            for i in 0..=samples {
                let t = i as f32 / samples as f32;
                let bezier_point =
                    self.evaluate_cubic_bezier(from_screen, control1, control2, to_screen_pos, t);

                let distance = point.distance(bezier_point);
                if distance < threshold {
                    return true;
                }
            }
        }

        false
    }

    /// Evaluate a cubic bezier curve at parameter t
    fn evaluate_cubic_bezier(&self, p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, t: f32) -> Pos2 {
        let t2 = t * t;
        let t3 = t2 * t;
        let mt = 1.0 - t;
        let mt2 = mt * mt;
        let mt3 = mt2 * mt;

        pos2(
            mt3 * p0.x + 3.0 * mt2 * t * p1.x + 3.0 * mt * t2 * p2.x + t3 * p3.x,
            mt3 * p0.y + 3.0 * mt2 * t * p1.y + 3.0 * mt * t2 * p2.y + t3 * p3.y,
        )
    }

    fn handle_pad_interaction(
        &mut self,
        ui: &Ui,
        element: &Element,
        element_info: Option<&ElementInfo>,
        rect: Rect,
    ) {
        let port_size = 16.0 * self.zoom;
        let interaction_size = port_size + 4.0 * self.zoom; // Slightly larger for easier interaction

        let mut any_hovered = false;

        // Get pads to render (same as draw_node)
        let (sink_pads_to_render, src_pads_to_render) =
            self.get_pads_to_render(element, element_info);

        if !sink_pads_to_render.is_empty() || !src_pads_to_render.is_empty() {
            // Handle sink pad interactions (inputs)
            let sink_count = sink_pads_to_render.len();
            for (idx, pad_to_render) in sink_pads_to_render.iter().enumerate() {
                // Calculate vertical position using tighter spacing (matching draw_node)
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let pad_count = sink_pads_to_render
                    .len()
                    .max(src_pads_to_render.len())
                    .max(1);
                let node_height = (80.0 + (pad_count.saturating_sub(1) * 30) as f32).min(400.0);
                let y_offset =
                    self.calculate_pad_y_offset(idx, sink_count, node_height) * self.zoom;

                let pad_center = pos2(rect.min.x, rect.min.y + y_offset);
                let pad_rect =
                    Rect::from_center_size(pad_center, vec2(interaction_size, interaction_size));

                let pad_response = ui.interact(
                    pad_rect,
                    ui.id().with((&element.id, &pad_to_render.name)),
                    Sense::click_and_drag(),
                );

                // Select element and switch to Input Pads tab when clicking input pad
                // (skip empty pads for selection)
                if pad_response.clicked() && !pad_response.dragged() && !pad_to_render.is_empty {
                    self.select_element_and_focus_pad(&element.id, &pad_to_render.name, true);
                }

                // Start creating link when dragging from input port
                if pad_response.drag_started()
                    || (pad_response.dragged() && self.creating_link.is_none())
                {
                    self.creating_link = Some((element.id.clone(), pad_to_render.name.clone()));
                }

                if pad_response.hovered() {
                    self.hovered_pad = Some((element.id.clone(), pad_to_render.name.clone()));
                    any_hovered = true;
                }
            }

            // Handle src pad interactions (outputs)
            let src_count = src_pads_to_render.len();
            for (idx, pad_to_render) in src_pads_to_render.iter().enumerate() {
                // Calculate vertical position using tighter spacing (matching draw_node)
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let pad_count = sink_pads_to_render
                    .len()
                    .max(src_pads_to_render.len())
                    .max(1);
                let node_height = (80.0 + (pad_count.saturating_sub(1) * 30) as f32).min(400.0);
                let y_offset = self.calculate_pad_y_offset(idx, src_count, node_height) * self.zoom;

                let pad_center = pos2(rect.max.x, rect.min.y + y_offset);
                let pad_rect =
                    Rect::from_center_size(pad_center, vec2(interaction_size, interaction_size));

                let pad_response = ui.interact(
                    pad_rect,
                    ui.id().with((&element.id, &pad_to_render.name)),
                    Sense::click_and_drag(),
                );

                // Select element and switch to Output Pads tab when clicking output pad
                // (skip empty pads for selection)
                if pad_response.clicked() && !pad_response.dragged() && !pad_to_render.is_empty {
                    self.select_element_and_focus_pad(&element.id, &pad_to_render.name, false);
                }

                // Start creating link when dragging from output port
                if pad_response.drag_started()
                    || (pad_response.dragged() && self.creating_link.is_none())
                {
                    self.creating_link = Some((element.id.clone(), pad_to_render.name.clone()));
                }

                if pad_response.hovered() {
                    self.hovered_pad = Some((element.id.clone(), pad_to_render.name.clone()));
                    any_hovered = true;
                }
            }
        } else {
            // Fallback for elements without metadata
            let is_source = element.element_type.ends_with("src");
            let is_sink = element.element_type.ends_with("sink");

            // Output pad (src)
            if !is_sink {
                let output_center = pos2(rect.max.x, rect.center().y);
                let output_rect =
                    Rect::from_center_size(output_center, vec2(interaction_size, interaction_size));
                let output_response = ui.interact(
                    output_rect,
                    ui.id().with((&element.id, "src")),
                    Sense::click_and_drag(),
                );

                if output_response.clicked() && !output_response.dragged() {
                    self.select_element_and_focus_pad(&element.id, "src", false);
                }

                if output_response.drag_started()
                    || (output_response.dragged() && self.creating_link.is_none())
                {
                    self.creating_link = Some((element.id.clone(), "src".to_string()));
                }

                if output_response.hovered() {
                    self.hovered_pad = Some((element.id.clone(), "src".to_string()));
                    any_hovered = true;
                }
            }

            // Input pad (sink)
            if !is_source {
                let input_center = pos2(rect.min.x, rect.center().y);
                let input_rect =
                    Rect::from_center_size(input_center, vec2(interaction_size, interaction_size));
                let input_response = ui.interact(
                    input_rect,
                    ui.id().with((&element.id, "sink")),
                    Sense::click_and_drag(),
                );

                if input_response.clicked() && !input_response.dragged() {
                    self.select_element_and_focus_pad(&element.id, "sink", true);
                }

                // Start creating link when dragging from input port
                if input_response.drag_started()
                    || (input_response.dragged() && self.creating_link.is_none())
                {
                    self.creating_link = Some((element.id.clone(), "sink".to_string()));
                }

                if input_response.hovered() {
                    self.hovered_pad = Some((element.id.clone(), "sink".to_string()));
                    any_hovered = true;
                }
            }
        }

        // Clear hovered_pad if mouse is not over any pad for this element
        if !any_hovered {
            if let Some((id, _)) = &self.hovered_pad {
                if id == &element.id {
                    self.hovered_pad = None;
                }
            }
        }
    }

    /// Get the currently selected element.
    pub fn get_selected_element(&self) -> Option<&Element> {
        self.selected
            .as_ref()
            .and_then(|id| self.elements.iter().find(|e| &e.id == id))
    }

    /// Get a mutable reference to the currently selected element.
    pub fn get_selected_element_mut(&mut self) -> Option<&mut Element> {
        let selected_id = self.selected.clone()?;
        self.elements.iter_mut().find(|e| e.id == selected_id)
    }

    /// Collect actual input pad names for an element from links.
    ///
    /// This discovers dynamic/request pads (like sink_0, sink_1 on audiomixer)
    /// by examining which pads are actually connected in the links.
    pub fn get_actual_input_pads(&self, element_id: &str) -> Vec<String> {
        let mut pads = std::collections::HashSet::new();

        for link in &self.links {
            // Extract element ID and pad name from link.to
            if let Some((to_elem_id, pad_name)) = parse_pad_ref(&link.to) {
                if to_elem_id == element_id {
                    pads.insert(pad_name);
                }
            }
        }

        let mut result: Vec<String> = pads.into_iter().collect();
        result.sort();
        result
    }

    /// Collect actual output pad names for an element from links.
    pub fn get_actual_output_pads(&self, element_id: &str) -> Vec<String> {
        let mut pads = std::collections::HashSet::new();

        for link in &self.links {
            // Extract element ID and pad name from link.from
            if let Some((from_elem_id, pad_name)) = parse_pad_ref(&link.from) {
                if from_elem_id == element_id {
                    pads.insert(pad_name);
                }
            }
        }

        let mut result: Vec<String> = pads.into_iter().collect();
        result.sort();
        result
    }

    /// Get pads to render from link data when element discovery doesn't provide pad info.
    /// This is used for elements with Sometimes pads (like decodebin) where pads
    /// don't exist until runtime.
    fn get_pads_from_links(&self, element: &Element) -> (Vec<PadToRender>, Vec<PadToRender>) {
        let sink_pads = self.get_actual_input_pads(&element.id);
        let mut src_pads = self.get_actual_output_pads(&element.id);

        // Add runtime dynamic pads that aren't already in the list
        // These are pads created by elements like decodebin that don't have links defined
        if let Some(dynamic_pads) = self.runtime_dynamic_pads.get(&element.id) {
            for pad_name in dynamic_pads.keys() {
                if !src_pads.contains(pad_name) {
                    src_pads.push(pad_name.clone());
                }
            }
            src_pads.sort();
        }

        let sink_pads_to_render: Vec<PadToRender> = sink_pads
            .into_iter()
            .map(|name| PadToRender {
                name: name.clone(),
                template_name: name,
                media_type: MediaType::Generic,
                is_empty: false,
            })
            .collect();

        let src_pads_to_render: Vec<PadToRender> = src_pads
            .into_iter()
            .map(|name| PadToRender {
                name: name.clone(),
                template_name: name,
                media_type: MediaType::Generic,
                is_empty: false,
            })
            .collect();

        (sink_pads_to_render, src_pads_to_render)
    }

    /// Select element and switch to appropriate property tab when clicking a pad.
    fn select_element_and_focus_pad(&mut self, element_id: &str, pad_name: &str, is_input: bool) {
        self.selected = Some(element_id.to_string());
        self.selected_link = None;
        self.focused_pad = Some(pad_name.to_string());
        self.active_property_tab = if is_input {
            PropertyTab::InputPads
        } else {
            PropertyTab::OutputPads
        };
    }

    /// Check if a pad is an output pad (src) or input pad (sink).
    /// Returns true if it's an output pad, false if it's an input pad.
    fn is_output_pad(&self, element_id: &str, pad_name: &str) -> bool {
        // Try to find as element first
        if let Some(element) = self.elements.iter().find(|e| e.id == element_id) {
            let element_info = self.element_info_map.get(&element.element_type);

            if let Some(info) = element_info {
                // Check if pad is in src_pads
                let (sink_pads, src_pads) = self.get_pads_to_render(element, Some(info));

                // Check if it's in src_pads_to_render
                if src_pads.iter().any(|p| p.name == pad_name) {
                    return true;
                }

                // Check if it's in sink_pads_to_render
                if sink_pads.iter().any(|p| p.name == pad_name) {
                    return false;
                }
            }

            // Fallback: check by naming convention
            if pad_name == "src" {
                return true;
            }
            if pad_name == "sink" {
                return false;
            }

            // For elements without metadata, assume based on element type
            if element.element_type.ends_with("src") {
                return true; // Source elements have output pads
            }
            if element.element_type.ends_with("sink") {
                return false; // Sink elements have input pads
            }

            // Default: assume it's an output if we can't determine
            return true;
        }

        // Try to find as block
        if let Some(block) = self.blocks.iter().find(|b| b.id == element_id) {
            let block_definition = self.block_definition_map.get(&block.block_definition_id);
            if let Some(external_pads) = self.get_block_external_pads(block, block_definition) {
                // Check if it's in outputs
                if external_pads.outputs.iter().any(|p| p.name == pad_name) {
                    return true;
                }

                // Check if it's in inputs
                if external_pads.inputs.iter().any(|p| p.name == pad_name) {
                    return false;
                }
            }
        }

        // Default: assume output if we can't determine
        true
    }

    /// Handle pad interactions for blocks.
    fn handle_block_pad_interaction(
        &mut self,
        ui: &Ui,
        block: &BlockInstance,
        definition: Option<&BlockDefinition>,
        rect: Rect,
    ) {
        let port_size = 16.0 * self.zoom;
        let interaction_size = port_size + 4.0 * self.zoom;

        let mut any_hovered = false;

        // Clone external_pads to avoid borrow checker issues
        let external_pads_clone = self.get_block_external_pads(block, definition).cloned();

        if let Some(external_pads) = external_pads_clone {
            // Calculate node height (same calculation as in get_pad_position for consistency)
            let pad_count = external_pads.inputs.len().max(external_pads.outputs.len());
            let base_height = 80.0 + (pad_count.saturating_sub(1) * 30) as f32;
            let content_height = self
                .block_content_map
                .get(&block.id)
                .map(|info| info.additional_height)
                .unwrap_or(0.0);
            let node_height = (base_height + content_height).min(400.0);

            // Handle input pad interactions
            let input_count = external_pads.inputs.len();
            for (idx, external_pad) in external_pads.inputs.iter().enumerate() {
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let y_offset =
                    self.calculate_pad_y_offset(idx, input_count, node_height) * self.zoom;

                let pad_center = pos2(rect.min.x, rect.min.y + y_offset);
                let pad_rect =
                    Rect::from_center_size(pad_center, vec2(interaction_size, interaction_size));

                let pad_response = ui.interact(
                    pad_rect,
                    ui.id().with((&block.id, &external_pad.name)),
                    Sense::click_and_drag(),
                );

                // Start creating link when dragging from input port
                if pad_response.drag_started()
                    || (pad_response.dragged() && self.creating_link.is_none())
                {
                    self.creating_link = Some((block.id.clone(), external_pad.name.clone()));
                }

                if pad_response.hovered() {
                    self.hovered_pad = Some((block.id.clone(), external_pad.name.clone()));
                    any_hovered = true;
                }
            }

            // Handle output pad interactions
            let output_count = external_pads.outputs.len();
            for (idx, external_pad) in external_pads.outputs.iter().enumerate() {
                // Note: calculate_pad_y_offset returns world-space offset, multiply by zoom for screen space
                let y_offset =
                    self.calculate_pad_y_offset(idx, output_count, node_height) * self.zoom;

                let pad_center = pos2(rect.max.x, rect.min.y + y_offset);
                let pad_rect =
                    Rect::from_center_size(pad_center, vec2(interaction_size, interaction_size));

                let pad_response = ui.interact(
                    pad_rect,
                    ui.id().with((&block.id, &external_pad.name)),
                    Sense::click_and_drag(),
                );

                // Start creating link when dragging from output port
                if pad_response.drag_started()
                    || (pad_response.dragged() && self.creating_link.is_none())
                {
                    self.creating_link = Some((block.id.clone(), external_pad.name.clone()));
                }

                if pad_response.hovered() {
                    self.hovered_pad = Some((block.id.clone(), external_pad.name.clone()));
                    any_hovered = true;
                }
            }
        }

        // Clear hovered_pad if mouse is not over any pad for this block
        if !any_hovered {
            if let Some((id, _)) = &self.hovered_pad {
                if id == &block.id {
                    self.hovered_pad = None;
                }
            }
        }
    }
}

/// Parse a pad reference like "element_id:pad_name" into (element_id, pad_name).
fn parse_pad_ref(pad_ref: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = pad_ref.split(':').collect();
    if parts.len() >= 2 {
        Some((parts[0].to_string(), parts[1..].join(":")))
    } else {
        None
    }
}

/// Check if a pad is a request pad (dynamic pad with template like "sink_%u").
fn is_request_pad(pad_info: &PadInfo) -> bool {
    use strom_types::element::PadPresence;

    // Check presence type
    if pad_info.presence == PadPresence::Request {
        return true;
    }

    // Also check for template patterns in the name
    pad_info.name.contains("%u") || pad_info.name.contains("%d") || pad_info.name.contains("%s")
}

/// Generate an actual pad name from a template by replacing %u with a number.
/// For example: "sink_%u" with index 0 becomes "sink_0"
fn generate_pad_name(template: &str, index: usize) -> String {
    template
        .replace("%u", &index.to_string())
        .replace("%d", &index.to_string())
}

/// Get all connected pad names for a request pad template.
/// For example, if template is "sink_%u" and there are links to "sink_0" and "sink_2",
/// this returns vec!["sink_0", "sink_2"].
fn get_connected_request_pad_names(
    element_id: &str,
    template: &str,
    links: &[Link],
    is_sink: bool,
) -> Vec<String> {
    let mut pad_names = std::collections::HashSet::new();

    // Extract the pattern (e.g., "sink_" from "sink_%u")
    let pattern = template
        .replace("%u", "")
        .replace("%d", "")
        .replace("%s", "");

    for link in links {
        let pad_ref = if is_sink { &link.to } else { &link.from };

        if let Some((elem_id, pad_name)) = parse_pad_ref(pad_ref) {
            if elem_id == element_id && pad_name.starts_with(&pattern) {
                pad_names.insert(pad_name);
            }
        }
    }

    let mut result: Vec<String> = pad_names.into_iter().collect();
    result.sort();
    result
}

/// Allocate the next available pad name for a request pad template.
/// For example, if "sink_0" and "sink_2" are taken, this returns "sink_1".
fn allocate_next_pad_name(
    element_id: &str,
    template: &str,
    links: &[Link],
    is_sink: bool,
) -> String {
    let connected = get_connected_request_pad_names(element_id, template, links, is_sink);

    // Find the first available index
    let mut index = 0;
    loop {
        let candidate = generate_pad_name(template, index);
        if !connected.contains(&candidate) {
            return candidate;
        }
        index += 1;

        // Safety limit to prevent infinite loop
        if index > 1000 {
            return generate_pad_name(template, index);
        }
    }
}
