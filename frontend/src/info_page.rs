//! Info page for displaying system and version information.

use eframe::glow::{self, HasContext};
use egui::{Color32, Pos2, Rect, Stroke, Ui, Vec2};
use std::collections::VecDeque;

use crate::api::SystemInfo;
use crate::system_monitor::SystemMonitorStore;

const HISTORY_SIZE: usize = 60;

/// Rendering backend information detected at startup.
pub struct RendererInfo {
    /// Short display label, e.g. "wgpu (Metal)" or "glow (OpenGL)"
    pub display: String,
    /// Key-value detail pairs for expanded view
    pub details: Vec<(&'static str, String)>,
}

/// Detect the active rendering backend from eframe's CreationContext.
/// Checks which renderer is actually active at runtime.
pub fn detect_renderer(cc: &eframe::CreationContext<'_>) -> RendererInfo {
    // wgpu renderer detection. The `wgpu_render_state` field only exists when
    // eframe is built with the `wgpu` feature, which we enable on native only
    // (WASM uses glow/WebGL), so this block must be gated to native targets.
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(render_state) = &cc.wgpu_render_state {
        let info = render_state.adapter.get_info();
        let mut details = vec![
            ("Backend", format!("{:?}", info.backend)),
            ("Device Type", format!("{:?}", info.device_type)),
            ("Name", info.name.clone()),
        ];
        if !info.driver.is_empty() {
            details.push(("Driver", info.driver.clone()));
        }
        if !info.driver_info.is_empty() {
            details.push(("Driver Info", info.driver_info.clone()));
        }
        if info.vendor != 0 {
            details.push(("Vendor", format!("0x{:04X}", info.vendor)));
        }
        return RendererInfo {
            display: format!("wgpu ({:?})", info.backend),
            details,
        };
    }

    if let Some(gl) = &cc.gl {
        let details = unsafe {
            vec![
                ("Version", gl.get_parameter_string(glow::VERSION)),
                ("Renderer", gl.get_parameter_string(glow::RENDERER)),
                ("Vendor", gl.get_parameter_string(glow::VENDOR)),
                (
                    "GLSL",
                    gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
                ),
            ]
        };
        let label = {
            #[cfg(target_arch = "wasm32")]
            {
                "glow (WebGL)"
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                "glow (OpenGL)"
            }
        };
        return RendererInfo {
            display: label.to_string(),
            details,
        };
    }

    RendererInfo {
        display: "unknown".to_string(),
        details: Vec::new(),
    }
}

/// Get the current time as Unix timestamp in milliseconds
pub(crate) fn current_time_millis() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        chrono::Local::now().timestamp_millis()
    }
}

/// Parse an ISO 8601 datetime string to Unix timestamp in milliseconds
/// Returns None if parsing fails
pub(crate) fn parse_iso8601_to_millis(s: &str) -> Option<i64> {
    #[cfg(target_arch = "wasm32")]
    {
        let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(s));
        let time = date.get_time();
        if time.is_nan() {
            None
        } else {
            Some(time as i64)
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Simple RFC3339 parser for native
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp_millis())
    }
}

/// Format an ISO 8601 datetime string to local time display
pub(crate) fn format_datetime_local(iso_str: &str) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(iso_str));
        if date.get_time().is_nan() {
            iso_str.to_string()
        } else {
            // Format as localized string
            date.to_locale_string("sv-SE", &js_sys::Object::new())
                .as_string()
                .unwrap_or_else(|| iso_str.to_string())
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        chrono::DateTime::parse_from_rfc3339(iso_str)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|_| iso_str.to_string())
    }
}

/// Format a duration in milliseconds to a human-readable string
/// e.g., "2d 5h 30m 15s" or "5h 30m" or "30m 15s"
pub(crate) fn format_uptime(millis: i64) -> String {
    if millis < 0 {
        return "N/A".to_string();
    }

    let total_seconds = millis / 1000;
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    let mut parts = Vec::new();

    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 || days > 0 {
        parts.push(format!("{}h", hours));
    }
    if minutes > 0 || hours > 0 || days > 0 {
        parts.push(format!("{}m", minutes));
    }
    parts.push(format!("{}s", seconds));

    parts.join(" ")
}
const MARGIN: f32 = 16.0;
const GAP: f32 = 12.0;
const GRAPH_HEIGHT: f32 = 60.0;
// Minimum content width to ensure readability
const MIN_CONTENT_WIDTH: f32 = 800.0;
// Frame inner margin (used in render_box)
const BOX_INNER_MARGIN: f32 = 12.0;

/// Quick-select level presets for strom (tracing) logging.
const STROM_LEVEL_PRESETS: &[(&str, &str)] = &[
    ("error", "error"),
    ("warn", "warn"),
    ("info", "info"),
    ("debug", "debug"),
    ("trace", "trace"),
];

/// Named filter presets for strom logging.
const STROM_FILTER_PRESETS: &[(&str, &str)] = &[
    ("API debug", "info,strom::api=debug"),
    ("Pipeline debug", "info,strom::gst=debug"),
    ("Pipeline trace", "info,strom::gst=trace"),
];

/// Quick-select level presets for GStreamer logging.
const GST_LEVEL_PRESETS: &[(&str, &str)] = &[
    ("none", "*:0"),
    ("error", "*:1"),
    ("warning", "*:2"),
    ("info", "*:4"),
    ("debug", "*:5"),
    ("trace", "*:7"),
];

/// Named filter presets for GStreamer logging.
const GST_FILTER_PRESETS: &[(&str, &str)] = &[
    ("WebRTC debug", "*:2,webrtcbin:5,webrtcice:5"),
    ("Encoding debug", "*:2,nvenc:5,x264enc:5,x265enc:5"),
    ("Compositor debug", "*:2,compositor:5,glvideomixer:5"),
];

/// Callback actions from the info page back to the app.
pub enum InfoPageAction {
    /// Load the current strom log level from the backend
    LoadStromLog,
    /// Set a new strom log filter on the backend
    ApplyStromFilter(String),
    /// Load the current GStreamer debug level from the backend
    LoadGstLog,
    /// Set a new GStreamer debug filter on the backend
    ApplyGstFilter(String),
}

/// Info page state.
pub struct InfoPage {
    /// Whether we've requested network interfaces load
    requested_network_load: bool,
    /// Whether we've requested the log level load
    requested_log_level_load: bool,
    /// Text input buffer for the strom log filter
    log_filter_input: String,
    /// Text input buffer for the GStreamer debug filter
    gst_filter_input: String,
}

impl InfoPage {
    pub fn new() -> Self {
        Self {
            requested_network_load: false,
            requested_log_level_load: false,
            log_filter_input: String::new(),
            gst_filter_input: String::new(),
        }
    }

    /// Check if network interfaces should be loaded (call once on page show).
    pub fn should_load_network(&mut self) -> bool {
        if !self.requested_network_load {
            self.requested_network_load = true;
            true
        } else {
            false
        }
    }

    /// Check if log level should be loaded (call once on page show).
    pub fn should_load_log_level(&mut self) -> bool {
        if !self.requested_log_level_load {
            self.requested_log_level_load = true;
            true
        } else {
            false
        }
    }

    /// Render the info page. Returns an optional action for the app to handle.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        ui: &mut Ui,
        version_info: Option<&SystemInfo>,
        system_monitor: &SystemMonitorStore,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        flows: &[strom_types::Flow],
        renderer_info: &RendererInfo,
        log_level_current: Option<&str>,
        log_level_default: Option<&str>,
        gst_log_level_current: Option<&str>,
        gst_log_level_default: Option<&str>,
    ) -> Option<InfoPageAction> {
        // Get available width and use minimum if window is too small
        // Subtract extra padding to account for scrollbar and frame overhead
        let available_width = ui.available_width() - 60.0;
        let content_width = (available_width - 2.0 * MARGIN).max(MIN_CONTENT_WIDTH);

        // Calculate box widths
        // Row 1: 3 boxes with 2 gaps → box = (content - 2*gap) / 3
        let box_width_3 = (content_width - 2.0 * GAP) / 3.0;
        // Row 2+3: 2 boxes with 1 gap → box = (content - gap) / 2
        let box_width_2 = (content_width - GAP) / 2.0;

        let mut action = None;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Disable default item spacing in horizontal layouts
                ui.spacing_mut().item_spacing.x = 0.0;

                ui.add_space(MARGIN);

                // Row 1: Version & Build | System | GStreamer
                ui.horizontal(|ui| {
                    ui.add_space(MARGIN);

                    render_box(ui, "Version & Build", box_width_3, |ui| {
                        self.render_version_content(ui, version_info);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "System", box_width_3, |ui| {
                        self.render_system_content(ui, version_info, flows);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "Process", box_width_3, |ui| {
                        self.render_process_content(ui, version_info, flows, renderer_info);
                    });
                });

                ui.add_space(GAP);

                // Row 2: CPU | Memory
                ui.horizontal(|ui| {
                    ui.add_space(MARGIN);

                    render_box(ui, "CPU", box_width_2, |ui| {
                        self.render_cpu_content(ui, system_monitor, box_width_2);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "Memory", box_width_2, |ui| {
                        self.render_memory_content(ui, system_monitor, box_width_2);
                    });
                });

                ui.add_space(GAP);

                // Row 3: GPU | Network
                ui.horizontal(|ui| {
                    ui.add_space(MARGIN);

                    render_box(ui, "GPU", box_width_2, |ui| {
                        self.render_gpu_content(ui, system_monitor, box_width_2);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "Network Interfaces", box_width_2, |ui| {
                        self.render_network_content(ui, network_interfaces);
                    });
                });

                ui.add_space(GAP);

                // Row 4: strom logging | GStreamer logging
                ui.horizontal(|ui| {
                    ui.add_space(MARGIN);

                    render_box(ui, "strom logging", box_width_2, |ui| {
                        action =
                            self.render_strom_logging(ui, log_level_current, log_level_default);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "GStreamer logging", box_width_2, |ui| {
                        let gst_action = self.render_gst_logging(
                            ui,
                            gst_log_level_current,
                            gst_log_level_default,
                        );
                        if gst_action.is_some() {
                            action = gst_action;
                        }
                    });
                });

                ui.add_space(MARGIN);
            });

        action
    }

    fn render_strom_logging(
        &mut self,
        ui: &mut Ui,
        current: Option<&str>,
        default: Option<&str>,
    ) -> Option<InfoPageAction> {
        let mut action = None;
        // Restore default horizontal spacing inside the box
        ui.spacing_mut().item_spacing.x = 8.0;

        if let Some(current_filter) = current {
            egui::Grid::new("strom_logging_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Active filter:");
                    ui.label(egui::RichText::new(current_filter).monospace());
                    ui.end_row();

                    if let Some(default_filter) = default {
                        ui.label("Default:");
                        ui.label(egui::RichText::new(default_filter).monospace().weak());
                        ui.end_row();
                    }
                });

            ui.add_space(8.0);

            // Quick-select level buttons
            ui.horizontal(|ui| {
                for (label, filter) in STROM_LEVEL_PRESETS {
                    let active = current_filter == *filter;
                    if ui
                        .add(egui::Button::new(if active {
                            egui::RichText::new(*label).strong()
                        } else {
                            egui::RichText::new(*label)
                        }))
                        .clicked()
                    {
                        self.log_filter_input = filter.to_string();
                        action = Some(InfoPageAction::ApplyStromFilter(filter.to_string()));
                    }
                }
            });

            ui.add_space(4.0);

            // Preset filter buttons
            ui.horizontal(|ui| {
                ui.label("Presets:");
                for (label, filter) in STROM_FILTER_PRESETS {
                    let active = current_filter == *filter;
                    if ui
                        .add(egui::Button::new(if active {
                            egui::RichText::new(*label).strong().monospace()
                        } else {
                            egui::RichText::new(*label).monospace()
                        }))
                        .on_hover_text(*filter)
                        .clicked()
                    {
                        self.log_filter_input = filter.to_string();
                        action = Some(InfoPageAction::ApplyStromFilter(filter.to_string()));
                    }
                }
            });

            ui.add_space(8.0);

            // Initialize the input buffer from current value if empty
            if self.log_filter_input.is_empty() {
                self.log_filter_input = current_filter.to_string();
            }

            let response = ui.horizontal(|ui| {
                ui.label("Filter:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.log_filter_input)
                        .desired_width(ui.available_width() - 8.0)
                        .font(egui::TextStyle::Monospace),
                )
            });
            let response = response.inner;

            let input_changed = self.log_filter_input.trim() != current_filter;

            ui.add_space(4.0);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        input_changed && !self.log_filter_input.trim().is_empty(),
                        egui::Button::new("Apply"),
                    )
                    .clicked()
                    || (response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && input_changed
                        && !self.log_filter_input.trim().is_empty())
                {
                    action = Some(InfoPageAction::ApplyStromFilter(
                        self.log_filter_input.trim().to_string(),
                    ));
                }

                if let Some(default_filter) = default {
                    if current_filter != default_filter && ui.button("Reset to Default").clicked() {
                        self.log_filter_input = default_filter.to_string();
                        action = Some(InfoPageAction::ApplyStromFilter(default_filter.to_string()));
                    }
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading...");
            });
            action = Some(InfoPageAction::LoadStromLog);
        }

        action
    }

    fn render_gst_logging(
        &mut self,
        ui: &mut Ui,
        current: Option<&str>,
        default: Option<&str>,
    ) -> Option<InfoPageAction> {
        let mut action = None;
        // Restore default horizontal spacing inside the box
        ui.spacing_mut().item_spacing.x = 8.0;

        if let Some(current_filter) = current {
            egui::Grid::new("gst_logging_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Active filter:");
                    ui.label(egui::RichText::new(current_filter).monospace());
                    ui.end_row();

                    if let Some(default_filter) = default {
                        ui.label("Default:");
                        ui.label(egui::RichText::new(default_filter).monospace().weak());
                        ui.end_row();
                    }
                });

            ui.add_space(8.0);

            // Quick-select level buttons
            ui.horizontal(|ui| {
                for (label, filter) in GST_LEVEL_PRESETS {
                    let active = current_filter == *filter;
                    if ui
                        .add(egui::Button::new(if active {
                            egui::RichText::new(*label).strong()
                        } else {
                            egui::RichText::new(*label)
                        }))
                        .clicked()
                    {
                        self.gst_filter_input = filter.to_string();
                        action = Some(InfoPageAction::ApplyGstFilter(filter.to_string()));
                    }
                }
            });

            ui.add_space(4.0);

            // Preset filter buttons
            ui.horizontal(|ui| {
                ui.label("Presets:");
                for (label, filter) in GST_FILTER_PRESETS {
                    let active = current_filter == *filter;
                    if ui
                        .add(egui::Button::new(if active {
                            egui::RichText::new(*label).strong().monospace()
                        } else {
                            egui::RichText::new(*label).monospace()
                        }))
                        .on_hover_text(*filter)
                        .clicked()
                    {
                        self.gst_filter_input = filter.to_string();
                        action = Some(InfoPageAction::ApplyGstFilter(filter.to_string()));
                    }
                }
            });

            ui.add_space(8.0);

            // Initialize the input buffer from current value if empty
            if self.gst_filter_input.is_empty() {
                self.gst_filter_input = current_filter.to_string();
            }

            let response = ui.horizontal(|ui| {
                ui.label("Filter:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.gst_filter_input)
                        .desired_width(ui.available_width() - 8.0)
                        .font(egui::TextStyle::Monospace),
                )
            });
            let response = response.inner;

            let input_changed = self.gst_filter_input.trim() != current_filter;

            ui.add_space(4.0);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        input_changed && !self.gst_filter_input.trim().is_empty(),
                        egui::Button::new("Apply"),
                    )
                    .clicked()
                    || (response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && input_changed
                        && !self.gst_filter_input.trim().is_empty())
                {
                    action = Some(InfoPageAction::ApplyGstFilter(
                        self.gst_filter_input.trim().to_string(),
                    ));
                }

                if let Some(default_filter) = default {
                    if current_filter != default_filter && ui.button("Reset to Default").clicked() {
                        self.gst_filter_input = default_filter.to_string();
                        action = Some(InfoPageAction::ApplyGstFilter(default_filter.to_string()));
                    }
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading...");
            });
            action = Some(InfoPageAction::LoadGstLog);
        }

        action
    }

    fn render_version_content(&self, ui: &mut Ui, version_info: Option<&SystemInfo>) {
        if let Some(info) = version_info {
            egui::Grid::new("version_grid")
                .num_columns(2)
                .spacing([8.0, 2.0])
                .show(ui, |ui| {
                    ui.label("Version:");
                    ui.label(egui::RichText::new(format!("v{}", info.version)).strong());
                    ui.end_row();

                    if !info.git_tag.is_empty() {
                        ui.label("Tag:");
                        ui.label(&info.git_tag);
                        ui.end_row();
                    }

                    ui.label("Git Hash:");
                    ui.label(egui::RichText::new(&info.git_hash).monospace());
                    ui.end_row();

                    ui.label("Branch:");
                    ui.label(&info.git_branch);
                    ui.end_row();

                    if info.git_dirty {
                        ui.label("Status:");
                        ui.colored_label(Color32::YELLOW, "Modified");
                        ui.end_row();
                    }

                    ui.label("Build:");
                    ui.label(&info.build_timestamp);
                    ui.end_row();

                    if !info.build_id.is_empty() {
                        ui.label("Build ID:");
                        ui.label(
                            egui::RichText::new(&info.build_id[..8.min(info.build_id.len())])
                                .monospace(),
                        );
                        ui.end_row();
                    }
                });
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading...");
            });
        }
    }

    fn render_system_content(
        &self,
        ui: &mut Ui,
        version_info: Option<&SystemInfo>,
        _flows: &[strom_types::Flow],
    ) {
        egui::Grid::new("system_grid")
            .num_columns(2)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                if let Some(info) = version_info {
                    ui.label("OS:");
                    ui.label(&info.os_info);
                    ui.end_row();

                    ui.label("Environment:");
                    if info.in_docker {
                        ui.colored_label(Color32::from_rgb(100, 180, 255), "Docker");
                    } else {
                        ui.label("Native");
                    }
                    ui.end_row();

                    // System boot time and uptime
                    if !info.system_boot_time.is_empty() {
                        ui.label("Booted:");
                        ui.label(format_datetime_local(&info.system_boot_time));
                        ui.end_row();

                        if let Some(boot_millis) = parse_iso8601_to_millis(&info.system_boot_time) {
                            let uptime_millis = current_time_millis() - boot_millis;
                            ui.label("Uptime:");
                            ui.label(format_uptime(uptime_millis));
                            ui.end_row();
                        }
                    }
                }

                // Get hostname from browser location in WASM
                #[cfg(target_arch = "wasm32")]
                {
                    if let Some(window) = web_sys::window() {
                        if let Ok(host) = window.location().host() {
                            ui.label("Backend:");
                            ui.label(host);
                            ui.end_row();
                        }
                    }
                }
            });
    }

    fn render_process_content(
        &self,
        ui: &mut Ui,
        version_info: Option<&SystemInfo>,
        flows: &[strom_types::Flow],
        renderer_info: &RendererInfo,
    ) {
        if let Some(info) = version_info {
            egui::Grid::new("process_grid")
                .num_columns(2)
                .spacing([8.0, 2.0])
                .show(ui, |ui| {
                    ui.label("GStreamer:");
                    ui.label(egui::RichText::new(&info.gstreamer_version).monospace());
                    ui.end_row();

                    // Show process start time and uptime
                    if !info.process_started_at.is_empty() {
                        ui.label("Started:");
                        ui.label(format_datetime_local(&info.process_started_at));
                        ui.end_row();

                        if let Some(started_millis) =
                            parse_iso8601_to_millis(&info.process_started_at)
                        {
                            let uptime_millis = current_time_millis() - started_millis;
                            ui.label("Uptime:");
                            ui.label(format_uptime(uptime_millis));
                            ui.end_row();
                        }
                    }

                    // Flow statistics
                    let running_flows = flows.iter().filter(|f| f.running).count();
                    let total_flows = flows.len();

                    ui.label("Flows:");
                    ui.label(format!("{} / {}", running_flows, total_flows));
                    ui.end_row();
                });

            // GUI renderer as collapsible with details inside
            ui.add_space(4.0);
            let header_text = format!("GUI Renderer (egui): {}", renderer_info.display);
            egui::CollapsingHeader::new(egui::RichText::new(header_text).monospace())
                .default_open(false)
                .show(ui, |ui| {
                    if !renderer_info.details.is_empty() {
                        egui::Grid::new("renderer_details")
                            .num_columns(2)
                            .spacing([8.0, 2.0])
                            .show(ui, |ui| {
                                for (key, value) in &renderer_info.details {
                                    ui.label(format!("{}:", key));
                                    ui.label(egui::RichText::new(value).monospace());
                                    ui.end_row();
                                }
                            });
                    }
                });
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading...");
            });
        }
    }

    fn render_cpu_content(&self, ui: &mut Ui, system_monitor: &SystemMonitorStore, box_width: f32) {
        if let Some(stats) = system_monitor.latest() {
            ui.label(format!(
                "{} cores, usage: {:.1}%",
                stats.num_cores, stats.cpu_usage
            ));
            ui.add_space(4.0);

            // Graph width = box width minus inner margin on both sides
            let graph_width = box_width - 2.0 * BOX_INNER_MARGIN;
            let (_, rect) = ui.allocate_space(Vec2::new(graph_width, GRAPH_HEIGHT));
            let bg_color = ui.visuals().extreme_bg_color;
            let stroke_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
            draw_graph(
                ui.painter(),
                rect,
                system_monitor.cpu_history(),
                Color32::from_rgb(100, 200, 255),
                bg_color,
                stroke_color,
            );
        } else {
            ui.label("Waiting for data...");
        }
    }

    fn render_memory_content(
        &self,
        ui: &mut Ui,
        system_monitor: &SystemMonitorStore,
        box_width: f32,
    ) {
        if let Some(stats) = system_monitor.latest() {
            let mem_percent = if stats.total_memory > 0 {
                (stats.used_memory as f32 / stats.total_memory as f32) * 100.0
            } else {
                0.0
            };

            ui.label(format!(
                "{:.1}% ({:.1} / {:.1} GB)",
                mem_percent,
                stats.used_memory as f64 / 1_073_741_824.0,
                stats.total_memory as f64 / 1_073_741_824.0
            ));
            ui.add_space(4.0);

            let graph_width = box_width - 2.0 * BOX_INNER_MARGIN;
            let (_, rect) = ui.allocate_space(Vec2::new(graph_width, GRAPH_HEIGHT));
            let bg_color = ui.visuals().extreme_bg_color;
            let stroke_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
            draw_graph(
                ui.painter(),
                rect,
                system_monitor.memory_history(),
                Color32::from_rgb(100, 255, 100),
                bg_color,
                stroke_color,
            );
        } else {
            ui.label("Waiting for data...");
        }
    }

    fn render_gpu_content(&self, ui: &mut Ui, system_monitor: &SystemMonitorStore, box_width: f32) {
        if let Some(stats) = system_monitor.latest() {
            // GStreamer OpenGL context — collapsible
            if let Some(gl) = &stats.gl_renderer {
                let is_software = {
                    let r = gl.renderer.to_lowercase();
                    r.contains("llvmpipe") || r.contains("softpipe") || r.contains("swrast")
                };

                let header_text = if is_software {
                    format!("GStreamer OpenGL: {} (software)", gl.renderer)
                } else {
                    format!("GStreamer OpenGL: {}", gl.renderer)
                };

                // Show warning outside collapsible if software rendering
                if is_software {
                    ui.colored_label(
                        Color32::from_rgb(255, 180, 50),
                        "Software rendering — no GPU acceleration for GStreamer GL",
                    );
                    ui.add_space(2.0);
                }

                egui::CollapsingHeader::new(egui::RichText::new(header_text).monospace())
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::Grid::new("gl_renderer_grid")
                            .num_columns(2)
                            .spacing([8.0, 2.0])
                            .show(ui, |ui| {
                                ui.label("Renderer:");
                                ui.label(egui::RichText::new(&gl.renderer).monospace());
                                ui.end_row();

                                ui.label("Version:");
                                ui.label(egui::RichText::new(&gl.version).monospace());
                                ui.end_row();

                                ui.label("Vendor:");
                                ui.label(egui::RichText::new(&gl.vendor).monospace());
                                ui.end_row();

                                ui.label("GLSL:");
                                ui.label(egui::RichText::new(&gl.glsl_version).monospace());
                                ui.end_row();
                            });
                    });

                ui.add_space(4.0);
            }

            if stats.gpu_stats.is_empty() && stats.gl_renderer.is_none() {
                ui.label("No GPU detected");
                return;
            }

            // Per-GPU hardware stats (NVIDIA, future: Intel)
            for (i, gpu) in stats.gpu_stats.iter().enumerate() {
                if i > 0 || stats.gl_renderer.is_some() {
                    ui.separator();
                    ui.add_space(4.0);
                }

                // Numbered GPU name
                ui.label(egui::RichText::new(format!("GPU {}: {}", i, gpu.name)).strong());
                ui.add_space(4.0);

                // GPU stats in a grid
                egui::Grid::new(format!("gpu_{}_grid", i))
                    .num_columns(2)
                    .spacing([8.0, 2.0])
                    .show(ui, |ui| {
                        ui.label("Utilization:");
                        ui.label(format!("{:.1}%", gpu.utilization));
                        ui.end_row();

                        ui.label("Memory:");
                        ui.label(format!(
                            "{:.1}% ({:.1} / {:.1} GB)",
                            gpu.memory_utilization,
                            gpu.used_memory as f64 / 1_073_741_824.0,
                            gpu.total_memory as f64 / 1_073_741_824.0
                        ));
                        ui.end_row();

                        if let Some(temp) = gpu.temperature {
                            ui.label("Temperature:");
                            let temp_color = if temp > 80.0 {
                                Color32::RED
                            } else if temp > 70.0 {
                                Color32::YELLOW
                            } else {
                                Color32::GREEN
                            };
                            ui.colored_label(temp_color, format!("{:.1}°C", temp));
                            ui.end_row();
                        }

                        if let Some(power) = gpu.power_usage {
                            ui.label("Power:");
                            ui.label(format!("{:.1}W", power));
                            ui.end_row();
                        }
                    });

                ui.add_space(4.0);

                // Full-width graph
                let graph_width = box_width - 2.0 * BOX_INNER_MARGIN;
                if let Some(gpu_hist) = system_monitor.gpu_history(i) {
                    let (_, rect) = ui.allocate_space(Vec2::new(graph_width, GRAPH_HEIGHT));
                    let bg_color = ui.visuals().extreme_bg_color;
                    let stroke_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
                    draw_graph(
                        ui.painter(),
                        rect,
                        gpu_hist,
                        Color32::from_rgb(255, 150, 100),
                        bg_color,
                        stroke_color,
                    );
                }
            }
        } else {
            ui.label("Waiting for data...");
        }
    }

    fn render_network_content(
        &self,
        ui: &mut Ui,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
    ) {
        if network_interfaces.is_empty() {
            ui.label("No network interfaces available.");
        } else {
            egui::Grid::new("network_grid")
                .num_columns(4)
                .spacing([16.0, 2.0])
                .striped(true)
                .show(ui, |ui| {
                    // Header
                    ui.strong("Interface");
                    ui.strong("IPv4");
                    ui.strong("Netmask");
                    ui.strong("Status");
                    ui.end_row();

                    for iface in network_interfaces {
                        // Skip loopback interfaces
                        if iface.is_loopback {
                            continue;
                        }

                        // Get the first IPv4 address if available
                        let (ip, netmask) = if let Some(addr) = iface.ipv4_addresses.first() {
                            (
                                addr.address.as_str(),
                                addr.netmask.as_deref().unwrap_or("-"),
                            )
                        } else {
                            ("-", "-")
                        };

                        ui.label(egui::RichText::new(&iface.name).monospace());
                        ui.label(egui::RichText::new(ip).monospace());
                        ui.label(egui::RichText::new(netmask).monospace());

                        if iface.is_up {
                            ui.colored_label(Color32::GREEN, "up");
                        } else {
                            ui.colored_label(Color32::GRAY, "down");
                        }
                        ui.end_row();
                    }
                });
        }
    }
}

impl Default for InfoPage {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a box with header and content.
fn render_box(ui: &mut Ui, title: &str, width: f32, content: impl FnOnce(&mut Ui)) {
    let fill_color = ui.visuals().widgets.noninteractive.bg_fill;
    let stroke_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let text_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
    egui::Frame::new()
        .fill(fill_color)
        .corner_radius(8.0)
        .stroke(Stroke::new(1.0_f32, stroke_color))
        .inner_margin(BOX_INNER_MARGIN)
        .show(ui, |ui| {
            ui.set_width(width);

            ui.vertical(|ui| {
                // Use explicit text color to ensure visibility across all themes
                ui.label(egui::RichText::new(title).strong().color(text_color));
                ui.add_space(8.0);
                content(ui);
            });
        });
}

/// Draw a graph with background and grid lines.
fn draw_graph(
    painter: &egui::Painter,
    rect: Rect,
    data: &VecDeque<f32>,
    color: Color32,
    bg_color: Color32,
    stroke_color: Color32,
) {
    // Draw background
    painter.rect_filled(rect, 4.0, bg_color);

    // Draw grid lines - use a slightly different shade from bg
    let grid_color = stroke_color.linear_multiply(0.5);
    for i in 0..=4 {
        let y = rect.min.y + (i as f32 / 4.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5_f32, grid_color),
        );
    }

    if data.is_empty() {
        return;
    }

    // Draw data line
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (HISTORY_SIZE - 1) as f32) * rect.width();
            let y = rect.max.y - (value / 100.0) * rect.height();
            Pos2::new(x, y.max(rect.min.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(2.0_f32, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0_f32, stroke_color),
        egui::StrokeKind::Outside,
    );
}
