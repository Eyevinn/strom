use super::*;

impl MixerEditor {
    /// Show the mixer in fullscreen mode.
    pub fn show_fullscreen(&mut self, ui: &mut Ui, ctx: &Context, meter_store: &MeterDataStore) {
        // Check for save result from async task
        if let Some(status) = crate::app::get_local_storage("mixer_save_status") {
            crate::app::remove_local_storage("mixer_save_status");
            if status == "ok" {
                self.status = "Mixer state saved".to_string();
            } else {
                self.error = Some(format!("Save failed: {}", status));
            }
        }

        self.handle_keyboard(ui, ctx);
        self.strip_interacted = false;

        let available_height = ui.available_height();
        let detail_panel_height = if self.selection.is_some() { 220.0 } else { 0.0 };
        let status_bar_height = 30.0;
        let channel_area_height = (available_height
            - BUS_ROW_MIN_HEIGHT
            - detail_panel_height
            - status_bar_height
            - 16.0)
            .max(300.0);

        // Outer vertical scroll wraps the entire mixer
        egui::ScrollArea::vertical()
            .id_salt("mixer_v_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    // ── Row 1: Channel strips ──
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), channel_area_height),
                        egui::Layout::left_to_right(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::horizontal()
                                .id_salt("ch_h_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        for i in 0..self.channels.len() {
                                            let meter_key =
                                                format!("{}:meter:{}", self.block_id, i + 1);
                                            let meter_data =
                                                meter_store.get(&self.flow_id, &meter_key);
                                            self.render_channel_strip(ui, ctx, i, meter_data);
                                            ui.add_space(STRIP_GAP);
                                        }
                                    });
                                });
                        },
                    );

                    ui.separator();

                    // ── Row 2: Bus masters (aux + groups + main) ──
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), BUS_ROW_MIN_HEIGHT),
                        egui::Layout::left_to_right(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::horizontal()
                                .id_salt("bus_h_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        if self.num_aux_buses > 0 {
                                            self.render_aux_masters(ui, ctx, meter_store);
                                            ui.add_space(8.0);
                                            ui.separator();
                                            ui.add_space(8.0);
                                        }

                                        if self.num_groups > 0 {
                                            self.render_group_strips(ui, ctx, meter_store);
                                            ui.add_space(8.0);
                                            ui.separator();
                                            ui.add_space(8.0);
                                        }

                                        self.render_main_strip(ui, ctx, meter_store);
                                    });
                                });
                        },
                    );

                    // ── Row 3: Detail panel (Gate/Comp/EQ) ──
                    if self.selection.is_some() {
                        ui.separator();
                        let detail_resp = ui.scope(|ui| {
                            egui::ScrollArea::horizontal()
                                .id_salt("detail_h_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.render_detail_panel(ui, ctx);
                                });
                        });
                        if ui.input(|i| i.pointer.any_pressed()) {
                            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                                if detail_resp.response.rect.contains(pos) {
                                    self.strip_interacted = true;
                                }
                            }
                        }
                    }

                    // ── Status bar ──
                    ui.separator();
                    let status_resp = ui.horizontal(|ui| {
                        if let Some(error) = &self.error {
                            ui.colored_label(Color32::RED, error);
                        } else if !self.status.is_empty() {
                            ui.label(&self.status);
                        }
                        // Keyboard shortcuts legend
                        ui.label(
                            egui::RichText::new(
                                "1-0: Select ch | M: Mute | P: PFL | Esc: Deselect | Arrows: Fader/Pan",
                            )
                            .small()
                            .color(Color32::from_gray(90)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            match &self.selection {
                                Some(Selection::Channel(ch)) => {
                                    ui.label(format!("Selected: Ch {}", ch + 1));
                                }
                                Some(Selection::Main) => {
                                    ui.label("Selected: Main");
                                }
                                None => {}
                            }
                            ui.checkbox(&mut self.live_updates, "Live");
                            // Fade duration applied to volume/mute updates.
                            // Sent as ramp_ms in PATCH; eliminates zipper noise on
                            // fader drag and matches auto-transition durations on
                            // explicit fades. 20ms is the safe default; > 50ms
                            // switches the backend to a dB-linear curve.
                            ui.add(
                                egui::DragValue::new(&mut self.fade_ms)
                                    .range(0..=5000)
                                    .suffix(" ms")
                                    .speed(1.0),
                            )
                            .on_hover_text(
                                "Fade duration applied to volume and mute changes.\n\
                                 20 ms (default): zipper-free fader drag.\n\
                                 > 50 ms: switches to a dB-linear curve.\n\
                                 Up to 5000 ms for explicit fades / auto-transition match.",
                            );
                            ui.label(
                                egui::RichText::new("Fade")
                                    .small()
                                    .color(Color32::from_gray(120)),
                            );
                            if ui.button(format!("{} Save", egui_phosphor::regular::FLOPPY_DISK)).clicked() {
                                self.save_requested = true;
                                self.status = "Saving mixer state...".to_string();
                            }
                            if ui.button(format!("{} Reset All", egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)).clicked() {
                                self.reset_to_defaults();
                                self.save_requested = true;
                                self.status = "Reset to defaults, saving...".to_string();
                            }
                        });
                    });
                    if ui.input(|i| i.pointer.any_pressed()) {
                        if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                            if status_resp.response.rect.contains(pos) {
                                self.strip_interacted = true;
                            }
                        }
                    }

                    // ── Background click to deselect ──
                    if ui.input(|i| i.pointer.any_pressed()) && !self.strip_interacted {
                        self.selection = None;
                    }
                });
            });
    }

    /// Render a single channel strip.
    pub(super) fn render_channel_strip(
        &mut self,
        ui: &mut Ui,
        ctx: &Context,
        index: usize,
        meter_data: Option<&MeterData>,
    ) {
        let channel_pan = self.channels[index].pan;
        let channel_fader = self.channels[index].fader;
        let channel_mute = self.channels[index].mute;
        let channel_pfl = self.channels[index].pfl;
        let channel_gate = self.channels[index].gate_enabled;
        let channel_comp = self.channels[index].comp_enabled;
        let channel_eq = self.channels[index].eq_enabled;
        let is_selected = self.selection == Some(Selection::Channel(index));

        let frame_color = if is_selected {
            Color32::from_rgb(50, 65, 80)
        } else {
            Color32::from_rgb(38, 38, 42)
        };

        let strip_inner = self.strip_inner();

        let frame_response = egui::Frame::default()
            .fill(frame_color)
            .corner_radius(CornerRadius::same(3))
            .inner_margin(STRIP_MARGIN)
            .show(ui, |ui| {
                ui.set_min_width(strip_inner);
                ui.set_max_width(strip_inner);

                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;

                    // ── Label (double-click to edit) ──
                    if self.editing_label == Some(index) {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.channels[index].label)
                                .desired_width(strip_inner - 4.0)
                                .font(egui::TextStyle::Body)
                                .horizontal_align(egui::Align::Center),
                        );
                        if response.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            self.editing_label = None;
                        }
                        // Auto-focus when first shown
                        response.request_focus();
                    } else {
                        let label_response = ui.add(
                            egui::Label::new(
                                egui::RichText::new(&self.channels[index].label)
                                    .strong()
                                    .size(11.0),
                            )
                            .sense(Sense::click()),
                        );
                        if label_response.double_clicked() {
                            self.editing_label = Some(index);
                        }
                    }

                    // ── H / G / C / E buttons ──
                    let hgce_btn_w = (strip_inner - 10.0) / 4.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;

                        // HPF button
                        let channel_hpf = self.channels[index].hpf_enabled;
                        let hpf_fill = if channel_hpf {
                            Color32::from_rgb(150, 80, 150)
                        } else {
                            Color32::from_rgb(48, 48, 52)
                        };
                        let hpf_text = if channel_hpf {
                            Color32::WHITE
                        } else {
                            Color32::from_gray(120)
                        };
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("H").small().color(hpf_text))
                                    .fill(hpf_fill)
                                    .min_size(Vec2::new(hgce_btn_w, SMALL_BTN_H)),
                            )
                            .clicked()
                        {
                            self.channels[index].hpf_enabled = !self.channels[index].hpf_enabled;
                            self.update_processing_param(ctx, index, "hpf", "enabled");
                        }

                        // G / C / E buttons
                        for (label, enabled, active_color, prop) in [
                            (
                                "G",
                                channel_gate,
                                Color32::from_rgb(0, 150, 0),
                                "gate_enabled",
                            ),
                            (
                                "C",
                                channel_comp,
                                Color32::from_rgb(180, 100, 0),
                                "comp_enabled",
                            ),
                            (
                                "E",
                                channel_eq,
                                Color32::from_rgb(0, 100, 180),
                                "eq_enabled",
                            ),
                        ] {
                            let fill = if enabled {
                                active_color
                            } else {
                                Color32::from_rgb(48, 48, 52)
                            };
                            let text_col = if enabled {
                                Color32::WHITE
                            } else {
                                Color32::from_gray(120)
                            };
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(label).small().color(text_col),
                                    )
                                    .fill(fill)
                                    .min_size(Vec2::new(hgce_btn_w, SMALL_BTN_H)),
                                )
                                .clicked()
                            {
                                match prop {
                                    "gate_enabled" => {
                                        self.channels[index].gate_enabled =
                                            !self.channels[index].gate_enabled
                                    }
                                    "comp_enabled" => {
                                        self.channels[index].comp_enabled =
                                            !self.channels[index].comp_enabled
                                    }
                                    "eq_enabled" => {
                                        self.channels[index].eq_enabled =
                                            !self.channels[index].eq_enabled
                                    }
                                    _ => {}
                                }
                                self.update_channel_property(ctx, index, prop);
                            }
                        }
                    });

                    // ── Aux send knobs ──
                    if self.num_aux_buses > 0 {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 1.0;
                            for aux_idx in 0..self.num_aux_buses.min(MAX_AUX_BUSES) {
                                let response = self.render_knob(ui, index, aux_idx);
                                if response.double_clicked() {
                                    self.bypass_throttle();
                                    self.update_aux_send(ctx, index, aux_idx);
                                } else if response.dragged() {
                                    self.active_control = ActiveControl::AuxSend(index, aux_idx);
                                    self.update_aux_send(ctx, index, aux_idx);
                                } else if response.drag_stopped() {
                                    self.active_control = ActiveControl::None;
                                }
                            }
                        });
                    }

                    // ── Routing buttons (M + group numbers) ──
                    {
                        let num_dest = 1 + self.num_groups;
                        let btn_w =
                            (strip_inner - 4.0 - (num_dest as f32 - 1.0) * 2.0) / num_dest as f32;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            // Main
                            let to_main = self.channels[index].to_main;
                            let fill = if to_main {
                                Color32::from_rgb(70, 110, 70)
                            } else {
                                Color32::from_rgb(48, 48, 52)
                            };
                            let text_col = if to_main {
                                Color32::WHITE
                            } else {
                                Color32::from_gray(100)
                            };
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("M").small().color(text_col),
                                    )
                                    .fill(fill)
                                    .min_size(Vec2::new(btn_w, SMALL_BTN_H)),
                                )
                                .clicked()
                            {
                                self.channels[index].to_main = !to_main;
                                self.update_routing(ctx, index);
                            }
                            // Groups
                            for g in 0..self.num_groups.min(MAX_GROUPS) {
                                let on = self.channels[index].to_grp[g];
                                let fill = if on {
                                    Color32::from_rgb(140, 90, 140)
                                } else {
                                    Color32::from_rgb(48, 48, 52)
                                };
                                let text_col = if on {
                                    Color32::WHITE
                                } else {
                                    Color32::from_gray(100)
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(format!("{}", g + 1))
                                                .small()
                                                .color(text_col),
                                        )
                                        .fill(fill)
                                        .min_size(Vec2::new(btn_w, SMALL_BTN_H)),
                                    )
                                    .clicked()
                                {
                                    self.channels[index].to_grp[g] = !on;
                                    self.update_routing(ctx, index);
                                }
                            }
                        });
                    }

                    // ── LCD display ──
                    let display_text = match &self.active_control {
                        ActiveControl::Fader(ch) if *ch == index => format_db(channel_fader),
                        ActiveControl::AuxSend(ch, aux) if *ch == index => {
                            let lvl = self.channels[index].aux_sends[*aux];
                            format!("A{} {}", aux + 1, format_db(lvl))
                        }
                        ActiveControl::Pan(ch) if *ch == index => format_pan(channel_pan),
                        _ => {
                            let db_str = format_db(channel_fader);
                            let pan_str = format_pan(channel_pan);
                            format!("{} {}", pan_str, db_str)
                        }
                    };
                    self.render_lcd(ui, &display_text, strip_inner - 4.0, LCD_H);

                    // ── Pan knob ──
                    let pan_response = self.render_pan_knob(ui, index);
                    if pan_response.double_clicked() {
                        self.bypass_throttle();
                        self.update_channel_property(ctx, index, "pan");
                    } else if pan_response.dragged() {
                        self.active_control = ActiveControl::Pan(index);
                        self.update_channel_property(ctx, index, "pan");
                    } else if pan_response.drag_stopped() {
                        self.active_control = ActiveControl::None;
                    }

                    ui.add_space(2.0);

                    // ── Fader + meter + scale ──
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), FADER_HEIGHT),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            self.render_stereo_meter(ui, meter_data, FADER_HEIGHT);
                            ui.add_space(1.0);
                            self.render_db_scale(ui, FADER_HEIGHT);
                            ui.add_space(1.0);
                            let fader_response = self.render_fader(ui, index, FADER_HEIGHT);
                            if fader_response.double_clicked() {
                                self.bypass_throttle();
                                self.update_channel_property(ctx, index, "fader");
                            } else if fader_response.dragged() {
                                self.active_control = ActiveControl::Fader(index);
                                self.update_channel_property(ctx, index, "fader");
                            } else if fader_response.drag_stopped() {
                                self.active_control = ActiveControl::None;
                            }
                        },
                    );

                    ui.add_space(2.0);

                    // ── Mute button ──
                    let mute_color = if channel_mute {
                        Color32::from_rgb(200, 50, 50)
                    } else {
                        Color32::from_rgb(55, 55, 60)
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("MUTE").small().color(Color32::WHITE),
                            )
                            .fill(mute_color)
                            .min_size(Vec2::new(strip_inner - 4.0, BTN_H)),
                        )
                        .clicked()
                    {
                        self.channels[index].mute = !self.channels[index].mute;
                        self.update_channel_property(ctx, index, "mute");
                    }

                    // ── PFL button ──
                    let pfl_color = if channel_pfl {
                        Color32::from_rgb(200, 200, 0)
                    } else {
                        Color32::from_rgb(48, 48, 52)
                    };
                    let pfl_text_col = if channel_pfl {
                        Color32::BLACK
                    } else {
                        Color32::from_gray(100)
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("PFL").small().color(pfl_text_col),
                            )
                            .fill(pfl_color)
                            .min_size(Vec2::new(strip_inner - 4.0, BTN_H)),
                        )
                        .clicked()
                    {
                        self.channels[index].pfl = !self.channels[index].pfl;
                        self.update_channel_property(ctx, index, "pfl");
                    }

                    // ── Select button ──
                    let sel_label = if is_selected { "Selected" } else { "Select" };
                    let sel_fill = if is_selected {
                        Color32::from_rgb(50, 100, 200)
                    } else {
                        Color32::from_rgb(48, 48, 52)
                    };
                    let sel_text_col = if is_selected {
                        Color32::WHITE
                    } else {
                        Color32::from_gray(100)
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(sel_label).small().color(sel_text_col),
                            )
                            .fill(sel_fill)
                            .min_size(Vec2::new(strip_inner - 4.0, BTN_H)),
                        )
                        .clicked()
                    {
                        self.selection = Some(Selection::Channel(index));
                    }
                });
            });

        // Detect click on strip background for selection.
        // Use pointer query instead of ui.interact() to avoid stealing
        // double-click events from child widgets (like faders).
        let strip_rect = frame_response.response.rect;
        if ui.input(|i| i.pointer.any_pressed())
            && ui
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|pos| strip_rect.contains(pos))
        {
            self.selection = Some(Selection::Channel(index));
            self.strip_interacted = true;
        }
    }

    /// Render the main/master strip (compact, for bus row).
    pub(super) fn render_main_strip(
        &mut self,
        ui: &mut Ui,
        ctx: &Context,
        meter_store: &MeterDataStore,
    ) {
        let main_meter_key = format!("{}:meter:main", self.block_id);
        let main_meter_data = meter_store.get(&self.flow_id, &main_meter_key);
        let is_selected = self.selection == Some(Selection::Main);

        let frame_color = if is_selected {
            Color32::from_rgb(55, 55, 75)
        } else {
            Color32::from_rgb(45, 45, 55)
        };

        let mut should_select = false;

        let frame_response = egui::Frame::default()
            .fill(frame_color)
            .corner_radius(CornerRadius::same(3))
            .inner_margin(STRIP_MARGIN)
            .show(ui, |ui| {
                ui.set_min_width(BUS_STRIP_INNER);
                ui.set_max_width(BUS_STRIP_INNER);

                let bg_rect = ui.available_rect_before_wrap();
                let bg_response =
                    ui.interact(bg_rect, ui.id().with("main_strip_bg"), Sense::click());
                if bg_response.clicked() {
                    should_select = true;
                }

                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;

                    ui.label(
                        egui::RichText::new("MAIN")
                            .strong()
                            .size(12.0)
                            .color(Color32::from_rgb(200, 200, 255)),
                    );

                    self.render_lcd(
                        ui,
                        &format_db(self.main_fader),
                        BUS_STRIP_INNER - 4.0,
                        LCD_H,
                    );

                    ui.add_space(2.0);

                    // Fader + meter + scale
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), BUS_FADER_HEIGHT),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            self.render_stereo_meter(ui, main_meter_data, BUS_FADER_HEIGHT);
                            ui.add_space(1.0);
                            self.render_db_scale(ui, BUS_FADER_HEIGHT);
                            ui.add_space(1.0);

                            let mut main_fader_db = linear_to_db(self.main_fader as f64) as f32;
                            let (rect, response) = ui.allocate_exact_size(
                                Vec2::new(20.0, BUS_FADER_HEIGHT),
                                Sense::click_and_drag(),
                            );
                            if response.double_clicked() {
                                if (main_fader_db - 0.0).abs() < 0.5 {
                                    self.main_fader = 0.0;
                                    main_fader_db = -60.0;
                                } else {
                                    self.main_fader = 1.0;
                                    main_fader_db = 0.0;
                                }
                                self.bypass_throttle();
                                self.update_main_fader(ctx);
                            } else if response.dragged() {
                                self.active_control = ActiveControl::MainFader;
                                let delta = -response.drag_delta().y;
                                let db_per_pixel = 66.0 / (BUS_FADER_HEIGHT - 10.0);
                                main_fader_db =
                                    (main_fader_db + delta * db_per_pixel).clamp(-60.0, 6.0);
                                self.main_fader = db_to_linear_f32(main_fader_db);
                                self.update_main_fader(ctx);
                            } else if response.drag_stopped() {
                                self.active_control = ActiveControl::None;
                                self.update_main_fader(ctx);
                            }
                            let painter = ui.painter();
                            let track_rect = Rect::from_center_size(
                                rect.center(),
                                Vec2::new(4.0, BUS_FADER_HEIGHT - 10.0),
                            );
                            painter.rect_filled(
                                track_rect,
                                CornerRadius::same(2),
                                Color32::from_gray(55),
                            );
                            let handle_y = db_to_y(main_fader_db, rect.min.y, rect.max.y);
                            let handle_rect = Rect::from_center_size(
                                egui::pos2(rect.center().x, handle_y),
                                Vec2::new(14.0, 30.0),
                            );
                            let handle_color = if response.dragged() {
                                Color32::from_rgb(100, 140, 240)
                            } else if response.hovered() {
                                Color32::from_rgb(190, 190, 200)
                            } else {
                                Color32::from_rgb(155, 155, 165)
                            };
                            painter.rect_filled(handle_rect, CornerRadius::same(3), handle_color);
                            painter.line_segment(
                                [
                                    egui::pos2(handle_rect.left() + 2.0, handle_y),
                                    egui::pos2(handle_rect.right() - 2.0, handle_y),
                                ],
                                Stroke::new(1.5, Color32::from_gray(40)),
                            );
                        },
                    );

                    ui.add_space(2.0);

                    let mute_color = if self.main_mute {
                        Color32::from_rgb(200, 50, 50)
                    } else {
                        Color32::from_rgb(55, 55, 60)
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("MUTE").small().color(Color32::WHITE),
                            )
                            .fill(mute_color)
                            .min_size(Vec2::new(BUS_STRIP_INNER - 4.0, BTN_H)),
                        )
                        .clicked()
                    {
                        self.main_mute = !self.main_mute;
                        self.update_main_mute(ctx);
                    }

                    // ── Select button ──
                    let sel_label = if is_selected { "Selected" } else { "Select" };
                    let sel_fill = if is_selected {
                        Color32::from_rgb(50, 100, 200)
                    } else {
                        Color32::from_rgb(48, 48, 52)
                    };
                    let sel_text_col = if is_selected {
                        Color32::WHITE
                    } else {
                        Color32::from_gray(100)
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(sel_label).small().color(sel_text_col),
                            )
                            .fill(sel_fill)
                            .min_size(Vec2::new(BUS_STRIP_INNER - 4.0, BTN_H)),
                        )
                        .clicked()
                    {
                        self.selection = Some(Selection::Main);
                    }
                });
            });

        if should_select {
            self.selection = Some(Selection::Main);
            self.strip_interacted = true;
        }
        // Track interaction for background-click deselection
        if ui.input(|i| i.pointer.any_pressed())
            && ui
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|pos| frame_response.response.rect.contains(pos))
        {
            self.strip_interacted = true;
        }
    }

    /// Render the group strips section (compact, for bus row).
    pub(super) fn render_group_strips(
        &mut self,
        ui: &mut Ui,
        ctx: &Context,
        meter_store: &MeterDataStore,
    ) {
        for sg_idx in 0..self.num_groups.min(MAX_GROUPS) {
            while self.groups.len() <= sg_idx {
                self.groups.push(GroupStrip::new(self.groups.len()));
            }

            let meter_key = format!("{}:meter:group{}", self.block_id, sg_idx + 1);
            let meter_data = meter_store.get(&self.flow_id, &meter_key);

            let grp_frame = egui::Frame::default()
                .fill(Color32::from_rgb(42, 38, 48))
                .corner_radius(CornerRadius::same(3))
                .inner_margin(STRIP_MARGIN)
                .show(ui, |ui| {
                    ui.set_min_width(BUS_STRIP_INNER);
                    ui.set_max_width(BUS_STRIP_INNER);

                    ui.vertical_centered(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;

                        ui.label(
                            egui::RichText::new(format!("GRP{}", sg_idx + 1))
                                .strong()
                                .size(11.0)
                                .color(Color32::from_rgb(200, 150, 200)),
                        );

                        self.render_lcd(
                            ui,
                            &format_db(self.groups[sg_idx].fader),
                            BUS_STRIP_INNER - 4.0,
                            LCD_H,
                        );

                        ui.add_space(2.0);

                        ui.allocate_ui_with_layout(
                            Vec2::new(ui.available_width(), BUS_FADER_HEIGHT),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                self.render_stereo_meter(ui, meter_data, BUS_FADER_HEIGHT);
                                ui.add_space(1.0);
                                self.render_db_scale(ui, BUS_FADER_HEIGHT);
                                ui.add_space(1.0);
                                let fader_response =
                                    self.render_group_fader(ui, sg_idx, BUS_FADER_HEIGHT);
                                if fader_response.double_clicked() {
                                    self.bypass_throttle();
                                    self.update_group_fader(ctx, sg_idx);
                                } else if fader_response.dragged() {
                                    self.active_control = ActiveControl::GroupFader(sg_idx);
                                    self.update_group_fader(ctx, sg_idx);
                                } else if fader_response.drag_stopped() {
                                    self.active_control = ActiveControl::None;
                                }
                            },
                        );

                        ui.add_space(2.0);

                        let mute = self.groups[sg_idx].mute;
                        let mute_color = if mute {
                            Color32::from_rgb(200, 50, 50)
                        } else {
                            Color32::from_rgb(55, 55, 60)
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("MUTE").small().color(Color32::WHITE),
                                )
                                .fill(mute_color)
                                .min_size(Vec2::new(BUS_STRIP_INNER - 4.0, BTN_H)),
                            )
                            .clicked()
                        {
                            self.groups[sg_idx].mute = !self.groups[sg_idx].mute;
                            self.update_group_mute(ctx, sg_idx);
                        }
                    });
                });
            if ui.input(|i| i.pointer.any_pressed())
                && ui
                    .input(|i| i.pointer.interact_pos())
                    .is_some_and(|pos| grp_frame.response.rect.contains(pos))
            {
                self.strip_interacted = true;
            }

            ui.add_space(STRIP_GAP);
        }
    }

    /// Render the aux master section (compact, for bus row).
    pub(super) fn render_aux_masters(
        &mut self,
        ui: &mut Ui,
        ctx: &Context,
        meter_store: &MeterDataStore,
    ) {
        for aux_idx in 0..self.num_aux_buses.min(MAX_AUX_BUSES) {
            while self.aux_masters.len() <= aux_idx {
                self.aux_masters
                    .push(AuxMaster::new(self.aux_masters.len()));
            }

            let meter_key = format!("{}:meter:aux{}", self.block_id, aux_idx + 1);
            let meter_data = meter_store.get(&self.flow_id, &meter_key);

            let aux_frame = egui::Frame::default()
                .fill(Color32::from_rgb(38, 42, 50))
                .corner_radius(CornerRadius::same(3))
                .inner_margin(STRIP_MARGIN)
                .show(ui, |ui| {
                    ui.set_min_width(BUS_STRIP_INNER);
                    ui.set_max_width(BUS_STRIP_INNER);

                    ui.vertical_centered(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;

                        ui.label(
                            egui::RichText::new(format!("AUX{}", aux_idx + 1))
                                .strong()
                                .size(11.0)
                                .color(Color32::from_rgb(150, 200, 255)),
                        );

                        self.render_lcd(
                            ui,
                            &format_db(self.aux_masters[aux_idx].fader),
                            BUS_STRIP_INNER - 4.0,
                            LCD_H,
                        );

                        ui.add_space(2.0);

                        ui.allocate_ui_with_layout(
                            Vec2::new(ui.available_width(), BUS_FADER_HEIGHT),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                self.render_stereo_meter(ui, meter_data, BUS_FADER_HEIGHT);
                                ui.add_space(1.0);
                                self.render_db_scale(ui, BUS_FADER_HEIGHT);
                                ui.add_space(1.0);
                                let fader_response =
                                    self.render_aux_master_fader(ui, aux_idx, BUS_FADER_HEIGHT);
                                if fader_response.double_clicked() {
                                    self.bypass_throttle();
                                    self.update_aux_master_fader(ctx, aux_idx);
                                } else if fader_response.dragged() {
                                    self.active_control = ActiveControl::AuxMasterFader(aux_idx);
                                    self.update_aux_master_fader(ctx, aux_idx);
                                } else if fader_response.drag_stopped() {
                                    self.active_control = ActiveControl::None;
                                }
                            },
                        );

                        ui.add_space(2.0);

                        let mute = self.aux_masters[aux_idx].mute;
                        let mute_color = if mute {
                            Color32::from_rgb(200, 50, 50)
                        } else {
                            Color32::from_rgb(55, 55, 60)
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("MUTE").small().color(Color32::WHITE),
                                )
                                .fill(mute_color)
                                .min_size(Vec2::new(BUS_STRIP_INNER - 4.0, BTN_H)),
                            )
                            .clicked()
                        {
                            self.aux_masters[aux_idx].mute = !self.aux_masters[aux_idx].mute;
                            self.update_aux_master_mute(ctx, aux_idx);
                        }
                    });
                });
            if ui.input(|i| i.pointer.any_pressed())
                && ui
                    .input(|i| i.pointer.interact_pos())
                    .is_some_and(|pos| aux_frame.response.rect.contains(pos))
            {
                self.strip_interacted = true;
            }

            ui.add_space(STRIP_GAP);
        }
    }
}
