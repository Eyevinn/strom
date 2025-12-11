//! Reusable list navigator widget with keyboard support.
//!
//! Provides a consistent look and feel for list navigation across:
//! - Flow navigator
//! - Stream discovery navigator
//! - PTP clocks navigator

use egui::{Color32, Key, RichText, Ui};

/// A single item in the list navigator.
pub struct ListItem<'a> {
    /// Unique identifier for this item
    pub id: &'a str,
    /// Primary label (shown prominently)
    pub label: &'a str,
    /// Optional tag shown before the label (e.g., "[TX]", "[RX]")
    pub tag: Option<(&'a str, Color32)>,
    /// Secondary line of text (shown below label)
    pub secondary: Option<String>,
    /// Optional right-aligned text on the first line
    pub right_text: Option<String>,
    /// Optional status tag on the right (e.g., "[SYNCED]")
    pub status: Option<(&'a str, Color32)>,
}

impl<'a> ListItem<'a> {
    pub fn new(id: &'a str, label: &'a str) -> Self {
        Self {
            id,
            label,
            tag: None,
            secondary: None,
            right_text: None,
            status: None,
        }
    }

    pub fn with_tag(mut self, tag: &'a str, color: Color32) -> Self {
        self.tag = Some((tag, color));
        self
    }

    pub fn with_secondary(mut self, text: impl Into<String>) -> Self {
        self.secondary = Some(text.into());
        self
    }

    pub fn with_right_text(mut self, text: impl Into<String>) -> Self {
        self.right_text = Some(text.into());
        self
    }

    pub fn with_status(mut self, status: &'a str, color: Color32) -> Self {
        self.status = Some((status, color));
        self
    }
}

/// Result of rendering the list navigator.
pub struct ListNavigatorResult {
    /// The ID of the newly selected item, if selection changed
    pub selected: Option<String>,
    /// Whether the list has focus
    pub has_focus: bool,
}

/// Renders a list of items with consistent styling and keyboard navigation.
///
/// Returns the ID of the selected item if selection changed.
pub fn list_navigator<'a>(
    ui: &mut Ui,
    id_source: &str,
    items: impl Iterator<Item = ListItem<'a>>,
    selected_id: Option<&str>,
) -> ListNavigatorResult {
    let items: Vec<_> = items.collect();

    if items.is_empty() {
        return ListNavigatorResult {
            selected: None,
            has_focus: false,
        };
    }

    // Create a unique ID for this list
    let list_id = ui.id().with(id_source);

    // Check if we have focus
    let has_focus = ui.memory(|mem| mem.has_focus(list_id));

    // Handle keyboard navigation
    let mut new_selection: Option<String> = None;

    if has_focus {
        let current_idx = selected_id.and_then(|sel| items.iter().position(|item| item.id == sel));

        ui.input(|input| {
            if input.key_pressed(Key::ArrowDown) {
                if let Some(idx) = current_idx {
                    if idx + 1 < items.len() {
                        new_selection = Some(items[idx + 1].id.to_string());
                    }
                } else if !items.is_empty() {
                    new_selection = Some(items[0].id.to_string());
                }
            } else if input.key_pressed(Key::ArrowUp) {
                if let Some(idx) = current_idx {
                    if idx > 0 {
                        new_selection = Some(items[idx - 1].id.to_string());
                    }
                } else if !items.is_empty() {
                    new_selection = Some(items[items.len() - 1].id.to_string());
                }
            } else if input.key_pressed(Key::Home) {
                if !items.is_empty() {
                    new_selection = Some(items[0].id.to_string());
                }
            } else if input.key_pressed(Key::End) && !items.is_empty() {
                new_selection = Some(items[items.len() - 1].id.to_string());
            }
        });
    }

    // Render items
    for item in &items {
        let is_selected = selected_id == Some(item.id);
        let item_id = ui.id().with(item.id);

        // Create a clickable button-like area
        let bg_color = if is_selected {
            Color32::from_gray(50)
        } else {
            Color32::from_gray(30)
        };

        let frame = egui::Frame::group(ui.style())
            .fill(bg_color)
            .inner_margin(egui::Margin::symmetric(8, 4));

        let frame_response = frame.show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                // First line: tag + label + status/right text
                ui.horizontal(|ui| {
                    if let Some((tag, color)) = item.tag {
                        ui.colored_label(color, RichText::new(tag).strong());
                    }
                    ui.label(RichText::new(item.label).strong());

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some((status, color)) = item.status {
                            ui.colored_label(color, RichText::new(status).strong());
                        } else if let Some(ref right) = item.right_text {
                            ui.label(right);
                        }
                    });
                });

                // Second line: secondary text
                if let Some(ref secondary) = item.secondary {
                    ui.label(secondary);
                }
            });
        });

        // Make the entire frame area clickable using ui.interact with the frame's rect
        let response = ui.interact(frame_response.response.rect, item_id, egui::Sense::click());

        if response.clicked() {
            new_selection = Some(item.id.to_string());
            ui.memory_mut(|mem| mem.request_focus(list_id));
        }

        // Scroll to selected item if it was just selected via keyboard
        if new_selection.as_deref() == Some(item.id) {
            response.scroll_to_me(Some(egui::Align::Center));
        }

        ui.add_space(2.0);
    }

    ListNavigatorResult {
        selected: new_selection,
        has_focus,
    }
}
