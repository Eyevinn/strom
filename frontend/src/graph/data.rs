use std::collections::HashMap;
use strom_types::{
    element::{ElementInfo, MediaType, PropertyValue},
    BlockDefinition, BlockInstance, Element, ElementId,
};
use uuid::Uuid;

use super::*;

impl GraphEditor {
    /// Add a new element to the graph at the given position.
    pub fn add_element(&mut self, element_type: String, pos: egui::Pos2) {
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
    pub fn add_block(&mut self, block_definition_id: String, pos: egui::Pos2) {
        self.add_block_with_props(block_definition_id, pos, HashMap::new());
    }

    /// Add a new block instance with initial properties.
    pub fn add_block_with_props(
        &mut self,
        block_definition_id: String,
        pos: egui::Pos2,
        properties: HashMap<String, PropertyValue>,
    ) {
        let id = format!("b{}", Uuid::new_v4().simple());
        let block = BlockInstance {
            id: id.clone(),
            block_definition_id,
            name: None,
            properties,
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

    /// Copy the currently selected element or block to the clipboard.
    pub fn copy_selected(&mut self) {
        if let Some(ref id) = self.selected {
            // Check if it's an element
            if let Some(element) = self.elements.iter().find(|e| &e.id == id) {
                self.clipboard = Some(ClipboardContent::Element(element.clone()));
                return;
            }
            // Check if it's a block
            if let Some(block) = self.blocks.iter().find(|b| &b.id == id) {
                self.clipboard = Some(ClipboardContent::Block(block.clone()));
            }
        }
    }

    /// Paste the clipboard content at an offset from the original position.
    /// Returns true if something was pasted.
    pub fn paste_clipboard(&mut self) -> bool {
        const PASTE_OFFSET: f32 = 30.0;

        if let Some(ref content) = self.clipboard.clone() {
            match content {
                ClipboardContent::Element(element) => {
                    let new_id = format!("e{}", Uuid::new_v4().simple());
                    let new_element = Element {
                        id: new_id.clone(),
                        element_type: element.element_type.clone(),
                        properties: element.properties.clone(),
                        pad_properties: element.pad_properties.clone(),
                        position: (
                            element.position.0 + PASTE_OFFSET,
                            element.position.1 + PASTE_OFFSET,
                        ),
                    };
                    self.elements.push(new_element);
                    self.selected = Some(new_id);
                    self.selected_link = None;
                    true
                }
                ClipboardContent::Block(block) => {
                    let new_id = format!("b{}", Uuid::new_v4().simple());
                    let new_block = BlockInstance {
                        id: new_id.clone(),
                        block_definition_id: block.block_definition_id.clone(),
                        name: block.name.clone(),
                        properties: block.properties.clone(),
                        position: strom_types::block::Position {
                            x: block.position.x + PASTE_OFFSET,
                            y: block.position.y + PASTE_OFFSET,
                        },
                        runtime_data: None, // Don't copy runtime data
                        computed_external_pads: block.computed_external_pads.clone(),
                    };
                    self.blocks.push(new_block);
                    self.selected = Some(new_id);
                    self.selected_link = None;
                    true
                }
            }
        } else {
            false
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

    /// Check if user requested to open the palette (double-click on background).
    pub fn take_open_palette_request(&self) -> bool {
        self.request_open_palette.replace(false)
    }

    /// Check if user clicked background while nothing was selected (toggle right pane).
    pub fn take_toggle_right_pane_request(&self) -> bool {
        self.request_toggle_right_pane.replace(false)
    }

    /// Get the last known canvas rect (for hit testing pinch gestures, WASM only).
    #[allow(dead_code)]
    pub fn canvas_rect(&self) -> Option<egui::Rect> {
        self.last_canvas_rect
    }

    /// Select a node (element) by its ID.
    pub fn select_node(&mut self, id: ElementId) {
        self.selected = Some(id);
        self.selected_link = None;
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

    /// Get a mutable reference to a block by ID.
    pub fn get_block_by_id_mut(&mut self, block_id: &str) -> Option<&mut BlockInstance> {
        self.blocks.iter_mut().find(|b| b.id == block_id)
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
    pub(super) fn get_block_external_pads<'a>(
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
    pub(super) fn get_pads_to_render(
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

    /// Get the currently selected link.
    pub fn get_selected_link(&self) -> Option<&strom_types::element::Link> {
        self.selected_link.and_then(|idx| self.links.get(idx))
    }

    /// Get a display label for an element or block by ID.
    ///
    /// Handles plain element IDs, block IDs, and namespaced block element IDs
    /// (format "block_id:internal_element") by resolving the block name.
    pub fn node_label(&self, id: &str) -> String {
        // Direct element match
        if let Some(element) = self.elements.iter().find(|e| e.id == id) {
            return element.element_type.clone();
        }
        // Direct block match
        if let Some(block) = self.blocks.iter().find(|b| b.id == id) {
            return block
                .name
                .clone()
                .unwrap_or_else(|| block.block_definition_id.clone());
        }
        // Namespaced block element: "block_id:internal_element"
        if let Some((block_id, _)) = id.split_once(':') {
            if let Some(block) = self.blocks.iter().find(|b| b.id == block_id) {
                return block
                    .name
                    .clone()
                    .unwrap_or_else(|| block.block_definition_id.clone());
            }
        }
        id.to_string()
    }

    /// Start a link drag from an input (sink) pad.
    ///
    /// If the pad already has an incoming connection, detach it and return the
    /// source end so the user can re-route or drop to delete.
    /// Only applies to input pads — output pads support fan-out so dragging
    /// from them always starts a new link.
    pub fn detach_link_from_input(&mut self, element_id: &str, pad_name: &str) -> (String, String) {
        let pad_ref = format!("{element_id}:{pad_name}");

        // Only detach links where this pad is the destination (input)
        let existing = self.links.iter().position(|link| link.to == pad_ref);

        if let Some(idx) = existing {
            let link = self.links.remove(idx);
            // Fix up selected_link index after removal
            match self.selected_link {
                Some(sel) if sel == idx => self.selected_link = None,
                Some(sel) if sel > idx => self.selected_link = Some(sel - 1),
                _ => {}
            }
            // Re-route from the source end of the removed link
            // Use rsplit_once to handle namespaced block element IDs (e.g. "block_id:element:pad")
            let (from_id, from_pad) = link
                .from
                .rsplit_once(':')
                .map(|(id, pad)| (id.to_string(), pad.to_string()))
                .unwrap_or_else(|| (link.from.clone(), String::new()));
            (from_id, from_pad)
        } else {
            (element_id.to_string(), pad_name.to_string())
        }
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
    pub(super) fn select_element_and_focus_pad(
        &mut self,
        element_id: &str,
        pad_name: &str,
        is_input: bool,
    ) {
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
    pub(super) fn is_output_pad(&self, element_id: &str, pad_name: &str) -> bool {
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
}
