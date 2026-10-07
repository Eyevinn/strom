use egui::{pos2, vec2, Color32, Pos2, Rect, Sense, Stroke, Ui};
use strom_types::{element::ElementInfo, BlockDefinition, BlockInstance, Element, Link};

use super::*;

impl GraphEditor {
    pub(super) fn draw_link(
        &self,
        painter: &egui::Painter,
        link: &Link,
        to_screen: &impl Fn(Pos2) -> Pos2,
        is_selected: bool,
        is_hovered: bool,
    ) {
        // Parse IDs and pad names from link
        let (from_id, from_pad) = self.resolve_link_endpoint(&link.from, false);
        let (to_id, to_pad) = self.resolve_link_endpoint(&link.to, true);

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
                (Color32::from_rgb(100, 150, 255), 3.0_f32) // Blue and thicker when selected
            } else if is_hovered {
                (Color32::from_rgb(200, 200, 200), 2.5_f32) // Brighter when hovered
            } else {
                (Color32::from_rgb(150, 150, 150), 2.0_f32) // Default gray
            };

            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                [from_screen, control1, control2, to_screen_pos],
                false,
                Color32::TRANSPARENT,
                Stroke::new(width, color),
            ));
        }
    }

    /// Resolve one end of a link to the (node_id, pad_name) the editor draws it at.
    ///
    /// A link end without a pad (`"id"`, `"id::"`) is an element-level link:
    /// GStreamer picks the pad at runtime. The editor draws it at the node's
    /// first pad in the link's direction (an output pad for `from`, an input
    /// pad for `to`). If the node has no known pad in that direction, the pad
    /// name is left empty and `get_pad_position` falls back to the node edge.
    /// Explicit pads are returned as parsed.
    pub(super) fn resolve_link_endpoint(&self, pad_ref: &str, is_input: bool) -> (String, String) {
        let (node_id, pad) = parse_link_endpoint(pad_ref);
        if let Some(pad) = pad {
            return (node_id, pad);
        }
        let first_pad = self.first_pad_name(&node_id, is_input).unwrap_or_default();
        (node_id, first_pad)
    }

    /// Name of the first pad in one direction on an element or block, as rendered.
    fn first_pad_name(&self, node_id: &str, is_input: bool) -> Option<String> {
        if let Some(element) = self.elements.iter().find(|e| e.id == node_id) {
            let element_info = self.element_info_map.get(&element.element_type);
            let (sink_pads, src_pads) = self.get_pads_to_render(element, element_info);
            let pads = if is_input { sink_pads } else { src_pads };
            return pads.into_iter().next().map(|p| p.name);
        }
        let block = self.blocks.iter().find(|b| b.id == node_id)?;
        let definition = self.block_definition_map.get(&block.block_definition_id);
        let external_pads = self.get_block_external_pads(block, definition)?;
        let pads = if is_input {
            &external_pads.inputs
        } else {
            &external_pads.outputs
        };
        pads.first().map(|p| p.name.clone())
    }

    /// Get the world position of a specific pad on an element or block.
    /// Returns position on the right edge for output pads, left edge for input pads.
    pub(super) fn get_pad_position(
        &self,
        element_id: &str,
        pad_name: &str,
        is_input: bool,
    ) -> Option<Pos2> {
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

                let name_extra = if block.name.as_ref().is_some_and(|n| !n.is_empty()) {
                    20.0
                } else {
                    0.0
                };

                let node_height = (base_height + content_height + name_extra).min(400.0);

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
    pub(super) fn is_point_near_link(
        &self,
        link: &Link,
        point: Pos2,
        to_screen: &impl Fn(Pos2) -> Pos2,
    ) -> bool {
        // Parse IDs and pad names from link
        let (from_id, from_pad) = self.resolve_link_endpoint(&link.from, false);
        let (to_id, to_pad) = self.resolve_link_endpoint(&link.to, true);

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

    pub(super) fn handle_pad_interaction(
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
                // If this input pad already has a connection, detach it and re-route from the source
                if pad_response.drag_started()
                    || (pad_response.dragged() && self.creating_link.is_none())
                {
                    let (drag_id, drag_pad) =
                        self.detach_link_from_input(&element.id, &pad_to_render.name);
                    self.creating_link = Some((drag_id, drag_pad));
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

                // Start creating link when dragging from output port (always new — outputs support fan-out)
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
                // If already connected, detach and re-route from the source
                if input_response.drag_started()
                    || (input_response.dragged() && self.creating_link.is_none())
                {
                    let (drag_id, drag_pad) = self.detach_link_from_input(&element.id, "sink");
                    self.creating_link = Some((drag_id, drag_pad));
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

    /// Handle pad interactions for blocks.
    pub(super) fn handle_block_pad_interaction(
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
            let name_extra = if block.name.as_ref().is_some_and(|n| !n.is_empty()) {
                20.0
            } else {
                0.0
            };
            let node_height = (base_height + content_height + name_extra).min(400.0);

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
                // If this input pad already has a connection, detach and re-route from source
                if pad_response.drag_started()
                    || (pad_response.dragged() && self.creating_link.is_none())
                {
                    let (drag_id, drag_pad) =
                        self.detach_link_from_input(&block.id, &external_pad.name);
                    self.creating_link = Some((drag_id, drag_pad));
                }

                if pad_response.hovered() {
                    self.hovered_pad = Some((block.id.clone(), external_pad.name.clone()));
                    any_hovered = true;
                }
                pad_response.on_hover_text(&external_pad.name);
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

                // Start creating link when dragging from output port (always new — outputs support fan-out)
                if pad_response.drag_started()
                    || (pad_response.dragged() && self.creating_link.is_none())
                {
                    self.creating_link = Some((block.id.clone(), external_pad.name.clone()));
                }

                if pad_response.hovered() {
                    self.hovered_pad = Some((block.id.clone(), external_pad.name.clone()));
                    any_hovered = true;
                }
                pad_response.on_hover_text(&external_pad.name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use strom_types::block::{ExternalPad, ExternalPads, Position};
    use strom_types::element::{MediaType, PadInfo, PadPresence};

    fn pad_info(name: &str) -> PadInfo {
        PadInfo {
            name: name.to_string(),
            caps: String::new(),
            presence: PadPresence::Always,
            media_type: MediaType::Video,
            properties: Vec::new(),
        }
    }

    fn element_info(name: &str, sinks: &[&str], srcs: &[&str]) -> ElementInfo {
        ElementInfo {
            name: name.to_string(),
            description: String::new(),
            category: String::new(),
            src_pads: srcs.iter().map(|p| pad_info(p)).collect(),
            sink_pads: sinks.iter().map(|p| pad_info(p)).collect(),
            properties: Vec::new(),
        }
    }

    fn element(id: &str, element_type: &str, x: f32) -> Element {
        Element {
            id: id.to_string(),
            element_type: element_type.to_string(),
            properties: HashMap::new(),
            pad_properties: HashMap::new(),
            position: (x, 0.0),
        }
    }

    fn external_pad(name: &str) -> ExternalPad {
        ExternalPad {
            name: name.to_string(),
            label: None,
            media_type: MediaType::Video,
            internal_element_id: "inner".to_string(),
            internal_pad_name: "sink".to_string(),
        }
    }

    fn link(from: &str, to: &str) -> Link {
        Link {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    /// The repro flow: element-level links next to explicit-pad links.
    fn editor(links: Vec<Link>) -> GraphEditor {
        let mut editor = GraphEditor::new();
        editor.set_element_info(
            "videotestsrc".to_string(),
            element_info("videotestsrc", &[], &["src"]),
        );
        editor.set_element_info(
            "capsfilter".to_string(),
            element_info("capsfilter", &["sink"], &["src"]),
        );
        editor.set_element_info(
            "valve".to_string(),
            element_info("valve", &["sink"], &["src"]),
        );
        editor.load(
            vec![
                element("vsrc", "videotestsrc", 0.0),
                element("vcaps", "capsfilter", 300.0),
                element("vvalve", "valve", 600.0),
                // No element info: pads come from links only
                element("dec", "decodebin", 900.0),
            ],
            links,
        );
        editor.load_blocks(vec![BlockInstance {
            id: "rec".to_string(),
            block_definition_id: "builtin.recorder".to_string(),
            name: None,
            properties: HashMap::new(),
            position: Position { x: 1200.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: Some(ExternalPads {
                inputs: vec![external_pad("video_in_0"), external_pad("audio_in_0")],
                outputs: vec![external_pad("out")],
            }),
        }]);
        editor
    }

    fn resolved(editor: &GraphEditor, pad_ref: &str, is_input: bool) -> (String, String) {
        editor.resolve_link_endpoint(pad_ref, is_input)
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn padless_link_ends_resolve_to_the_first_pad_in_the_link_direction() {
        let editor = editor(vec![
            link("vsrc", "vcaps"),
            link("vcaps::", "vvalve::"),
            link("vvalve", "rec:video_in_0"),
            link("rec", "rec"),
            link("vsrc:src", "dec::"),
        ]);

        // Padless from: the element's (only) output pad
        assert_eq!(resolved(&editor, "vsrc", false), pair("vsrc", "src"));
        // Padless to: the element's (only) input pad
        assert_eq!(resolved(&editor, "vcaps", true), pair("vcaps", "sink"));
        // "id::" form, both ends
        assert_eq!(resolved(&editor, "vcaps::", false), pair("vcaps", "src"));
        assert_eq!(resolved(&editor, "vvalve::", true), pair("vvalve", "sink"));
        // Blocks: first external pad of the right direction
        assert_eq!(resolved(&editor, "rec", true), pair("rec", "video_in_0"));
        assert_eq!(resolved(&editor, "rec::", false), pair("rec", "out"));
        // No known pad in that direction: empty pad name, node-edge fallback
        assert_eq!(resolved(&editor, "dec::", true), pair("dec", ""));
        assert_eq!(resolved(&editor, "vsrc", true), pair("vsrc", ""));

        // Every link resolves to a drawable position at both ends
        for l in &editor.links {
            let (from_id, from_pad) = resolved(&editor, &l.from, false);
            let (to_id, to_pad) = resolved(&editor, &l.to, true);
            assert!(
                editor
                    .get_pad_position(&from_id, &from_pad, false)
                    .is_some(),
                "no from position for {l:?}"
            );
            assert!(
                editor.get_pad_position(&to_id, &to_pad, true).is_some(),
                "no to position for {l:?}"
            );
        }

        // A padless end is drawn at the same spot as its first pad
        assert_eq!(
            editor.get_pad_position("rec", &resolved(&editor, "rec", true).1, true),
            editor.get_pad_position("rec", "video_in_0", true),
        );
        assert_ne!(
            editor.get_pad_position("rec", "video_in_0", true),
            editor.get_pad_position("rec", "audio_in_0", true),
        );
    }

    #[test]
    fn explicit_pad_link_ends_are_unchanged() {
        let editor = editor(vec![link("vsrc:src", "vcaps:sink")]);
        assert_eq!(resolved(&editor, "vsrc:src", false), pair("vsrc", "src"));
        assert_eq!(resolved(&editor, "vcaps:sink", true), pair("vcaps", "sink"));
        assert_eq!(
            resolved(&editor, "rec:audio_in_0", true),
            pair("rec", "audio_in_0")
        );
        // Same first-colon split as parse_pad_ref, including namespaced ids
        assert_eq!(resolved(&editor, "a:b:c", true), pair("a", "b:c"));
    }

    #[test]
    fn padless_link_ends_add_no_pad_to_an_element_without_pad_info() {
        let editor = editor(vec![link("vsrc", "dec::"), link("dec", "vcaps")]);
        assert!(editor.get_actual_input_pads("dec").is_empty());
        assert!(editor.get_actual_output_pads("dec").is_empty());
    }
}
