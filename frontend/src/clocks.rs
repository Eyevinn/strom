//! Clocks page for PTP synchronization monitoring.
//!
//! This page displays PTP clock statistics grouped by domain, since PTP clocks
//! are shared resources - one clock instance per domain regardless of how many
//! flows use it.

use egui::{Color32, RichText, Ui};
use std::collections::HashMap;

use crate::list_navigator::{list_navigator, ListItem};
use crate::ptp_monitor::{PtpStatsData, PtpStatsStore};

/// Clocks page state.
pub struct ClocksPage {
    /// Selected domain for detailed view
    selected_domain: Option<u8>,
}

impl ClocksPage {
    pub fn new() -> Self {
        Self {
            selected_domain: None,
        }
    }

    /// Render the clocks page.
    pub fn render(
        &mut self,
        ui: &mut Ui,
        ptp_stats: &PtpStatsStore,
        flows: &[strom_types::Flow],
        system_clock: Option<&strom_types::api::SystemClockInfo>,
        system_clock_unsupported: bool,
    ) {
        let domain_info = self.collect_domain_info(ptp_stats, flows);
        let ntp_flows: Vec<&strom_types::Flow> = flows
            .iter()
            .filter(|f| f.properties.clock_type == strom_types::flow::GStreamerClockType::Ntp)
            .collect();

        // System clock section (always visible at top).
        // NTP is rendered before PTP: PTP's inner split-pane layout with graphs
        // tends to stretch vertically, and placing NTP above it keeps the short
        // NTP cards visible without scrolling.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_system_clock_panel(ui, system_clock, system_clock_unsupported);
                ui.add_space(12.0);

                ui.separator();
                ui.heading("NTP Clocks");
                ui.add_space(6.0);
                if ntp_flows.is_empty() {
                    ui.label("No NTP clocks configured.");
                } else {
                    render_ntp_section(ui, &ntp_flows);
                }

                ui.add_space(12.0);
                ui.separator();
                ui.heading("PTP Domains");
                ui.add_space(6.0);
                if domain_info.is_empty() {
                    ui.label(
                        "No PTP clocks configured. Set a flow's clock type to PTP to see stats.",
                    );
                } else {
                    self.render_ptp_section(ui, ptp_stats, flows, &domain_info);
                }
            });
    }

    /// Render the PTP domain list + details inline (no longer consumes the whole page).
    fn render_ptp_section(
        &mut self,
        ui: &mut Ui,
        ptp_stats: &PtpStatsStore,
        flows: &[strom_types::Flow],
        domain_info: &HashMap<u8, DomainInfo>,
    ) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(320.0);
                self.render_domain_list(ui, domain_info);
            });
            ui.separator();
            ui.vertical(|ui| {
                self.render_details_panel(ui, ptp_stats, flows, domain_info);
            });
        });
    }

    /// Collect information about each PTP domain.
    fn collect_domain_info(
        &self,
        ptp_stats: &PtpStatsStore,
        flows: &[strom_types::Flow],
    ) -> HashMap<u8, DomainInfo> {
        let mut domains: HashMap<u8, DomainInfo> = HashMap::new();

        // First, find all flows configured for PTP
        for flow in flows {
            if flow.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp {
                let domain = flow.properties.ptp_domain.unwrap_or(0);
                let info = domains.entry(domain).or_insert_with(|| DomainInfo {
                    domain,
                    flow_count: 0,
                    stats: None,
                });
                info.flow_count += 1;

                // Get stats from this flow if available
                if info.stats.is_none() {
                    if let Some(history) = ptp_stats.get_history(&flow.id) {
                        info.stats = history.latest().cloned();
                    }
                }
            }
        }

        domains
    }

    fn render_domain_list(&mut self, ui: &mut Ui, domain_info: &HashMap<u8, DomainInfo>) {
        ui.label(format!("{} PTP domain(s) configured", domain_info.len()));
        ui.separator();

        // Sort domains by number and prepare data
        let mut domains: Vec<_> = domain_info.values().collect();
        domains.sort_by_key(|d| d.domain);

        // Build item data with owned strings
        let items_data: Vec<_> = domains
            .iter()
            .map(|info| {
                let id = info.domain.to_string();
                let label = format!("Domain {}", info.domain);

                // Build secondary text with stats
                let secondary = if let Some(ref stats) = info.stats {
                    let mut parts = vec![format!("{} flow(s)", info.flow_count)];
                    if let Some(offset_ns) = stats.clock_offset_ns {
                        parts.push(format!("Offset: {:.1}us", offset_ns as f64 / 1000.0));
                    }
                    if let Some(r_squared) = stats.r_squared {
                        parts.push(format!("R²: {:.4}", r_squared));
                    }
                    parts.join(" | ")
                } else {
                    format!("{} flow(s)", info.flow_count)
                };

                // Determine status
                let (status_text, status_color) = if let Some(ref stats) = info.stats {
                    if stats.synced {
                        ("SYNCED", Color32::from_rgb(100, 255, 100))
                    } else {
                        ("NOT SYNCED", Color32::from_rgb(255, 100, 100))
                    }
                } else {
                    ("No stats", Color32::GRAY)
                };

                (id, label, secondary, status_text, status_color)
            })
            .collect();

        // Get selected domain as string
        let selected_id = self.selected_domain.map(|d| d.to_string());

        // Render the list directly: the page already lives inside a vertical
        // ScrollArea, and a nested vertical ScrollArea with auto_shrink([false,
        // false]) here would expand to the full row width and push the details
        // panel off-screen.
        let items = items_data
            .iter()
            .map(|(id, label, secondary, status_text, status_color)| {
                ListItem::new(id, label)
                    .with_secondary(secondary.clone())
                    .with_status(status_text, *status_color)
            });

        let result = list_navigator(ui, "ptp_domains", items, selected_id.as_deref());

        if let Some(new_id) = result.selected {
            if let Ok(domain) = new_id.parse::<u8>() {
                self.selected_domain = Some(domain);
            }
        }
    }

    fn render_details_panel(
        &mut self,
        ui: &mut Ui,
        ptp_stats: &PtpStatsStore,
        flows: &[strom_types::Flow],
        domain_info: &HashMap<u8, DomainInfo>,
    ) {
        ui.heading("Domain Details");
        ui.separator();

        let Some(domain) = self.selected_domain else {
            ui.label("Select a domain to view detailed statistics");
            return;
        };

        let Some(info) = domain_info.get(&domain) else {
            ui.label("Domain not found");
            self.selected_domain = None;
            return;
        };

        ui.label(RichText::new(format!("PTP Domain {}", domain)).heading());
        ui.add_space(10.0);

        if let Some(ref stats) = info.stats {
            // Sync status
            let (status_color, status_text) = if stats.synced {
                (Color32::from_rgb(100, 255, 100), "Synchronized")
            } else {
                (Color32::from_rgb(255, 100, 100), "Not Synchronized")
            };
            ui.horizontal(|ui| {
                ui.label("Status:");
                ui.colored_label(status_color, RichText::new(status_text).strong());
            });

            egui::Grid::new("ptp_details_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Grandmaster:");
                    if let Some(gm_id) = stats.grandmaster_id {
                        ui.label(format!("{:016X}", gm_id));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Master:");
                    if let Some(master_id) = stats.master_id {
                        ui.label(format!("{:016X}", master_id));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Clock Offset:");
                    if let Some(offset_ns) = stats.clock_offset_ns {
                        let offset_us = offset_ns as f64 / 1000.0;
                        ui.label(format!("{:.2} us ({} ns)", offset_us, offset_ns));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Mean Path Delay:");
                    if let Some(delay_ns) = stats.mean_path_delay_ns {
                        let delay_us = delay_ns as f64 / 1000.0;
                        ui.label(format!("{:.2} us ({} ns)", delay_us, delay_ns));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("R2 (Quality):");
                    if let Some(r_squared) = stats.r_squared {
                        let color = if r_squared >= 0.99 {
                            Color32::from_rgb(100, 255, 100)
                        } else if r_squared >= 0.95 {
                            Color32::from_rgb(255, 200, 100)
                        } else {
                            Color32::from_rgb(255, 100, 100)
                        };
                        ui.colored_label(color, format!("{:.6}", r_squared));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Clock Rate:");
                    if let Some(rate) = stats.clock_rate {
                        ui.label(format!("{:.9}", rate));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();
                });

            // Graphs
            ui.add_space(10.0);
            ui.separator();
            ui.heading("Graphs");
            ui.add_space(10.0);

            // Find a flow with this domain to get history
            let history = flows
                .iter()
                .filter(|f| {
                    f.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp
                        && f.properties.ptp_domain.unwrap_or(0) == domain
                })
                .find_map(|f| ptp_stats.get_history(&f.id));

            if let Some(history) = history {
                let graph_height = 100.0;
                let graph_width = ui.available_width() - 20.0;

                // Clock offset graph
                ui.label("Clock Offset (us):");
                let offset_rect = ui.allocate_space(egui::Vec2::new(graph_width, graph_height));
                draw_large_graph(
                    ui.painter(),
                    offset_rect.1,
                    history.clock_offset_history(),
                    Color32::from_rgb(100, 200, 255),
                    true,
                );
                ui.add_space(10.0);

                // R² graph
                ui.label("R2 (Clock Estimation Quality):");
                let r2_rect = ui.allocate_space(egui::Vec2::new(graph_width, graph_height));
                draw_large_graph_fixed(
                    ui.painter(),
                    r2_rect.1,
                    history.r_squared_history(),
                    Color32::from_rgb(100, 255, 100),
                    0.9,
                    1.0,
                );
                ui.add_space(10.0);

                // Path delay graph
                ui.label("Mean Path Delay (us):");
                let delay_rect = ui.allocate_space(egui::Vec2::new(graph_width, graph_height));
                draw_large_graph(
                    ui.painter(),
                    delay_rect.1,
                    history.path_delay_history(),
                    Color32::from_rgb(255, 150, 100),
                    false,
                );
            } else {
                ui.label("No historical data available");
            }
        } else {
            ui.label("No statistics available for this domain yet.");
            ui.label("Statistics will appear once PTP synchronization begins.");
        }

        // Show flows using this domain
        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("Flows using this domain:").strong());
        ui.add_space(5.0);

        let domain_flows: Vec<_> = flows
            .iter()
            .filter(|f| {
                f.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp
                    && f.properties.ptp_domain.unwrap_or(0) == domain
            })
            .collect();

        if domain_flows.is_empty() {
            ui.label("No flows configured for this domain");
        } else {
            for flow in domain_flows {
                ui.label(format!("  - {}", flow.name));
            }
        }
    }
}

impl Default for ClocksPage {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-language explanations for each metric in the system-clock panel,
/// shown on hover so non-experts can interpret the kernel's NTP state.
const TIP_STATUS: &str = "Whether the kernel considers the system clock disciplined.\n\
    \"Synchronized\" only means the STA_UNSYNC bit is clear — it does NOT guarantee \
    accuracy. Always read it together with Max error and TAI − UTC offset.";
const TIP_STATE: &str = "Raw return value from ntp_adjtime():\n\
    • ok — normal operation\n\
    • ins / del — leap-second insertion/deletion pending\n\
    • oop / wait — leap second in progress\n\
    • error — clock is not disciplined";
const TIP_TAI: &str = "Leap-second offset between TAI and UTC, set by the discipline \
    daemon (chrony/ntpd/systemd-timesyncd).\n\n\
    Expected: 37 s (as of 2026).\n\
    0 s = the daemon has not configured leap seconds, so CLOCK_TAI cannot be \
    trusted as a global time source. Fix by enabling chrony's leapsectz or \
    installing tzdata-leaps.";
const TIP_PLL: &str = "Whether the kernel's phase-locked loop is actively steering the clock.\n\n\
    yes — ntpd / chrony in kernel-PLL mode are feeding updates.\n\
    no — systemd-timesyncd or chrony in SHM/userspace mode is fine; \
    discipline still happens but bypasses STA_PLL. Don't worry about this on its own.";
const TIP_OFFSET: &str = "Phase correction the kernel still has to apply, NOT the measured \
    error against the reference.\n\n\
    Two normal cases:\n\
    • Non-zero with PLL active = ntpd/chrony writes an offset, the kernel's PLL \
    slews it toward 0 over a few seconds. ±100 µs typical, ±1 ms acceptable.\n\
    • Exactly 0 with PLL inactive = daemon disciplines via frequency only \
    (systemd-timesyncd, chrony in SHM mode). The kernel never receives a phase \
    correction, so this field stays at 0 by design — read est_error / max_error \
    for accuracy instead.\n\n\
    Sustained large values (>1 ms with PLL active) indicate drift or a poor source.";
const TIP_FREQ: &str = "Frequency compensation the kernel applies to keep the CPU's local \
    crystal in tune, in parts-per-million.\n\n\
    Typical range: ±50 ppm for a stable machine.\n\
    Stays roughly constant once the daemon has converged. \
    Values close to ±500 ppm mean the daemon is saturated and the host clock is way off.";
const TIP_EST_ERR: &str = "Kernel's best-case error estimate.\n\n\
    Some daemons (chrony, ntpd) populate this; others \
    (systemd-timesyncd) leave it at 0.\n\
    A value of 0 here does NOT mean perfect sync — judge accuracy from \
    Max error and Current offset instead.";
const TIP_MAX_ERR: &str = "Kernel's worst-case error estimate. Behaves as a sawtooth: \
    grows linearly between daemon updates (at MAXFREQ, ~500 µs per second), \
    then resets when the daemon feeds a fresh estimate.\n\n\
    What to read from it:\n\
    • Peak value = how stale the kernel's view is just before each update.\n\
    • A small peak (a few ms) and short cycle (1–10 s) = daemon is healthy and polling often.\n\
    • A large peak (>500 ms) = daemon has stopped feeding updates; the kernel has no idea \
    what time it is.\n\
    • A frozen value that never resets = daemon dead or never started.";

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum HealthLevel {
    Healthy,
    Degraded,
    Bad,
}

struct ClockHealth {
    level: HealthLevel,
    findings: Vec<String>,
}

fn assess_clock_health(info: &strom_types::api::SystemClockInfo) -> ClockHealth {
    let mut level = HealthLevel::Healthy;
    let mut findings = Vec::new();
    let bump = |to: HealthLevel, level: &mut HealthLevel| {
        if to > *level {
            *level = to;
        }
    };

    if !info.synchronized || info.state == "error" {
        bump(HealthLevel::Bad, &mut level);
        findings.push(
            "Kernel reports the clock is NOT synchronized — discipline source missing or failing."
                .into(),
        );
    }

    if info.tai_offset_sec == 0 {
        bump(HealthLevel::Degraded, &mut level);
        findings.push(
            "TAI − UTC offset is 0. The discipline daemon has not configured leap seconds, \
             so CLOCK_TAI cannot be trusted as a global time source. \
             Expected value as of 2026 is 37 s."
                .into(),
        );
    } else if info.tai_offset_sec != 37 {
        findings.push(format!(
            "TAI − UTC offset is {} s (expected 37 as of 2026). \
             OK if your discipline source is authoritative on leap seconds.",
            info.tai_offset_sec
        ));
    }

    let max_err_ms = info.max_error_us as f64 / 1000.0;
    if max_err_ms > 500.0 {
        bump(HealthLevel::Degraded, &mut level);
        findings.push(format!(
            "Max error estimate is {:.0} ms — kernel is uncertain about sync quality. \
             A healthy disciplined clock stays under 100 ms.",
            max_err_ms
        ));
    }

    let offset_us_abs = info.offset_ns.abs() as f64 / 1000.0;
    if offset_us_abs > 1000.0 {
        bump(HealthLevel::Degraded, &mut level);
        findings.push(format!(
            "Current offset is {:.0} µs (>1 ms) — large correction in flight, sync is drifting.",
            offset_us_abs
        ));
    }

    if findings.is_empty() {
        findings.push("No issues detected. Clock looks well disciplined.".into());
    }

    ClockHealth { level, findings }
}

/// Render the kernel system-clock panel (TAI offset, NTP discipline, etc.).
fn render_system_clock_panel(
    ui: &mut Ui,
    info: Option<&strom_types::api::SystemClockInfo>,
    unsupported: bool,
) {
    ui.heading("System Clock");
    ui.add_space(6.0);

    if unsupported {
        ui.colored_label(
            Color32::GRAY,
            "Kernel clock discipline info is not exposed on this platform (Linux only).",
        );
        return;
    }

    let Some(info) = info else {
        ui.label("System clock state is not yet loaded…");
        return;
    };

    let sync_color = if info.synchronized {
        Color32::from_rgb(100, 255, 100)
    } else {
        Color32::from_rgb(255, 120, 100)
    };
    let sync_text = if info.synchronized {
        "Synchronized"
    } else {
        "Unsynced"
    };

    let health = assess_clock_health(info);
    let (health_color, health_text) = match health.level {
        HealthLevel::Healthy => (Color32::from_rgb(100, 255, 100), "Healthy"),
        HealthLevel::Degraded => (Color32::from_rgb(255, 200, 100), "Degraded"),
        HealthLevel::Bad => (Color32::from_rgb(255, 120, 100), "Unsynced"),
    };
    let health_findings = health.findings.clone();
    let health_summary = match health.level {
        HealthLevel::Healthy => "All checks pass.",
        HealthLevel::Degraded => {
            "Clock is being disciplined but at least one metric is outside the healthy range. \
             See details below."
        }
        HealthLevel::Bad => "Clock is not synchronized. Media timestamps will not be reliable.",
    };

    ui.horizontal(|ui| {
        ui.label("Health:");
        ui.colored_label(health_color, RichText::new(health_text).strong())
            .on_hover_ui(|ui| {
                ui.set_max_width(420.0);
                ui.label(RichText::new(health_summary).strong());
                ui.add_space(4.0);
                for f in &health_findings {
                    ui.label(format!("• {}", f));
                }
            });
    });
    ui.add_space(2.0);

    ui.horizontal(|ui| {
        ui.label("Status:").on_hover_text(TIP_STATUS);
        ui.colored_label(sync_color, RichText::new(sync_text).strong())
            .on_hover_text(TIP_STATUS);
        ui.add_space(12.0);
        ui.label("State:").on_hover_text(TIP_STATE);
        ui.label(&info.state).on_hover_text(TIP_STATE);
    });

    let warn = Color32::from_rgb(255, 200, 100);

    egui::Grid::new("system_clock_grid")
        .num_columns(2)
        .spacing([14.0, 4.0])
        .show(ui, |ui| {
            ui.label("TAI − UTC offset:").on_hover_text(TIP_TAI);
            let tai_text = format!("{} s", info.tai_offset_sec);
            if info.tai_offset_sec == 0 {
                ui.colored_label(warn, &tai_text).on_hover_text(TIP_TAI);
            } else {
                ui.label(&tai_text).on_hover_text(TIP_TAI);
            }
            ui.end_row();

            ui.label("PLL active:").on_hover_text(TIP_PLL);
            ui.label(if info.pll_active { "yes" } else { "no" })
                .on_hover_text(TIP_PLL);
            ui.end_row();

            ui.label("Current offset:").on_hover_text(TIP_OFFSET);
            let offset_us = info.offset_ns as f64 / 1000.0;
            let offset_text = format!("{:.3} µs ({} ns)", offset_us, info.offset_ns);
            if info.offset_ns.abs() > 1_000_000 {
                ui.colored_label(warn, &offset_text)
                    .on_hover_text(TIP_OFFSET);
            } else {
                ui.label(&offset_text).on_hover_text(TIP_OFFSET);
            }
            ui.end_row();

            ui.label("Frequency adj:").on_hover_text(TIP_FREQ);
            ui.label(format!("{:+.3} ppm", info.frequency_ppm))
                .on_hover_text(TIP_FREQ);
            ui.end_row();

            ui.label("Estimated error:").on_hover_text(TIP_EST_ERR);
            ui.label(format!("{} µs", info.est_error_us))
                .on_hover_text(TIP_EST_ERR);
            ui.end_row();

            ui.label("Max error:").on_hover_text(TIP_MAX_ERR);
            let max_err_text = format!("{} µs", info.max_error_us);
            if info.max_error_us > 500_000 {
                ui.colored_label(warn, &max_err_text)
                    .on_hover_text(TIP_MAX_ERR);
            } else {
                ui.label(&max_err_text).on_hover_text(TIP_MAX_ERR);
            }
            ui.end_row();
        });
}

/// Render one card per unique NTP endpoint (server:port), listing every flow
/// that references it. GStreamer shares an NtpClock instance across flows that
/// target the same endpoint, so the calibration/sync state is identical — one
/// card per clock, not per flow.
fn render_ntp_section(ui: &mut Ui, ntp_flows: &[&strom_types::Flow]) {
    let mut groups: Vec<((String, u16), Vec<&strom_types::Flow>)> = Vec::new();
    for flow in ntp_flows {
        let key = (
            flow.properties
                .ntp_server
                .clone()
                .unwrap_or_else(|| "pool.ntp.org".to_string()),
            flow.properties.ntp_port.unwrap_or(123),
        );
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, flows)) => flows.push(flow),
            None => groups.push((key, vec![flow])),
        }
    }

    for ((server, port), flows) in &groups {
        // Pick the first running flow with live ntp_info as the stats representative.
        let stats = flows
            .iter()
            .find_map(|f| f.properties.ntp_info.as_ref().map(|info| (f, info)));

        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}:{}", server, port)).strong());
                ui.add_space(8.0);
                match stats {
                    Some((_, info)) if info.synced => {
                        ui.colored_label(Color32::from_rgb(100, 255, 100), "Synced");
                    }
                    Some(_) => {
                        ui.colored_label(Color32::from_rgb(255, 180, 100), "Not synced");
                    }
                    None if flows.iter().any(|f| f.running) => {
                        ui.colored_label(Color32::GRAY, "Stats loading");
                    }
                    None => {
                        ui.colored_label(Color32::GRAY, "Stopped");
                    }
                }
            });

            if let Some((rep_flow, info)) = stats {
                egui::Grid::new(("ntp_grid", rep_flow.id))
                    .num_columns(2)
                    .spacing([14.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Offset vs local:");
                        if let Some(offset) = info.offset_ns {
                            let offset_us = offset as f64 / 1000.0;
                            ui.label(format!("{:.3} µs ({} ns)", offset_us, offset));
                        } else {
                            ui.label("-");
                        }
                        ui.end_row();

                        ui.label("Rate:");
                        if let Some(rate) = info.rate {
                            ui.label(format!("{:.9}", rate));
                        } else {
                            ui.label("-");
                        }
                        ui.end_row();

                        ui.label("Min update interval:");
                        ui.label(format!(
                            "{:.2} s",
                            info.minimum_update_interval_ns as f64 / 1e9
                        ));
                        ui.end_row();

                        ui.label("Round-trip limit:");
                        ui.label(format!("{:.2} ms", info.round_trip_limit_ns as f64 / 1e6));
                        ui.end_row();
                    });
            } else if !flows.iter().any(|f| f.running) {
                ui.label("Start a flow to see live stats");
            }

            ui.add_space(4.0);
            ui.label(RichText::new("Used by").weak());
            ui.horizontal_wrapped(|ui| {
                for flow in flows {
                    let (color, suffix) = if flow.running {
                        (Color32::from_rgb(100, 255, 100), "running")
                    } else {
                        (Color32::GRAY, "stopped")
                    };
                    ui.label(&flow.name);
                    ui.colored_label(color, format!("({})", suffix));
                }
            });
        });
        ui.add_space(6.0);
    }
}

/// Information about a PTP domain.
struct DomainInfo {
    domain: u8,
    flow_count: usize,
    stats: Option<PtpStatsData>,
}

/// Draw a larger graph with labels.
fn draw_large_graph(
    painter: &egui::Painter,
    rect: egui::Rect,
    data: &std::collections::VecDeque<f64>,
    color: Color32,
    signed: bool,
) {
    use egui::{Pos2, Stroke};

    // Draw background
    painter.rect_filled(rect, 4.0, Color32::from_gray(20));

    if data.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No data",
            egui::FontId::default(),
            Color32::GRAY,
        );
        return;
    }

    // Calculate range
    let (min_val, max_val) = if signed {
        let max_abs = data.iter().map(|v| v.abs()).fold(0.0_f64, f64::max);
        let range = max_abs.max(1.0) * 1.1;
        (-range, range)
    } else {
        let max = data.iter().fold(0.0_f64, |a, &b| a.max(b));
        (0.0, max.max(1.0) * 1.1)
    };

    // Draw grid lines
    for i in 0..=4 {
        let y = rect.min.y + (i as f32 / 4.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5_f32, Color32::from_gray(40)),
        );
    }

    // Draw center line for signed values
    if signed {
        let y_center = rect.center().y;
        painter.line_segment(
            [
                Pos2::new(rect.min.x, y_center),
                Pos2::new(rect.max.x, y_center),
            ],
            Stroke::new(1.0_f32, Color32::from_gray(80)),
        );
    }

    // Draw data line
    let range = max_val - min_val;
    let history_size = 60;
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (history_size - 1).max(1) as f32) * rect.width();
            let normalized = ((value - min_val) / range) as f32;
            let y = rect.max.y - normalized * rect.height();
            Pos2::new(x, y.clamp(rect.min.y, rect.max.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(2.0_f32, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0_f32, Color32::from_gray(80)),
        egui::StrokeKind::Outside,
    );

    // Draw current value
    if let Some(&last) = data.back() {
        let text = format!("{:.2}", last);
        painter.text(
            Pos2::new(rect.max.x - 5.0, rect.min.y + 15.0),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::default(),
            color,
        );
    }
}

/// Draw a graph with fixed range.
fn draw_large_graph_fixed(
    painter: &egui::Painter,
    rect: egui::Rect,
    data: &std::collections::VecDeque<f64>,
    color: Color32,
    min_val: f64,
    max_val: f64,
) {
    use egui::{Pos2, Stroke};

    // Draw background
    painter.rect_filled(rect, 4.0, Color32::from_gray(20));

    // Draw grid lines
    for i in 0..=4 {
        let y = rect.min.y + (i as f32 / 4.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5_f32, Color32::from_gray(40)),
        );
    }

    if data.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No data",
            egui::FontId::default(),
            Color32::GRAY,
        );
        return;
    }

    // Draw data line
    let range = max_val - min_val;
    let history_size = 60;
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (history_size - 1).max(1) as f32) * rect.width();
            let normalized = ((value - min_val) / range) as f32;
            let y = rect.max.y - normalized * rect.height();
            Pos2::new(x, y.clamp(rect.min.y, rect.max.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(2.0_f32, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0_f32, Color32::from_gray(80)),
        egui::StrokeKind::Outside,
    );

    // Draw current value
    if let Some(&last) = data.back() {
        let text = format!("{:.4}", last);
        painter.text(
            Pos2::new(rect.max.x - 5.0, rect.min.y + 15.0),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::default(),
            color,
        );
    }
}
