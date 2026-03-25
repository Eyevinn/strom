//! Node-based graph editor for GStreamer pipelines.

mod data;
mod interaction;
mod rendering;

use egui::{Color32, Vec2};
use std::collections::HashMap;
use strom_types::{
    element::{ElementInfo, PadInfo},
    BlockDefinition, BlockInstance, Element, ElementId, Link,
};

/// Grid size for snapping (in world coordinates)
const GRID_SIZE: f32 = 50.0;

/// Default zoom level
pub(super) const DEFAULT_ZOOM: f32 = 0.8;

/// Maximum zoom level for zoom-to-fit (to avoid excessive zoom on single elements)
pub(super) const MAX_ZOOM_TO_FIT: f32 = 1.0;

/// Minimum zoom level for zoom-to-fit
pub(super) const MIN_ZOOM_TO_FIT: f32 = 0.1;

/// Padding around all elements when using zoom-to-fit (in screen pixels)
pub(super) const ZOOM_TO_FIT_PADDING: f32 = 50.0;

/// Node width (in world coordinates)
pub(super) const NODE_WIDTH: f32 = 200.0;

/// Snap a value to the grid
pub(super) fn snap_to_grid(value: f32) -> f32 {
    (value / GRID_SIZE).round() * GRID_SIZE
}

/// Parse a hex color string (e.g., "#4CAF50") to Color32
pub(super) fn parse_hex_color(hex: &str) -> Option<Color32> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

/// Brighten a color by adding to each component
pub(super) fn brighten_color(color: Color32, amount: u8) -> Color32 {
    Color32::from_rgb(
        color.r().saturating_add(amount),
        color.g().saturating_add(amount),
        color.b().saturating_add(amount),
    )
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
    /// Buffer age health status per element (for rendering clock indicators)
    buffer_age_health_map: HashMap<String, crate::buffer_age::BufferAgeHealth>,
    /// Last known canvas rect (for centering calculations)
    last_canvas_rect: Option<egui::Rect>,
    /// Flag indicating a QoS marker was clicked (to signal log panel should open)
    /// Uses Cell for interior mutability since draw_* functions take &self
    qos_marker_clicked: std::cell::Cell<bool>,
    /// Flag indicating user double-clicked on background (to signal palette should open)
    request_open_palette: std::cell::Cell<bool>,
    /// Flag indicating user clicked background while nothing was selected (toggle right pane)
    request_toggle_right_pane: std::cell::Cell<bool>,
    /// Clipboard for copy/paste operations
    clipboard: Option<ClipboardContent>,
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
pub(super) struct PadToRender {
    /// The actual pad name (e.g., "sink_0" for a request pad, or "sink" for a static pad)
    pub name: String,
    /// The template name (e.g., "sink_%u" for request pads, or "sink" for static pads)
    #[allow(dead_code)]
    pub template_name: String,
    /// Media type for coloring
    pub media_type: strom_types::element::MediaType,
    /// Whether this is the "empty" pad (always unconnected, for creating new links)
    pub is_empty: bool,
}

/// Callback type for rendering custom block content
pub type BlockRenderCallback = Box<dyn Fn(&mut egui::Ui, egui::Rect) + 'static>;

/// Content that can be stored in the clipboard for copy/paste operations
#[derive(Clone)]
pub enum ClipboardContent {
    Element(Element),
    Block(BlockInstance),
}

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
            zoom: DEFAULT_ZOOM,
            creating_link: None,
            hovered_pad: None,
            hovered_element: None,
            selected_link: None,
            hovered_link: None,
            active_property_tab: PropertyTab::Element,
            focused_pad: None,
            qos_health_map: HashMap::new(),
            buffer_age_health_map: HashMap::new(),
            last_canvas_rect: None,
            qos_marker_clicked: std::cell::Cell::new(false),
            request_open_palette: std::cell::Cell::new(false),
            request_toggle_right_pane: std::cell::Cell::new(false),
            clipboard: None,
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

    /// Set the buffer age health map for rendering clock indicators on nodes
    pub fn set_buffer_age_health_map(
        &mut self,
        health_map: HashMap<String, crate::buffer_age::BufferAgeHealth>,
    ) {
        self.buffer_age_health_map = health_map;
    }

    /// Calculate the vertical offset for a pad given its index and total count.
    /// Uses tighter spacing (20 pixels between pads) instead of spreading across the full height.
    pub(super) fn calculate_pad_y_offset(&self, idx: usize, count: usize, node_height: f32) -> f32 {
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
}

/// Parse a pad reference like "element_id:pad_name" into (element_id, pad_name).
pub(super) fn parse_pad_ref(pad_ref: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = pad_ref.split(':').collect();
    if parts.len() >= 2 {
        Some((parts[0].to_string(), parts[1..].join(":")))
    } else {
        None
    }
}

/// Check if a pad is a request pad (dynamic pad with template like "sink_%u").
pub(super) fn is_request_pad(pad_info: &PadInfo) -> bool {
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
pub(super) fn get_connected_request_pad_names(
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
pub(super) fn allocate_next_pad_name(
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
