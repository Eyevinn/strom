//! WebRTC statistics visualization widget.

use egui::{Color32, Ui};
use instant::Instant;
use std::collections::HashMap;
use strom_types::api::{RtpStreamStats, WebRtcConnectionStats, WebRtcStats};
use strom_types::FlowId;

/// Key for identifying WebRTC stats data (flow).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct WebRtcStatsKey {
    pub flow_id: FlowId,
}

/// Storage for all WebRTC stats data in the application.
#[derive(Debug, Clone, Default)]
pub struct WebRtcStatsStore {
    data: HashMap<WebRtcStatsKey, WebRtcStats>,
    /// Timestamp of last update for each flow
    last_update: HashMap<FlowId, Instant>,
}

impl WebRtcStatsStore {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
            last_update: HashMap::new(),
        }
    }

    /// Update WebRTC stats for a specific flow.
    pub fn update(&mut self, flow_id: FlowId, stats: WebRtcStats) {
        let key = WebRtcStatsKey { flow_id };
        self.data.insert(key, stats);
        self.last_update.insert(flow_id, Instant::now());
    }

    /// Get WebRTC stats for a specific flow.
    pub fn get(&self, flow_id: &FlowId) -> Option<&WebRtcStats> {
        let key = WebRtcStatsKey { flow_id: *flow_id };
        self.data.get(&key)
    }

    /// Check if stats are stale (older than given duration).
    pub fn is_stale(&self, flow_id: &FlowId, max_age: std::time::Duration) -> bool {
        self.last_update
            .get(flow_id)
            .map(|t| t.elapsed() > max_age)
            .unwrap_or(true)
    }

    /// Clear all WebRTC stats data.
    pub fn clear(&mut self) {
        self.data.clear();
        self.last_update.clear();
    }

    /// Remove WebRTC stats for a specific flow.
    pub fn clear_flow(&mut self, flow_id: &FlowId) {
        let key = WebRtcStatsKey { flow_id: *flow_id };
        self.data.remove(&key);
        self.last_update.remove(flow_id);
    }
}

/// Format bytes to human-readable string.
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

/// Format bitrate to human-readable string.
fn format_bitrate(bitrate: u64) -> String {
    if bitrate >= 1_000_000 {
        format!("{:.2} Mbps", bitrate as f64 / 1_000_000.0)
    } else if bitrate >= 1_000 {
        format!("{:.2} Kbps", bitrate as f64 / 1_000.0)
    } else {
        format!("{} bps", bitrate)
    }
}

/// Render a minimal WebRTC stats widget (for graph nodes).
/// Shows just session count and key metrics in a single line.
pub fn show_compact(ui: &mut Ui, stats: &WebRtcStats) {
    if stats.connections.is_empty() {
        return;
    }

    let session_count = stats.connections.len();

    // Count connected sessions
    let connected_count = stats
        .connections
        .values()
        .filter(|c| {
            c.ice_candidates
                .as_ref()
                .and_then(|ice| ice.state.as_ref())
                .map(|s| s == "connected" || s == "completed")
                .unwrap_or(false)
        })
        .count();

    // Calculate average jitter across all inbound streams (in ms)
    let jitters: Vec<f64> = stats
        .connections
        .values()
        .flat_map(|c| c.inbound_rtp.iter())
        .filter_map(|rtp| rtp.jitter)
        .map(|j| j * 1000.0) // Convert to ms
        .collect();
    let avg_jitter_ms = if !jitters.is_empty() {
        jitters.iter().sum::<f64>() / jitters.len() as f64
    } else {
        0.0
    };

    // Build compact status line (centered)
    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.horizontal(|ui| {
            // Session count with color indicator
            let color = if connected_count == session_count && session_count > 0 {
                Color32::from_rgb(0, 180, 0)
            } else if connected_count > 0 {
                Color32::from_rgb(255, 165, 0)
            } else {
                Color32::GRAY
            };

            ui.colored_label(
                color,
                format!(
                    "{} {}",
                    session_count,
                    if session_count == 1 {
                        "session"
                    } else {
                        "sessions"
                    }
                ),
            );

            // Show jitter if significant
            if avg_jitter_ms > 0.1 {
                ui.label(format!("jitter: {:.0}ms", avg_jitter_ms));
            }
        });
    });
}

/// Render RTP stream stats.
fn show_rtp_stats(ui: &mut Ui, stats: &RtpStreamStats, label: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).strong());
        if let Some(media_type) = &stats.media_type {
            ui.label(format!("({})", media_type));
        }
    });

    egui::Grid::new(format!("rtp_stats_{:?}_{}", stats.ssrc, label))
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            if let Some(ssrc) = stats.ssrc {
                ui.label("SSRC:");
                ui.label(format!("{}", ssrc));
                ui.end_row();
            }

            if let Some(codec) = &stats.codec {
                ui.label("Codec:");
                ui.label(codec);
                ui.end_row();
            }

            if let Some(bitrate) = stats.bitrate {
                ui.label("Bitrate:");
                ui.label(format_bitrate(bitrate));
                ui.end_row();
            }

            if let Some(packets) = stats.packets {
                ui.label("Packets:");
                ui.label(format!("{}", packets));
                ui.end_row();
            }

            if let Some(bytes) = stats.bytes {
                ui.label("Bytes:");
                ui.label(format_bytes(bytes));
                ui.end_row();
            }

            if let Some(packets_lost) = stats.packets_lost {
                let color = if packets_lost > 0 {
                    Color32::from_rgb(255, 165, 0)
                } else {
                    Color32::GRAY
                };
                ui.label("Packets Lost:");
                ui.colored_label(color, format!("{}", packets_lost));
                ui.end_row();
            }

            if let Some(fraction_lost) = stats.fraction_lost {
                let color = if fraction_lost > 0.01 {
                    Color32::from_rgb(255, 165, 0)
                } else if fraction_lost > 0.05 {
                    Color32::from_rgb(255, 0, 0)
                } else {
                    Color32::GRAY
                };
                ui.label("Loss Rate:");
                ui.colored_label(color, format!("{:.1}%", fraction_lost * 100.0));
                ui.end_row();
            }

            if let Some(jitter) = stats.jitter {
                let color = if jitter > 0.05 {
                    Color32::from_rgb(255, 165, 0)
                } else {
                    Color32::GRAY
                };
                ui.label("Jitter:");
                ui.colored_label(color, format!("{:.3} s", jitter));
                ui.end_row();
            }

            if let Some(rtt) = stats.round_trip_time {
                let color = if rtt > 0.2 {
                    Color32::from_rgb(255, 0, 0)
                } else if rtt > 0.1 {
                    Color32::from_rgb(255, 165, 0)
                } else {
                    Color32::GRAY
                };
                ui.label("RTT:");
                // Show in ms for better readability
                ui.colored_label(color, format!("{:.0} ms", rtt * 1000.0));
                ui.end_row();
            }
        });
}

/// Render ICE candidate stats.
fn show_ice_stats(ui: &mut Ui, ice: &strom_types::api::IceCandidateStats) {
    ui.label(egui::RichText::new("ICE Candidates").strong());

    egui::Grid::new("ice_stats")
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            if let Some(ref state) = ice.state {
                let (text_color, bg_color) = match state.as_str() {
                    "connected" | "completed" => (Color32::WHITE, Color32::from_rgb(0, 150, 0)),
                    "checking" => (Color32::BLACK, Color32::from_rgb(255, 200, 0)),
                    "failed" | "disconnected" => (Color32::WHITE, Color32::from_rgb(200, 0, 0)),
                    _ => (Color32::WHITE, Color32::GRAY),
                };
                ui.label("State:");
                egui::Frame::NONE
                    .fill(bg_color)
                    .inner_margin(egui::Margin::symmetric(4, 1))
                    .corner_radius(2.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(state.as_str()).color(text_color));
                    });
                ui.end_row();
            }

            if let Some(ref local_type) = ice.local_candidate_type {
                ui.label("Local Type:");
                ui.label(local_type);
                ui.end_row();
            }

            // Show local endpoint
            if ice.local_address.is_some() || ice.local_port.is_some() {
                ui.label("Local:");
                let addr = ice.local_address.as_deref().unwrap_or("?");
                let port = ice
                    .local_port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let proto = ice.local_protocol.as_deref().unwrap_or("");
                ui.label(format!("{}:{} {}", addr, port, proto));
                ui.end_row();
            }

            if let Some(ref remote_type) = ice.remote_candidate_type {
                ui.label("Remote Type:");
                ui.label(remote_type);
                ui.end_row();
            }

            // Show remote endpoint
            if ice.remote_address.is_some() || ice.remote_port.is_some() {
                ui.label("Remote:");
                let addr = ice.remote_address.as_deref().unwrap_or("?");
                let port = ice
                    .remote_port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let proto = ice.remote_protocol.as_deref().unwrap_or("");
                ui.label(format!("{}:{} {}", addr, port, proto));
                ui.end_row();
            }
        });
}

/// Render transport stats.
fn show_transport_stats(ui: &mut Ui, transport: &strom_types::api::TransportStats) {
    ui.label(egui::RichText::new("Transport").strong());

    egui::Grid::new("transport_stats")
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            if let Some(bytes) = transport.bytes_sent {
                ui.label("Bytes Sent:");
                ui.label(format_bytes(bytes));
                ui.end_row();
            }
            if let Some(bytes) = transport.bytes_received {
                ui.label("Bytes Received:");
                ui.label(format_bytes(bytes));
                ui.end_row();
            }
            if let Some(packets) = transport.packets_sent {
                ui.label("Packets Sent:");
                ui.label(format!("{}", packets));
                ui.end_row();
            }
            if let Some(packets) = transport.packets_received {
                ui.label("Packets Received:");
                ui.label(format!("{}", packets));
                ui.end_row();
            }
        });
}

/// Render codec stats.
fn show_codec_stats(ui: &mut Ui, codecs: &[strom_types::api::CodecStats]) {
    if codecs.is_empty() {
        return;
    }

    ui.label(egui::RichText::new("Codecs").strong());

    for (i, codec) in codecs.iter().enumerate() {
        egui::Grid::new(format!("codec_stats_{}", i))
            .num_columns(2)
            .spacing([10.0, 2.0])
            .show(ui, |ui| {
                if let Some(ref mime) = codec.mime_type {
                    ui.label("Type:");
                    ui.label(mime);
                    ui.end_row();
                }
                if let Some(clock_rate) = codec.clock_rate {
                    ui.label("Clock Rate:");
                    ui.label(format!("{} Hz", clock_rate));
                    ui.end_row();
                }
                if let Some(pt) = codec.payload_type {
                    ui.label("Payload Type:");
                    ui.label(format!("{}", pt));
                    ui.end_row();
                }
                if let Some(channels) = codec.channels {
                    ui.label("Channels:");
                    ui.label(format!("{}", channels));
                    ui.end_row();
                }
            });
    }
}

/// Render connection stats.
fn show_connection_stats(ui: &mut Ui, name: &str, conn: &WebRtcConnectionStats) {
    ui.collapsing(egui::RichText::new(name).strong(), |ui| {
        // ICE candidates
        if let Some(ref ice) = conn.ice_candidates {
            show_ice_stats(ui, ice);
            ui.add_space(10.0);
        }

        // Transport stats
        if let Some(ref transport) = conn.transport {
            show_transport_stats(ui, transport);
            ui.add_space(10.0);
        }

        // Codec stats
        if !conn.codecs.is_empty() {
            show_codec_stats(ui, &conn.codecs);
            ui.add_space(10.0);
        }

        // Inbound RTP streams
        if !conn.inbound_rtp.is_empty() {
            ui.label(egui::RichText::new("Inbound RTP Streams").underline());
            for (i, rtp) in conn.inbound_rtp.iter().enumerate() {
                show_rtp_stats(ui, rtp, &format!("Stream {}", i + 1));
                ui.add_space(5.0);
            }
        }

        // Outbound RTP streams
        if !conn.outbound_rtp.is_empty() {
            ui.label(egui::RichText::new("Outbound RTP Streams").underline());
            for (i, rtp) in conn.outbound_rtp.iter().enumerate() {
                show_rtp_stats(ui, rtp, &format!("Stream {}", i + 1));
                ui.add_space(5.0);
            }
        }

        // Show message if no parsed stats available
        if conn.ice_candidates.is_none()
            && conn.inbound_rtp.is_empty()
            && conn.outbound_rtp.is_empty()
            && conn.transport.is_none()
            && conn.codecs.is_empty()
        {
            ui.colored_label(
                Color32::from_rgb(150, 150, 150),
                "Waiting for WebRTC stats...",
            );
        }
    });
}

/// Render a full WebRTC stats widget (for property inspector or dedicated panel).
pub fn show_full(ui: &mut Ui, stats: &WebRtcStats) {
    ui.heading("WebRTC Statistics");
    ui.separator();

    if stats.connections.is_empty() {
        ui.label("No WebRTC connections found");
        ui.label("Start a flow with WHIP/WHEP blocks to see statistics.");
        return;
    }

    ui.label(format!("{} connection(s)", stats.connections.len()));
    ui.add_space(5.0);

    // Sort connections by name for stable display order
    let mut sorted_connections: Vec<_> = stats.connections.iter().collect();
    sorted_connections.sort_by_key(|(name, _)| *name);

    for (name, conn) in sorted_connections {
        show_connection_stats(ui, name, conn);
        ui.add_space(10.0);
    }
}

/// Render a summary line of WebRTC stats for toolbar display.
pub fn show_summary(ui: &mut Ui, stats: &WebRtcStats) {
    if stats.connections.is_empty() {
        return;
    }

    let total_connections = stats.connections.len();
    let connected_count = stats
        .connections
        .values()
        .filter(|c| {
            c.ice_candidates
                .as_ref()
                .and_then(|ice| ice.state.as_ref())
                .map(|s| s == "connected" || s == "completed")
                .unwrap_or(false)
        })
        .count();

    let color = if connected_count == total_connections {
        Color32::from_rgb(0, 200, 0)
    } else if connected_count > 0 {
        Color32::from_rgb(255, 165, 0)
    } else {
        Color32::from_rgb(255, 0, 0)
    };

    ui.colored_label(
        color,
        format!("WebRTC: {}/{}", connected_count, total_connections),
    );
}
