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
    /// Deferred selection to apply at start of next frame (avoids egui two-pass ID instability).
    /// Outer Option: None = no pending change. Inner Option: the new selection value.
    pending_selected: Option<Option<ElementId>>,
    /// Deferred link selection to apply at start of next frame.
    pending_selected_link: Option<Option<usize>>,
    /// Deferred property tab to apply at start of next frame.
    pending_property_tab: Option<PropertyTab>,
    /// Deferred focused pad to apply at start of next frame.
    pending_focused_pad: Option<Option<String>>,
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
            pending_selected: None,
            pending_selected_link: None,
            pending_property_tab: None,
            pending_focused_pad: None,
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

    /// Apply any deferred selection changes. Call at the start of each frame,
    /// before any rendering, to keep selection state stable across egui's two passes.
    pub fn apply_pending_selection(&mut self) {
        if let Some(sel) = self.pending_selected.take() {
            self.selected = sel;
        }
        if let Some(link) = self.pending_selected_link.take() {
            self.selected_link = link;
        }
        if let Some(tab) = self.pending_property_tab.take() {
            self.active_property_tab = tab;
        }
        if let Some(pad) = self.pending_focused_pad.take() {
            self.focused_pad = pad;
        }
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

    // Numeric order by index (sink_2 before sink_10); names without a numeric
    // index after the prefix go last, in string order.
    let mut result: Vec<String> = pad_names.into_iter().collect();
    result.sort_by_cached_key(|name| {
        let index = name[pattern.len()..].parse::<u64>().ok();
        (index.is_none(), index, name.clone())
    });
    result
}

/// Allocate the next available pad name for a request pad template.
/// For example, if "sink_0" and "sink_2" are taken, this returns "sink_1".
///
/// A request template without a placeholder (flvmux's `audio`, for example)
/// names exactly one pad, so once it is connected there is none to offer
/// and this returns `None`.
pub(super) fn allocate_next_pad_name(
    element_id: &str,
    template: &str,
    links: &[Link],
    is_sink: bool,
) -> Option<String> {
    let connected = get_connected_request_pad_names(element_id, template, links, is_sink);

    if !template.contains('%') {
        return (!connected.iter().any(|name| name == template)).then(|| template.to_string());
    }

    // Find the first available index
    let mut index = 0;
    loop {
        let candidate = generate_pad_name(template, index);
        if !connected.contains(&candidate) {
            return Some(candidate);
        }
        index += 1;

        // Safety limit to prevent infinite loop
        if index > 1000 {
            return Some(generate_pad_name(template, index));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::element::{MediaType, PadPresence};

    fn link(from: &str, to: &str) -> Link {
        Link {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    fn pad(name: &str, presence: PadPresence) -> PadInfo {
        PadInfo {
            name: name.to_string(),
            caps: String::new(),
            presence,
            media_type: MediaType::Generic,
            properties: Vec::new(),
        }
    }

    fn pair(a: &str, b: &str) -> Option<(String, String)> {
        Some((a.to_string(), b.to_string()))
    }

    #[test]
    fn parse_pad_ref_splits_at_the_first_colon() {
        assert_eq!(parse_pad_ref("mixer:sink_0"), pair("mixer", "sink_0"));
        // Everything after the first colon is the pad name.
        assert_eq!(parse_pad_ref("a:b:c"), pair("a", "b:c"));
        assert_eq!(parse_pad_ref("a:"), pair("a", ""));
        assert_eq!(parse_pad_ref(":sink"), pair("", "sink"));
    }

    #[test]
    fn parse_pad_ref_without_a_colon_is_none() {
        assert_eq!(parse_pad_ref("mixer"), None);
        assert_eq!(parse_pad_ref(""), None);
    }

    #[test]
    fn is_request_pad_from_presence_or_template_name() {
        assert!(is_request_pad(&pad("audio", PadPresence::Request)));
        for name in ["sink_%u", "src_%d", "sink_%s"] {
            assert!(is_request_pad(&pad(name, PadPresence::Always)), "{name}");
        }
        assert!(!is_request_pad(&pad("sink", PadPresence::Always)));
        assert!(!is_request_pad(&pad("src", PadPresence::Sometimes)));
    }

    #[test]
    fn connected_names_are_sorted_deduplicated_and_scoped() {
        let links = [
            link("src1:src", "mix:sink_2"),
            link("src2:src", "mix:sink_0"),
            // Same pad twice: listed once.
            link("src3:src", "mix:sink_2"),
            // Other element, same pad name.
            link("src4:src", "other:sink_1"),
            // The element as a source is not a sink connection.
            link("mix:sink_5", "out:sink"),
            // Static pad that does not start with the template prefix.
            link("src5:src", "mix:sink"),
            // Element-level link with no pad.
            link("src6", "mix"),
        ];
        assert_eq!(
            get_connected_request_pad_names("mix", "sink_%u", &links, true),
            vec!["sink_0".to_string(), "sink_2".to_string()]
        );
        assert_eq!(
            get_connected_request_pad_names("mix", "sink_%u", &links, false),
            vec!["sink_5".to_string()]
        );
        assert!(get_connected_request_pad_names("mix", "sink_%u", &[], true).is_empty());
    }

    #[test]
    fn connected_names_keep_templates_apart_by_prefix() {
        let links = [
            link("v:src", "mux:video_0"),
            link("a:src", "mux:audio_0"),
            link("a2:src", "mux:audio_1"),
        ];
        assert_eq!(
            get_connected_request_pad_names("mux", "video_%u", &links, true),
            vec!["video_0"]
        );
        assert_eq!(
            get_connected_request_pad_names("mux", "audio_%u", &links, true),
            vec!["audio_0", "audio_1"]
        );
    }

    #[test]
    fn allocate_starts_at_zero() {
        assert_eq!(
            allocate_next_pad_name("mix", "sink_%u", &[], true),
            Some("sink_0".to_string())
        );
        assert_eq!(
            allocate_next_pad_name("tee", "src_%d", &[], false),
            Some("src_0".to_string())
        );
    }

    #[test]
    fn allocate_fills_the_first_gap() {
        let links = [
            link("a:src", "mix:sink_0"),
            link("b:src", "mix:sink_2"),
            link("c:src", "mix:sink_3"),
        ];
        assert_eq!(
            allocate_next_pad_name("mix", "sink_%u", &links, true),
            Some("sink_1".to_string())
        );
    }

    #[test]
    fn allocate_goes_past_the_last_contiguous_index() {
        let links: Vec<Link> = (0..12)
            .map(|i| link(&format!("s{i}:src"), &format!("mix:sink_{i}")))
            .collect();
        assert_eq!(
            allocate_next_pad_name("mix", "sink_%u", &links, true),
            Some("sink_12".to_string())
        );
    }

    #[test]
    fn allocate_ignores_other_elements_and_the_other_direction() {
        let links = [
            link("a:src", "other:sink_0"),
            link("mix:sink_0", "b:sink"),
            link("tee:src_0", "b:sink"),
            link("tee:src_1", "c:sink"),
        ];
        assert_eq!(
            allocate_next_pad_name("mix", "sink_%u", &links, true),
            Some("sink_0".to_string())
        );
        assert_eq!(
            allocate_next_pad_name("tee", "src_%u", &links, false),
            Some("src_2".to_string())
        );
        assert_eq!(
            allocate_next_pad_name("tee", "src_%u", &links, true),
            Some("src_0".to_string())
        );
    }

    fn element(id: &str, element_type: &str) -> Element {
        Element {
            id: id.to_string(),
            element_type: element_type.to_string(),
            properties: HashMap::new(),
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        }
    }

    fn info(name: &str, sink_pads: Vec<PadInfo>, src_pads: Vec<PadInfo>) -> ElementInfo {
        ElementInfo {
            name: name.to_string(),
            description: String::new(),
            category: String::new(),
            src_pads,
            sink_pads,
            properties: Vec::new(),
        }
    }

    /// Sink pads the editor draws, as (name, is_empty).
    fn drawn_sink_pads(
        links: Vec<Link>,
        el: &Element,
        el_info: &ElementInfo,
    ) -> Vec<(String, bool)> {
        let mut editor = GraphEditor::new();
        editor.load(vec![el.clone()], links);
        let (sinks, _) = editor.get_pads_to_render(el, Some(el_info));
        sinks.into_iter().map(|p| (p.name, p.is_empty)).collect()
    }

    /// flvmux and hlssink2 have request pads named plainly `audio` and
    /// `video`: there is one of each, so a connected one leaves nothing to
    /// offer. The editor drew a second, empty `audio` pad next to the
    /// connected one, and a link to it fails when the flow starts.
    #[test]
    fn fixed_name_request_pad_is_drawn_once_when_connected() {
        let mux = element("mux", "flvmux");
        let mux_info = info(
            "flvmux",
            vec![
                pad("audio", PadPresence::Request),
                pad("video", PadPresence::Request),
            ],
            vec![pad("src", PadPresence::Always)],
        );

        assert_eq!(
            drawn_sink_pads(vec![link("enc:src", "mux:audio")], &mux, &mux_info),
            vec![("audio".to_string(), false), ("video".to_string(), true)]
        );
        assert_eq!(
            drawn_sink_pads(Vec::new(), &mux, &mux_info),
            vec![("audio".to_string(), true), ("video".to_string(), true)]
        );
    }

    /// A mixer with more than ten inputs drew them as sink_0, sink_1,
    /// sink_10, sink_2, ...: connecting the eleventh input moved it between
    /// the second and third.
    #[test]
    fn request_pads_are_drawn_in_numeric_order() {
        let mix = element("mix", "compositor");
        let mix_info = info(
            "compositor",
            vec![pad("sink_%u", PadPresence::Request)],
            vec![pad("src", PadPresence::Always)],
        );
        let links: Vec<Link> = [10, 2, 0, 1, 11]
            .iter()
            .map(|i| link(&format!("s{i}:src"), &format!("mix:sink_{i}")))
            .collect();

        let names: Vec<String> = drawn_sink_pads(links, &mix, &mix_info)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            ["sink_0", "sink_1", "sink_2", "sink_10", "sink_11", "sink_3"]
        );
    }
}
