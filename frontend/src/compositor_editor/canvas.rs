use egui::{Pos2, Rect, Response, Sense, Vec2};
use strom_types::PropertyValue;

use super::*;

impl CompositorEditor {
    /// Show the canvas with input boxes.
    pub(super) fn show_canvas(&mut self, ui: &mut egui::Ui) {
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
            Stroke::new(2.0_f32, Color32::from_gray(100)),
            StrokeKind::Inside,
        );

        // Draw grid if enabled
        if self.snap_to_grid {
            for x in (0..self.output_width).step_by(self.grid_size as usize) {
                let p1 = to_screen(Pos2::new(x as f32, 0.0));
                let p2 = to_screen(Pos2::new(x as f32, self.output_height as f32));
                painter.line_segment([p1, p2], Stroke::new(1.0_f32, Color32::from_gray(40)));
            }
            for y in (0..self.output_height).step_by(self.grid_size as usize) {
                let p1 = to_screen(Pos2::new(0.0, y as f32));
                let p2 = to_screen(Pos2::new(self.output_width as f32, y as f32));
                painter.line_segment([p1, p2], Stroke::new(1.0_f32, Color32::from_gray(40)));
            }
        }

        // Sort inputs by zorder for rendering
        let mut sorted_indices: Vec<usize> = (0..self.inputs.len()).collect();
        sorted_indices.sort_by_key(|&i| self.inputs[i].zorder);

        // Draw input boxes
        let has_selection = self.selected_input.is_some();
        for &idx in &sorted_indices {
            let input = &self.inputs[idx];
            let rect = input.rect();
            let screen_rect = Rect::from_min_max(to_screen(rect.min), to_screen(rect.max));

            // When an input is selected, draw others at half opacity
            let dimmed = has_selection && !input.selected;
            let opacity_mult = if dimmed { 0.4 } else { 1.0 };

            // Draw thumbnail or fallback to colored box
            if let Some(texture) = self.thumbnails.get(&idx) {
                // Apply input's alpha and selection dimming
                let alpha = (input.alpha * opacity_mult).clamp(0.0, 1.0);
                let tint = Color32::from_rgba_unmultiplied(255, 255, 255, (255.0 * alpha) as u8);

                // Calculate UV rect based on sizing policy
                // Thumbnail is 320x180 (16:9), input rect may have different aspect ratio
                let thumb_aspect = 320.0 / 180.0; // 16:9
                let input_aspect = rect.width() / rect.height();

                let uv_rect = if input.sizing_policy == "none" {
                    // Stretch: use full UV (0,0 to 1,1)
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
                } else {
                    // keep-aspect-ratio: crop the thumbnail to match input's aspect ratio
                    if input_aspect > thumb_aspect {
                        // Input is wider - crop top/bottom of thumbnail
                        let visible_height = thumb_aspect / input_aspect;
                        let margin = (1.0 - visible_height) / 2.0;
                        Rect::from_min_max(egui::pos2(0.0, margin), egui::pos2(1.0, 1.0 - margin))
                    } else {
                        // Input is taller - crop left/right of thumbnail
                        let visible_width = input_aspect / thumb_aspect;
                        let margin = (1.0 - visible_width) / 2.0;
                        Rect::from_min_max(egui::pos2(margin, 0.0), egui::pos2(1.0 - margin, 1.0))
                    }
                };

                painter.image(texture.id(), screen_rect, uv_rect, tint);
            } else {
                // Fallback to colored box - apply input alpha and selection dimming
                let alpha = (input.alpha * opacity_mult).clamp(0.0, 1.0);
                let mut color = input.color();
                color = Color32::from_rgba_unmultiplied(
                    color.r(),
                    color.g(),
                    color.b(),
                    (color.a() as f64 * alpha) as u8,
                );
                painter.rect_filled(screen_rect, 0.0, color);
            }

            let border_width = if input.selected { 3.0_f32 } else { 1.0_f32 };
            let border_color = if input.selected {
                Color32::WHITE
            } else if dimmed {
                Color32::from_rgba_unmultiplied(150, 150, 150, 100)
            } else {
                Color32::from_gray(150)
            };
            painter.rect_stroke(
                screen_rect,
                0.0,
                Stroke::new(border_width, border_color),
                StrokeKind::Inside,
            );

            // Draw label with adjusted opacity
            let text_color = if dimmed {
                Color32::from_rgba_unmultiplied(255, 255, 255, 100)
            } else {
                Color32::WHITE
            };
            let label = format!("Input {}", input.input_index);
            painter.text(
                screen_rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(14.0),
                text_color,
            );

            // Draw zorder indicator
            let zorder_label = format!("z:{}", input.zorder);
            painter.text(
                screen_rect.left_top() + Vec2::new(5.0, 5.0),
                egui::Align2::LEFT_TOP,
                zorder_label,
                egui::FontId::proportional(10.0),
                text_color,
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
                        Stroke::new(1.0_f32, Color32::BLACK),
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

        // Check for input box click/drag start
        // If an input is selected, only that input can be dragged (regardless of z-order)
        // Clicking still works on any input to change selection
        if let Some(selected_idx) = self.selected_input {
            // Selected input - can be dragged
            let input_rect = self.inputs[selected_idx].rect();
            if input_rect.contains(canvas_pos) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);

                if response.clicked() {
                    self.toggle_input_selection(selected_idx);
                }

                if response.drag_started() {
                    self.dragging_input = Some(selected_idx);
                    self.drag_start_pos = canvas_pos;
                    self.drag_start_xpos = self.inputs[selected_idx].xpos;
                    self.drag_start_ypos = self.inputs[selected_idx].ypos;
                }
                return;
            }

            // Check other inputs for click only (to change selection), no drag
            for idx in (0..self.inputs.len()).rev() {
                if idx == selected_idx {
                    continue;
                }
                let input_rect = self.inputs[idx].rect();
                if input_rect.contains(canvas_pos) {
                    if response.clicked() {
                        self.toggle_input_selection(idx);
                    }
                    return;
                }
            }
        } else {
            // No selection - check all inputs (reverse z-order), click to select
            for idx in (0..self.inputs.len()).rev() {
                let input_rect = self.inputs[idx].rect();

                if input_rect.contains(canvas_pos) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);

                    if response.clicked() {
                        self.toggle_input_selection(idx);
                    }

                    // No drag without selection
                    return;
                }
            }
        }

        // Click on background deselects
        if response.clicked() {
            self.deselect_input();
        }
    }

    /// Show the properties panel for the selected input.
    pub(super) fn show_properties_panel(&mut self, ui: &mut egui::Ui, selected_idx: usize) {
        // Force vertical layout and respect allocated width
        ui.set_max_width(250.0);

        let out_w = self.output_width as i32;
        let out_h = self.output_height as i32;

        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.heading(format!("Input {}", selected_idx));
                if ui
                    .button("Reset")
                    .on_hover_text("Reset position (0,0), full size, alpha 1.0 (R)")
                    .clicked()
                {
                    self.reset_input(ui.ctx(), selected_idx, out_w, out_h);
                }
            });
            ui.separator();

            // Clone current values to avoid borrowing conflicts
            let mut xpos = self.inputs[selected_idx].xpos;
            let mut ypos = self.inputs[selected_idx].ypos;
            let mut width = self.inputs[selected_idx].width;
            let mut height = self.inputs[selected_idx].height;
            let mut alpha = self.inputs[selected_idx].alpha;
            let mut zorder = self.inputs[selected_idx].zorder;
            let mut sizing_policy = self.inputs[selected_idx].sizing_policy.clone();

            // Position section
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

            // Position quick buttons (quadrants)
            ui.horizontal(|ui| {
                let btn_size = Vec2::new(24.0, 18.0);
                // Top-left quadrant
                if ui
                    .add(egui::Button::new("TL").min_size(btn_size))
                    .on_hover_text("Top-left (0, 0)")
                    .clicked()
                {
                    self.set_input_position(ui.ctx(), selected_idx, 0, 0);
                }
                // Top-right quadrant
                if ui
                    .add(egui::Button::new("TR").min_size(btn_size))
                    .on_hover_text("Top-right")
                    .clicked()
                {
                    let w = self.inputs[selected_idx].width;
                    self.set_input_position(ui.ctx(), selected_idx, out_w - w, 0);
                }
                // Bottom-left quadrant
                if ui
                    .add(egui::Button::new("BL").min_size(btn_size))
                    .on_hover_text("Bottom-left")
                    .clicked()
                {
                    let h = self.inputs[selected_idx].height;
                    self.set_input_position(ui.ctx(), selected_idx, 0, out_h - h);
                }
                // Bottom-right quadrant
                if ui
                    .add(egui::Button::new("BR").min_size(btn_size))
                    .on_hover_text("Bottom-right")
                    .clicked()
                {
                    let w = self.inputs[selected_idx].width;
                    let h = self.inputs[selected_idx].height;
                    self.set_input_position(ui.ctx(), selected_idx, out_w - w, out_h - h);
                }
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);

            // Size section
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

            // Size quick buttons
            ui.horizontal(|ui| {
                let btn_size = Vec2::new(28.0, 18.0);
                // Full size (and position 0,0)
                if ui
                    .add(egui::Button::new("Full").min_size(btn_size))
                    .on_hover_text("Full screen")
                    .clicked()
                {
                    self.set_input_position(ui.ctx(), selected_idx, 0, 0);
                    self.set_input_size(ui.ctx(), selected_idx, out_w, out_h);
                }
                // Half size
                if ui
                    .add(egui::Button::new("1/2").min_size(btn_size))
                    .on_hover_text("Half size")
                    .clicked()
                {
                    self.set_input_size(ui.ctx(), selected_idx, out_w / 2, out_h / 2);
                }
                // Third size
                if ui
                    .add(egui::Button::new("1/3").min_size(btn_size))
                    .on_hover_text("Third size")
                    .clicked()
                {
                    self.set_input_size(ui.ctx(), selected_idx, out_w / 3, out_h / 3);
                }
                // Quarter size
                if ui
                    .add(egui::Button::new("1/4").min_size(btn_size))
                    .on_hover_text("Quarter size")
                    .clicked()
                {
                    self.set_input_size(ui.ctx(), selected_idx, out_w / 4, out_h / 4);
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

            // Z-Order section with quick buttons
            ui.horizontal(|ui| {
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
            });

            // Z-order quick buttons
            ui.horizontal(|ui| {
                let btn_size = Vec2::new(24.0, 18.0);
                // Send to back
                if ui
                    .add(
                        egui::Button::new(egui_phosphor::regular::ARROW_LINE_DOWN)
                            .min_size(btn_size),
                    )
                    .on_hover_text("Send to back (Home)")
                    .clicked()
                {
                    self.inputs[selected_idx].zorder = 0;
                    if self.live_updates {
                        self.update_pad_property(
                            ui.ctx(),
                            selected_idx,
                            "zorder",
                            PropertyValue::UInt(0),
                        );
                    }
                }
                // Move down
                if ui
                    .add(egui::Button::new(egui_phosphor::regular::CARET_DOWN).min_size(btn_size))
                    .on_hover_text("Move down (PgDn)")
                    .clicked()
                    && self.inputs[selected_idx].zorder > 0
                {
                    self.inputs[selected_idx].zorder -= 1;
                    if self.live_updates {
                        self.update_pad_property(
                            ui.ctx(),
                            selected_idx,
                            "zorder",
                            PropertyValue::UInt(self.inputs[selected_idx].zorder as u64),
                        );
                    }
                }
                // Move up
                if ui
                    .add(egui::Button::new(egui_phosphor::regular::CARET_UP).min_size(btn_size))
                    .on_hover_text("Move up (PgUp)")
                    .clicked()
                {
                    self.inputs[selected_idx].zorder += 1;
                    if self.live_updates {
                        self.update_pad_property(
                            ui.ctx(),
                            selected_idx,
                            "zorder",
                            PropertyValue::UInt(self.inputs[selected_idx].zorder as u64),
                        );
                    }
                }
                // Bring to front
                if ui
                    .add(
                        egui::Button::new(egui_phosphor::regular::ARROW_LINE_UP).min_size(btn_size),
                    )
                    .on_hover_text("Bring to front (End)")
                    .clicked()
                {
                    let max_z = self.inputs.iter().map(|i| i.zorder).max().unwrap_or(0);
                    self.inputs[selected_idx].zorder = max_z + 1;
                    if self.live_updates {
                        self.update_pad_property(
                            ui.ctx(),
                            selected_idx,
                            "zorder",
                            PropertyValue::UInt(self.inputs[selected_idx].zorder as u64),
                        );
                    }
                }
            });

            ui.add_space(4.0);

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
