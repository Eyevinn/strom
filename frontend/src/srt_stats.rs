//! SRT statistics visualization widget.
//!
//! Mirrors the pattern used by [`crate::webrtc_stats`]: a per-flow store keyed by
//! `FlowId`, a compact inline renderer for the graph node, and a detailed renderer
//! for the property inspector.

use egui::{Color32, Ui};
use instant::Instant;
use std::collections::HashMap;
use strom_types::api::{SrtCallerStats, SrtConnectionStats, SrtRole, SrtStats};
use strom_types::FlowId;

/// Block definition IDs that wrap SRT input/output endpoints.
pub const SRT_BLOCK_DEFINITION_IDS: &[&str] = &[
    "builtin.efpsrt_input",
    "builtin.efpsrt_output",
    "builtin.mpegtssrt_input",
    "builtin.mpegtssrt_output",
];

/// Returns true if the given block definition ID is an SRT input/output block.
pub fn is_srt_block_def(definition_id: &str) -> bool {
    SRT_BLOCK_DEFINITION_IDS.contains(&definition_id)
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct SrtStatsKey {
    pub flow_id: FlowId,
}

/// Per-second deltas computed between two consecutive polls.
///
/// Lifetime totals (`packets_sent`, `bytes_received`, …) on long-running streams
/// don't tell the user whether the link is healthy *right now* — a count of 50k
/// lost packets is meaningless without knowing whether they accumulated over a
/// day or the last minute. These rates fill that gap.
#[derive(Debug, Clone, Default)]
pub struct CallerRates {
    pub packets_sent_per_sec: f64,
    pub packets_received_per_sec: f64,
    pub bytes_sent_per_sec: f64,
    pub bytes_received_per_sec: f64,
    /// Sender-side counters.
    pub packets_sent_lost_per_sec: f64,
    pub packets_sent_dropped_per_sec: f64,
    pub packets_retransmitted_per_sec: f64,
    /// Receiver-side counters.
    pub packets_received_lost_per_sec: f64,
    pub packets_received_dropped_per_sec: f64,
    pub packets_received_retransmitted_per_sec: f64,
    /// Recent loss fraction (0.0..=1.0) — unrecovered loss / data movement over
    /// the interval. "Unrecovered" excludes NAK'd-then-retransmitted packets,
    /// because those *did* reach the receiver.
    pub loss_fraction: f64,
}

/// Storage keyed by `(flow_id, connection_name, caller_id)` where `caller_id` is
/// the peer address if known, otherwise the index in the callers list. Index
/// keys are brittle (peer ordering can shift in listener mode) but address keys
/// are stable.
pub type CallerKey = (String, String);
pub type FlowRates = HashMap<CallerKey, CallerRates>;

#[derive(Debug, Clone, Default)]
pub struct SrtStatsStore {
    data: HashMap<SrtStatsKey, SrtStats>,
    last_update: HashMap<FlowId, Instant>,
    /// Per-flow per-caller rates derived from the previous-to-current delta.
    rates: HashMap<FlowId, FlowRates>,
}

impl SrtStatsStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, flow_id: FlowId, stats: SrtStats) {
        let now = Instant::now();
        let key = SrtStatsKey { flow_id };

        // Compute deltas against the previous reading before we overwrite it.
        let rates = match (self.data.get(&key), self.last_update.get(&flow_id)) {
            (Some(prev_stats), Some(&prev_time)) => {
                let dt = now.saturating_duration_since(prev_time).as_secs_f64();
                if dt > 0.001 {
                    compute_rates(&stats, prev_stats, dt)
                } else {
                    HashMap::new()
                }
            }
            _ => HashMap::new(),
        };

        self.data.insert(key, stats);
        self.rates.insert(flow_id, rates);
        self.last_update.insert(flow_id, now);
    }

    pub fn get(&self, flow_id: &FlowId) -> Option<&SrtStats> {
        let key = SrtStatsKey { flow_id: *flow_id };
        self.data.get(&key)
    }

    /// Get all per-caller rates for a flow, suitable for handing to the
    /// renderers. Returns `None` until the second poll completes (we need two
    /// readings to compute a delta).
    pub fn rates_for_flow(&self, flow_id: &FlowId) -> Option<&FlowRates> {
        self.rates.get(flow_id).filter(|m| !m.is_empty())
    }

    /// Filter a flow's stats and per-caller rates down to a single block.
    ///
    /// Connection keys are `{block_id}:{element_suffix}` and rate keys reuse
    /// that connection name as their first tuple element, so a single prefix
    /// match handles both. Returned values are owned so they can be moved into
    /// render closures without lifetime hassles.
    pub fn snapshot_for_block(
        &self,
        flow_id: &FlowId,
        block_id: &str,
    ) -> Option<(SrtStats, FlowRates)> {
        let stats = self.get(flow_id)?;
        let prefix = format!("{}:", block_id);
        let connections: HashMap<String, SrtConnectionStats> = stats
            .connections
            .iter()
            .filter(|(name, _)| name.starts_with(&prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if connections.is_empty() {
            return None;
        }
        let rates: FlowRates = self
            .rates_for_flow(flow_id)
            .map(|r| {
                r.iter()
                    .filter(|((conn, _), _)| conn.starts_with(&prefix))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Some((SrtStats { connections }, rates))
    }

    pub fn evict_stale(&mut self, ttl: std::time::Duration) {
        let stale: Vec<FlowId> = self
            .last_update
            .iter()
            .filter(|(_, t)| t.elapsed() > ttl)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.clear_flow(&id);
        }
    }

    pub fn clear_flow(&mut self, flow_id: &FlowId) {
        let key = SrtStatsKey { flow_id: *flow_id };
        self.data.remove(&key);
        self.last_update.remove(flow_id);
        self.rates.remove(flow_id);
    }
}

/// Stable caller identifier: prefer the resolved peer address, fall back to a
/// synthetic index-based key. Listener-mode peer ordering can change between
/// polls, so the index fallback is best-effort.
pub fn caller_id(caller: &SrtCallerStats, index: usize) -> String {
    caller
        .address
        .clone()
        .unwrap_or_else(|| format!("#idx-{}", index))
}

fn compute_rates(current: &SrtStats, previous: &SrtStats, dt: f64) -> FlowRates {
    let mut out = HashMap::new();
    let inv_dt = 1.0 / dt;

    for (conn_name, cur_conn) in &current.connections {
        let prev_conn = previous.connections.get(conn_name);

        for (idx, cur_caller) in cur_conn.callers.iter().enumerate() {
            let id = caller_id(cur_caller, idx);
            // Match the previous caller by address when possible — index can
            // shift if a peer disconnected since the last poll.
            let prev_caller = prev_conn.and_then(|pc| match &cur_caller.address {
                Some(addr) => pc.callers.iter().find(|c| c.address.as_ref() == Some(addr)),
                None => pc.callers.get(idx),
            });
            let Some(prev_caller) = prev_caller else {
                continue;
            };

            let sent = delta(cur_caller.packets_sent, prev_caller.packets_sent);
            let received = delta(cur_caller.packets_received, prev_caller.packets_received);
            let sent_lost = delta(cur_caller.packets_sent_lost, prev_caller.packets_sent_lost);
            let sent_dropped = delta(
                cur_caller.packets_sent_dropped,
                prev_caller.packets_sent_dropped,
            );
            let retx = delta(
                cur_caller.packets_retransmitted,
                prev_caller.packets_retransmitted,
            );
            let recv_lost = delta(
                cur_caller.packets_received_lost,
                prev_caller.packets_received_lost,
            );
            let recv_dropped = delta(
                cur_caller.packets_received_dropped,
                prev_caller.packets_received_dropped,
            );
            let recv_retx = delta(
                cur_caller.packets_received_retransmitted,
                prev_caller.packets_received_retransmitted,
            );
            let bytes_sent = delta(cur_caller.bytes_sent, prev_caller.bytes_sent);
            let bytes_received = delta(cur_caller.bytes_received, prev_caller.bytes_received);

            // Unrecovered loss only: sender's TLPKTDROP and receiver's lost/skipped.
            // NAK'd packets that were retransmitted reached the destination, so
            // they shouldn't count as loss in the rate-based view.
            let unrecovered = sent_dropped + recv_lost + recv_dropped;
            let total_flow = sent + received;
            let loss_fraction = if total_flow > 0 {
                unrecovered as f64 / total_flow as f64
            } else {
                0.0
            };

            out.insert(
                (conn_name.clone(), id),
                CallerRates {
                    packets_sent_per_sec: sent as f64 * inv_dt,
                    packets_received_per_sec: received as f64 * inv_dt,
                    bytes_sent_per_sec: bytes_sent as f64 * inv_dt,
                    bytes_received_per_sec: bytes_received as f64 * inv_dt,
                    packets_sent_lost_per_sec: sent_lost as f64 * inv_dt,
                    packets_sent_dropped_per_sec: sent_dropped as f64 * inv_dt,
                    packets_retransmitted_per_sec: retx as f64 * inv_dt,
                    packets_received_lost_per_sec: recv_lost as f64 * inv_dt,
                    packets_received_dropped_per_sec: recv_dropped as f64 * inv_dt,
                    packets_received_retransmitted_per_sec: recv_retx as f64 * inv_dt,
                    loss_fraction,
                },
            );
        }
    }

    out
}

fn delta(current: Option<u64>, previous: Option<u64>) -> u64 {
    current.unwrap_or(0).saturating_sub(previous.unwrap_or(0))
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.2} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.2} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{} B", bytes)
    }
}

fn rtt_color(rtt_ms: f64) -> Color32 {
    if rtt_ms > 200.0 {
        Color32::from_rgb(220, 50, 50)
    } else if rtt_ms > 100.0 {
        Color32::from_rgb(255, 165, 0)
    } else {
        Color32::GRAY
    }
}

fn loss_color(packets_lost: u64, packets_total: u64) -> Color32 {
    if packets_total == 0 {
        return Color32::GRAY;
    }
    let rate = packets_lost as f64 / packets_total.max(1) as f64;
    if rate > 0.05 {
        Color32::from_rgb(220, 50, 50)
    } else if rate > 0.01 {
        Color32::from_rgb(255, 165, 0)
    } else {
        Color32::GRAY
    }
}

fn state_chip(ui: &mut Ui, connected: bool) {
    let (bg, fg, label) = if connected {
        (Color32::from_rgb(0, 150, 0), Color32::WHITE, "connected")
    } else {
        (Color32::GRAY, Color32::WHITE, "waiting")
    };
    egui::Frame::NONE
        .fill(bg)
        .inner_margin(egui::Margin::symmetric(4, 1))
        .corner_radius(2.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).color(fg));
        });
}

/// Compact inline view for graph nodes. Shows peer count + the two metrics that
/// matter most at a glance on a single short line. The block has limited width,
/// so the text is intentionally terse.
///
/// When `rates` is supplied the loss reading reflects the *last interval* —
/// otherwise we fall back to the lifetime ratio (which becomes unreadable on
/// long-running streams).
pub fn show_compact(ui: &mut Ui, stats: &SrtStats, rates: Option<&FlowRates>) {
    if stats.connections.is_empty() {
        return;
    }

    // Aggregate across every caller of every srtsink/srtsrc element in the block.
    let mut peer_count: usize = 0;
    let mut rtt_samples: Vec<f64> = Vec::new();
    for conn in stats.connections.values() {
        peer_count += conn.callers.len();
        for caller in &conn.callers {
            if let Some(rtt) = caller.rtt_ms {
                if rtt > 0.0 {
                    rtt_samples.push(rtt);
                }
            }
        }
    }
    let avg_rtt_ms = if rtt_samples.is_empty() {
        0.0
    } else {
        rtt_samples.iter().sum::<f64>() / rtt_samples.len() as f64
    };

    // Loss percentage: prefer recent (rate-based) measurement so the figure
    // tracks live link health rather than lifetime totals.
    let loss_pct = match rates {
        Some(r) if !r.is_empty() => {
            // Recompute aggregate loss fraction across callers: sum unrecovered
            // packets/sec divided by sum data movement/sec.
            let mut unrecovered = 0.0_f64;
            let mut total_flow = 0.0_f64;
            for rate in r.values() {
                unrecovered += rate.packets_sent_dropped_per_sec
                    + rate.packets_received_lost_per_sec
                    + rate.packets_received_dropped_per_sec;
                total_flow += rate.packets_sent_per_sec + rate.packets_received_per_sec;
            }
            if total_flow > 0.0 {
                unrecovered / total_flow * 100.0
            } else {
                0.0
            }
        }
        _ => {
            // First-poll fallback: cumulative since pipeline start.
            let mut total_packets: u64 = 0;
            let mut total_lost: u64 = 0;
            for conn in stats.connections.values() {
                for caller in &conn.callers {
                    total_packets +=
                        caller.packets_sent.unwrap_or(0) + caller.packets_received.unwrap_or(0);
                    total_lost += caller.packets_sent_dropped.unwrap_or(0)
                        + caller.packets_received_lost.unwrap_or(0)
                        + caller.packets_received_dropped.unwrap_or(0);
                }
            }
            if total_packets > 0 {
                total_lost as f64 / total_packets as f64 * 100.0
            } else {
                0.0
            }
        }
    };
    // The compact view needs an integer count to colour-grade loss against —
    // use packets/sec when we have rates, lifetime totals otherwise.
    let (color_lost, color_total) = match rates {
        Some(r) if !r.is_empty() => {
            let unrec: f64 = r
                .values()
                .map(|x| {
                    x.packets_sent_dropped_per_sec
                        + x.packets_received_lost_per_sec
                        + x.packets_received_dropped_per_sec
                })
                .sum();
            let total: f64 = r
                .values()
                .map(|x| x.packets_sent_per_sec + x.packets_received_per_sec)
                .sum();
            (unrec as u64, total as u64)
        }
        _ => (0u64, 0u64),
    };

    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.horizontal(|ui| {
            ui.add_space(30.0);
            ui.spacing_mut().item_spacing.x = 6.0;

            // Peer count — green when at least one peer is exchanging data, gray when idle.
            let peer_color = if peer_count > 0 {
                Color32::from_rgb(0, 180, 0)
            } else {
                Color32::GRAY
            };
            ui.colored_label(peer_color, format!("{}p", peer_count));

            if avg_rtt_ms > 0.0 {
                ui.colored_label(rtt_color(avg_rtt_ms), format!("{:.0}ms", avg_rtt_ms));
            }

            if loss_pct > 0.05 {
                ui.colored_label(
                    loss_color(color_lost, color_total.max(1)),
                    format!("{:.1}%", loss_pct),
                );
            }
        });
    });
}

/// `show_address_in_grid` controls whether to include the peer address as the
/// first row of the stats grid. The caller-list view already shows the address
/// as a header above the grid, so callers in that view pass `false` to avoid
/// duplicating the field.
///
/// When `rates` is supplied, cumulative counters are followed by a "(N/s)"
/// annotation so the user can see recent activity alongside lifetime totals.
fn show_caller_stats(
    ui: &mut Ui,
    idx: usize,
    role: SrtRole,
    caller: &SrtCallerStats,
    rates: Option<&CallerRates>,
    show_address_in_grid: bool,
) {
    egui::Grid::new(format!("srt_caller_{}_{}", role, idx))
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            if show_address_in_grid {
                if let Some(addr) = &caller.address {
                    ui.label("Peer:");
                    ui.label(addr);
                    ui.end_row();
                }
            }

            if let Some(rtt) = caller.rtt_ms {
                ui.label("RTT:");
                ui.colored_label(rtt_color(rtt), format!("{:.1} ms", rtt));
                ui.end_row();
            }

            if let Some(latency) = caller.negotiated_latency_ms {
                ui.label("Negotiated latency:");
                ui.label(format!("{} ms", latency));
                ui.end_row();
            }

            if let Some(bw) = caller.bandwidth_mbps {
                ui.label("Bandwidth:");
                ui.label(format!("{:.2} Mbps", bw));
                ui.end_row();
            }

            // Sender stats — only show if there is sent data.
            let has_send =
                caller.packets_sent.unwrap_or(0) > 0 || caller.bytes_sent.unwrap_or(0) > 0;
            if has_send {
                if let Some(rate) = caller.send_rate_mbps {
                    ui.label("Send rate:");
                    ui.label(format!("{:.2} Mbps", rate));
                    ui.end_row();
                }
                if let Some(packets) = caller.packets_sent {
                    ui.label("Packets sent:");
                    ui.label(format_count_with_rate(
                        packets,
                        rates.map(|r| r.packets_sent_per_sec),
                    ));
                    ui.end_row();
                }
                if let Some(bytes) = caller.bytes_sent {
                    ui.label("Bytes sent:");
                    ui.label(format_bytes_with_rate(
                        bytes,
                        rates.map(|r| r.bytes_sent_per_sec),
                    ));
                    ui.end_row();
                }
                if let Some(buf) = caller.snd_buf_level_ms {
                    ui.label("Snd buf:");
                    ui.label(format!("{} ms", buf));
                    ui.end_row();
                }
                // Sender-side loss bookkeeping: NAK'd packets and the matching retx
                // count. If every NAK was answered with a retransmit the recovery is
                // 100% — colour green so the user isn't alarmed by the raw "lost"
                // figure. TLPKTDROP is the actually-dropped counter.
                if let Some(lost) = caller.packets_sent_lost {
                    let rtx = caller.packets_retransmitted.unwrap_or(0);
                    let recovery = if lost > 0 {
                        (rtx as f64 / lost as f64).min(1.0)
                    } else {
                        1.0
                    };
                    let color = if lost == 0 {
                        Color32::GRAY
                    } else if recovery >= 0.999 {
                        Color32::from_rgb(0, 150, 0)
                    } else if recovery >= 0.9 {
                        Color32::from_rgb(255, 165, 0)
                    } else {
                        Color32::from_rgb(220, 50, 50)
                    };
                    ui.label("NAK'd by peer:")
                        .on_hover_text("Packets the peer reported missing via NAK");
                    ui.colored_label(
                        color,
                        format_count_with_rate(lost, rates.map(|r| r.packets_sent_lost_per_sec)),
                    );
                    ui.end_row();
                    if rtx > 0 || lost > 0 {
                        ui.label("Retransmitted:")
                            .on_hover_text("Packets we re-sent in response to NAKs");
                        let rate_str = rates
                            .map(|r| format!(" ({:.0}/s)", r.packets_retransmitted_per_sec))
                            .unwrap_or_default();
                        ui.label(format!(
                            "{} ({:.0}% recovered){}",
                            rtx,
                            recovery * 100.0,
                            rate_str
                        ));
                        ui.end_row();
                    }
                }
                if let Some(dropped) = caller.packets_sent_dropped {
                    if dropped > 0 {
                        ui.label("Dropped (TLPKTDROP):")
                            .on_hover_text(
                                "Packets dropped locally because they couldn't be sent in time. \
                                 These never reach the receiver.",
                            );
                        ui.colored_label(
                            Color32::from_rgb(220, 50, 50),
                            format_count_with_rate(
                                dropped,
                                rates.map(|r| r.packets_sent_dropped_per_sec),
                            ),
                        );
                        ui.end_row();
                    }
                }
            }

            // Receiver stats — only show if there is received data.
            let has_recv = caller.packets_received.unwrap_or(0) > 0
                || caller.bytes_received.unwrap_or(0) > 0;
            if has_recv {
                if let Some(rate) = caller.recv_rate_mbps {
                    ui.label("Recv rate:");
                    ui.label(format!("{:.2} Mbps", rate));
                    ui.end_row();
                }
                if let Some(packets) = caller.packets_received {
                    ui.label("Packets received:");
                    ui.label(format_count_with_rate(
                        packets,
                        rates.map(|r| r.packets_received_per_sec),
                    ));
                    ui.end_row();
                }
                if let Some(bytes) = caller.bytes_received {
                    ui.label("Bytes received:");
                    ui.label(format_bytes_with_rate(
                        bytes,
                        rates.map(|r| r.bytes_received_per_sec),
                    ));
                    ui.end_row();
                }
                if let Some(buf) = caller.recv_buf_level_ms {
                    ui.label("Recv buf:");
                    ui.label(format!("{} ms", buf));
                    ui.end_row();
                }
                if let Some(lost) = caller.packets_received_lost {
                    let total = caller.packets_received.unwrap_or(0);
                    ui.label("Packets lost:")
                        .on_hover_text("Packets detected as missing in the received stream");
                    ui.colored_label(
                        loss_color(lost, total),
                        format_count_with_rate(
                            lost,
                            rates.map(|r| r.packets_received_lost_per_sec),
                        ),
                    );
                    ui.end_row();
                }
                if let Some(rtx) = caller.packets_received_retransmitted {
                    if rtx > 0 {
                        ui.label("Recovered via retx:")
                            .on_hover_text("Packets received that were retransmissions of earlier loss");
                        ui.label(format_count_with_rate(
                            rtx,
                            rates.map(|r| r.packets_received_retransmitted_per_sec),
                        ));
                        ui.end_row();
                    }
                }
                if let Some(dropped) = caller.packets_received_dropped {
                    if dropped > 0 {
                        ui.label("Skipped (TSBPD):")
                            .on_hover_text(
                                "Packets skipped because they arrived too late for the negotiated latency",
                            );
                        ui.colored_label(
                            Color32::from_rgb(220, 50, 50),
                            format_count_with_rate(
                                dropped,
                                rates.map(|r| r.packets_received_dropped_per_sec),
                            ),
                        );
                        ui.end_row();
                    }
                }
            }

            // Recent unrecovered loss rate — the single most important "is the
            // link healthy right now?" number. Skip when there's no signal yet
            // (first poll) or when loss is essentially zero.
            if let Some(r) = rates {
                if r.loss_fraction > 0.00005 {
                    let pct = r.loss_fraction * 100.0;
                    let color = if r.loss_fraction > 0.05 {
                        Color32::from_rgb(220, 50, 50)
                    } else if r.loss_fraction > 0.01 {
                        Color32::from_rgb(255, 165, 0)
                    } else {
                        Color32::from_rgb(255, 200, 50)
                    };
                    ui.label("Recent loss:")
                        .on_hover_text("Unrecovered packet loss over the last poll interval");
                    ui.colored_label(color, format!("{:.2}%", pct));
                    ui.end_row();
                }
            }
        });
}

fn format_count_with_rate(count: u64, rate_per_sec: Option<f64>) -> String {
    match rate_per_sec {
        Some(r) if r >= 0.05 => format!("{} ({:.0}/s)", count, r),
        _ => format!("{}", count),
    }
}

fn format_bytes_with_rate(bytes: u64, rate_per_sec: Option<f64>) -> String {
    match rate_per_sec {
        Some(r) if r >= 1.0 => {
            let bits_per_sec = (r * 8.0) as u64;
            let rate_str = if bits_per_sec >= 1_000_000 {
                format!("{:.2} Mbps", bits_per_sec as f64 / 1_000_000.0)
            } else if bits_per_sec >= 1_000 {
                format!("{:.1} Kbps", bits_per_sec as f64 / 1_000.0)
            } else {
                format!("{} bps", bits_per_sec)
            };
            format!("{} ({})", format_bytes(bytes), rate_str)
        }
        _ => format_bytes(bytes),
    }
}

fn show_connection(ui: &mut Ui, name: &str, conn: &SrtConnectionStats, rates: Option<&FlowRates>) {
    ui.collapsing(egui::RichText::new(name).strong(), |ui| {
        egui::Grid::new(format!("srt_conn_header_{}", name))
            .num_columns(2)
            .spacing([10.0, 2.0])
            .show(ui, |ui| {
                ui.label("Role:");
                ui.label(conn.role.as_str());
                ui.end_row();

                if let Some(mode) = conn.mode {
                    ui.label("Mode:");
                    ui.label(mode.as_str());
                    ui.end_row();
                }

                ui.label("State:");
                state_chip(ui, conn.connected);
                ui.end_row();
            });
        ui.add_space(5.0);

        if conn.callers.is_empty() {
            ui.colored_label(Color32::from_rgb(150, 150, 150), "Waiting for SRT peer…");
            return;
        }

        let multi = conn.callers.len() > 1;
        for (i, caller) in conn.callers.iter().enumerate() {
            if multi {
                if i > 0 {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                }
                let label = caller
                    .address
                    .clone()
                    .unwrap_or_else(|| format!("Caller {}", i + 1));
                ui.label(egui::RichText::new(label).strong());
                ui.add_space(2.0);
            }
            let caller_rate = rates.and_then(|r| {
                let id = caller_id(caller, i);
                r.get(&(name.to_string(), id))
            });
            // When we render a per-caller header (multi-caller case) the address
            // is already visible, so we suppress the duplicate "Peer:" row.
            show_caller_stats(ui, i, conn.role, caller, caller_rate, !multi);
        }
    });
}

/// Detailed view for the property inspector panel.
pub fn show_full(ui: &mut Ui, stats: &SrtStats, rates: Option<&FlowRates>) {
    if stats.connections.is_empty() {
        ui.label("No SRT elements found");
        return;
    }

    ui.label(format!("{} SRT element(s)", stats.connections.len()));
    ui.add_space(5.0);

    let mut sorted: Vec<_> = stats.connections.iter().collect();
    sorted.sort_by_key(|(name, _)| name.as_str());

    for (name, conn) in sorted {
        show_connection(ui, name, conn, rates);
        ui.add_space(10.0);
    }
}
