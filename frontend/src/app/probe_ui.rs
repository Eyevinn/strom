//! Buffer age probe UI rendered in the properties panel.
//!
//! Header row: single Probe/Stop button.
//! Detail section: per-pad values rendered vertically below the header.

use super::*;

impl StromApp {
    /// Render probe button in the properties panel header row.
    /// Shows "Probe" to activate or "Stop" to deactivate all probes.
    pub(crate) fn render_probe_button(&mut self, ui: &mut egui::Ui, element_id: &str) {
        let flow_id = match self.selected_flow_id {
            Some(id) => id,
            None => return,
        };

        let probes = self.buffer_age_data.get_probes_for_element(element_id);

        if probes.is_empty() {
            if ui
                .small_button(format!("{} Probe", egui_phosphor::regular::CLOCK))
                .on_hover_text("Measure buffer age on this element")
                .clicked()
            {
                let api = self.api.clone();
                let eid = element_id.to_string();
                let tx = self.channels.sender();
                let ctx = ui.ctx().clone();
                crate::app::spawn_task(async move {
                    match api.activate_probe(&flow_id, &eid, Some(1), Some(300)).await {
                        Ok(resp) => {
                            tracing::info!("Probe {} activated on {}", resp.probe_id, eid);
                        }
                        Err(e) => {
                            tracing::error!("Failed to activate probe on {}: {}", eid, e);
                            let _ = tx.send(crate::state::AppMessage::FlowOperationError(format!(
                                "Probe failed: {}",
                                e
                            )));
                        }
                    }
                    ctx.request_repaint();
                });
            }
        } else {
            let probe_ids: Vec<String> = probes.iter().map(|p| p.probe_id.clone()).collect();
            if ui
                .small_button(format!("{} Stop", egui_phosphor::regular::STOP))
                .on_hover_text("Stop all buffer age probes on this element")
                .clicked()
            {
                let api = self.api.clone();
                let ctx = ui.ctx().clone();
                crate::app::spawn_task(async move {
                    for pid in probe_ids {
                        let _ = api.deactivate_probe(&flow_id, &pid).await;
                    }
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Render per-pad probe values vertically. Call after the header separator.
    /// Returns true if any probes were rendered (so callers can add spacing).
    pub(crate) fn render_probe_details(&self, ui: &mut egui::Ui, element_id: &str) -> bool {
        let mut probes: Vec<(String, u64, u64, u64)> = self
            .buffer_age_data
            .get_probes_for_element(element_id)
            .iter()
            .map(|p| {
                (
                    p.pad_name.clone(),
                    p.current_age_ms,
                    p.avg_age_ms(),
                    p.max_age_ms,
                )
            })
            .collect();

        // Probes come from a HashMap, so iteration order is undefined. Sort by
        // pad name so labels render in a stable, intuitive grouping (A0..An,
        // PGM*, V0..Vn).
        probes.sort_by(|a, b| a.0.cmp(&b.0));

        if probes.is_empty() {
            return false;
        }

        ui.add_space(2.0);
        egui::Grid::new(format!("probe_grid_{}", element_id))
            .num_columns(3)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                // Header
                ui.label(egui::RichText::new("Pad").weak().small());
                ui.label(egui::RichText::new("Age").weak().small());
                ui.label(egui::RichText::new("Avg / Max").weak().small());
                ui.end_row();

                for (pad_name, current_ms, avg_ms, max_ms) in &probes {
                    ui.label(egui::RichText::new(pad_name).small().monospace());
                    ui.label(
                        egui::RichText::new(format!("{}ms", current_ms))
                            .color(
                                if *current_ms > strom_types::BUFFER_AGE_WARNING_THRESHOLD_MS {
                                    egui::Color32::from_rgb(255, 165, 0)
                                } else {
                                    egui::Color32::from_rgb(100, 200, 100)
                                },
                            )
                            .small()
                            .strong()
                            .monospace(),
                    );
                    ui.label(
                        egui::RichText::new(format!("{} / {}ms", avg_ms, max_ms))
                            .small()
                            .weak()
                            .monospace(),
                    );
                    ui.end_row();
                }
            });
        ui.add_space(2.0);
        ui.separator();

        true
    }
}
