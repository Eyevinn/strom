//! Element palette for browsing and adding GStreamer elements and blocks.

use egui::{ScrollArea, Ui};
use std::collections::{HashMap, HashSet};
use strom_types::element::ElementInfo;
use strom_types::BlockDefinition;

/// Which tab is currently selected in the palette
#[derive(Default, PartialEq)]
enum PaletteTab {
    #[default]
    Elements,
    Blocks,
}

/// Manages the element and block palette UI.
#[derive(Default)]
pub struct ElementPalette {
    /// Available GStreamer elements
    #[allow(dead_code)]
    elements: Vec<ElementInfo>,
    /// Available blocks (built-in + user-defined)
    blocks: Vec<BlockDefinition>,
    /// Cached element info with properties (lazy loaded)
    element_properties_cache: HashMap<String, ElementInfo>,
    /// Cached element info with pad properties (lazy loaded separately)
    element_pad_properties_cache: HashMap<String, ElementInfo>,
    /// Element types that failed to load (to prevent retry loops)
    failed_element_lookups: HashSet<String>,
    /// Element types that failed to load pad properties (to prevent retry loops)
    failed_pad_property_lookups: HashSet<String>,
    /// Search filter text
    search: String,
    /// Selected category filter (None = all categories)
    category_filter: Option<String>,
    /// Element being dragged from the palette
    pub dragging_element: Option<String>,
    /// Block being dragged from the palette
    pub dragging_block: Option<String>,
    /// Currently selected tab
    current_tab: PaletteTab,
    /// Request to focus the search box on next frame
    focus_search_requested: bool,
}

impl ElementPalette {
    /// Create a new element palette.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load elements into the palette.
    pub fn load_elements(&mut self, elements: Vec<ElementInfo>) {
        tracing::info!("Loading {} elements into palette", elements.len());
        self.elements = elements;
    }

    /// Load blocks into the palette.
    pub fn load_blocks(&mut self, blocks: Vec<BlockDefinition>) {
        tracing::info!("Loading {} blocks into palette", blocks.len());
        self.blocks = blocks;
    }

    /// Add some common GStreamer elements as defaults for testing.
    pub fn load_default_elements(&mut self) {
        self.elements = vec![
            ElementInfo {
                name: "videotestsrc".to_string(),
                description: "Video test pattern generator".to_string(),
                category: "Source".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "audiotestsrc".to_string(),
                description: "Audio test tone generator".to_string(),
                category: "Source".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "filesrc".to_string(),
                description: "Read from a file".to_string(),
                category: "Source".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "rtspsrc".to_string(),
                description: "Receive data from an RTSP server".to_string(),
                category: "Source".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "x264enc".to_string(),
                description: "H.264 video encoder".to_string(),
                category: "Codec".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "vp8enc".to_string(),
                description: "VP8 video encoder".to_string(),
                category: "Codec".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "avenc_aac".to_string(),
                description: "AAC audio encoder".to_string(),
                category: "Codec".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "queue".to_string(),
                description: "Simple data queue".to_string(),
                category: "Generic".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "tee".to_string(),
                description: "Split data to multiple outputs".to_string(),
                category: "Generic".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "filesink".to_string(),
                description: "Write to a file".to_string(),
                category: "Sink".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "autovideosink".to_string(),
                description: "Auto-detect video output".to_string(),
                category: "Sink".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "autoaudiosink".to_string(),
                description: "Auto-detect audio output".to_string(),
                category: "Sink".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "rtmpsink".to_string(),
                description: "Send data to an RTMP server".to_string(),
                category: "Sink".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "hlssink2".to_string(),
                description: "HTTP Live Streaming sink".to_string(),
                category: "Sink".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "videoconvert".to_string(),
                description: "Convert video format".to_string(),
                category: "Filter".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "audioresample".to_string(),
                description: "Resample audio".to_string(),
                category: "Filter".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
            ElementInfo {
                name: "capsfilter".to_string(),
                description: "Enforce caps on stream".to_string(),
                category: "Filter".to_string(),
                src_pads: vec![],
                sink_pads: vec![],
                properties: vec![],
            },
        ];
    }

    /// Render the element palette.
    pub fn show(&mut self, ui: &mut Ui) {
        // Tabs for Elements and Blocks
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.current_tab, PaletteTab::Elements, "Elements");
            ui.selectable_value(&mut self.current_tab, PaletteTab::Blocks, "Blocks");
        });
        ui.separator();

        // Search box
        ui.horizontal(|ui| {
            ui.label("Search:");
            let search_id = egui::Id::new("palette_search_box");
            let response = ui.add(egui::TextEdit::singleline(&mut self.search).id(search_id));
            if self.focus_search_requested {
                self.focus_search_requested = false;
                response.request_focus();
            }
        });

        ui.add_space(5.0);

        // Category filter based on current tab
        let mut categories: Vec<String> = match self.current_tab {
            PaletteTab::Elements => self
                .elements
                .iter()
                .map(|e| e.category.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect(),
            PaletteTab::Blocks => self
                .blocks
                .iter()
                .map(|b| b.category.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect(),
        };
        categories.sort();

        ui.horizontal(|ui| {
            ui.label("Category:");
            egui::ComboBox::from_id_salt("category_filter")
                .selected_text(self.category_filter.as_ref().unwrap_or(&"All".to_string()))
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.category_filter.is_none(), "All")
                        .clicked()
                    {
                        self.category_filter = None;
                    }
                    for category in &categories {
                        let selected = self.category_filter.as_ref() == Some(category);
                        if ui.selectable_label(selected, category).clicked() {
                            self.category_filter = Some(category.clone());
                        }
                    }
                });
        });

        ui.separator();

        // List rendering based on current tab
        let search = self.search.clone();
        let category_filter = self.category_filter.clone();

        // Use full available height
        ScrollArea::both()
            .id_salt("palette_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                match self.current_tab {
                    PaletteTab::Elements => {
                        let mut filtered: Vec<ElementInfo> = self
                            .elements
                            .iter()
                            .filter(|e| {
                                // Filter by search text if provided
                                let matches_search = search.is_empty()
                                    || e.name.to_lowercase().contains(&search.to_lowercase())
                                    || e.description
                                        .to_lowercase()
                                        .contains(&search.to_lowercase());

                                let matches_category = category_filter.is_none()
                                    || category_filter.as_ref() == Some(&e.category);

                                matches_search && matches_category
                            })
                            .cloned()
                            .collect();

                        // Sort by name and limit to 50 for performance
                        filtered.sort_by(|a, b| a.name.cmp(&b.name));
                        let total_count = filtered.len();
                        let display_limit = 50;
                        filtered.truncate(display_limit);

                        if filtered.is_empty() {
                            ui.label("No elements found");
                        } else {
                            for element in &filtered {
                                self.draw_element_item(ui, element);
                            }
                            if total_count > display_limit {
                                ui.add_space(4.0);
                                ui.label(format!(
                                    "Showing {} of {} elements. Use filter to find more.",
                                    display_limit, total_count
                                ));
                            }
                        }
                    }
                    PaletteTab::Blocks => {
                        let filtered: Vec<BlockDefinition> = self
                            .blocks
                            .iter()
                            .filter(|b| {
                                // Filter by search text
                                let matches_search = if search.is_empty() {
                                    true
                                } else {
                                    b.name.to_lowercase().contains(&search.to_lowercase())
                                        || b.description
                                            .to_lowercase()
                                            .contains(&search.to_lowercase())
                                };

                                let matches_category = category_filter.is_none()
                                    || category_filter.as_ref() == Some(&b.category);

                                matches_search && matches_category
                            })
                            .cloned()
                            .collect();

                        if filtered.is_empty() {
                            ui.label("No blocks found");
                        } else {
                            for block in filtered {
                                self.draw_block_item(ui, &block);
                            }
                        }
                    }
                }
            });
    }

    fn draw_element_item(&mut self, ui: &mut Ui, element: &ElementInfo) {
        let name = element.name.clone();
        let description = element.description.clone();

        ui.push_id(&name, |ui| {
            // Main horizontal layout for element item
            ui.horizontal(|ui| {
                // Element name label with truncation
                let available_width = ui.available_width() - 70.0; // Reserve space for button
                ui.allocate_ui_with_layout(
                    egui::vec2(available_width, ui.spacing().interact_size.y),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(&name).truncate())
                            .on_hover_text(&description);
                    },
                );

                // Add button on the right
                if ui.button("+ Add").on_hover_text("Add to canvas").clicked() {
                    self.dragging_element = Some(name.clone());
                }
            });

            // Show category and description below (wrapped)
            ui.horizontal_wrapped(|ui| {
                ui.small(&element.category);
                ui.small("|");
                ui.small(&element.description);
            });

            ui.separator();
        });
    }

    fn draw_block_item(&mut self, ui: &mut Ui, block: &BlockDefinition) {
        let name = block.name.clone();
        let id = block.id.clone();
        let description = block.description.clone();
        let built_in = block.built_in;

        ui.push_id(&id, |ui| {
            // Main horizontal layout for block item
            ui.horizontal(|ui| {
                // Block name label with truncation
                let available_width = ui.available_width() - 70.0; // Reserve space for button
                ui.allocate_ui_with_layout(
                    egui::vec2(available_width, ui.spacing().interact_size.y),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        let label_text = if built_in {
                            format!("📦 {}", name)
                        } else {
                            format!("⚙️ {}", name)
                        };
                        ui.add(egui::Label::new(&label_text).truncate())
                            .on_hover_text(&description);
                    },
                );

                // Add button on the right
                if ui.button("+ Add").on_hover_text("Add to canvas").clicked() {
                    self.dragging_block = Some(id.clone());
                }
            });

            // Show category and description below (wrapped)
            ui.horizontal_wrapped(|ui| {
                ui.small(&block.category);
                ui.small("|");
                ui.small(&description);
                if built_in {
                    ui.small("|");
                    ui.small("Built-in");
                }
            });

            ui.separator();
        });
    }

    /// Check if an element is being dragged and return it.
    pub fn take_dragging_element(&mut self) -> Option<String> {
        self.dragging_element.take()
    }

    /// Check if a block is being dragged and return it.
    pub fn take_dragging_block(&mut self) -> Option<String> {
        self.dragging_block.take()
    }

    /// Get element info for a specific element type.
    /// First checks the properties cache, then falls back to the lightweight elements list.
    /// Note: This returns regular element properties. For pad properties, use get_element_info_with_pads.
    pub fn get_element_info(&self, element_type: &str) -> Option<&ElementInfo> {
        // Check properties cache first (has full properties)
        if let Some(cached) = self.element_properties_cache.get(element_type) {
            return Some(cached);
        }
        // Fall back to lightweight elements list (no properties)
        self.elements.iter().find(|e| e.name == element_type)
    }

    /// Get element info with pad properties (for showing Input/Output Pads tabs).
    /// First checks pad properties cache, then falls back to regular element info.
    pub fn get_element_info_with_pads(&self, element_type: &str) -> Option<&ElementInfo> {
        // Check pad properties cache first (has pad properties populated)
        if let Some(cached) = self.element_pad_properties_cache.get(element_type) {
            return Some(cached);
        }
        // Fall back to regular element info (might not have pad properties)
        self.get_element_info(element_type)
    }

    /// Check if we have properties cached (or marked as failed) for this element type.
    pub fn has_properties_cached(&self, element_type: &str) -> bool {
        self.element_properties_cache.contains_key(element_type)
            || self.failed_element_lookups.contains(element_type)
    }

    /// Check if we have pad properties cached (or marked as failed) for this element type.
    pub fn has_pad_properties_cached(&self, element_type: &str) -> bool {
        self.element_pad_properties_cache.contains_key(element_type)
            || self.failed_pad_property_lookups.contains(element_type)
    }

    /// Mark an element type as failed to load (to prevent retry loops).
    pub fn mark_element_lookup_failed(&mut self, element_type: String) {
        tracing::warn!(
            "Marking element '{}' as failed lookup (will not retry)",
            element_type
        );
        self.failed_element_lookups.insert(element_type);
    }

    /// Mark an element type as failed to load pad properties (to prevent retry loops).
    pub fn mark_pad_properties_lookup_failed(&mut self, element_type: String) {
        tracing::warn!(
            "Marking element '{}' pad properties as failed lookup (will not retry)",
            element_type
        );
        self.failed_pad_property_lookups.insert(element_type);
    }

    /// Cache element info with properties.
    pub fn cache_element_properties(&mut self, element_info: ElementInfo) {
        tracing::info!(
            "Caching properties for element '{}' ({} properties)",
            element_info.name,
            element_info.properties.len()
        );
        self.element_properties_cache
            .insert(element_info.name.clone(), element_info);
    }

    /// Cache element info with pad properties.
    pub fn cache_element_pad_properties(&mut self, element_info: ElementInfo) {
        let sink_prop_count: usize = element_info
            .sink_pads
            .iter()
            .map(|p| p.properties.len())
            .sum();
        let src_prop_count: usize = element_info
            .src_pads
            .iter()
            .map(|p| p.properties.len())
            .sum();
        tracing::info!(
            "Caching pad properties for element '{}' (sink_pads: {} props, src_pads: {} props)",
            element_info.name,
            sink_prop_count,
            src_prop_count
        );
        self.element_pad_properties_cache
            .insert(element_info.name.clone(), element_info);
    }

    /// Get block definition for a specific block ID.
    pub fn get_block_info(&self, block_id: &str) -> Option<&BlockDefinition> {
        self.blocks.iter().find(|b| b.id == block_id)
    }

    /// Request focus on the search box (will be applied on next frame).
    pub fn focus_search(&mut self) {
        self.focus_search_requested = true;
    }

    /// Check if the Elements tab is currently active.
    pub fn is_elements_tab(&self) -> bool {
        self.current_tab == PaletteTab::Elements
    }

    /// Switch to Elements tab.
    pub fn switch_to_elements(&mut self) {
        self.current_tab = PaletteTab::Elements;
    }

    /// Switch to Blocks tab.
    pub fn switch_to_blocks(&mut self) {
        self.current_tab = PaletteTab::Blocks;
    }
}
