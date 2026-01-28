//! System monitoring widget for displaying CPU and GPU statistics.

use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, Widget};
use std::collections::VecDeque;
use strom_types::{FlowId, SystemStats};

/// Navigation action from thread monitor clicks.
#[derive(Debug, Clone)]
pub enum ThreadNavigationAction {
    /// Navigate to a flow
    Flow(FlowId),
    /// Navigate to a flow and select a block
    Block { flow_id: FlowId, block_id: String },
    /// Navigate to a flow and select an element
    Element {
        flow_id: FlowId,
        element_name: String,
    },
}

const HISTORY_SIZE: usize = 60; // Keep 60 seconds of history

/// Data store for system monitoring statistics with history.
#[derive(Clone)]
pub struct SystemMonitorStore {
    /// History of CPU usage (0-100)
    cpu_history: VecDeque<f32>,
    /// History of memory usage (0-100)
    memory_history: VecDeque<f32>,
    /// History of GPU usage per GPU
    gpu_history: Vec<VecDeque<f32>>,
    /// Latest system stats
    latest_stats: Option<SystemStats>,
}

impl Default for SystemMonitorStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMonitorStore {
    /// Create a new system monitor store.
    pub fn new() -> Self {
        Self {
            cpu_history: VecDeque::with_capacity(HISTORY_SIZE),
            memory_history: VecDeque::with_capacity(HISTORY_SIZE),
            gpu_history: Vec::new(),
            latest_stats: None,
        }
    }

    /// Update with new system statistics.
    pub fn update(&mut self, stats: SystemStats) {
        // Update CPU history
        self.cpu_history.push_back(stats.cpu_usage);
        if self.cpu_history.len() > HISTORY_SIZE {
            self.cpu_history.pop_front();
        }

        // Update memory history
        let memory_percent = if stats.total_memory > 0 {
            (stats.used_memory as f32 / stats.total_memory as f32) * 100.0
        } else {
            0.0
        };
        self.memory_history.push_back(memory_percent);
        if self.memory_history.len() > HISTORY_SIZE {
            self.memory_history.pop_front();
        }

        // Update GPU history
        // Ensure we have enough GPU history vectors
        while self.gpu_history.len() < stats.gpu_stats.len() {
            self.gpu_history.push(VecDeque::with_capacity(HISTORY_SIZE));
        }

        // Update each GPU's history
        for (i, gpu_stats) in stats.gpu_stats.iter().enumerate() {
            if let Some(gpu_hist) = self.gpu_history.get_mut(i) {
                gpu_hist.push_back(gpu_stats.utilization);
                if gpu_hist.len() > HISTORY_SIZE {
                    gpu_hist.pop_front();
                }
            }
        }

        self.latest_stats = Some(stats);
    }

    /// Get the latest system stats.
    pub fn latest(&self) -> Option<&SystemStats> {
        self.latest_stats.as_ref()
    }

    /// Get CPU history.
    pub fn cpu_history(&self) -> &VecDeque<f32> {
        &self.cpu_history
    }

    /// Get memory history.
    pub fn memory_history(&self) -> &VecDeque<f32> {
        &self.memory_history
    }

    /// Get GPU history for a specific GPU.
    pub fn gpu_history(&self, index: usize) -> Option<&VecDeque<f32>> {
        self.gpu_history.get(index)
    }
}

/// Compact system monitor widget for the toolbar.
pub struct CompactSystemMonitor<'a> {
    store: &'a SystemMonitorStore,
    width: f32,
    height: f32,
}

impl<'a> CompactSystemMonitor<'a> {
    pub fn new(store: &'a SystemMonitorStore) -> Self {
        Self {
            store,
            width: 200.0,
            height: 24.0,
        }
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }
}

impl<'a> Widget for CompactSystemMonitor<'a> {
    fn ui(self, ui: &mut Ui) -> Response {
        let desired_size = Vec2::new(self.width, self.height);
        let (rect, response) = ui.allocate_exact_size(desired_size, Sense::click());

        if ui.is_rect_visible(rect) {
            let painter = ui.painter();

            // Draw background
            painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);

            if let Some(stats) = self.store.latest() {
                let has_gpu = !stats.gpu_stats.is_empty();
                let num_cols = if has_gpu { 3.0 } else { 2.0 };
                let col_width = rect.width() / num_cols;
                let graph_height = rect.height();

                // Draw CPU graph
                let cpu_rect = Rect::from_min_size(rect.min, Vec2::new(col_width, graph_height));
                draw_mini_graph(
                    painter,
                    cpu_rect,
                    self.store.cpu_history(),
                    Color32::from_rgb(100, 200, 255),
                );

                // Draw memory graph
                let mem_rect = Rect::from_min_size(
                    Pos2::new(rect.min.x + col_width, rect.min.y),
                    Vec2::new(col_width, graph_height),
                );
                draw_mini_graph(
                    painter,
                    mem_rect,
                    self.store.memory_history(),
                    Color32::from_rgb(100, 255, 100),
                );

                // Draw GPU graph if available (first GPU)
                if has_gpu {
                    if let Some(gpu_hist) = self.store.gpu_history(0) {
                        let gpu_rect = Rect::from_min_size(
                            Pos2::new(rect.min.x + col_width * 2.0, rect.min.y),
                            Vec2::new(col_width, graph_height),
                        );
                        draw_mini_graph(
                            painter,
                            gpu_rect,
                            gpu_hist,
                            Color32::from_rgb(255, 150, 100),
                        );
                    }
                }
            } else {
                // No data yet
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "No data",
                    egui::FontId::proportional(10.0),
                    ui.visuals().weak_text_color(),
                );
            }

            // Draw border
            painter.rect_stroke(
                rect,
                2.0,
                Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                egui::StrokeKind::Outside,
            );
        }

        response
    }
}

/// Draw a mini sparkline graph.
fn draw_mini_graph(painter: &egui::Painter, rect: Rect, data: &VecDeque<f32>, color: Color32) {
    if data.is_empty() {
        return;
    }

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
        painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));
    }
}

/// Tab selection for the detailed system monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SystemMonitorTab {
    #[default]
    System,
    Threads,
}

/// Column to sort threads by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadSortColumn {
    #[default]
    Cpu,
    Element,
    Block,
    Flow,
    ThreadId,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDirection {
    #[default]
    Descending,
    Ascending,
}

impl SortDirection {
    fn toggle(&self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }

    fn arrow(&self) -> &'static str {
        match self {
            SortDirection::Ascending => " ^",
            SortDirection::Descending => " v",
        }
    }
}

/// Detailed system monitor window.
pub struct DetailedSystemMonitor<'a> {
    system_store: &'a SystemMonitorStore,
    thread_store: &'a crate::thread_monitor::ThreadMonitorStore,
    selected_tab: &'a mut SystemMonitorTab,
    sort_column: &'a mut ThreadSortColumn,
    sort_direction: &'a mut SortDirection,
    /// Flow ID to name mapping for display
    flow_names: &'a std::collections::HashMap<FlowId, String>,
}

impl<'a> DetailedSystemMonitor<'a> {
    pub fn new(
        system_store: &'a SystemMonitorStore,
        thread_store: &'a crate::thread_monitor::ThreadMonitorStore,
        selected_tab: &'a mut SystemMonitorTab,
        sort_column: &'a mut ThreadSortColumn,
        sort_direction: &'a mut SortDirection,
        flow_names: &'a std::collections::HashMap<FlowId, String>,
    ) -> Self {
        Self {
            system_store,
            thread_store,
            selected_tab,
            sort_column,
            sort_direction,
            flow_names,
        }
    }

    /// Show the system monitor UI and return any navigation action.
    pub fn show(&mut self, ui: &mut Ui) -> Option<ThreadNavigationAction> {
        // Tab bar
        ui.horizontal(|ui| {
            if ui
                .selectable_label(*self.selected_tab == SystemMonitorTab::System, "System")
                .clicked()
            {
                *self.selected_tab = SystemMonitorTab::System;
            }
            if ui
                .selectable_label(*self.selected_tab == SystemMonitorTab::Threads, "Threads")
                .clicked()
            {
                *self.selected_tab = SystemMonitorTab::Threads;
            }
        });
        ui.separator();

        match self.selected_tab {
            SystemMonitorTab::System => {
                self.show_system_tab(ui);
                None
            }
            SystemMonitorTab::Threads => self.show_threads_tab(ui),
        }
    }

    fn show_system_tab(&self, ui: &mut Ui) {
        if let Some(stats) = self.system_store.latest() {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label("CPU Usage");
                    let cpu_rect = ui.allocate_space(Vec2::new(300.0, 100.0));
                    let bg_color = ui.visuals().extreme_bg_color;
                    let stroke_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
                    draw_large_graph(
                        ui.painter(),
                        cpu_rect.1,
                        self.system_store.cpu_history(),
                        Color32::from_rgb(100, 200, 255),
                        "CPU %",
                        bg_color,
                        stroke_color,
                    );
                    ui.label(format!("Current: {:.1}%", stats.cpu_usage));
                });

                ui.separator();

                ui.vertical(|ui| {
                    ui.label("Memory Usage");
                    let mem_rect = ui.allocate_space(Vec2::new(300.0, 100.0));
                    let bg_color = ui.visuals().extreme_bg_color;
                    let stroke_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
                    draw_large_graph(
                        ui.painter(),
                        mem_rect.1,
                        self.system_store.memory_history(),
                        Color32::from_rgb(100, 255, 100),
                        "Memory %",
                        bg_color,
                        stroke_color,
                    );
                    let mem_percent = if stats.total_memory > 0 {
                        (stats.used_memory as f32 / stats.total_memory as f32) * 100.0
                    } else {
                        0.0
                    };
                    ui.label(format!("Current: {:.1}%", mem_percent));
                    ui.label(format!(
                        "Used: {:.1} GB / {:.1} GB",
                        stats.used_memory as f64 / 1_073_741_824.0,
                        stats.total_memory as f64 / 1_073_741_824.0
                    ));
                });
            });

            if !stats.gpu_stats.is_empty() {
                ui.separator();
                ui.heading("GPU Information");

                for (i, gpu) in stats.gpu_stats.iter().enumerate() {
                    ui.group(|ui| {
                        ui.label(format!("GPU {}: {}", i, gpu.name));

                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label("GPU Utilization");
                                if let Some(gpu_hist) = self.system_store.gpu_history(i) {
                                    let gpu_rect = ui.allocate_space(Vec2::new(250.0, 80.0));
                                    let bg_color = ui.visuals().extreme_bg_color;
                                    let stroke_color =
                                        ui.visuals().widgets.noninteractive.bg_stroke.color;
                                    draw_large_graph(
                                        ui.painter(),
                                        gpu_rect.1,
                                        gpu_hist,
                                        Color32::from_rgb(255, 150, 100),
                                        "GPU %",
                                        bg_color,
                                        stroke_color,
                                    );
                                }
                                ui.label(format!("Current: {:.1}%", gpu.utilization));
                            });

                            ui.separator();

                            ui.vertical(|ui| {
                                ui.label("Memory");
                                ui.label(format!("Used: {:.1}%", gpu.memory_utilization));
                                ui.label(format!(
                                    "{:.1} GB / {:.1} GB",
                                    gpu.used_memory as f64 / 1_073_741_824.0,
                                    gpu.total_memory as f64 / 1_073_741_824.0
                                ));

                                if let Some(temp) = gpu.temperature {
                                    ui.label(format!("Temperature: {:.1}°C", temp));
                                }

                                if let Some(power) = gpu.power_usage {
                                    ui.label(format!("Power: {:.1} W", power));
                                }
                            });
                        });
                    });
                }
            }
        } else {
            ui.label("No system monitoring data available");
        }
    }

    fn show_threads_tab(&mut self, ui: &mut Ui) -> Option<ThreadNavigationAction> {
        if self.thread_store.is_empty() {
            ui.label("No GStreamer streaming threads active.");
            ui.label("Start a flow to see thread CPU usage.");
            return None;
        }

        // Summary
        ui.horizontal(|ui| {
            ui.label(format!(
                "Active threads: {}",
                self.thread_store.thread_count()
            ));
            ui.separator();
            ui.label(format!(
                "Total CPU: {:.1}%",
                self.thread_store.total_cpu_usage()
            ));
        });
        ui.separator();

        // Collect threads for sorting
        let mut threads: Vec<_> = self
            .thread_store
            .get_sorted_threads()
            .into_iter()
            .filter_map(|h| h.latest.clone())
            .collect();

        // Sort by selected column
        let sort_dir = *self.sort_direction;
        match self.sort_column {
            ThreadSortColumn::Cpu => {
                threads.sort_by(|a, b| {
                    let cmp = a
                        .cpu_usage
                        .partial_cmp(&b.cpu_usage)
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if matches!(sort_dir, SortDirection::Descending) {
                        cmp.reverse()
                    } else {
                        cmp
                    }
                });
            }
            ThreadSortColumn::Element => {
                threads.sort_by(|a, b| {
                    let cmp = a.element_name.cmp(&b.element_name);
                    if matches!(sort_dir, SortDirection::Descending) {
                        cmp.reverse()
                    } else {
                        cmp
                    }
                });
            }
            ThreadSortColumn::Block => {
                threads.sort_by(|a, b| {
                    let cmp = a.block_id.cmp(&b.block_id);
                    if matches!(sort_dir, SortDirection::Descending) {
                        cmp.reverse()
                    } else {
                        cmp
                    }
                });
            }
            ThreadSortColumn::Flow => {
                threads.sort_by(|a, b| {
                    let cmp = a.flow_id.cmp(&b.flow_id);
                    if matches!(sort_dir, SortDirection::Descending) {
                        cmp.reverse()
                    } else {
                        cmp
                    }
                });
            }
            ThreadSortColumn::ThreadId => {
                threads.sort_by(|a, b| {
                    let cmp = a.thread_id.cmp(&b.thread_id);
                    if matches!(sort_dir, SortDirection::Descending) {
                        cmp.reverse()
                    } else {
                        cmp
                    }
                });
            }
        }

        let mut nav_action: Option<ThreadNavigationAction> = None;

        // Thread table
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                egui::Grid::new("thread_grid")
                    .num_columns(5)
                    .spacing([20.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        // Clickable headers for sorting
                        self.sortable_header(ui, "CPU %", ThreadSortColumn::Cpu);
                        self.sortable_header(ui, "Element", ThreadSortColumn::Element);
                        self.sortable_header(ui, "Block", ThreadSortColumn::Block);
                        self.sortable_header(ui, "Flow", ThreadSortColumn::Flow);
                        self.sortable_header(ui, "Thread ID", ThreadSortColumn::ThreadId);
                        ui.end_row();

                        // Rows
                        for stats in &threads {
                            // CPU % with color coding
                            let cpu = stats.cpu_usage;
                            let color = if cpu > 80.0 {
                                Color32::from_rgb(255, 80, 80) // Red
                            } else if cpu > 50.0 {
                                Color32::from_rgb(255, 200, 50) // Yellow
                            } else {
                                ui.visuals().text_color()
                            };
                            ui.colored_label(color, format!("{:.1}%", cpu));

                            // Element name (clickable)
                            if ui.link(&stats.element_name).clicked() {
                                nav_action = Some(ThreadNavigationAction::Element {
                                    flow_id: stats.flow_id,
                                    element_name: stats.element_name.clone(),
                                });
                            }

                            // Block ID (clickable if present)
                            if let Some(block_id) = &stats.block_id {
                                if ui.link(block_id).clicked() {
                                    nav_action = Some(ThreadNavigationAction::Block {
                                        flow_id: stats.flow_id,
                                        block_id: block_id.clone(),
                                    });
                                }
                            } else {
                                ui.label("-");
                            }

                            // Flow name (clickable)
                            let flow_display = self
                                .flow_names
                                .get(&stats.flow_id)
                                .cloned()
                                .unwrap_or_else(|| {
                                    let id = stats.flow_id.to_string();
                                    if id.len() > 8 {
                                        format!("{}...", &id[..8])
                                    } else {
                                        id
                                    }
                                });
                            if ui.link(&flow_display).clicked() {
                                nav_action = Some(ThreadNavigationAction::Flow(stats.flow_id));
                            }

                            // Thread ID
                            ui.label(format!("{}", stats.thread_id));

                            ui.end_row();
                        }
                    });
            });

        nav_action
    }

    /// Render a clickable sortable column header.
    fn sortable_header(&mut self, ui: &mut Ui, label: &str, column: ThreadSortColumn) {
        let is_selected = *self.sort_column == column;
        let text = if is_selected {
            format!("{}{}", label, self.sort_direction.arrow())
        } else {
            label.to_string()
        };

        if ui
            .selectable_label(is_selected, egui::RichText::new(text).strong())
            .clicked()
        {
            if *self.sort_column == column {
                // Toggle direction
                *self.sort_direction = self.sort_direction.toggle();
            } else {
                // New column, default to descending for CPU, ascending for others
                *self.sort_column = column;
                *self.sort_direction = if column == ThreadSortColumn::Cpu {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                };
            }
        }
    }
}

/// Draw a larger graph with grid lines and labels.
fn draw_large_graph(
    painter: &egui::Painter,
    rect: Rect,
    data: &VecDeque<f32>,
    color: Color32,
    _label: &str,
    bg_color: Color32,
    stroke_color: Color32,
) {
    // Draw background
    painter.rect_filled(rect, 2.0, bg_color);

    // Draw grid lines
    let grid_color = stroke_color.linear_multiply(0.5);
    for i in 0..=4 {
        let y = rect.min.y + (i as f32 / 4.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5, grid_color),
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
        painter.add(egui::Shape::line(points, Stroke::new(2.0, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, stroke_color),
        egui::StrokeKind::Outside,
    );
}
