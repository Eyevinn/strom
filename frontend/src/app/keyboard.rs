use crate::state::AppMessage;
use egui::Context;
use strom_types::Flow;

use super::FocusTarget;
use super::*;
impl StromApp {
    /// Format keyboard shortcut for display (adapts to platform).
    pub(super) fn format_shortcut(shortcut: &str) -> String {
        #[cfg(target_os = "macos")]
        {
            shortcut.replace("Ctrl", "⌘")
        }
        #[cfg(not(target_os = "macos"))]
        {
            shortcut.to_string()
        }
    }

    /// Navigate to the previous flow in the sorted flow list.
    pub(super) fn navigate_flow_list_up(&mut self) {
        if self.flows.is_empty() {
            return;
        }

        // Create sorted list to match the display order (by name)
        let mut sorted_flows: Vec<&Flow> = self.flows.iter().collect();
        sorted_flows.sort_by_key(|a| a.name.to_lowercase());

        if let Some(current_id) = self.selected_flow_id {
            // Find position of current selection in sorted list
            if let Some(pos) = sorted_flows.iter().position(|f| f.id == current_id) {
                if pos > 0 {
                    // Move to previous flow
                    let flow = sorted_flows[pos - 1];
                    self.selected_flow_id = Some(flow.id);
                    // Clear graph selection when switching flows
                    self.graph.deselect_all();
                    self.graph.clear_runtime_dynamic_pads();
                    self.graph.load(flow.elements.clone(), flow.links.clone());
                    self.graph.load_blocks(flow.blocks.clone());
                }
            }
        } else if !sorted_flows.is_empty() {
            // No selection, select first flow
            let flow = sorted_flows[0];
            self.selected_flow_id = Some(flow.id);
            // Clear graph selection when switching flows
            self.graph.deselect_all();
            self.graph.clear_runtime_dynamic_pads();
            self.graph.load(flow.elements.clone(), flow.links.clone());
            self.graph.load_blocks(flow.blocks.clone());
        }
    }

    /// Navigate to the next flow in the sorted flow list.
    pub(super) fn navigate_flow_list_down(&mut self) {
        if self.flows.is_empty() {
            return;
        }

        // Create sorted list to match the display order (by name)
        let mut sorted_flows: Vec<&Flow> = self.flows.iter().collect();
        sorted_flows.sort_by_key(|a| a.name.to_lowercase());

        if let Some(current_id) = self.selected_flow_id {
            // Find position of current selection in sorted list
            if let Some(pos) = sorted_flows.iter().position(|f| f.id == current_id) {
                if pos < sorted_flows.len() - 1 {
                    // Move to next flow
                    let flow = sorted_flows[pos + 1];
                    self.selected_flow_id = Some(flow.id);
                    // Clear graph selection when switching flows
                    self.graph.deselect_all();
                    self.graph.clear_runtime_dynamic_pads();
                    self.graph.load(flow.elements.clone(), flow.links.clone());
                    self.graph.load_blocks(flow.blocks.clone());
                }
            }
        } else if !sorted_flows.is_empty() {
            // No selection, select first flow
            let flow = sorted_flows[0];
            self.selected_flow_id = Some(flow.id);
            // Clear graph selection when switching flows
            self.graph.deselect_all();
            self.graph.clear_runtime_dynamic_pads();
            self.graph.load(flow.elements.clone(), flow.links.clone());
            self.graph.load_blocks(flow.blocks.clone());
        }
    }

    /// Handle global keyboard shortcuts.
    pub(super) fn handle_keyboard_shortcuts(&mut self, ctx: &Context) {
        // Track key sequence for overlay activation (works globally, even in text inputs)
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                {
                    if !modifiers.command && !modifiers.ctrl && !modifiers.alt {
                        self.key_sequence_buffer.push(*key);
                        if self.key_sequence_buffer.len() > 5 {
                            self.key_sequence_buffer
                                .drain(..self.key_sequence_buffer.len() - 5);
                        }
                    }
                }
            }
        });

        if self.key_sequence_buffer.len() == 5
            && self.key_sequence_buffer
                == [
                    egui::Key::I,
                    egui::Key::D,
                    egui::Key::D,
                    egui::Key::Q,
                    egui::Key::D,
                ]
        {
            self.interactive_overlay = Some(crate::interactive_overlay::OverlayState::new());
            self.key_sequence_buffer.clear();
            return;
        }

        // When the overlay is active, skip all normal keyboard handling
        if self.interactive_overlay.is_some() {
            return;
        }

        // Don't process shortcuts if a text input has focus (except ESC)
        let wants_keyboard = ctx.egui_wants_keyboard_input();

        // ESC key - highest priority, works even in text inputs
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            // Priority 1: Close dialogs and windows
            if self.show_new_flow_dialog {
                self.show_new_flow_dialog = false;
            } else if self.show_import_dialog {
                self.show_import_dialog = false;
            } else if self.flow_pending_deletion.is_some() {
                self.flow_pending_deletion = None;
            } else if self.editing_properties_flow_id.is_some() {
                self.editing_properties_flow_id = None;
            } else if !wants_keyboard {
                // Priority 2: Deselect in graph editor
                self.graph.deselect_all();
            }
        }

        // Ctrl+S - Save (works even in text inputs)
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save_current_flow(ctx);
        }

        // F5 or Ctrl+R - Refresh (works even in text inputs)
        if ctx.input(|i| {
            i.key_pressed(egui::Key::F5) || (i.modifiers.command && i.key_pressed(egui::Key::R))
        }) {
            self.needs_refresh = true;
        }

        // Ctrl+D - Debug Graph (works even in text inputs)
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::D)) {
            if let Some(flow) = self.current_flow() {
                let url = self.api.get_debug_graph_url(flow.id);
                ctx.open_url(egui::OpenUrl::new_tab(&url));
            }
        }

        // Shift+F9 - Stop Flow (works even in text inputs, must be checked before plain F9)
        if ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::F9)) {
            self.stop_flow(ctx);
        }
        // F9 - Start/Restart Flow (works even in text inputs)
        else if ctx.input(|i| !i.modifiers.shift && i.key_pressed(egui::Key::F9)) {
            if let Some(flow) = self.current_flow() {
                let is_running = flow.running;

                if is_running {
                    // Restart
                    let api = self.api.clone();
                    let tx = self.channels.sender();
                    let flow_id = flow.id;
                    let ctx_clone = ctx.clone();

                    self.status = "Restarting flow...".to_string();

                    spawn_task(async move {
                        match api.stop_flow(flow_id).await {
                            Ok(_) => match api.start_flow(flow_id).await {
                                Ok(_) => {
                                    let _ = tx.send(AppMessage::FlowOperationSuccess(
                                        "Flow restarted".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                                        "Failed to restart flow: {}",
                                        e
                                    )));
                                }
                            },
                            Err(e) => {
                                let _ = tx.send(AppMessage::FlowOperationError(format!(
                                    "Failed to restart flow: {}",
                                    e
                                )));
                            }
                        }
                        ctx_clone.request_repaint();
                    });
                } else {
                    self.start_flow(ctx);
                }
            }
        }

        // Ctrl+F - Find: cycle through filter boxes (works even in text inputs)
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            // Deselect any selected element/block
            self.graph.deselect_all();

            // Cycle to next focus target based on current page
            match self.current_page {
                AppPage::Flows => {
                    self.focus_target = match self.focus_target {
                        FocusTarget::None | FocusTarget::PaletteBlocks => {
                            self.focus_flow_filter_requested = true;
                            FocusTarget::FlowFilter
                        }
                        FocusTarget::FlowFilter => {
                            self.palette.switch_to_elements();
                            self.palette.focus_search();
                            FocusTarget::PaletteElements
                        }
                        FocusTarget::PaletteElements => {
                            self.palette.switch_to_blocks();
                            self.palette.focus_search();
                            FocusTarget::PaletteBlocks
                        }
                        _ => {
                            self.focus_flow_filter_requested = true;
                            FocusTarget::FlowFilter
                        }
                    };
                }
                AppPage::Discovery => {
                    self.discovery_page.focus_search();
                    self.focus_target = FocusTarget::DiscoveryFilter;
                }
                AppPage::Clocks => {
                    // No filters on Clocks page
                }
                AppPage::Media => {
                    self.media_page.focus_search();
                    self.focus_target = FocusTarget::MediaFilter;
                }
                AppPage::Info => {
                    // No search/filters on Info page
                }
                AppPage::Links => {
                    // No search/filters on Links page
                }
            }
        }

        // Don't process other shortcuts if text input has focus
        if wants_keyboard {
            return;
        }

        // Up/Down arrow keys - Navigate flow list
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            // Clear focus before changing graph structure to prevent accesskit panic
            ctx.memory_mut(|mem| {
                if let Some(focused_id) = mem.focused() {
                    mem.surrender_focus(focused_id);
                }
            });
            self.navigate_flow_list_up();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            // Clear focus before changing graph structure to prevent accesskit panic
            ctx.memory_mut(|mem| {
                if let Some(focused_id) = mem.focused() {
                    mem.surrender_focus(focused_id);
                }
            });
            self.navigate_flow_list_down();
        }

        // Delete key - Delete selected flow (only if nothing is selected in graph editor)
        if ctx.input(|i| i.key_pressed(egui::Key::Delete)) && !self.graph.has_selection() {
            if let Some(flow) = self.current_flow() {
                self.flow_pending_deletion = Some((flow.id, flow.name.clone()));
            }
        }

        // Ctrl+N - New Flow
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::N)) {
            self.show_new_flow_dialog = true;
        }

        // Ctrl+O - Import
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.show_import_dialog = true;
            self.import_json_buffer.clear();
            self.import_error = None;
        }

        // F1 - Help (GitHub)
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Eyevinn/strom"));
        }

        // Ctrl+C - Copy selected element/block in graph
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) {
            self.graph.copy_selected();
        }

        // Ctrl+V - Paste element/block in graph
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::V)) {
            self.graph.paste_clipboard();
        }
    }
}
