//! Property inspector for editing element properties.

use crate::graph::PropertyTab;
use egui::{Color32, ScrollArea, Ui};
use strom_types::{
    block::{EnumValue, ExposedProperty},
    element::{ElementInfo, PropertyInfo, PropertyType},
    BlockDefinition, BlockInstance, Element, PropertyValue,
};

/// Property inspector panel.
pub struct PropertyInspector;

impl PropertyInspector {
    /// Match an actual pad name (e.g., "sink_0") to a pad template (e.g., "sink_%u").
    /// Returns true if the actual pad name matches the template.
    fn matches_pad_template(actual_pad: &str, template: &str) -> bool {
        // First try exact match
        if actual_pad == template {
            return true;
        }

        // Check for request pad patterns like "sink_%u", "src_%u", "sink_%d", etc.
        // Replace common patterns with regex-like matching
        if template.contains("%u") || template.contains("%d") {
            // Extract the prefix before the pattern
            let prefix = if let Some(idx) = template.find("%u") {
                &template[..idx]
            } else if let Some(idx) = template.find("%d") {
                &template[..idx]
            } else {
                return false;
            };

            // Check if actual pad starts with the prefix
            if !actual_pad.starts_with(prefix) {
                return false;
            }

            // Check if the suffix is numeric
            let suffix = &actual_pad[prefix.len()..];
            suffix.chars().all(|c| c.is_ascii_digit() || c == '_')
        } else {
            false
        }
    }

    /// Show the property inspector for the given element with tabbed interface.
    /// Returns (new_active_tab, delete_requested).
    pub fn show(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
        active_tab: PropertyTab,
        focused_pad: Option<String>,
        input_pads: Vec<String>,
        output_pads: Vec<String>,
    ) -> (PropertyTab, bool) {
        let element_id = element.id.clone();
        let mut new_tab = active_tab;
        let mut delete_requested = false;

        ui.push_id(&element_id, |ui| {
            // Element type (read-only)
            ui.horizontal(|ui| {
                ui.label("Type:");
                ui.monospace(&element.element_type);
            });

            // Element ID (read-only)
            ui.horizontal(|ui| {
                ui.label("ID:");
                ui.monospace(&element.id);
            });

            ui.separator();

            // Delete button
            if ui.button("🗑 Delete Element").clicked() {
                delete_requested = true;
            }

            ui.separator();

            // Tab buttons
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(new_tab == PropertyTab::Element, "Element Properties")
                    .clicked()
                {
                    new_tab = PropertyTab::Element;
                }
                if ui
                    .selectable_label(new_tab == PropertyTab::InputPads, "Input Pads")
                    .clicked()
                {
                    new_tab = PropertyTab::InputPads;
                }
                if ui
                    .selectable_label(new_tab == PropertyTab::OutputPads, "Output Pads")
                    .clicked()
                {
                    new_tab = PropertyTab::OutputPads;
                }
            });

            ui.separator();

            // Tab content
            match new_tab {
                PropertyTab::Element => {
                    Self::show_element_properties_tab(ui, element, element_info);
                }
                PropertyTab::InputPads => {
                    Self::show_input_pads_tab(
                        ui,
                        element,
                        element_info,
                        &input_pads,
                        focused_pad.as_deref(),
                    );
                }
                PropertyTab::OutputPads => {
                    Self::show_output_pads_tab(
                        ui,
                        element,
                        element_info,
                        &output_pads,
                        focused_pad.as_deref(),
                    );
                }
            }
        });

        (new_tab, delete_requested)
    }

    /// Show the Element Properties tab content.
    fn show_element_properties_tab(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
    ) {
        ui.label("💡 Only modified properties are saved");

        ScrollArea::both()
            .id_salt("element_properties_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(info) = element_info {
                    if !info.properties.is_empty() {
                        for prop_info in &info.properties {
                            Self::show_property_from_info(ui, element, prop_info);
                        }
                    } else {
                        ui.label("No element properties available");
                    }
                } else {
                    ui.label("No element metadata available");
                }
            });
    }

    /// Show the Input Pads tab content.
    fn show_input_pads_tab(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
        actual_pads: &[String],
        focused_pad: Option<&str>,
    ) {
        ui.label("💡 Only modified properties are saved");

        ScrollArea::both()
            .id_salt("input_pads_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if actual_pads.is_empty() {
                    ui.label("No input pads connected");
                    return;
                }

                for pad_name in actual_pads {
                    // Highlight focused pad
                    let is_focused = focused_pad == Some(pad_name.as_str());
                    if is_focused {
                        ui.colored_label(
                            Color32::from_rgb(255, 200, 100),
                            format!("▶ Input Pad: {}", pad_name),
                        );
                    } else {
                        ui.label(format!("Input Pad: {}", pad_name));
                    }

                    ui.indent(pad_name, |ui| {
                        // Find properties for this pad from element_info
                        if let Some(info) = element_info {
                            // Check if there's a matching sink pad in metadata (try template matching)
                            let pad_info = info
                                .sink_pads
                                .iter()
                                .find(|p| Self::matches_pad_template(pad_name, &p.name));

                            if let Some(pad_info) = pad_info {
                                if !pad_info.properties.is_empty() {
                                    for prop_info in &pad_info.properties {
                                        Self::show_pad_property_from_info(
                                            ui, element, pad_name, prop_info,
                                        );
                                    }
                                } else {
                                    ui.small("No configurable properties");
                                }
                            } else {
                                ui.small(format!(
                                    "No metadata for pad (tried matching: {})",
                                    pad_name
                                ));
                            }
                        } else {
                            ui.small("No element metadata available");
                        }
                    });
                    ui.add_space(8.0);
                }
            });
    }

    /// Show the Output Pads tab content.
    fn show_output_pads_tab(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
        actual_pads: &[String],
        focused_pad: Option<&str>,
    ) {
        ui.label("💡 Only modified properties are saved");

        ScrollArea::both()
            .id_salt("output_pads_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if actual_pads.is_empty() {
                    ui.label("No output pads connected");
                    return;
                }

                for pad_name in actual_pads {
                    // Highlight focused pad
                    let is_focused = focused_pad == Some(pad_name.as_str());
                    if is_focused {
                        ui.colored_label(
                            Color32::from_rgb(255, 200, 100),
                            format!("▶ Output Pad: {}", pad_name),
                        );
                    } else {
                        ui.label(format!("Output Pad: {}", pad_name));
                    }

                    ui.indent(pad_name, |ui| {
                        // Find properties for this pad from element_info
                        if let Some(info) = element_info {
                            // Check if there's a matching source pad in metadata (try template matching)
                            let pad_info = info
                                .src_pads
                                .iter()
                                .find(|p| Self::matches_pad_template(pad_name, &p.name));

                            if let Some(pad_info) = pad_info {
                                if !pad_info.properties.is_empty() {
                                    for prop_info in &pad_info.properties {
                                        Self::show_pad_property_from_info(
                                            ui, element, pad_name, prop_info,
                                        );
                                    }
                                } else {
                                    ui.small("No configurable properties");
                                }
                            } else {
                                ui.small(format!(
                                    "No metadata for pad (tried matching: {})",
                                    pad_name
                                ));
                            }
                        } else {
                            ui.small("No element metadata available");
                        }
                    });
                    ui.add_space(8.0);
                }
            });
    }

    /// Show the property inspector for the given block.
    /// Returns true if delete was requested.
    #[allow(clippy::too_many_arguments)]
    pub fn show_block(
        ui: &mut Ui,
        block: &mut BlockInstance,
        definition: &BlockDefinition,
        flow_id: Option<strom_types::FlowId>,
        meter_data_store: &crate::meter::MeterDataStore,
        webrtc_stats_store: &crate::webrtc_stats::WebRtcStatsStore,
        stats: Option<&crate::api::FlowStatsInfo>,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        available_channels: &[strom_types::api::AvailableOutput],
    ) -> bool {
        let block_id = block.id.clone();
        let mut delete_requested = false;

        ui.push_id(&block_id, |ui| {
            // Block name (read-only)
            ui.horizontal(|ui| {
                ui.label("Block:");
                ui.monospace(&definition.name);
            });

            // Block ID (read-only)
            ui.horizontal(|ui| {
                ui.label("ID:");
                ui.monospace(&block.id);
            });

            // Block description
            if !definition.description.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label("Description:");
                    ui.label(&definition.description);
                });
            }

            ui.separator();

            // Delete button
            if ui.button("🗑 Delete Block").clicked() {
                delete_requested = true;
            }

            // Edit Layout button for compositor blocks
            if (definition.id == "builtin.glcompositor" || definition.id == "builtin.compositor")
                && ui.button("✏ Edit Layout").clicked()
            {
                crate::app::set_local_storage("open_compositor_editor", &block.id);
            }

            ui.separator();

            ui.label("💡 Only modified properties are saved");

            ScrollArea::both()
                .id_salt("block_properties_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if !definition.exposed_properties.is_empty() {
                        for exposed_prop in &definition.exposed_properties {
                            Self::show_exposed_property(
                                ui,
                                block,
                                exposed_prop,
                                definition,
                                flow_id,
                                network_interfaces,
                                available_channels,
                            );
                        }
                    } else {
                        ui.label("This block has no configurable properties");
                    }

                    // Show meter visualization for meter blocks
                    if definition.id == "builtin.meter" {
                        ui.separator();
                        tracing::debug!("📊 Checking for meter data: flow_id={:?}, block_id={}", flow_id, block.id);
                        if let Some(flow_id) = flow_id {
                            if let Some(meter_data) = meter_data_store.get(&flow_id, &block.id) {
                                tracing::debug!("📊 Found meter data, calling show_full");
                                crate::meter::show_full(ui, &block.id, meter_data);
                            } else {
                                tracing::debug!("📊 No meter data found for this block");
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "⚠ No audio level data available",
                                );
                                ui.add_space(4.0);
                                ui.small("Meter data will appear when audio is flowing through this block.");
                            }
                        } else {
                            tracing::debug!("📊 No flow_id available");
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ No flow selected",
                            );
                        }
                    }

                    // Show SDP for AES67 output blocks
                    if definition.id == "builtin.aes67_output" {
                        ui.separator();
                        ui.heading("📡 SDP (Session Description)");
                        ui.add_space(4.0);

                        // Get SDP from runtime_data (only available when flow is running)
                        let sdp = block
                            .runtime_data
                            .as_ref()
                            .and_then(|data| data.get("sdp"))
                            .map(|s| s.as_str());

                        if let Some(mut sdp_text) = sdp {
                            ui.label("Copy this SDP to configure receivers:");
                            ui.add_space(4.0);

                            // Display SDP in a code-style text box
                            ui.add(
                                egui::TextEdit::multiline(&mut sdp_text)
                                    .desired_rows(12)
                                    .desired_width(f32::INFINITY)
                                    .code_editor()
                                    .interactive(false),
                            );

                            ui.add_space(4.0);

                            // Copy button
                            if ui.button("📋 Copy to Clipboard").clicked() {
                                ui.ctx().copy_text(sdp_text.to_string());
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ SDP is only available when the flow is running",
                            );
                            ui.add_space(4.0);
                            ui.small("Start the flow to generate SDP based on the actual stream capabilities.");
                        }
                    }

                    // Show WebRTC statistics for WHIP/WHEP blocks
                    if definition.id == "builtin.whep_input" || definition.id == "builtin.whip_output" {
                        ui.separator();
                        ui.heading("📊 WebRTC Statistics");
                        ui.add_space(4.0);

                        if let Some(flow_id) = flow_id {
                            if let Some(stats) = webrtc_stats_store.get(&flow_id) {
                                if !stats.connections.is_empty() {
                                    crate::webrtc_stats::show_full(ui, stats);
                                } else {
                                    ui.colored_label(
                                        Color32::from_rgb(200, 200, 100),
                                        "⚠ No WebRTC connections established",
                                    );
                                    ui.add_space(4.0);
                                    ui.small("WebRTC statistics will appear when the connection is established.");
                                }
                            } else {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "⚠ WebRTC statistics not available",
                                );
                                ui.add_space(4.0);
                                ui.small("Start the flow to see WebRTC statistics.");
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ No flow selected",
                            );
                        }
                    }

                    // Show RTP statistics for AES67 input blocks
                    if definition.id == "builtin.aes67_input" {
                        ui.separator();
                        ui.heading("📊 RTP Statistics");
                        ui.add_space(4.0);

                        // Find stats for this block
                        let block_stats = stats.and_then(|s| {
                            s.blocks.iter().find(|bs| bs.block_instance_id == block.id)
                        });

                        if let Some(block_stats) = block_stats {
                            // Use a grid for clean alignment
                            egui::Grid::new("rtp_stats_grid")
                                .num_columns(2)
                                .spacing([20.0, 4.0])
                                .show(ui, |ui| {
                                    for stat in &block_stats.stats {
                                        // Stat name with description tooltip
                                        let label = ui.label(&stat.metadata.display_name);
                                        label.on_hover_text(&stat.metadata.description);

                                        // Stat value formatted appropriately
                                        let formatted = stat.value.format();
                                        ui.monospace(&formatted);
                                        ui.end_row();
                                    }
                                });
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ Statistics are only available when the flow is running",
                            );
                            ui.add_space(4.0);
                            ui.small("Start the flow to see RTP jitterbuffer statistics.");
                        }
                    }
                });
        });

        delete_requested
    }

    fn show_exposed_property(
        ui: &mut Ui,
        block: &mut BlockInstance,
        exposed_prop: &ExposedProperty,
        definition: &BlockDefinition,
        _flow_id: Option<strom_types::FlowId>,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        available_channels: &[strom_types::api::AvailableOutput],
    ) {
        let prop_name = &exposed_prop.name;
        let display_label = &exposed_prop.label;
        let default_value = exposed_prop.default_value.as_ref();
        let is_multiline = matches!(
            exposed_prop.property_type,
            strom_types::block::PropertyType::Multiline
        );

        // Get current value or use default
        let mut current_value = block.properties.get(prop_name).cloned();
        let has_custom_value = current_value.is_some();

        if current_value.is_none() {
            current_value = default_value.cloned();
        }

        // For string properties without a value, initialize to empty string
        if current_value.is_none() {
            if let strom_types::block::PropertyType::String = &exposed_prop.property_type {
                current_value = Some(PropertyValue::String(String::new()));
            }
        }

        // For multiline, use vertical layout
        if is_multiline {
            // Property label with indicator
            ui.horizontal(|ui| {
                if has_custom_value {
                    ui.colored_label(
                        Color32::from_rgb(150, 100, 255), // Purple for blocks
                        format!("• {}:", display_label),
                    );
                } else {
                    ui.label(format!("{}:", display_label));
                }

                // Reset button if modified
                if has_custom_value
                    && ui
                        .small_button("↺")
                        .on_hover_text("Reset to default")
                        .clicked()
                {
                    block.properties.remove(prop_name);
                }
            });

            // Multiline editor
            let mut text = match current_value {
                Some(PropertyValue::String(s)) => s,
                _ => String::new(),
            };

            let response = ui.add(
                egui::TextEdit::multiline(&mut text)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .code_editor(),
            );

            if response.changed() {
                // Only save if different from default
                if let Some(PropertyValue::String(default)) = default_value {
                    if text != *default {
                        block
                            .properties
                            .insert(prop_name.clone(), PropertyValue::String(text));
                    } else {
                        block.properties.remove(prop_name);
                    }
                } else if !text.is_empty() {
                    block
                        .properties
                        .insert(prop_name.clone(), PropertyValue::String(text));
                } else {
                    block.properties.remove(prop_name);
                }
            }
        } else {
            // For non-multiline, use horizontal layout
            ui.horizontal(|ui| {
                // Show property label with indicator if modified
                if has_custom_value {
                    ui.colored_label(
                        Color32::from_rgb(150, 100, 255), // Purple for blocks
                        format!("• {}:", display_label),
                    );
                } else {
                    ui.label(format!("{}:", display_label));
                }

                if let Some(mut value) = current_value {
                    // Special handling for InterInput channel property - show dropdown with available channels
                    let is_inter_input_channel =
                        definition.id == "builtin.inter_input" && prop_name == "channel";

                    let changed = if is_inter_input_channel {
                        Self::show_inter_channel_editor(ui, &mut value, available_channels)
                    } else {
                        // Check property type for special handling
                        match &exposed_prop.property_type {
                            strom_types::block::PropertyType::Enum { values } => {
                                Self::show_block_enum_editor(ui, &mut value, values)
                            }
                            strom_types::block::PropertyType::NetworkInterface => {
                                Self::show_network_interface_editor(
                                    ui,
                                    &mut value,
                                    network_interfaces,
                                )
                            }
                            _ => {
                                // Convert block::PropertyType to element::PropertyType for other types
                                let prop_type =
                                    Self::convert_block_prop_type(&exposed_prop.property_type);
                                Self::show_property_editor(
                                    ui,
                                    &mut value,
                                    prop_type.as_ref(),
                                    default_value,
                                    false, // Block properties are always writable
                                )
                            }
                        }
                    };

                    if changed {
                        // Only save if different from default
                        if let Some(default) = default_value {
                            if !Self::values_equal(&value, default) {
                                block.properties.insert(prop_name.clone(), value);
                            } else {
                                block.properties.remove(prop_name);
                            }
                        } else {
                            block.properties.insert(prop_name.clone(), value);
                        }
                    }
                }

                // Reset button if modified
                if has_custom_value
                    && ui
                        .small_button("↺")
                        .on_hover_text("Reset to default")
                        .clicked()
                {
                    block.properties.remove(prop_name);
                }
            });
        }

        // Show description
        if !exposed_prop.description.is_empty() {
            ui.indent(prop_name, |ui| {
                ui.small(&exposed_prop.description);
            });
        }

        // Add spacing after each property
        ui.add_space(8.0);
    }

    fn show_pad_property_from_info(
        ui: &mut Ui,
        element: &mut Element,
        pad_name: &str,
        prop_info: &PropertyInfo,
    ) {
        let prop_name = &prop_info.name;
        let default_value = prop_info.default_value.as_ref();

        // Get current value from pad_properties or use default
        let mut current_value = element
            .pad_properties
            .get(pad_name)
            .and_then(|props| props.get(prop_name))
            .cloned();
        let has_custom_value = current_value.is_some();

        if current_value.is_none() {
            current_value = default_value.cloned();

            // For enum properties without default value, initialize to first option
            if current_value.is_none() {
                if let PropertyType::Enum { values } = &prop_info.property_type {
                    if let Some(first_value) = values.first() {
                        current_value = Some(PropertyValue::String(first_value.clone()));
                    }
                }
            }
        }

        ui.horizontal(|ui| {
            // Show property name with indicator if modified
            if has_custom_value {
                ui.colored_label(
                    Color32::from_rgb(255, 150, 100), // Orange for pad properties
                    format!("• {}:", prop_name),
                );
            } else {
                ui.label(format!("{}:", prop_name));
            }

            if let Some(mut value) = current_value {
                let changed = Self::show_property_editor(
                    ui,
                    &mut value,
                    Some(&prop_info.property_type),
                    default_value,
                    !prop_info.writable, // Read-only if not writable
                );

                if changed {
                    // Ensure the pad_properties map exists
                    element
                        .pad_properties
                        .entry(pad_name.to_string())
                        .or_default();

                    // Only save if different from default
                    if let Some(default) = default_value {
                        if !Self::values_equal(&value, default) {
                            element
                                .pad_properties
                                .get_mut(pad_name)
                                .unwrap()
                                .insert(prop_name.clone(), value);
                        } else {
                            // Remove if same as default
                            if let Some(props) = element.pad_properties.get_mut(pad_name) {
                                props.remove(prop_name);
                                // Clean up empty pad property maps
                                if props.is_empty() {
                                    element.pad_properties.remove(pad_name);
                                }
                            }
                        }
                    } else {
                        element
                            .pad_properties
                            .get_mut(pad_name)
                            .unwrap()
                            .insert(prop_name.clone(), value);
                    }
                }
            }

            // Reset button if modified (only show for writable properties)
            if has_custom_value
                && prop_info.writable
                && ui
                    .small_button("↺")
                    .on_hover_text("Reset to default")
                    .clicked()
            {
                if let Some(props) = element.pad_properties.get_mut(pad_name) {
                    props.remove(prop_name);
                    // Clean up empty pad property maps
                    if props.is_empty() {
                        element.pad_properties.remove(pad_name);
                    }
                }
            }
        });

        // Show description
        if !prop_info.description.is_empty() {
            ui.indent(prop_name, |ui| {
                ui.small(&prop_info.description);
            });
        }

        // Add spacing after each property
        ui.add_space(8.0);
    }

    fn show_property_from_info(ui: &mut Ui, element: &mut Element, prop_info: &PropertyInfo) {
        let prop_name = &prop_info.name;
        let default_value = prop_info.default_value.as_ref();

        // Debug logging for location property
        if prop_name == "location" {
            tracing::debug!(
                "Rendering property '{}' for element '{}': writable={}, construct_only={}, type={:?}",
                prop_name,
                element.element_type,
                prop_info.writable,
                prop_info.construct_only,
                prop_info.property_type
            );
        }

        // Get current value or use default
        let mut current_value = element.properties.get(prop_name).cloned();
        let has_custom_value = current_value.is_some();

        if current_value.is_none() {
            current_value = default_value.cloned();

            // For enum properties without default value, initialize to first option
            if current_value.is_none() {
                if let PropertyType::Enum { values } = &prop_info.property_type {
                    if let Some(first_value) = values.first() {
                        current_value = Some(PropertyValue::String(first_value.clone()));
                    }
                }
            }

            // For writable properties without a value, create an empty/default value
            if current_value.is_none() && prop_info.writable {
                current_value = Some(match &prop_info.property_type {
                    PropertyType::String => PropertyValue::String(String::new()),
                    PropertyType::Int { min, .. } => PropertyValue::Int(*min),
                    PropertyType::UInt { min, .. } => PropertyValue::UInt(*min),
                    PropertyType::Float { min, .. } => PropertyValue::Float(*min),
                    PropertyType::Bool => PropertyValue::Bool(false),
                    PropertyType::Enum { values } => {
                        PropertyValue::String(values.first().cloned().unwrap_or_default())
                    }
                });
            }
        }

        ui.horizontal(|ui| {
            // Show property name with indicator if modified
            if has_custom_value {
                ui.colored_label(
                    Color32::from_rgb(100, 200, 255),
                    format!("• {}:", prop_name),
                );
            } else {
                ui.label(format!("{}:", prop_name));
            }

            if let Some(mut value) = current_value {
                let changed = Self::show_property_editor(
                    ui,
                    &mut value,
                    Some(&prop_info.property_type),
                    default_value,
                    !prop_info.writable, // Read-only if not writable
                );

                if changed {
                    // Only save if different from default
                    if let Some(default) = default_value {
                        if !Self::values_equal(&value, default) {
                            element.properties.insert(prop_name.clone(), value);
                        } else {
                            element.properties.remove(prop_name);
                        }
                    } else {
                        element.properties.insert(prop_name.clone(), value);
                    }
                }
            }

            // Reset button if modified (only show for writable properties)
            if has_custom_value
                && prop_info.writable
                && ui
                    .small_button("↺")
                    .on_hover_text("Reset to default")
                    .clicked()
            {
                element.properties.remove(prop_name);
            }
        });

        // Show description
        if !prop_info.description.is_empty() {
            ui.indent(prop_name, |ui| {
                ui.small(&prop_info.description);
            });
        }

        // Add spacing after each property
        ui.add_space(8.0);
    }

    fn values_equal(a: &PropertyValue, b: &PropertyValue) -> bool {
        match (a, b) {
            (PropertyValue::String(a), PropertyValue::String(b)) => a == b,
            (PropertyValue::Int(a), PropertyValue::Int(b)) => a == b,
            (PropertyValue::UInt(a), PropertyValue::UInt(b)) => a == b,
            (PropertyValue::Float(a), PropertyValue::Float(b)) => (a - b).abs() < 0.0001,
            (PropertyValue::Bool(a), PropertyValue::Bool(b)) => a == b,
            _ => false,
        }
    }

    /// Convert block::PropertyType to element::PropertyType for the property editor.
    fn convert_block_prop_type(
        block_prop: &strom_types::block::PropertyType,
    ) -> Option<PropertyType> {
        match block_prop {
            strom_types::block::PropertyType::Enum { values } => Some(PropertyType::Enum {
                values: values.iter().map(|ev| ev.value.clone()).collect(),
            }),
            // Other types don't need conversion (no constraints)
            _ => None,
        }
    }

    /// Show enum editor for block properties with labels.
    fn show_block_enum_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        enum_values: &[EnumValue],
    ) -> bool {
        if let PropertyValue::String(s) = value {
            let mut changed = false;

            // Find the label for the current value
            let current_label = enum_values
                .iter()
                .find(|ev| ev.value == *s)
                .and_then(|ev| ev.label.as_ref())
                .cloned()
                .unwrap_or_else(|| s.clone());

            egui::ComboBox::from_id_salt(ui.next_auto_id())
                .selected_text(&current_label)
                .show_ui(ui, |ui| {
                    for enum_val in enum_values {
                        // Display label if available, otherwise just the value
                        let display_text = enum_val.label.as_deref().unwrap_or(&enum_val.value);

                        if ui
                            .selectable_label(*s == enum_val.value, display_text)
                            .clicked()
                        {
                            *s = enum_val.value.clone();
                            changed = true;
                        }
                    }
                });
            changed
        } else {
            false
        }
    }

    /// Show network interface selector dropdown.
    fn show_network_interface_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        interfaces: &[strom_types::NetworkInterfaceInfo],
    ) -> bool {
        if let PropertyValue::String(s) = value {
            let mut changed = false;

            // Build display text for current selection
            let selected_display = if s.is_empty() {
                "Default (all interfaces)".to_string()
            } else {
                // Find interface to show with IP info
                interfaces
                    .iter()
                    .find(|iface| iface.name == *s)
                    .map(|iface| {
                        let ip = iface
                            .ipv4_addresses
                            .first()
                            .map(|addr| addr.address.as_str())
                            .unwrap_or("no IP");
                        format!("{} ({})", iface.name, ip)
                    })
                    .unwrap_or_else(|| s.clone())
            };

            egui::ComboBox::from_id_salt(ui.next_auto_id())
                .selected_text(&selected_display)
                .show_ui(ui, |ui| {
                    // Default option - empty string
                    if ui
                        .selectable_label(s.is_empty(), "Default (all interfaces)")
                        .clicked()
                    {
                        *s = String::new();
                        changed = true;
                    }

                    // List all available interfaces
                    for iface in interfaces {
                        // Skip loopback interfaces
                        if iface.is_loopback {
                            continue;
                        }

                        // Build display with IP info
                        let ip_info = iface
                            .ipv4_addresses
                            .first()
                            .map(|addr| addr.address.as_str())
                            .unwrap_or("no IP");
                        let display = format!("{} ({})", iface.name, ip_info);

                        if ui.selectable_label(*s == iface.name, &display).clicked() {
                            *s = iface.name.clone();
                            changed = true;
                        }
                    }
                });

            changed
        } else {
            false
        }
    }

    /// Show channel selector for InterInput blocks.
    /// Shows a dropdown with available channels (from all flows with InterOutput blocks).
    fn show_inter_channel_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        available_channels: &[strom_types::api::AvailableOutput],
    ) -> bool {
        if let PropertyValue::String(s) = value {
            let mut changed = false;

            // Helper to format display text for a channel
            let format_channel_display = |ch: &strom_types::api::AvailableOutput| -> String {
                let name_part = ch
                    .description
                    .as_ref()
                    .filter(|d| !d.is_empty())
                    .map(|d| d.as_str())
                    .unwrap_or(&ch.name);
                let status = if ch.is_active { "▶" } else { "■" };
                format!("{} {} / {}", status, ch.flow_name, name_part)
            };

            // Build display text for current selection
            let selected_display = if s.is_empty() {
                "(select channel)".to_string()
            } else {
                // Find channel to show with more info
                available_channels
                    .iter()
                    .find(|ch| ch.channel_name == *s)
                    .map(format_channel_display)
                    .unwrap_or_else(|| format!("(unknown: {})", s))
            };

            egui::ComboBox::from_id_salt(ui.next_auto_id())
                .selected_text(&selected_display)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if available_channels.is_empty() {
                        ui.label("No Inter Output blocks found");
                        ui.small("Add Inter Output blocks to flows to publish streams");
                    } else {
                        for channel in available_channels {
                            let display = format_channel_display(channel);
                            let response =
                                ui.selectable_label(*s == channel.channel_name, &display);

                            // Show tooltip with channel details
                            response.clone().on_hover_ui(|ui| {
                                ui.label(format!("Flow: {}", channel.flow_name));
                                if let Some(desc) = &channel.description {
                                    ui.label(format!("Description: {}", desc));
                                }
                                ui.label(format!("Block ID: {}", channel.name));
                                ui.label(format!(
                                    "Status: {}",
                                    if channel.is_active {
                                        "Active"
                                    } else {
                                        "Inactive"
                                    }
                                ));
                                ui.small(&channel.channel_name);
                            });

                            if response.clicked() {
                                *s = channel.channel_name.clone();
                                changed = true;
                            }
                        }
                    }
                });

            changed
        } else {
            false
        }
    }

    fn show_property_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        prop_type: Option<&PropertyType>,
        _default_value: Option<&PropertyValue>,
        read_only: bool,
    ) -> bool {
        if read_only {
            // Display as non-editable text with a subtle background
            let text = match value {
                PropertyValue::String(s) => s.clone(),
                PropertyValue::Int(i) => i.to_string(),
                PropertyValue::UInt(u) => u.to_string(),
                PropertyValue::Float(f) => format!("{:.3}", f),
                PropertyValue::Bool(b) => b.to_string(),
            };
            ui.label(egui::RichText::new(text).color(Color32::from_rgb(150, 150, 150)))
                .on_hover_text("Read-only property");
            false
        } else {
            match (value, prop_type) {
                (PropertyValue::String(s), Some(PropertyType::Enum { values })) => {
                    // Enum dropdown
                    let mut changed = false;
                    egui::ComboBox::from_id_salt(ui.next_auto_id())
                        .selected_text(s.as_str())
                        .show_ui(ui, |ui| {
                            for val in values {
                                if ui.selectable_label(s == val, val).clicked() {
                                    *s = val.clone();
                                    changed = true;
                                }
                            }
                        });
                    changed
                }
                (PropertyValue::String(s), _) => ui.text_edit_singleline(s).changed(),
                (PropertyValue::Int(i), Some(PropertyType::Int { min, max })) => {
                    ui.add(egui::Slider::new(i, *min..=*max)).changed()
                }
                (PropertyValue::Int(i), _) => ui.add(egui::DragValue::new(i)).changed(),
                (PropertyValue::UInt(u), Some(PropertyType::UInt { min, max })) => {
                    ui.add(egui::Slider::new(u, *min..=*max)).changed()
                }
                (PropertyValue::UInt(u), _) => ui.add(egui::DragValue::new(u)).changed(),
                (PropertyValue::Float(f), Some(PropertyType::Float { min, max })) => {
                    ui.add(egui::Slider::new(f, *min..=*max)).changed()
                }
                (PropertyValue::Float(f), _) => {
                    ui.add(egui::DragValue::new(f).speed(0.1)).changed()
                }
                (PropertyValue::Bool(b), _) => ui.checkbox(b, "").changed(),
            }
        }
    }
}
