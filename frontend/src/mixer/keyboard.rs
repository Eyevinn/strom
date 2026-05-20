use super::*;

impl MixerEditor {
    /// Handle keyboard shortcuts.
    pub(super) fn handle_keyboard(&mut self, ui: &mut Ui, ctx: &Context) {
        // Ctrl+S = Save mixer state
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save_requested = true;
            self.status = "Saving mixer state...".to_string();
        }

        // Escape = Deselect (close detail panel)
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selection = None;
        }

        // Don't process other shortcuts if a text input has focus
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        // Number keys 1-9, 0 for channel selection
        for (key, ch) in [
            (egui::Key::Num1, 0),
            (egui::Key::Num2, 1),
            (egui::Key::Num3, 2),
            (egui::Key::Num4, 3),
            (egui::Key::Num5, 4),
            (egui::Key::Num6, 5),
            (egui::Key::Num7, 6),
            (egui::Key::Num8, 7),
            (egui::Key::Num9, 8),
            (egui::Key::Num0, 9),
        ] {
            if ui.input(|i| i.key_pressed(key)) && ch < self.channels.len() {
                self.selection = Some(Selection::Channel(ch));
            }
        }

        // Extract selected channel index for channel-specific shortcuts
        let selected_ch = match self.selection {
            Some(Selection::Channel(ch)) => Some(ch),
            _ => None,
        };

        // M = Mute selected channel
        if ui.input(|i| i.key_pressed(egui::Key::M)) {
            if let Some(ch) = selected_ch {
                self.channels[ch].mute = !self.channels[ch].mute;
                self.update_channel_property(ctx, ch, "mute");
            }
        }

        // P = PFL selected channel
        if ui.input(|i| i.key_pressed(egui::Key::P)) {
            if let Some(ch) = selected_ch {
                self.channels[ch].pfl = !self.channels[ch].pfl;
                self.update_channel_property(ctx, ch, "pfl");
            }
        }

        // A = AFL selected channel
        if ui.input(|i| i.key_pressed(egui::Key::A)) {
            if let Some(ch) = selected_ch {
                self.channels[ch].afl = !self.channels[ch].afl;
                self.update_channel_property(ctx, ch, "afl");
            }
        }

        // Up/Down = Adjust fader (1 dB steps in dB space)
        if let Some(ch) = selected_ch {
            let db_step: f64 = 1.0;
            if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                let db = linear_to_db(self.channels[ch].fader as f64) + db_step;
                self.channels[ch].fader = db_to_linear_f32(db as f32).clamp(0.0, 2.0);
                self.update_channel_property(ctx, ch, "fader");
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                let db = linear_to_db(self.channels[ch].fader as f64) - db_step;
                self.channels[ch].fader = db_to_linear_f32(db as f32).clamp(0.0, 2.0);
                self.update_channel_property(ctx, ch, "fader");
            }

            // Left/Right = Adjust pan
            let pan_step = 0.1;
            if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                self.channels[ch].pan = (self.channels[ch].pan - pan_step).max(-1.0);
                self.update_channel_property(ctx, ch, "pan");
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                self.channels[ch].pan = (self.channels[ch].pan + pan_step).min(1.0);
                self.update_channel_property(ctx, ch, "pan");
            }
        }
    }
}
