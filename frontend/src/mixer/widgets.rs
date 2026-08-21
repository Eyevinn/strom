use super::*;

impl MixerEditor {
    /// Render a vertical fader using dB scale.
    /// Internally converts between dB (-60 to +6) and linear (0.0 to 2.0).
    pub(super) fn render_fader(&mut self, ui: &mut Ui, index: usize, height: f32) -> Response {
        let channel = &mut self.channels[index];

        // Convert linear fader value to dB for display
        let mut fader_db = linear_to_db(channel.fader as f64) as f32;

        // Allocate exact size for the fader
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(16.0, height), Sense::click_and_drag());

        // Double-click: toggle between 0 dB and -inf
        if response.double_clicked() {
            if (fader_db - 0.0).abs() < 0.5 {
                channel.fader = 0.0;
                fader_db = -60.0;
            } else {
                channel.fader = 1.0;
                fader_db = 0.0;
            }
        } else if response.dragged() {
            // Calculate new dB value based on drag position
            let delta = -response.drag_delta().y; // Negative because y increases downward
            let db_per_pixel = 66.0 / (height - 10.0); // -60 to +6 = 66 dB range
            fader_db = (fader_db + delta * db_per_pixel).clamp(-60.0, 6.0);
            channel.fader = db_to_linear_f32(fader_db);
        }

        // Draw fader track
        let painter = ui.painter();
        let track_width = 4.0;
        let track_rect =
            Rect::from_center_size(rect.center(), Vec2::new(track_width, height - 10.0));
        painter.rect_filled(track_rect, CornerRadius::same(2), Color32::from_gray(60));

        // Draw fader handle
        let handle_y = db_to_y(fader_db, rect.min.y, rect.max.y);
        let handle_rect =
            Rect::from_center_size(egui::pos2(rect.center().x, handle_y), Vec2::new(14.0, 36.0));
        let handle_color = if response.dragged() {
            Color32::from_rgb(100, 150, 255)
        } else if response.hovered() {
            Color32::from_rgb(200, 200, 200)
        } else {
            Color32::from_rgb(160, 160, 160)
        };
        painter.rect_filled(handle_rect, CornerRadius::same(3), handle_color);
        // Center line indicating exact value
        painter.line_segment(
            [
                egui::pos2(handle_rect.left() + 2.0, handle_y),
                egui::pos2(handle_rect.right() - 2.0, handle_y),
            ],
            Stroke::new(1.5_f32, Color32::from_gray(40)),
        );

        response
    }

    /// Render a group fader.
    pub(super) fn render_group_fader(
        &mut self,
        ui: &mut Ui,
        sg_idx: usize,
        height: f32,
    ) -> Response {
        let fader_val = self.groups[sg_idx].fader;
        let mut fader_db = linear_to_db(fader_val as f64) as f32;

        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(16.0, height), Sense::click_and_drag());

        if response.double_clicked() {
            if (fader_db - 0.0).abs() < 0.5 {
                self.groups[sg_idx].fader = 0.0;
                fader_db = -60.0;
            } else {
                self.groups[sg_idx].fader = 1.0;
                fader_db = 0.0;
            }
        } else if response.dragged() {
            let delta = -response.drag_delta().y;
            let db_per_pixel = 66.0 / (height - 10.0);
            fader_db = (fader_db + delta * db_per_pixel).clamp(-60.0, 6.0);
            self.groups[sg_idx].fader = db_to_linear_f32(fader_db);
        }

        // Draw fader track
        let painter = ui.painter();
        let track_rect = Rect::from_center_size(rect.center(), Vec2::new(4.0, height - 10.0));
        painter.rect_filled(track_rect, CornerRadius::same(2), Color32::from_gray(60));

        // Draw handle
        let handle_y = db_to_y(fader_db, rect.min.y, rect.max.y);
        let handle_rect =
            Rect::from_center_size(egui::pos2(rect.center().x, handle_y), Vec2::new(12.0, 30.0));
        let handle_color = if response.dragged() {
            Color32::from_rgb(200, 150, 200)
        } else if response.hovered() {
            Color32::from_rgb(200, 200, 200)
        } else {
            Color32::from_rgb(160, 160, 160)
        };
        painter.rect_filled(handle_rect, CornerRadius::same(3), handle_color);
        painter.line_segment(
            [
                egui::pos2(handle_rect.left() + 2.0, handle_y),
                egui::pos2(handle_rect.right() - 2.0, handle_y),
            ],
            Stroke::new(1.5_f32, Color32::from_gray(40)),
        );

        response
    }

    /// Render an aux master fader.
    pub(super) fn render_aux_master_fader(
        &mut self,
        ui: &mut Ui,
        aux_idx: usize,
        height: f32,
    ) -> Response {
        let fader_val = self.aux_masters[aux_idx].fader;
        let mut fader_db = linear_to_db(fader_val as f64) as f32;

        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(16.0, height), Sense::click_and_drag());

        if response.double_clicked() {
            if (fader_db - 0.0).abs() < 0.5 {
                self.aux_masters[aux_idx].fader = 0.0;
                fader_db = -60.0;
            } else {
                self.aux_masters[aux_idx].fader = 1.0;
                fader_db = 0.0;
            }
        } else if response.dragged() {
            let delta = -response.drag_delta().y;
            let db_per_pixel = 66.0 / (height - 10.0);
            fader_db = (fader_db + delta * db_per_pixel).clamp(-60.0, 6.0);
            self.aux_masters[aux_idx].fader = db_to_linear_f32(fader_db);
        }

        // Draw fader track
        let painter = ui.painter();
        let track_rect = Rect::from_center_size(rect.center(), Vec2::new(4.0, height - 10.0));
        painter.rect_filled(track_rect, CornerRadius::same(2), Color32::from_gray(60));

        // Draw handle
        let handle_y = db_to_y(fader_db, rect.min.y, rect.max.y);
        let handle_rect =
            Rect::from_center_size(egui::pos2(rect.center().x, handle_y), Vec2::new(12.0, 30.0));
        let handle_color = if response.dragged() {
            Color32::from_rgb(150, 200, 255)
        } else if response.hovered() {
            Color32::from_rgb(200, 200, 200)
        } else {
            Color32::from_rgb(160, 160, 160)
        };
        painter.rect_filled(handle_rect, CornerRadius::same(3), handle_color);
        painter.line_segment(
            [
                egui::pos2(handle_rect.left() + 2.0, handle_y),
                egui::pos2(handle_rect.right() - 2.0, handle_y),
            ],
            Stroke::new(1.5_f32, Color32::from_gray(40)),
        );

        response
    }

    /// Render a pan knob. Center (0.0) at 12 o'clock, L at 7:30, R at 4:30.
    pub(super) fn render_pan_knob(&mut self, ui: &mut Ui, index: usize) -> Response {
        let pan = self.channels[index].pan;
        let (rect, response) =
            ui.allocate_exact_size(Vec2::splat(PAN_KNOB_SIZE), Sense::click_and_drag());

        if response.dragged() {
            let delta = -response.drag_delta().y * 0.01;
            self.channels[index].pan = (self.channels[index].pan + delta).clamp(-1.0, 1.0);
        }

        // Double-click: reset to center
        if response.double_clicked() {
            self.channels[index].pan = 0.0;
        }

        let painter = ui.painter();
        let center = rect.center();
        let radius = PAN_KNOB_SIZE * 0.5 - 1.0;

        // Background circle
        painter.circle_filled(center, radius, Color32::from_rgb(28, 28, 32));

        // Pan maps: -1.0 → 0.0 (7:30), 0.0 → 0.5 (12 o'clock), 1.0 → 1.0 (4:30)
        let normalized = (pan + 1.0) * 0.5;

        let arc_start = std::f32::consts::PI * 1.75; // 7:30
        let arc_sweep = std::f32::consts::PI * 1.5; // 270°

        // Draw arc from center (12 o'clock) to current pan position
        let center_norm = 0.5;
        let (from_norm, to_norm) = if normalized < center_norm {
            (normalized, center_norm)
        } else {
            (center_norm, normalized)
        };

        if (to_norm - from_norm) > 0.005 {
            let from_angle = arc_start + from_norm * arc_sweep;
            let to_angle = arc_start + to_norm * arc_sweep;
            let sweep = to_angle - from_angle;
            let segments = (sweep.abs() / arc_sweep * 24.0).max(4.0) as usize;
            let arc_color = if response.dragged() {
                Color32::from_rgb(255, 180, 100)
            } else {
                Color32::from_rgb(200, 140, 70)
            };
            let arc_r = radius - 1.5;
            for i in 0..segments {
                let t0 = i as f32 / segments as f32;
                let t1 = (i + 1) as f32 / segments as f32;
                let a0 = from_angle + sweep * t0;
                let a1 = from_angle + sweep * t1;
                painter.line_segment(
                    [
                        egui::pos2(center.x - a0.cos() * arc_r, center.y - a0.sin() * arc_r),
                        egui::pos2(center.x - a1.cos() * arc_r, center.y - a1.sin() * arc_r),
                    ],
                    Stroke::new(2.5_f32, arc_color),
                );
            }
        }

        // Center marker tick at 12 o'clock
        let center_angle = arc_start + 0.5 * arc_sweep;
        let tick_inner = radius - 3.5;
        let tick_outer = radius + 0.5;
        painter.line_segment(
            [
                egui::pos2(
                    center.x - center_angle.cos() * tick_inner,
                    center.y - center_angle.sin() * tick_inner,
                ),
                egui::pos2(
                    center.x - center_angle.cos() * tick_outer,
                    center.y - center_angle.sin() * tick_outer,
                ),
            ],
            Stroke::new(1.0_f32, Color32::from_gray(90)),
        );

        // Pointer line
        let pointer_angle = arc_start + normalized * arc_sweep;
        let inner_r = radius * 0.25;
        let outer_r = radius - 3.0;
        let pointer_color = Color32::WHITE;
        painter.line_segment(
            [
                egui::pos2(
                    center.x - pointer_angle.cos() * inner_r,
                    center.y - pointer_angle.sin() * inner_r,
                ),
                egui::pos2(
                    center.x - pointer_angle.cos() * outer_r,
                    center.y - pointer_angle.sin() * outer_r,
                ),
            ],
            Stroke::new(1.5_f32, pointer_color),
        );

        // Border
        let border_color = if response.hovered() || response.dragged() {
            Color32::from_rgb(200, 150, 80)
        } else {
            Color32::from_gray(55)
        };
        painter.circle_stroke(center, radius, Stroke::new(1.0_f32, border_color));

        // Tooltip
        let resp = response.on_hover_text(format!("Pan: {}", format_pan(pan)));
        resp
    }

    /// Render a styled LCD display box.
    pub(super) fn render_lcd(&self, ui: &mut Ui, text: &str, width: f32, height: f32) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(2), Color32::from_rgb(18, 22, 28));
        painter.rect_stroke(
            rect,
            CornerRadius::same(2),
            Stroke::new(1.0_f32, Color32::from_rgb(45, 50, 55)),
            egui::epaint::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::monospace(10.0),
            Color32::from_rgb(90, 200, 90),
        );
    }

    /// Render an aux send rotary knob for a channel.
    ///
    /// dB-scaled arc: -inf at 7:30 (CCW), 0 dB (unity) at 12 o'clock, +6 dB at 4:30 (CW).
    /// The first half of the 270° arc covers -60..0 dB, the second half covers 0..+6 dB.
    pub(super) fn render_knob(&mut self, ui: &mut Ui, ch_idx: usize, aux_idx: usize) -> Response {
        let aux_level = self.channels[ch_idx].aux_sends[aux_idx];
        let (rect, response) =
            ui.allocate_exact_size(Vec2::splat(KNOB_SIZE), Sense::click_and_drag());

        // Drag in dB space for natural feel
        if response.dragged() {
            let current_db = linear_to_db(aux_level as f64) as f32;
            // Sensitivity: ~1 dB per 3 pixels
            let db_delta = -response.drag_delta().y * 0.35;
            let new_db = (current_db + db_delta).clamp(-60.0, 6.0);
            self.channels[ch_idx].aux_sends[aux_idx] = db_to_linear_f32(new_db);
        }

        // Double-click: toggle between off and unity
        if response.double_clicked() {
            self.channels[ch_idx].aux_sends[aux_idx] = if aux_level > 0.01 { 0.0 } else { 1.0 };
        }

        let painter = ui.painter();
        let center = rect.center();
        let radius = KNOB_SIZE * 0.5 - 1.0;

        // Background
        painter.circle_filled(center, radius, Color32::from_rgb(28, 28, 32));

        // Convert level to arc position (dB-scaled, 0 dB at center)
        let level = self.channels[ch_idx].aux_sends[aux_idx];
        let normalized = knob_linear_to_normalized(level);

        // Arc geometry: 270° sweep starting at 7:30 (bottom-left)
        // In our coord system (x = cx - cos*r, y = cy - sin*r):
        //   12 o'clock = π/2, clockwise = increasing angle
        //   7:30 = 7π/4, 4:30 = 5π/4 + 2π = 13π/4
        let arc_start = std::f32::consts::PI * 1.75; // 7:30 position
        let arc_sweep = std::f32::consts::PI * 1.5; // 270°

        // Draw filled arc up to current level
        if normalized > 0.005 {
            let sweep = normalized * arc_sweep;
            let segments = (normalized * 24.0).max(4.0) as usize;
            let arc_color = if response.dragged() {
                Color32::from_rgb(130, 190, 255)
            } else {
                Color32::from_rgb(90, 145, 200)
            };
            let arc_r = radius - 1.5;
            for i in 0..segments {
                let t0 = i as f32 / segments as f32;
                let t1 = (i + 1) as f32 / segments as f32;
                let a0 = arc_start + sweep * t0;
                let a1 = arc_start + sweep * t1;
                painter.line_segment(
                    [
                        egui::pos2(center.x - a0.cos() * arc_r, center.y - a0.sin() * arc_r),
                        egui::pos2(center.x - a1.cos() * arc_r, center.y - a1.sin() * arc_r),
                    ],
                    Stroke::new(2.5_f32, arc_color),
                );
            }
        }

        // Unity marker (small tick at 12 o'clock = center of arc)
        let unity_angle = arc_start + 0.5 * arc_sweep; // 12 o'clock
        let tick_inner = radius - 3.5;
        let tick_outer = radius + 0.5;
        painter.line_segment(
            [
                egui::pos2(
                    center.x - unity_angle.cos() * tick_inner,
                    center.y - unity_angle.sin() * tick_inner,
                ),
                egui::pos2(
                    center.x - unity_angle.cos() * tick_outer,
                    center.y - unity_angle.sin() * tick_outer,
                ),
            ],
            Stroke::new(1.0_f32, Color32::from_gray(90)),
        );

        // Pointer indicator line
        let pointer_angle = arc_start + normalized * arc_sweep;
        let inner_r = radius * 0.25;
        let outer_r = radius - 3.0;
        let pointer_color = if level > 0.01 {
            Color32::WHITE
        } else {
            Color32::from_gray(70)
        };
        painter.line_segment(
            [
                egui::pos2(
                    center.x - pointer_angle.cos() * inner_r,
                    center.y - pointer_angle.sin() * inner_r,
                ),
                egui::pos2(
                    center.x - pointer_angle.cos() * outer_r,
                    center.y - pointer_angle.sin() * outer_r,
                ),
            ],
            Stroke::new(1.5_f32, pointer_color),
        );

        // Border
        let border_color = if response.hovered() || response.dragged() {
            Color32::from_rgb(90, 140, 200)
        } else {
            Color32::from_gray(55)
        };
        painter.circle_stroke(center, radius, Stroke::new(1.0_f32, border_color));

        // Hover tooltip
        let db_str = if level > 0.001 {
            format!("{:.1} dB", 20.0 * level.log10())
        } else {
            "-inf".to_string()
        };
        let resp = response.on_hover_text(format!("Aux {} send: {}", aux_idx + 1, db_str));

        resp
    }

    /// Render a stereo vertical meter (L/R side by side).
    pub(super) fn render_stereo_meter(
        &self,
        ui: &mut Ui,
        meter_data: Option<&MeterData>,
        height: f32,
    ) {
        let bar_width = 6.0;
        let gap = 2.0;
        let total_width = bar_width * 2.0 + gap;
        let (rect, _response) =
            ui.allocate_exact_size(Vec2::new(total_width, height), Sense::hover());

        let painter = ui.painter();

        // Left channel rect
        let left_rect = Rect::from_min_size(rect.min, Vec2::new(bar_width, height));
        // Right channel rect
        let right_rect = Rect::from_min_size(
            egui::pos2(rect.min.x + bar_width + gap, rect.min.y),
            Vec2::new(bar_width, height),
        );

        // Background for both
        painter.rect_filled(
            left_rect,
            CornerRadius::same(2),
            Color32::from_rgb(20, 20, 20),
        );
        painter.rect_filled(
            right_rect,
            CornerRadius::same(2),
            Color32::from_rgb(20, 20, 20),
        );

        // Always draw the dim zone background so the meter shows the colored
        // sectors (green/yellow/orange/red) even when no signal has arrived.
        draw_meter_zones_background(painter, left_rect);
        draw_meter_zones_background(painter, right_rect);

        if let Some(data) = meter_data {
            // Get L/R peak values (or use same for both if mono)
            let (left_peak, right_peak) = if data.peak.len() >= 2 {
                (data.peak[0], data.peak[1])
            } else if !data.peak.is_empty() {
                (data.peak[0], data.peak[0])
            } else {
                return;
            };

            // Light up each zone from the bottom up to the peak. Each zone
            // keeps its own colour, matching the standalone VU meter block.
            draw_meter_zones_lit(painter, left_rect, left_peak as f32);
            draw_meter_zones_lit(painter, right_rect, right_peak as f32);

            // Draw decay lines if available
            if data.decay.len() >= 2 {
                let left_decay_y = db_to_y(data.decay[0] as f32, rect.min.y, rect.max.y);
                painter.line_segment(
                    [
                        egui::pos2(left_rect.min.x, left_decay_y),
                        egui::pos2(left_rect.max.x, left_decay_y),
                    ],
                    Stroke::new(1.0_f32, Color32::WHITE),
                );

                let right_decay_y = db_to_y(data.decay[1] as f32, rect.min.y, rect.max.y);
                painter.line_segment(
                    [
                        egui::pos2(right_rect.min.x, right_decay_y),
                        egui::pos2(right_rect.max.x, right_decay_y),
                    ],
                    Stroke::new(1.0_f32, Color32::WHITE),
                );
            }
        }

        // Borders
        painter.rect_stroke(
            left_rect,
            CornerRadius::same(2),
            Stroke::new(1.0_f32, Color32::from_gray(60)),
            egui::epaint::StrokeKind::Inside,
        );
        painter.rect_stroke(
            right_rect,
            CornerRadius::same(2),
            Stroke::new(1.0_f32, Color32::from_gray(60)),
            egui::epaint::StrokeKind::Inside,
        );
    }

    /// Render a dB scale next to the fader.
    /// Scale is in dB with equal visual spacing for equal dB steps.
    pub(super) fn render_db_scale(&self, ui: &mut Ui, height: f32) {
        let width = 18.0;
        let (rect, _response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());

        let painter = ui.painter();

        let marks: &[f32] = &[6.0, 0.0, -6.0, -12.0, -20.0, -30.0, -40.0, -60.0];
        let text_color = Color32::from_gray(140);

        for &db in marks {
            let y = db_to_y(db, rect.min.y, rect.max.y);

            let label = if db > 0.0 {
                format!("+{}", db as i32)
            } else {
                format!("{}", db as i32)
            };

            painter.line_segment(
                [egui::pos2(rect.max.x - 3.0, y), egui::pos2(rect.max.x, y)],
                Stroke::new(1.0_f32, Color32::from_gray(100)),
            );

            painter.text(
                egui::pos2(rect.min.x, y),
                egui::Align2::LEFT_CENTER,
                &label,
                egui::FontId::proportional(8.0),
                text_color,
            );
        }
    }
}
