use super::*;

/// Fixed minimum height for processing sections so all columns are equal height.
const SECTION_MIN_HEIGHT: f32 = 140.0;

/// Build a styled frame for a processing section.
/// Enabled sections get a tinted background and accent-colored border.
/// Disabled sections get a dark background with a subtle border.
fn section_frame(color: Color32, enabled: bool) -> egui::Frame {
    let base = if enabled {
        egui::Frame::NONE
            .fill(Color32::from_rgb(
                25 + color.r() / 5,
                25 + color.g() / 5,
                30 + color.b() / 5,
            ))
            .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.6)))
    } else {
        egui::Frame::NONE
            .fill(Color32::from_rgb(25, 25, 28))
            .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(40, 40, 44)))
    };
    base.corner_radius(4.0).inner_margin(6.0)
}

/// Render a full-width toggle button for a processing section header.
/// Returns true if the button was clicked (toggled).
fn section_toggle(ui: &mut Ui, name: &str, color: Color32, enabled: bool) -> bool {
    let (text, fill) = if enabled {
        (
            egui::RichText::new(format!("{} - Enabled", name))
                .strong()
                .color(Color32::WHITE),
            color.gamma_multiply(0.8),
        )
    } else {
        (
            egui::RichText::new(format!("{} - Disabled", name))
                .color(Color32::from_rgb(140, 140, 140)),
            Color32::from_rgb(45, 45, 50),
        )
    };

    let btn = egui::Button::new(text).fill(fill).corner_radius(3.0);
    let width = ui.available_width().clamp(140.0, 250.0);
    ui.add_sized([width, 0.0], btn).clicked()
}

impl MixerEditor {
    /// Render the detail panel for the current selection.
    pub(super) fn render_detail_panel(&mut self, ui: &mut Ui, ctx: &Context) {
        match self.selection {
            Some(Selection::Channel(index)) if index < self.channels.len() => {
                self.render_channel_detail_panel(ui, ctx, index);
            }
            Some(Selection::Channel(_)) => {
                self.selection = None;
            }
            Some(Selection::Main) => {
                self.render_main_detail_panel(ui, ctx);
            }
            None => {}
        }
    }

    /// Render the detail panel for a selected channel (HPF/Gate/Comp/EQ).
    pub(super) fn render_channel_detail_panel(&mut self, ui: &mut Ui, ctx: &Context, index: usize) {
        let ch_num = index + 1;
        let label = &self.channels[index].label;
        let header = if *label == format!("Ch {}", ch_num) {
            format!("Channel {} - Processing", ch_num)
        } else {
            format!("{} (Ch {}) - Processing", label, ch_num)
        };

        egui::Frame::default()
            .fill(Color32::from_rgb(35, 35, 40))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(header).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(egui_phosphor::regular::X)
                            .on_hover_text("Close")
                            .clicked()
                        {
                            self.selection = None;
                        }
                    });
                });

                ui.add_space(8.0);

                egui::Grid::new(format!("ch_processing_{}", index))
                    .num_columns(5)
                    .spacing([8.0, 0.0])
                    .show(ui, |ui| {
                        self.render_gain_section(ui, ctx, index);
                        self.render_hpf_section(ui, ctx, index);
                        self.render_gate_section(ui, ctx, index);
                        self.render_comp_section(ui, ctx, index);
                        self.render_eq_section(ui, ctx, index);
                        ui.end_row();
                    });
            });
    }

    /// Render the detail panel for the main bus (Comp/EQ/Limiter).
    pub(super) fn render_main_detail_panel(&mut self, ui: &mut Ui, ctx: &Context) {
        egui::Frame::default()
            .fill(Color32::from_rgb(35, 35, 40))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Main - Processing")
                            .strong()
                            .color(Color32::from_rgb(200, 200, 255)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(egui_phosphor::regular::X)
                            .on_hover_text("Close")
                            .clicked()
                        {
                            self.selection = None;
                        }
                    });
                });

                ui.add_space(8.0);

                egui::Grid::new("main_processing")
                    .num_columns(3)
                    .spacing([8.0, 0.0])
                    .show(ui, |ui| {
                        self.render_main_comp_section(ui, ctx);
                        self.render_main_eq_section(ui, ctx);
                        self.render_main_limiter_section(ui, ctx);
                        ui.end_row();
                    });
            });
    }

    // ---- Main bus sections ----

    pub(super) fn render_main_comp_section(&mut self, ui: &mut Ui, ctx: &Context) {
        let color = Color32::from_rgb(180, 100, 0);
        let enabled = self.main_comp_enabled;

        section_frame(color, enabled).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                if section_toggle(ui, "Compressor", color, enabled) {
                    self.main_comp_enabled = !self.main_comp_enabled;
                    self.update_main_processing_param(ctx, "comp", "enabled");
                }

                ui.add_space(4.0);
                if !enabled {
                    ui.disable();
                }

                egui::Grid::new("main_comp_grid")
                    .num_columns(4)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Threshold:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.main_comp_threshold)
                                    .range(-60.0..=0.0)
                                    .suffix(" dB")
                                    .speed(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_main_processing_param(ctx, "comp", "threshold");
                        }
                        ui.label("Ratio:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.main_comp_ratio)
                                    .range(1.0..=20.0)
                                    .suffix(":1")
                                    .speed(0.1)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_main_processing_param(ctx, "comp", "ratio");
                        }
                        ui.end_row();

                        ui.label("Attack:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.main_comp_attack)
                                    .range(0.1..=200.0)
                                    .suffix(" ms")
                                    .speed(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_main_processing_param(ctx, "comp", "attack");
                        }
                        ui.label("Release:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.main_comp_release)
                                    .range(10.0..=1000.0)
                                    .suffix(" ms")
                                    .speed(1.0)
                                    .fixed_decimals(0),
                            )
                            .changed()
                        {
                            self.update_main_processing_param(ctx, "comp", "release");
                        }
                        ui.end_row();

                        ui.label("Makeup:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.main_comp_makeup)
                                    .range(0.0..=24.0)
                                    .suffix(" dB")
                                    .speed(0.2)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_main_processing_param(ctx, "comp", "makeup");
                        }
                        ui.label("Knee:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.main_comp_knee)
                                    .range(-24.0..=0.0)
                                    .suffix(" dB")
                                    .speed(0.2)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_main_processing_param(ctx, "comp", "knee");
                        }
                        ui.end_row();
                    });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.main_comp_enabled = false;
                    self.main_comp_threshold = DEFAULT_COMP_THRESHOLD;
                    self.main_comp_ratio = DEFAULT_COMP_RATIO;
                    self.main_comp_attack = DEFAULT_COMP_ATTACK;
                    self.main_comp_release = DEFAULT_COMP_RELEASE;
                    self.main_comp_makeup = DEFAULT_COMP_MAKEUP;
                    self.main_comp_knee = DEFAULT_COMP_KNEE;
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "comp", "enabled");
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "comp", "threshold");
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "comp", "ratio");
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "comp", "attack");
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "comp", "release");
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "comp", "makeup");
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "comp", "knee");
                }
            });
        });
    }

    pub(super) fn render_main_eq_section(&mut self, ui: &mut Ui, ctx: &Context) {
        let color = Color32::from_rgb(0, 100, 180);
        let enabled = self.main_eq_enabled;

        section_frame(color, enabled).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                if section_toggle(ui, "EQ", color, enabled) {
                    self.main_eq_enabled = !self.main_eq_enabled;
                    self.update_main_processing_param(ctx, "eq", "enabled");
                }

                ui.add_space(4.0);
                if !enabled {
                    ui.disable();
                }

                let band_names = ["Low", "Lo-Mid", "Hi-Mid", "High"];

                ui.horizontal(|ui| {
                    for (band, name) in band_names.iter().enumerate() {
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(*name).small());

                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.main_eq_bands[band].0)
                                        .range(20.0..=20000.0)
                                        .suffix(" Hz")
                                        .speed(10.0)
                                        .fixed_decimals(0),
                                )
                                .changed()
                            {
                                self.update_main_eq_param(ctx, band, "freq");
                            }

                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.main_eq_bands[band].1)
                                        .range(-15.0..=15.0)
                                        .suffix(" dB")
                                        .speed(0.1)
                                        .fixed_decimals(1),
                                )
                                .changed()
                            {
                                self.update_main_eq_param(ctx, band, "gain");
                            }

                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.main_eq_bands[band].2)
                                        .range(0.1..=10.0)
                                        .prefix("Q ")
                                        .speed(0.05)
                                        .fixed_decimals(2),
                                )
                                .changed()
                            {
                                self.update_main_eq_param(ctx, band, "q");
                            }
                        });

                        if band < 3 {
                            ui.add_space(8.0);
                        }
                    }
                });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.main_eq_enabled = false;
                    self.main_eq_bands = DEFAULT_EQ_BANDS;
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "eq", "enabled");
                    for band in 0..4 {
                        self.bypass_throttle();
                        self.update_main_eq_param(ctx, band, "freq");
                        self.bypass_throttle();
                        self.update_main_eq_param(ctx, band, "gain");
                        self.bypass_throttle();
                        self.update_main_eq_param(ctx, band, "q");
                    }
                }
            });
        });
    }

    pub(super) fn render_main_limiter_section(&mut self, ui: &mut Ui, ctx: &Context) {
        let color = Color32::from_rgb(200, 60, 60);
        let enabled = self.main_limiter_enabled;

        section_frame(color, enabled).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                if section_toggle(ui, "Limiter", color, enabled) {
                    self.main_limiter_enabled = !self.main_limiter_enabled;
                    self.update_main_processing_param(ctx, "limiter", "enabled");
                }

                ui.add_space(4.0);
                if !enabled {
                    ui.disable();
                }

                ui.horizontal(|ui| {
                    ui.label("Threshold:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.main_limiter_threshold)
                                .range(-20.0..=0.0)
                                .suffix(" dB")
                                .speed(0.2)
                                .fixed_decimals(1),
                        )
                        .changed()
                    {
                        self.update_main_processing_param(ctx, "limiter", "threshold");
                    }
                });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.main_limiter_enabled = false;
                    self.main_limiter_threshold = DEFAULT_LIMITER_THRESHOLD;
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "limiter", "enabled");
                    self.bypass_throttle();
                    self.update_main_processing_param(ctx, "limiter", "threshold");
                }
            });
        });
    }

    // ---- Channel sections ----

    pub(super) fn render_gain_section(&mut self, ui: &mut Ui, ctx: &Context, index: usize) {
        let color = Color32::from_rgb(180, 160, 60);
        let active = (self.channels[index].gain).abs() > f32::EPSILON;

        section_frame(color, active).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                ui.label(egui::RichText::new("Gain").strong().color(if active {
                    color
                } else {
                    Color32::GRAY
                }));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Gain:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.channels[index].gain)
                                .range(-20.0..=20.0)
                                .suffix(" dB")
                                .speed(0.2)
                                .fixed_decimals(1),
                        )
                        .changed()
                    {
                        self.update_channel_property(ctx, index, "gain");
                    }
                });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.channels[index].gain = DEFAULT_GAIN;
                    self.update_channel_property(ctx, index, "gain");
                }
            });
        });
    }

    pub(super) fn render_hpf_section(&mut self, ui: &mut Ui, ctx: &Context, index: usize) {
        let color = Color32::from_rgb(150, 80, 150);
        let enabled = self.channels[index].hpf_enabled;

        section_frame(color, enabled).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                if section_toggle(ui, "HPF", color, enabled) {
                    self.channels[index].hpf_enabled = !self.channels[index].hpf_enabled;
                    self.update_processing_param(ctx, index, "hpf", "enabled");
                }

                ui.add_space(4.0);
                if !enabled {
                    ui.disable();
                }

                ui.horizontal(|ui| {
                    ui.label("Cutoff:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.channels[index].hpf_freq)
                                .range(20.0..=500.0)
                                .suffix(" Hz")
                                .speed(1.0)
                                .fixed_decimals(0),
                        )
                        .changed()
                    {
                        self.update_processing_param(ctx, index, "hpf", "freq");
                    }
                });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.channels[index].hpf_enabled = false;
                    self.channels[index].hpf_freq = DEFAULT_HPF_FREQ;
                    self.update_processing_param(ctx, index, "hpf", "enabled");
                }
            });
        });
    }

    pub(super) fn render_gate_section(&mut self, ui: &mut Ui, ctx: &Context, index: usize) {
        let color = Color32::from_rgb(0, 150, 0);
        let enabled = self.channels[index].gate_enabled;

        section_frame(color, enabled).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                if section_toggle(ui, "Gate", color, enabled) {
                    self.channels[index].gate_enabled = !self.channels[index].gate_enabled;
                    self.update_channel_property(ctx, index, "gate_enabled");
                }

                ui.add_space(4.0);
                if !enabled {
                    ui.disable();
                }

                ui.horizontal(|ui| {
                    ui.label("Threshold:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.channels[index].gate_threshold)
                                .range(-60.0..=0.0)
                                .suffix(" dB")
                                .speed(0.5)
                                .fixed_decimals(1),
                        )
                        .changed()
                    {
                        self.update_processing_param(ctx, index, "gate", "threshold");
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Attack:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.channels[index].gate_attack)
                                .range(0.1..=200.0)
                                .suffix(" ms")
                                .speed(0.5)
                                .fixed_decimals(1),
                        )
                        .changed()
                    {
                        self.update_processing_param(ctx, index, "gate", "attack");
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Release:");
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.channels[index].gate_release)
                                .range(10.0..=1000.0)
                                .suffix(" ms")
                                .speed(1.0)
                                .fixed_decimals(0),
                        )
                        .changed()
                    {
                        self.update_processing_param(ctx, index, "gate", "release");
                    }
                });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.channels[index].gate_enabled = false;
                    self.channels[index].gate_threshold = DEFAULT_GATE_THRESHOLD;
                    self.channels[index].gate_attack = DEFAULT_GATE_ATTACK;
                    self.channels[index].gate_release = DEFAULT_GATE_RELEASE;
                    self.bypass_throttle();
                    self.update_channel_property(ctx, index, "gate_enabled");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "gate", "threshold");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "gate", "attack");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "gate", "release");
                }
            });
        });
    }

    pub(super) fn render_comp_section(&mut self, ui: &mut Ui, ctx: &Context, index: usize) {
        let color = Color32::from_rgb(180, 100, 0);
        let enabled = self.channels[index].comp_enabled;

        section_frame(color, enabled).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                if section_toggle(ui, "Compressor", color, enabled) {
                    self.channels[index].comp_enabled = !self.channels[index].comp_enabled;
                    self.update_channel_property(ctx, index, "comp_enabled");
                }

                ui.add_space(4.0);
                if !enabled {
                    ui.disable();
                }

                egui::Grid::new(format!("ch_comp_grid_{}", index))
                    .num_columns(4)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Threshold:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.channels[index].comp_threshold)
                                    .range(-60.0..=0.0)
                                    .suffix(" dB")
                                    .speed(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_processing_param(ctx, index, "comp", "threshold");
                        }
                        ui.label("Ratio:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.channels[index].comp_ratio)
                                    .range(1.0..=20.0)
                                    .suffix(":1")
                                    .speed(0.1)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_processing_param(ctx, index, "comp", "ratio");
                        }
                        ui.end_row();

                        ui.label("Attack:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.channels[index].comp_attack)
                                    .range(0.1..=200.0)
                                    .suffix(" ms")
                                    .speed(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_processing_param(ctx, index, "comp", "attack");
                        }
                        ui.label("Release:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.channels[index].comp_release)
                                    .range(10.0..=1000.0)
                                    .suffix(" ms")
                                    .speed(1.0)
                                    .fixed_decimals(0),
                            )
                            .changed()
                        {
                            self.update_processing_param(ctx, index, "comp", "release");
                        }
                        ui.end_row();

                        ui.label("Makeup:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.channels[index].comp_makeup)
                                    .range(0.0..=24.0)
                                    .suffix(" dB")
                                    .speed(0.2)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_processing_param(ctx, index, "comp", "makeup");
                        }
                        ui.label("Knee:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.channels[index].comp_knee)
                                    .range(-24.0..=0.0)
                                    .suffix(" dB")
                                    .speed(0.2)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            self.update_processing_param(ctx, index, "comp", "knee");
                        }
                        ui.end_row();
                    });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.channels[index].comp_enabled = false;
                    self.channels[index].comp_threshold = DEFAULT_COMP_THRESHOLD;
                    self.channels[index].comp_ratio = DEFAULT_COMP_RATIO;
                    self.channels[index].comp_attack = DEFAULT_COMP_ATTACK;
                    self.channels[index].comp_release = DEFAULT_COMP_RELEASE;
                    self.channels[index].comp_makeup = DEFAULT_COMP_MAKEUP;
                    self.channels[index].comp_knee = DEFAULT_COMP_KNEE;
                    self.bypass_throttle();
                    self.update_channel_property(ctx, index, "comp_enabled");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "comp", "threshold");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "comp", "ratio");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "comp", "attack");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "comp", "release");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "comp", "makeup");
                    self.bypass_throttle();
                    self.update_processing_param(ctx, index, "comp", "knee");
                }
            });
        });
    }

    pub(super) fn render_eq_section(&mut self, ui: &mut Ui, ctx: &Context, index: usize) {
        let color = Color32::from_rgb(0, 100, 180);
        let enabled = self.channels[index].eq_enabled;

        section_frame(color, enabled).show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_min_height(SECTION_MIN_HEIGHT);
                if section_toggle(ui, "EQ", color, enabled) {
                    self.channels[index].eq_enabled = !self.channels[index].eq_enabled;
                    self.update_channel_property(ctx, index, "eq_enabled");
                }

                ui.add_space(4.0);
                if !enabled {
                    ui.disable();
                }

                let band_names = ["Low", "Lo-Mid", "Hi-Mid", "High"];

                ui.horizontal(|ui| {
                    for (band, name) in band_names.iter().enumerate() {
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(*name).small());

                            if ui
                                .add(
                                    egui::DragValue::new(
                                        &mut self.channels[index].eq_bands[band].0,
                                    )
                                    .range(20.0..=20000.0)
                                    .suffix(" Hz")
                                    .speed(10.0)
                                    .fixed_decimals(0),
                                )
                                .changed()
                            {
                                self.update_eq_param(ctx, index, band, "freq");
                            }

                            if ui
                                .add(
                                    egui::DragValue::new(
                                        &mut self.channels[index].eq_bands[band].1,
                                    )
                                    .range(-15.0..=15.0)
                                    .suffix(" dB")
                                    .speed(0.1)
                                    .fixed_decimals(1),
                                )
                                .changed()
                            {
                                self.update_eq_param(ctx, index, band, "gain");
                            }

                            if ui
                                .add(
                                    egui::DragValue::new(
                                        &mut self.channels[index].eq_bands[band].2,
                                    )
                                    .range(0.1..=10.0)
                                    .prefix("Q ")
                                    .speed(0.05)
                                    .fixed_decimals(2),
                                )
                                .changed()
                            {
                                self.update_eq_param(ctx, index, band, "q");
                            }
                        });

                        if band < 3 {
                            ui.add_space(8.0);
                        }
                    }
                });

                ui.add_space(4.0);
                if ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset")
                    .clicked()
                {
                    self.channels[index].eq_enabled = false;
                    self.channels[index].eq_bands = DEFAULT_EQ_BANDS;
                    self.bypass_throttle();
                    self.update_channel_property(ctx, index, "eq_enabled");
                    for band in 0..4 {
                        self.bypass_throttle();
                        self.update_eq_param(ctx, index, band, "freq");
                        self.bypass_throttle();
                        self.update_eq_param(ctx, index, band, "gain");
                        self.bypass_throttle();
                        self.update_eq_param(ctx, index, band, "q");
                    }
                }
            });
        });
    }
}
