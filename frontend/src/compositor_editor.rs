//! Visual compositor layout editor.
//!
//! Provides an interactive canvas for editing glcompositor block layouts with:
//! - Drag-and-drop positioning of input boxes
//! - Resize handles for changing input dimensions
//! - Real-time updates via API to running pipeline
//! - Property panel for fine-tuning alpha, zorder, sizing-policy

use egui::{Color32, Context, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Vec2};
use strom_types::{FlowId, PropertyValue};

use crate::api::ApiClient;

/// Handle for resizing an input box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeHandle {
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl ResizeHandle {
    /// Get all resize handles.
    fn all() -> &'static [ResizeHandle] {
        &[
            ResizeHandle::TopLeft,
            ResizeHandle::Top,
            ResizeHandle::TopRight,
            ResizeHandle::Left,
            ResizeHandle::Right,
            ResizeHandle::BottomLeft,
            ResizeHandle::Bottom,
            ResizeHandle::BottomRight,
        ]
    }

    /// Get the cursor icon for this handle.
    fn cursor_icon(&self) -> egui::CursorIcon {
        match self {
            ResizeHandle::TopLeft | ResizeHandle::BottomRight => egui::CursorIcon::ResizeNwSe,
            ResizeHandle::TopRight | ResizeHandle::BottomLeft => egui::CursorIcon::ResizeNeSw,
            ResizeHandle::Top | ResizeHandle::Bottom => egui::CursorIcon::ResizeVertical,
            ResizeHandle::Left | ResizeHandle::Right => egui::CursorIcon::ResizeHorizontal,
        }
    }
}

/// Represents a single input in the compositor.
#[derive(Debug, Clone)]
struct InputBox {
    /// Input index (0-based)
    input_index: usize,
    /// Pad name on the mixer element (e.g., "sink_0")
    pad_name: String,

    // Current properties (synced with mixer pad)
    xpos: i32,
    ypos: i32,
    width: i32,
    height: i32,
    alpha: f64,
    zorder: u32,
    sizing_policy: String,

    // UI state
    /// Whether this input is currently selected
    selected: bool,
    /// Pending API update (shows spinner)
    pending_update: bool,
    /// Last error from API update
    last_error: Option<String>,
}

impl InputBox {
    /// Create a new input box with default values.
    fn new(input_index: usize) -> Self {
        Self {
            input_index,
            pad_name: format!("sink_{}", input_index),
            xpos: 0,
            ypos: 0,
            width: 640,
            height: 360,
            alpha: 1.0,
            zorder: input_index as u32,
            sizing_policy: "keep-aspect-ratio".to_string(),
            selected: false,
            pending_update: false,
            last_error: None,
        }
    }

    /// Get the bounding rect in canvas coordinates.
    fn rect(&self) -> Rect {
        Rect::from_min_size(
            Pos2::new(self.xpos as f32, self.ypos as f32),
            Vec2::new(self.width as f32, self.height as f32),
        )
    }

    /// Get color for this input box (based on index).
    fn color(&self) -> Color32 {
        let hue = (self.input_index as f32 * 137.5) % 360.0; // Golden angle for good color distribution
        let (r, g, b) = hsv_to_rgb(hue, 0.7, 0.9);
        Color32::from_rgba_unmultiplied(r, g, b, (self.alpha * 255.0) as u8)
    }

    /// Get resize handle rect in canvas coordinates.
    fn resize_handle_rect(&self, handle: ResizeHandle, handle_size: f32) -> Rect {
        let rect = self.rect();

        match handle {
            ResizeHandle::TopLeft => {
                Rect::from_center_size(rect.left_top(), Vec2::splat(handle_size))
            }
            ResizeHandle::Top => Rect::from_center_size(
                Pos2::new(rect.center().x, rect.top()),
                Vec2::new(handle_size * 2.0, handle_size),
            ),
            ResizeHandle::TopRight => {
                Rect::from_center_size(rect.right_top(), Vec2::splat(handle_size))
            }
            ResizeHandle::Left => Rect::from_center_size(
                Pos2::new(rect.left(), rect.center().y),
                Vec2::new(handle_size, handle_size * 2.0),
            ),
            ResizeHandle::Right => Rect::from_center_size(
                Pos2::new(rect.right(), rect.center().y),
                Vec2::new(handle_size, handle_size * 2.0),
            ),
            ResizeHandle::BottomLeft => {
                Rect::from_center_size(rect.left_bottom(), Vec2::splat(handle_size))
            }
            ResizeHandle::Bottom => Rect::from_center_size(
                Pos2::new(rect.center().x, rect.bottom()),
                Vec2::new(handle_size * 2.0, handle_size),
            ),
            ResizeHandle::BottomRight => {
                Rect::from_center_size(rect.right_bottom(), Vec2::splat(handle_size))
            }
        }
    }
}

/// Convert HSV to RGB (0-255).
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// Compositor layout editor state.
pub struct CompositorEditor {
    /// Flow ID
    flow_id: FlowId,
    /// Block ID (e.g., "b0")
    block_id: String,
    /// Mixer element ID (e.g., "b0:mixer")
    mixer_element_id: String,

    /// Output canvas width
    output_width: u32,
    /// Output canvas height
    output_height: u32,

    /// Input boxes
    inputs: Vec<InputBox>,
    /// Currently selected input index
    selected_input: Option<usize>,

    /// Dragging state
    dragging_input: Option<usize>,
    drag_start_pos: Pos2,
    drag_start_xpos: i32,
    drag_start_ypos: i32,

    /// Resizing state
    resizing_input: Option<(usize, ResizeHandle)>,
    resize_start_pos: Pos2,
    resize_start_width: i32,
    resize_start_height: i32,
    resize_start_xpos: i32,
    resize_start_ypos: i32,

    /// Settings
    snap_to_grid: bool,
    grid_size: u32,
    live_updates: bool,

    /// API client
    api: ApiClient,

    /// Status message
    status: String,
    /// Error message
    error: Option<String>,

    /// Last time we sent a live update (for throttling)
    last_live_update: instant::Instant,
}

impl CompositorEditor {
    /// Create a new compositor editor.
    pub fn new(
        flow_id: FlowId,
        block_id: String,
        output_width: u32,
        output_height: u32,
        num_inputs: usize,
        api: ApiClient,
    ) -> Self {
        let mixer_element_id = format!("{}:mixer", block_id);

        // Create input boxes
        let inputs: Vec<_> = (0..num_inputs).map(InputBox::new).collect();

        Self {
            flow_id,
            block_id,
            mixer_element_id,
            output_width,
            output_height,
            inputs,
            selected_input: None,
            dragging_input: None,
            drag_start_pos: Pos2::ZERO,
            drag_start_xpos: 0,
            drag_start_ypos: 0,
            resizing_input: None,
            resize_start_pos: Pos2::ZERO,
            resize_start_width: 0,
            resize_start_height: 0,
            resize_start_xpos: 0,
            resize_start_ypos: 0,
            snap_to_grid: false,
            grid_size: 10,
            live_updates: true,
            api,
            status: "Loading...".to_string(),
            error: None,
            last_live_update: instant::Instant::now(),
        }
    }

    /// Load current properties from the backend.
    pub fn load_properties(&mut self, ctx: &Context) {
        let flow_id = self.flow_id;
        let mixer_element_id = self.mixer_element_id.clone();
        let api = self.api.clone();
        let ctx = ctx.clone();

        // Load properties for each input
        for input in &self.inputs {
            let pad_name = input.pad_name.clone();
            let api = api.clone();
            let ctx = ctx.clone();
            let mixer_element_id = mixer_element_id.clone();

            crate::app::spawn_task(async move {
                match api
                    .get_pad_properties(&flow_id.to_string(), &mixer_element_id, &pad_name)
                    .await
                {
                    Ok(props) => {
                        // Store properties in local storage for the UI loop to pick up
                        let key = format!("compositor_props_{}_{}", flow_id, pad_name);
                        if let Ok(json) = serde_json::to_string(&props) {
                            crate::app::set_local_storage(&key, &json);
                        }
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        tracing::error!("Failed to load pad properties for {}: {}", pad_name, e);
                    }
                }
            });
        }
    }

    /// Check for loaded properties and update inputs.
    fn check_loaded_properties(&mut self) {
        for input in &mut self.inputs {
            let key = format!("compositor_props_{}_{}", self.flow_id, input.pad_name);
            if let Some(json) = crate::app::get_local_storage(&key) {
                if let Ok(props) =
                    serde_json::from_str::<std::collections::HashMap<String, PropertyValue>>(&json)
                {
                    // Update input box with loaded properties
                    if let Some(PropertyValue::Int(v)) = props.get("xpos") {
                        input.xpos = *v as i32;
                    }
                    if let Some(PropertyValue::Int(v)) = props.get("ypos") {
                        input.ypos = *v as i32;
                    }
                    if let Some(PropertyValue::Int(v)) = props.get("width") {
                        input.width = *v as i32;
                    }
                    if let Some(PropertyValue::Int(v)) = props.get("height") {
                        input.height = *v as i32;
                    }
                    if let Some(PropertyValue::Float(v)) = props.get("alpha") {
                        input.alpha = *v;
                    }
                    if let Some(PropertyValue::UInt(v)) = props.get("zorder") {
                        input.zorder = *v as u32;
                    }
                    if let Some(PropertyValue::String(v)) = props.get("sizing-policy") {
                        input.sizing_policy = v.clone();
                    }

                    // Clear the storage key
                    crate::app::remove_local_storage(&key);

                    self.status = "Properties loaded".to_string();
                }
            }
        }
    }

    /// Update a pad property via API.
    fn update_pad_property(
        &mut self,
        ctx: &Context,
        input_index: usize,
        property_name: &str,
        value: PropertyValue,
    ) {
        if !self.live_updates {
            tracing::debug!(
                "Live updates disabled, skipping API call for {}={:?}",
                property_name,
                value
            );
            return;
        }

        let flow_id = self.flow_id;
        let mixer_element_id = self.mixer_element_id.clone();
        let pad_name = self.inputs[input_index].pad_name.clone();
        let api = self.api.clone();
        let ctx = ctx.clone();
        let property_name = property_name.to_string();

        tracing::info!(
            "🎨 Updating compositor pad property: flow={} element={} pad={} property={}={:?}",
            flow_id,
            mixer_element_id,
            pad_name,
            property_name,
            value
        );

        self.inputs[input_index].pending_update = true;
        self.status = format!("Updating {}...", property_name);

        crate::app::spawn_task(async move {
            match api
                .update_pad_property(
                    &flow_id.to_string(),
                    &mixer_element_id,
                    &pad_name,
                    &property_name,
                    value.clone(),
                )
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        "✅ Compositor pad property updated: {}={:?}",
                        property_name,
                        value
                    );
                    let key = format!(
                        "compositor_update_success_{}_{}",
                        input_index, property_name
                    );
                    crate::app::set_local_storage(&key, "1");
                }
                Err(e) => {
                    tracing::error!(
                        "❌ Failed to update compositor pad property {}: {}",
                        property_name,
                        e
                    );
                    let key = format!("compositor_update_error_{}_{}", input_index, property_name);
                    crate::app::set_local_storage(&key, &e.to_string());
                }
            }
            ctx.request_repaint();
        });
    }

    /// Apply all properties for all inputs (used when live updates is off).
    fn apply_all_properties(&mut self, ctx: &Context) {
        // Temporarily enable live updates to send all properties
        let was_live = self.live_updates;
        self.live_updates = true;

        for idx in 0..self.inputs.len() {
            let input = &self.inputs[idx];
            let xpos = input.xpos;
            let ypos = input.ypos;
            let width = input.width;
            let height = input.height;
            let alpha = input.alpha;
            let zorder = input.zorder;
            let sizing_policy = input.sizing_policy.clone();

            self.update_pad_property(ctx, idx, "xpos", PropertyValue::Int(xpos as i64));
            self.update_pad_property(ctx, idx, "ypos", PropertyValue::Int(ypos as i64));
            self.update_pad_property(ctx, idx, "width", PropertyValue::Int(width as i64));
            self.update_pad_property(ctx, idx, "height", PropertyValue::Int(height as i64));
            self.update_pad_property(ctx, idx, "alpha", PropertyValue::Float(alpha));
            self.update_pad_property(ctx, idx, "zorder", PropertyValue::UInt(zorder as u64));
            self.update_pad_property(
                ctx,
                idx,
                "sizing-policy",
                PropertyValue::String(sizing_policy),
            );
        }

        // Restore live updates setting
        self.live_updates = was_live;
        self.status = "Layout applied".to_string();
    }

    /// Check for update results.
    fn check_update_results(&mut self) {
        for input in &mut self.inputs {
            let success_key = format!("compositor_update_success_{}_", input.input_index);
            let error_key = format!("compositor_update_error_{}_", input.input_index);

            // Check for success
            if crate::app::get_local_storage(&success_key).is_some() {
                input.pending_update = false;
                input.last_error = None;
                crate::app::remove_local_storage(&success_key);
            }

            // Check for error
            if let Some(err) = crate::app::get_local_storage(&error_key) {
                input.pending_update = false;
                input.last_error = Some(err);
                crate::app::remove_local_storage(&error_key);
            }
        }
    }

    /// Snap value to grid if enabled.
    fn snap(&self, value: i32) -> i32 {
        if self.snap_to_grid {
            let grid = self.grid_size as i32;
            ((value + grid / 2) / grid) * grid
        } else {
            value
        }
    }

    // ===== Layout Templates =====

    /// Multiview - Row 1: 2 large, Row 2: 4 medium, Row 3: remaining small
    fn apply_template_multiview(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let spacing = h * 3 / 100; // 3% spacing between rows

        // Row heights: 45%, 28%, remainder
        let row1_h = h * 45 / 100;
        let row2_h = h * 28 / 100;
        let row1_y = 0;
        let row2_y = row1_h + spacing;
        let row3_y = row2_y + row2_h + spacing;
        let row3_h = h - row3_y;

        // Row 1: First 2 inputs (large, side by side)
        let row1_w = w / 2;
        for i in 0..2 {
            if let Some(input) = self.inputs.get_mut(i) {
                input.xpos = (i as i32) * row1_w;
                input.ypos = row1_y;
                input.width = row1_w;
                input.height = row1_h;
                input.zorder = i as u32;
            }
        }

        // Row 2: Next 4 inputs (medium, 4 columns)
        let row2_w = w / 4;
        for i in 0..4 {
            let idx = 2 + i;
            if let Some(input) = self.inputs.get_mut(idx) {
                input.xpos = (i as i32) * row2_w;
                input.ypos = row2_y;
                input.width = row2_w;
                input.height = row2_h;
                input.zorder = idx as u32;
            }
        }

        // Row 3: Remaining inputs (small, evenly distributed)
        let remaining_count = self.inputs.len().saturating_sub(6);
        if remaining_count > 0 {
            let row3_w = w / remaining_count as i32;
            for i in 0..remaining_count {
                let idx = 6 + i;
                if let Some(input) = self.inputs.get_mut(idx) {
                    input.xpos = (i as i32) * row3_w;
                    input.ypos = row3_y;
                    input.width = row3_w;
                    input.height = row3_h;
                    input.zorder = idx as u32;
                }
            }
        }

        // Hide any inputs beyond what we have (shouldn't happen, but be safe)
        // All inputs should be positioned by now
    }

    /// Full screen - Input 0 fills the entire output
    fn apply_template_fullscreen(&mut self) {
        if let Some(input) = self.inputs.get_mut(0) {
            input.xpos = 0;
            input.ypos = 0;
            input.width = self.output_width as i32;
            input.height = self.output_height as i32;
            input.zorder = 0;
        }
        // Hide other inputs off-screen
        for (i, input) in self.inputs.iter_mut().enumerate().skip(1) {
            input.xpos = -(self.output_width as i32);
            input.ypos = 0;
            input.width = 1;
            input.height = 1;
            input.zorder = i as u32;
        }
    }

    /// Picture-in-Picture - Input 0 full screen, Input 1 small in corner
    fn apply_template_pip(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let pip_w = w / 4;
        let pip_h = h / 4;
        let margin = 20;

        if let Some(input) = self.inputs.get_mut(0) {
            input.xpos = 0;
            input.ypos = 0;
            input.width = w;
            input.height = h;
            input.zorder = 0;
        }
        if let Some(input) = self.inputs.get_mut(1) {
            input.xpos = w - pip_w - margin;
            input.ypos = h - pip_h - margin;
            input.width = pip_w;
            input.height = pip_h;
            input.zorder = 1;
        }
        // Hide remaining inputs
        for (i, input) in self.inputs.iter_mut().enumerate().skip(2) {
            input.xpos = -(w);
            input.zorder = i as u32;
        }
    }

    /// Side by Side - Two inputs split horizontally
    fn apply_template_side_by_side(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let half_w = w / 2;

        if let Some(input) = self.inputs.get_mut(0) {
            input.xpos = 0;
            input.ypos = 0;
            input.width = half_w;
            input.height = h;
            input.zorder = 0;
        }
        if let Some(input) = self.inputs.get_mut(1) {
            input.xpos = half_w;
            input.ypos = 0;
            input.width = half_w;
            input.height = h;
            input.zorder = 1;
        }
        for (i, input) in self.inputs.iter_mut().enumerate().skip(2) {
            input.xpos = -(w);
            input.zorder = i as u32;
        }
    }

    /// Top / Bottom - Two inputs split vertically
    fn apply_template_top_bottom(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let half_h = h / 2;

        if let Some(input) = self.inputs.get_mut(0) {
            input.xpos = 0;
            input.ypos = 0;
            input.width = w;
            input.height = half_h;
            input.zorder = 0;
        }
        if let Some(input) = self.inputs.get_mut(1) {
            input.xpos = 0;
            input.ypos = half_h;
            input.width = w;
            input.height = half_h;
            input.zorder = 1;
        }
        for (i, input) in self.inputs.iter_mut().enumerate().skip(2) {
            input.xpos = -(w);
            input.zorder = i as u32;
        }
    }

    /// 2x2 Grid - Four inputs in a grid
    fn apply_template_grid_2x2(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let cell_w = w / 2;
        let cell_h = h / 2;

        let positions = [(0, 0), (cell_w, 0), (0, cell_h), (cell_w, cell_h)];
        for (i, input) in self.inputs.iter_mut().enumerate() {
            if i < 4 {
                input.xpos = positions[i].0;
                input.ypos = positions[i].1;
                input.width = cell_w;
                input.height = cell_h;
                input.zorder = i as u32;
            } else {
                input.xpos = -(w);
                input.zorder = i as u32;
            }
        }
    }

    /// 3x3 Grid - Nine inputs in a grid
    fn apply_template_grid_3x3(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let cell_w = w / 3;
        let cell_h = h / 3;

        for (i, input) in self.inputs.iter_mut().enumerate() {
            if i < 9 {
                let col = (i % 3) as i32;
                let row = (i / 3) as i32;
                input.xpos = col * cell_w;
                input.ypos = row * cell_h;
                input.width = cell_w;
                input.height = cell_h;
                input.zorder = i as u32;
            } else {
                input.xpos = -(w);
                input.zorder = i as u32;
            }
        }
    }

    /// 1 Large + 2 Small - Main input with two smaller on the side
    fn apply_template_1_large_2_small(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let main_w = (w * 3) / 4;
        let side_w = w - main_w;
        let side_h = h / 2;

        if let Some(input) = self.inputs.get_mut(0) {
            input.xpos = 0;
            input.ypos = 0;
            input.width = main_w;
            input.height = h;
            input.zorder = 0;
        }
        if let Some(input) = self.inputs.get_mut(1) {
            input.xpos = main_w;
            input.ypos = 0;
            input.width = side_w;
            input.height = side_h;
            input.zorder = 1;
        }
        if let Some(input) = self.inputs.get_mut(2) {
            input.xpos = main_w;
            input.ypos = side_h;
            input.width = side_w;
            input.height = side_h;
            input.zorder = 2;
        }
        for (i, input) in self.inputs.iter_mut().enumerate().skip(3) {
            input.xpos = -(w);
            input.zorder = i as u32;
        }
    }

    /// 1 Large + 3 Small - Main input with three smaller below
    fn apply_template_1_large_3_small(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let main_h = (h * 3) / 4;
        let bottom_h = h - main_h;
        let bottom_w = w / 3;

        if let Some(input) = self.inputs.get_mut(0) {
            input.xpos = 0;
            input.ypos = 0;
            input.width = w;
            input.height = main_h;
            input.zorder = 0;
        }
        for i in 1..=3 {
            if let Some(input) = self.inputs.get_mut(i) {
                input.xpos = ((i - 1) as i32) * bottom_w;
                input.ypos = main_h;
                input.width = bottom_w;
                input.height = bottom_h;
                input.zorder = i as u32;
            }
        }
        for (i, input) in self.inputs.iter_mut().enumerate().skip(4) {
            input.xpos = -(w);
            input.zorder = i as u32;
        }
    }

    /// Vertical Stack - All inputs stacked vertically
    fn apply_template_vertical_stack(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let count = self.inputs.len().max(1);
        let cell_h = h / count as i32;

        for (i, input) in self.inputs.iter_mut().enumerate() {
            input.xpos = 0;
            input.ypos = (i as i32) * cell_h;
            input.width = w;
            input.height = cell_h;
            input.zorder = i as u32;
        }
    }

    /// Horizontal Stack - All inputs stacked horizontally
    fn apply_template_horizontal_stack(&mut self) {
        let w = self.output_width as i32;
        let h = self.output_height as i32;
        let count = self.inputs.len().max(1);
        let cell_w = w / count as i32;

        for (i, input) in self.inputs.iter_mut().enumerate() {
            input.xpos = (i as i32) * cell_w;
            input.ypos = 0;
            input.width = cell_w;
            input.height = h;
            input.zorder = i as u32;
        }
    }

    /// Show the compositor editor UI as a window.
    /// Returns true if the window should stay open.
    pub fn show(&mut self, ctx: &Context) -> bool {
        // Check for loaded properties
        self.check_loaded_properties();
        self.check_update_results();

        let mut is_open = true;

        // Cap window to app size - 200
        let available_rect = ctx.available_rect();
        let max_width = (available_rect.width() - 200.0).max(300.0);
        let max_height = (available_rect.height() - 200.0).max(200.0);

        egui::Window::new(format!("Compositor Layout Editor - {}", self.block_id))
            .id(egui::Id::new("compositor_editor_window"))
            .default_size([800.0, 500.0])
            .min_size([400.0, 300.0])
            .max_size([max_width, max_height])
            .resizable(true)
            .scroll(false)
            .open(&mut is_open)
            .show(ctx, |ui| {
                // Toolbar
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "Output: {}×{}",
                        self.output_width, self.output_height
                    ));
                    ui.separator();

                    ui.checkbox(&mut self.snap_to_grid, "Snap to grid");
                    if self.snap_to_grid {
                        ui.add(
                            egui::DragValue::new(&mut self.grid_size)
                                .prefix("Grid: ")
                                .suffix("px"),
                        );
                    }
                    ui.separator();

                    ui.checkbox(&mut self.live_updates, "Live updates");

                    // Show Apply button when live updates is off
                    if !self.live_updates {
                        ui.separator();
                        if ui.button("Apply Layout").clicked() {
                            self.apply_all_properties(ctx);
                        }
                    }

                    ui.separator();

                    // Layout templates dropdown
                    let mut template_applied = false;
                    egui::ComboBox::from_id_salt("layout_templates")
                        .selected_text("Templates")
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(false, "Multiview (2+4+N)").clicked() {
                                self.apply_template_multiview();
                                template_applied = true;
                            }
                            if ui
                                .selectable_label(false, "Full Screen (Input 0)")
                                .clicked()
                            {
                                self.apply_template_fullscreen();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "Picture-in-Picture").clicked() {
                                self.apply_template_pip();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "Side by Side").clicked() {
                                self.apply_template_side_by_side();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "Top / Bottom").clicked() {
                                self.apply_template_top_bottom();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "2x2 Grid").clicked() {
                                self.apply_template_grid_2x2();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "3x3 Grid").clicked() {
                                self.apply_template_grid_3x3();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "1 Large + 2 Small").clicked() {
                                self.apply_template_1_large_2_small();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "1 Large + 3 Small").clicked() {
                                self.apply_template_1_large_3_small();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "Vertical Stack").clicked() {
                                self.apply_template_vertical_stack();
                                template_applied = true;
                            }
                            if ui.selectable_label(false, "Horizontal Stack").clicked() {
                                self.apply_template_horizontal_stack();
                                template_applied = true;
                            }
                        });

                    // Send updates to API if template was applied and live updates is enabled
                    if template_applied && self.live_updates {
                        self.apply_all_properties(ctx);
                    }
                });

                ui.separator();

                // Get the remaining space after toolbar
                let remaining = ui.available_size();
                let properties_width = 200.0;
                let spacing = 8.0;
                let canvas_width = (remaining.x - properties_width - spacing).max(100.0);
                let content_height = remaining.y.max(100.0);

                ui.horizontal(|ui| {
                    // Canvas area - use Group to contain it
                    ui.group(|ui| {
                        ui.set_min_size(Vec2::new(canvas_width, content_height));
                        ui.set_max_size(Vec2::new(canvas_width, content_height));
                        self.show_canvas(ui);
                    });

                    // Properties panel (fixed width)
                    ui.group(|ui| {
                        ui.set_min_size(Vec2::new(properties_width, content_height));
                        ui.set_max_size(Vec2::new(properties_width, content_height));
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            if let Some(idx) = self.selected_input {
                                self.show_properties_panel(ui, idx);
                            } else {
                                ui.vertical_centered(|ui| {
                                    ui.add_space(20.0);
                                    ui.label("Select an input");
                                    ui.label("to edit properties");
                                });
                            }
                        });
                    });
                });
            });

        is_open
    }

    /// Show the canvas with input boxes.
    fn show_canvas(&mut self, ui: &mut egui::Ui) {
        // Use available size from the Resize container (stable, no feedback loop)
        let canvas_size = ui.available_size();
        let (response, painter) = ui.allocate_painter(canvas_size, Sense::click_and_drag());

        let canvas_rect = response.rect;

        // Calculate scale to fit output dimensions into available space while maintaining aspect ratio
        let output_aspect = self.output_width as f32 / self.output_height as f32;
        let available_aspect = canvas_rect.width() / canvas_rect.height();

        let canvas_scale = if available_aspect > output_aspect {
            // Height-constrained: fit to height
            canvas_rect.height() / self.output_height as f32
        } else {
            // Width-constrained: fit to width
            canvas_rect.width() / self.output_width as f32
        };

        // Center the output canvas in the available space
        let scaled_output_width = self.output_width as f32 * canvas_scale;
        let scaled_output_height = self.output_height as f32 * canvas_scale;
        let canvas_offset = Vec2::new(
            (canvas_rect.width() - scaled_output_width) / 2.0,
            (canvas_rect.height() - scaled_output_height) / 2.0,
        );

        let to_screen = |pos: Pos2| -> Pos2 {
            canvas_rect.left_top() + canvas_offset + pos.to_vec2() * canvas_scale
        };

        // Draw background
        painter.rect_filled(canvas_rect, 0.0, Color32::from_gray(30));

        // Draw output canvas bounds
        let output_rect = Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(self.output_width as f32, self.output_height as f32),
        );
        let screen_output_rect =
            Rect::from_min_max(to_screen(output_rect.min), to_screen(output_rect.max));
        painter.rect_filled(screen_output_rect, 0.0, Color32::BLACK);
        painter.rect_stroke(
            screen_output_rect,
            0.0,
            Stroke::new(2.0, Color32::from_gray(100)),
            StrokeKind::Inside,
        );

        // Draw grid if enabled
        if self.snap_to_grid {
            for x in (0..self.output_width).step_by(self.grid_size as usize) {
                let p1 = to_screen(Pos2::new(x as f32, 0.0));
                let p2 = to_screen(Pos2::new(x as f32, self.output_height as f32));
                painter.line_segment([p1, p2], Stroke::new(1.0, Color32::from_gray(40)));
            }
            for y in (0..self.output_height).step_by(self.grid_size as usize) {
                let p1 = to_screen(Pos2::new(0.0, y as f32));
                let p2 = to_screen(Pos2::new(self.output_width as f32, y as f32));
                painter.line_segment([p1, p2], Stroke::new(1.0, Color32::from_gray(40)));
            }
        }

        // Sort inputs by zorder for rendering
        let mut sorted_indices: Vec<usize> = (0..self.inputs.len()).collect();
        sorted_indices.sort_by_key(|&i| self.inputs[i].zorder);

        // Draw input boxes
        for &idx in &sorted_indices {
            let input = &self.inputs[idx];
            let rect = input.rect();
            let screen_rect = Rect::from_min_max(to_screen(rect.min), to_screen(rect.max));

            // Draw box
            painter.rect_filled(screen_rect, 0.0, input.color());

            let border_width = if input.selected { 3.0 } else { 1.0 };
            let border_color = if input.selected {
                Color32::WHITE
            } else {
                Color32::from_gray(150)
            };
            painter.rect_stroke(
                screen_rect,
                0.0,
                Stroke::new(border_width, border_color),
                StrokeKind::Inside,
            );

            // Draw label
            let label = format!("Input {}", input.input_index);
            painter.text(
                screen_rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(14.0),
                Color32::WHITE,
            );

            // Draw zorder indicator
            let zorder_label = format!("z:{}", input.zorder);
            painter.text(
                screen_rect.left_top() + Vec2::new(5.0, 5.0),
                egui::Align2::LEFT_TOP,
                zorder_label,
                egui::FontId::proportional(10.0),
                Color32::WHITE,
            );

            // Draw resize handles if selected (in screen space for consistent size)
            if input.selected {
                let screen_handle_size = 12.0; // Visual size in screen pixels
                for &handle in ResizeHandle::all() {
                    // Get handle position in canvas coordinates
                    let canvas_handle_pos = match handle {
                        ResizeHandle::TopLeft => rect.left_top(),
                        ResizeHandle::Top => Pos2::new(rect.center().x, rect.top()),
                        ResizeHandle::TopRight => rect.right_top(),
                        ResizeHandle::Left => Pos2::new(rect.left(), rect.center().y),
                        ResizeHandle::Right => Pos2::new(rect.right(), rect.center().y),
                        ResizeHandle::BottomLeft => rect.left_bottom(),
                        ResizeHandle::Bottom => Pos2::new(rect.center().x, rect.bottom()),
                        ResizeHandle::BottomRight => rect.right_bottom(),
                    };

                    // Convert to screen space and create fixed-size handle
                    let screen_pos = to_screen(canvas_handle_pos);
                    let screen_handle_rect =
                        Rect::from_center_size(screen_pos, Vec2::splat(screen_handle_size));

                    painter.rect_filled(screen_handle_rect, 2.0, Color32::WHITE);
                    painter.rect_stroke(
                        screen_handle_rect,
                        2.0,
                        Stroke::new(1.0, Color32::BLACK),
                        StrokeKind::Inside,
                    );
                }
            }
        }

        // Handle interactions
        self.handle_canvas_interaction(ui, &response, canvas_rect, canvas_scale, canvas_offset);
    }

    /// Handle mouse interactions on the canvas.
    fn handle_canvas_interaction(
        &mut self,
        ui: &mut egui::Ui,
        response: &Response,
        canvas_rect: Rect,
        canvas_scale: f32,
        canvas_offset: Vec2,
    ) {
        let to_screen = |pos: Pos2| -> Pos2 {
            canvas_rect.left_top() + canvas_offset + pos.to_vec2() * canvas_scale
        };
        let from_screen = |pos: Pos2| -> Pos2 {
            ((pos - canvas_rect.left_top() - canvas_offset) / canvas_scale).to_pos2()
        };

        // Get mouse position - use interact_pos for drags (more reliable than hover_pos)
        let mouse_pos = ui
            .input(|i| i.pointer.interact_pos())
            .or_else(|| response.hover_pos());
        let canvas_pos = mouse_pos.map(from_screen);

        // FIRST: Handle ongoing drags (must be checked before hover detection)
        if let Some(idx) = self.dragging_input {
            if let Some(canvas_pos) = canvas_pos {
                // Update position while dragging
                let delta = canvas_pos - self.drag_start_pos;
                let new_xpos_float = self.drag_start_xpos as f32 + delta.x;
                let new_ypos_float = self.drag_start_ypos as f32 + delta.y;

                let new_xpos = if self.snap_to_grid {
                    self.snap(new_xpos_float as i32)
                } else {
                    new_xpos_float.round() as i32
                };
                let new_ypos = if self.snap_to_grid {
                    self.snap(new_ypos_float as i32)
                } else {
                    new_ypos_float.round() as i32
                };

                self.inputs[idx].xpos = new_xpos.max(0).min(self.output_width as i32);
                self.inputs[idx].ypos = new_ypos.max(0).min(self.output_height as i32);
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);

                // Send throttled live updates while dragging (every 100ms)
                if self.live_updates && self.last_live_update.elapsed().as_millis() > 100 {
                    self.last_live_update = instant::Instant::now();
                    let xpos = self.inputs[idx].xpos;
                    let ypos = self.inputs[idx].ypos;
                    self.update_pad_property(
                        ui.ctx(),
                        idx,
                        "xpos",
                        PropertyValue::Int(xpos as i64),
                    );
                    self.update_pad_property(
                        ui.ctx(),
                        idx,
                        "ypos",
                        PropertyValue::Int(ypos as i64),
                    );
                }
            }

            // Check if drag stopped
            if ui.input(|i| i.pointer.any_released()) {
                let xpos = self.inputs[idx].xpos;
                let ypos = self.inputs[idx].ypos;
                self.update_pad_property(ui.ctx(), idx, "xpos", PropertyValue::Int(xpos as i64));
                self.update_pad_property(ui.ctx(), idx, "ypos", PropertyValue::Int(ypos as i64));
                self.dragging_input = None;
            }
            return; // Don't process other interactions while dragging
        }

        // SECOND: Handle ongoing resizes
        if let Some((idx, handle)) = self.resizing_input {
            if let Some(canvas_pos) = canvas_pos {
                let delta = canvas_pos - self.resize_start_pos;

                match handle {
                    ResizeHandle::TopLeft => {
                        self.inputs[idx].width =
                            self.snap(self.resize_start_width - delta.x as i32).max(10);
                        self.inputs[idx].height =
                            self.snap(self.resize_start_height - delta.y as i32).max(10);
                        self.inputs[idx].xpos = self.snap(self.resize_start_xpos + delta.x as i32);
                        self.inputs[idx].ypos = self.snap(self.resize_start_ypos + delta.y as i32);
                    }
                    ResizeHandle::Top => {
                        self.inputs[idx].height =
                            self.snap(self.resize_start_height - delta.y as i32).max(10);
                        self.inputs[idx].ypos = self.snap(self.resize_start_ypos + delta.y as i32);
                    }
                    ResizeHandle::TopRight => {
                        self.inputs[idx].width =
                            self.snap(self.resize_start_width + delta.x as i32).max(10);
                        self.inputs[idx].height =
                            self.snap(self.resize_start_height - delta.y as i32).max(10);
                        self.inputs[idx].ypos = self.snap(self.resize_start_ypos + delta.y as i32);
                    }
                    ResizeHandle::Left => {
                        self.inputs[idx].width =
                            self.snap(self.resize_start_width - delta.x as i32).max(10);
                        self.inputs[idx].xpos = self.snap(self.resize_start_xpos + delta.x as i32);
                    }
                    ResizeHandle::Right => {
                        self.inputs[idx].width =
                            self.snap(self.resize_start_width + delta.x as i32).max(10);
                    }
                    ResizeHandle::BottomLeft => {
                        self.inputs[idx].width =
                            self.snap(self.resize_start_width - delta.x as i32).max(10);
                        self.inputs[idx].height =
                            self.snap(self.resize_start_height + delta.y as i32).max(10);
                        self.inputs[idx].xpos = self.snap(self.resize_start_xpos + delta.x as i32);
                    }
                    ResizeHandle::Bottom => {
                        self.inputs[idx].height =
                            self.snap(self.resize_start_height + delta.y as i32).max(10);
                    }
                    ResizeHandle::BottomRight => {
                        self.inputs[idx].width =
                            self.snap(self.resize_start_width + delta.x as i32).max(10);
                        self.inputs[idx].height =
                            self.snap(self.resize_start_height + delta.y as i32).max(10);
                    }
                }
                ui.ctx().set_cursor_icon(handle.cursor_icon());

                // Send throttled live updates while resizing (every 100ms)
                if self.live_updates && self.last_live_update.elapsed().as_millis() > 100 {
                    self.last_live_update = instant::Instant::now();
                    let xpos = self.inputs[idx].xpos;
                    let ypos = self.inputs[idx].ypos;
                    let width = self.inputs[idx].width;
                    let height = self.inputs[idx].height;
                    self.update_pad_property(
                        ui.ctx(),
                        idx,
                        "xpos",
                        PropertyValue::Int(xpos as i64),
                    );
                    self.update_pad_property(
                        ui.ctx(),
                        idx,
                        "ypos",
                        PropertyValue::Int(ypos as i64),
                    );
                    self.update_pad_property(
                        ui.ctx(),
                        idx,
                        "width",
                        PropertyValue::Int(width as i64),
                    );
                    self.update_pad_property(
                        ui.ctx(),
                        idx,
                        "height",
                        PropertyValue::Int(height as i64),
                    );
                }
            }

            // Check if resize stopped
            if ui.input(|i| i.pointer.any_released()) {
                let xpos = self.inputs[idx].xpos;
                let ypos = self.inputs[idx].ypos;
                let width = self.inputs[idx].width;
                let height = self.inputs[idx].height;
                self.update_pad_property(ui.ctx(), idx, "xpos", PropertyValue::Int(xpos as i64));
                self.update_pad_property(ui.ctx(), idx, "ypos", PropertyValue::Int(ypos as i64));
                self.update_pad_property(ui.ctx(), idx, "width", PropertyValue::Int(width as i64));
                self.update_pad_property(
                    ui.ctx(),
                    idx,
                    "height",
                    PropertyValue::Int(height as i64),
                );
                self.resizing_input = None;
            }
            return; // Don't process other interactions while resizing
        }

        // THIRD: Check for new interactions (only when not already dragging/resizing)
        let Some(mouse_pos) = mouse_pos else { return };
        let Some(canvas_pos) = canvas_pos else { return };

        // Check for resize handle hover/start
        if let Some(selected_idx) = self.selected_input {
            let input = &self.inputs[selected_idx];
            let rect = input.rect();
            let screen_handle_size = 24.0; // Large hit area for easy grabbing

            for &handle in ResizeHandle::all() {
                let canvas_handle_pos = match handle {
                    ResizeHandle::TopLeft => rect.left_top(),
                    ResizeHandle::Top => Pos2::new(rect.center().x, rect.top()),
                    ResizeHandle::TopRight => rect.right_top(),
                    ResizeHandle::Left => Pos2::new(rect.left(), rect.center().y),
                    ResizeHandle::Right => Pos2::new(rect.right(), rect.center().y),
                    ResizeHandle::BottomLeft => rect.left_bottom(),
                    ResizeHandle::Bottom => Pos2::new(rect.center().x, rect.bottom()),
                    ResizeHandle::BottomRight => rect.right_bottom(),
                };

                let screen_pos = to_screen(canvas_handle_pos);
                let screen_handle_rect =
                    Rect::from_center_size(screen_pos, Vec2::splat(screen_handle_size));

                if screen_handle_rect.contains(mouse_pos) {
                    ui.ctx().set_cursor_icon(handle.cursor_icon());

                    if response.drag_started() {
                        self.resizing_input = Some((selected_idx, handle));
                        self.resize_start_pos = canvas_pos;
                        self.resize_start_width = input.width;
                        self.resize_start_height = input.height;
                        self.resize_start_xpos = input.xpos;
                        self.resize_start_ypos = input.ypos;
                    }
                    return;
                }
            }
        }

        // Check for input box click/drag start (reverse order for z-order)
        for idx in (0..self.inputs.len()).rev() {
            let input_rect = self.inputs[idx].rect();

            if input_rect.contains(canvas_pos) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);

                if response.clicked() {
                    self.selected_input = Some(idx);
                    for i in 0..self.inputs.len() {
                        self.inputs[i].selected = i == idx;
                    }
                }

                if response.drag_started() {
                    self.dragging_input = Some(idx);
                    self.drag_start_pos = canvas_pos;
                    self.drag_start_xpos = self.inputs[idx].xpos;
                    self.drag_start_ypos = self.inputs[idx].ypos;
                }
                return;
            }
        }

        // Click on background deselects
        if response.clicked() {
            self.selected_input = None;
            for input in &mut self.inputs {
                input.selected = false;
            }
        }
    }

    /// Show the properties panel for the selected input.
    fn show_properties_panel(&mut self, ui: &mut egui::Ui, selected_idx: usize) {
        // Force vertical layout and respect allocated width
        ui.set_max_width(250.0);

        ui.vertical(|ui| {
            ui.heading(format!("Input {}", selected_idx));
            ui.separator();

            // Clone current values to avoid borrowing conflicts
            let mut xpos = self.inputs[selected_idx].xpos;
            let mut ypos = self.inputs[selected_idx].ypos;
            let mut width = self.inputs[selected_idx].width;
            let mut height = self.inputs[selected_idx].height;
            let mut alpha = self.inputs[selected_idx].alpha;
            let mut zorder = self.inputs[selected_idx].zorder;
            let mut sizing_policy = self.inputs[selected_idx].sizing_policy.clone();

            ui.label("Position:");
            ui.horizontal(|ui| {
                ui.label("X:");
                if ui
                    .add(egui::DragValue::new(&mut xpos).suffix("px"))
                    .changed()
                {
                    self.inputs[selected_idx].xpos = xpos;
                    self.update_pad_property(
                        ui.ctx(),
                        selected_idx,
                        "xpos",
                        PropertyValue::Int(xpos as i64),
                    );
                }
                ui.label("Y:");
                if ui
                    .add(egui::DragValue::new(&mut ypos).suffix("px"))
                    .changed()
                {
                    self.inputs[selected_idx].ypos = ypos;
                    self.update_pad_property(
                        ui.ctx(),
                        selected_idx,
                        "ypos",
                        PropertyValue::Int(ypos as i64),
                    );
                }
            });

            ui.label("Size:");
            ui.horizontal(|ui| {
                ui.label("W:");
                if ui
                    .add(egui::DragValue::new(&mut width).suffix("px"))
                    .changed()
                {
                    self.inputs[selected_idx].width = width;
                    self.update_pad_property(
                        ui.ctx(),
                        selected_idx,
                        "width",
                        PropertyValue::Int(width as i64),
                    );
                }
                ui.label("H:");
                if ui
                    .add(egui::DragValue::new(&mut height).suffix("px"))
                    .changed()
                {
                    self.inputs[selected_idx].height = height;
                    self.update_pad_property(
                        ui.ctx(),
                        selected_idx,
                        "height",
                        PropertyValue::Int(height as i64),
                    );
                }
            });

            ui.separator();

            ui.label("Alpha:");
            if ui.add(egui::Slider::new(&mut alpha, 0.0..=1.0)).changed() {
                self.inputs[selected_idx].alpha = alpha;
                self.update_pad_property(
                    ui.ctx(),
                    selected_idx,
                    "alpha",
                    PropertyValue::Float(alpha),
                );
            }

            ui.label("Z-Order:");
            if ui
                .add(egui::DragValue::new(&mut zorder).range(0..=15))
                .changed()
            {
                self.inputs[selected_idx].zorder = zorder;
                self.update_pad_property(
                    ui.ctx(),
                    selected_idx,
                    "zorder",
                    PropertyValue::UInt(zorder as u64),
                );
            }

            ui.label("Sizing:");
            let mut sizing_changed = false;
            egui::ComboBox::from_id_salt("sizing_policy")
                .selected_text(if sizing_policy == "none" {
                    "Stretch"
                } else {
                    "Keep Aspect"
                })
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(sizing_policy == "none", "Stretch")
                        .clicked()
                    {
                        sizing_policy = "none".to_string();
                        sizing_changed = true;
                    }
                    if ui
                        .selectable_label(sizing_policy != "none", "Keep Aspect")
                        .clicked()
                    {
                        sizing_policy = "keep-aspect-ratio".to_string();
                        sizing_changed = true;
                    }
                });

            if sizing_changed {
                self.inputs[selected_idx].sizing_policy = sizing_policy.clone();
                self.update_pad_property(
                    ui.ctx(),
                    selected_idx,
                    "sizing-policy",
                    PropertyValue::String(sizing_policy),
                );
            }
        });
    }
}
