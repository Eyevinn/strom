//! SRT statistics collection.
//!
//! Queries the `stats` GstStructure exposed by every `srtsink` / `srtsrc` element
//! in the pipeline and returns a curated, typed view suitable for the API and
//! frontend. Handles both caller mode (single peer at the top level) and listener
//! mode (multiple peers in a `callers` GstValueArray).

use super::PipelineManager;
use gio::prelude::*;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use strom_types::api::{SrtCallerStats, SrtConnectionStats, SrtMode, SrtRole, SrtStats};
use tracing::{debug, trace};

impl PipelineManager {
    /// Collect SRT statistics for every srtsink/srtsrc element in the pipeline.
    pub fn get_srt_stats(&self) -> SrtStats {
        let mut stats = SrtStats::default();

        for (name, element) in &self.elements {
            let factory_name = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();

            let role = match factory_name.as_str() {
                "srtsink" => SrtRole::Sink,
                "srtsrc" => SrtRole::Source,
                _ => continue,
            };

            trace!(
                "get_srt_stats: collecting stats from {} ({})",
                name,
                factory_name
            );

            let conn = collect_element_stats(role, element);
            stats.connections.insert(name.clone(), conn);
        }

        debug!(
            "get_srt_stats: returning {} SRT connection(s)",
            stats.connections.len()
        );
        stats
    }
}

fn collect_element_stats(role: SrtRole, element: &gst::Element) -> SrtConnectionStats {
    let mut conn = SrtConnectionStats {
        role,
        mode: read_mode(element),
        connected: false,
        callers: Vec::new(),
    };

    if !element.has_property("stats") {
        return conn;
    }

    let stats_value = element.property_value("stats");
    let Ok(structure) = stats_value.get::<gst::Structure>() else {
        trace!("get_srt_stats: 'stats' property is not a GstStructure");
        return conn;
    };

    // Listener mode (or any srtsink/srtsrc with multiple callers) exposes a
    // `callers` GstValueArray. Its *presence* is what marks the element as
    // multi-peer mode — even an empty array means "listener, no peer attached",
    // not "fall back to single-peer parsing." Skipping this early-return on
    // empty would push a phantom caller built from top-level link metrics
    // (libsrt populates `bandwidth-mbps`/`negotiated-latency-ms` at the element
    // level before any handshake), making the UI report "1p connected" on an
    // idle listener.
    if let Some(callers) = read_callers_array(&structure) {
        conn.callers = callers;
        conn.connected = conn.callers.iter().any(caller_is_active);
        return conn;
    }

    // Caller / rendezvous / single-peer mode: the top-level structure IS the
    // per-caller stats. Wrap it as a single entry so the frontend can render
    // uniformly.
    let single = parse_caller_stats(&structure);
    if has_any_data(&single) {
        conn.connected = caller_is_active(&single);
        conn.callers.push(single);
    }

    conn
}

/// Read the `mode` enum property and map its nick to `SrtMode`. Returns `None`
/// for the `"none"` placeholder value or any unrecognised nick.
fn read_mode(element: &gst::Element) -> Option<SrtMode> {
    if !element.has_property("mode") {
        return None;
    }
    let value = element.property_value("mode");
    if !value.type_().is_a(glib::Type::ENUM) {
        return None;
    }
    let enum_class = glib::EnumClass::with_type(value.type_())?;
    let transformed = value.transform::<i32>().ok()?;
    let int_val = transformed.get::<i32>().ok()?;
    let ev = enum_class.value(int_val)?;
    SrtMode::from_nick(ev.nick())
}

fn read_callers_array(structure: &gst::Structure) -> Option<Vec<SrtCallerStats>> {
    if !structure.has_field("callers") {
        return None;
    }

    // The SRT plugin packs per-caller stats in different container types depending on
    // the gst-plugins-bad version: `GValueArray` (legacy, used by srtsink listener mode
    // through 1.24), `GST_TYPE_ARRAY`, or `GST_TYPE_LIST`. Try them all.
    if let Ok(va) = structure.get::<glib::ValueArray>("callers") {
        let parsed: Vec<SrtCallerStats> = va
            .iter()
            .filter_map(|v| v.get::<gst::Structure>().ok())
            .map(|s| parse_caller_stats(&s))
            .collect();
        return Some(parsed);
    }
    if let Ok(array) = structure.get::<gst::Array>("callers") {
        return Some(
            array
                .as_slice()
                .iter()
                .filter_map(|v| v.get::<gst::Structure>().ok())
                .map(|s| parse_caller_stats(&s))
                .collect(),
        );
    }
    if let Ok(list) = structure.get::<gst::List>("callers") {
        return Some(
            list.as_slice()
                .iter()
                .filter_map(|v| v.get::<gst::Structure>().ok())
                .map(|s| parse_caller_stats(&s))
                .collect(),
        );
    }
    debug!(
        "get_srt_stats: 'callers' field present but type unrecognized: {:?}",
        structure.value("callers").map(|v| v.type_())
    );
    None
}

pub(super) fn parse_caller_stats(s: &gst::Structure) -> SrtCallerStats {
    SrtCallerStats {
        address: read_caller_address(s)
            .or_else(|| get_string(s, &["caller-address", "address", "peer-address"])),

        // Link metrics
        rtt_ms: get_f64(s, &["rtt-ms", "ms-rtt"]),
        bandwidth_mbps: get_f64(
            s,
            &[
                "bandwidth-mbps",
                "estimated-bandwidth-mbps",
                "mbps-bandwidth",
            ],
        ),
        negotiated_latency_ms: get_u32(s, &["negotiated-latency-ms", "ms-negotiated-latency"]),

        // Sender metrics — populated by srtsink (libsrt mbStats sender section).
        packets_sent: get_u64(s, &["packets-sent", "packets-sent-total"]),
        packets_sent_lost: get_u64(s, &["packets-sent-lost"]),
        packets_sent_dropped: get_u64(s, &["packets-sent-dropped"]),
        packets_retransmitted: get_u64(s, &["packets-retransmitted"]),
        bytes_sent: get_u64(s, &["bytes-sent", "bytes-sent-total"]),
        send_rate_mbps: get_f64(s, &["send-rate-mbps", "mbps-send-rate"]),
        snd_buf_level_ms: get_u32(s, &["snd-buf-level-ms", "ms-snd-buf"]),

        // Receiver metrics — populated by srtsrc (libsrt mbStats receiver section).
        packets_received: get_u64(s, &["packets-received", "packets-received-total"]),
        packets_received_lost: get_u64(s, &["packets-received-lost"]),
        packets_received_dropped: get_u64(s, &["packets-received-dropped"]),
        packets_received_retransmitted: get_u64(s, &["packets-received-retransmitted"]),
        bytes_received: get_u64(s, &["bytes-received", "bytes-received-total"]),
        recv_rate_mbps: get_f64(
            s,
            &["receive-rate-mbps", "mbps-recv-rate", "recv-rate-mbps"],
        ),
        recv_buf_level_ms: get_u32(s, &["recv-buf-level-ms", "ms-recv-buf"]),
    }
}

fn caller_is_active(c: &SrtCallerStats) -> bool {
    let sent = c.packets_sent.unwrap_or(0);
    let recv = c.packets_received.unwrap_or(0);
    let bytes = c.bytes_sent.unwrap_or(0) + c.bytes_received.unwrap_or(0);
    sent > 0 || recv > 0 || bytes > 0
}

fn has_any_data(c: &SrtCallerStats) -> bool {
    c.packets_sent.is_some()
        || c.packets_received.is_some()
        || c.bytes_sent.is_some()
        || c.bytes_received.is_some()
        || c.rtt_ms.is_some()
        || c.bandwidth_mbps.is_some()
        || c.negotiated_latency_ms.is_some()
}

fn get_u64(s: &gst::Structure, keys: &[&str]) -> Option<u64> {
    for &key in keys {
        if let Ok(v) = s.get::<u64>(key) {
            return Some(v);
        }
        if let Ok(v) = s.get::<i64>(key) {
            return Some(v.max(0) as u64);
        }
        if let Ok(v) = s.get::<u32>(key) {
            return Some(v as u64);
        }
        if let Ok(v) = s.get::<i32>(key) {
            return Some(v.max(0) as u64);
        }
    }
    None
}

fn get_u32(s: &gst::Structure, keys: &[&str]) -> Option<u32> {
    for &key in keys {
        if let Ok(v) = s.get::<u32>(key) {
            return Some(v);
        }
        if let Ok(v) = s.get::<i32>(key) {
            return Some(v.max(0) as u32);
        }
        if let Ok(v) = s.get::<u64>(key) {
            return Some(v.min(u32::MAX as u64) as u32);
        }
    }
    None
}

fn get_f64(s: &gst::Structure, keys: &[&str]) -> Option<f64> {
    for &key in keys {
        if let Ok(v) = s.get::<f64>(key) {
            return Some(v);
        }
        if let Ok(v) = s.get::<f32>(key) {
            return Some(v as f64);
        }
    }
    None
}

/// Read the `caller-address` field as a `GInetSocketAddress` and format as `ip:port`.
/// The SRT plugin exposes the peer address as a `GSocketAddress` (a `GObject`),
/// not a string, so `Structure::get::<String>` fails silently.
fn read_caller_address(s: &gst::Structure) -> Option<String> {
    let value = s.value("caller-address").ok()?;
    let sock_addr = value.get::<gio::SocketAddress>().ok()?;
    let inet = sock_addr.downcast::<gio::InetSocketAddress>().ok()?;
    let addr = inet.address().to_str();
    let port = inet.port();
    Some(format!("{}:{}", addr, port))
}

fn get_string(s: &gst::Structure, keys: &[&str]) -> Option<String> {
    for &key in keys {
        if let Ok(v) = s.get::<String>(key) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        let _ = gst::init();
    }

    /// Build a GstStructure that looks like libsrt's `mbStats` output for a
    /// single caller-mode srtsink — i.e. populated sender-side fields with
    /// SRT-plugin field names and integer widths matching gst-plugins-bad.
    fn sender_caller_structure() -> gst::Structure {
        gst::Structure::builder("application/x-srt-statistics")
            .field("packets-sent", 1000i64)
            .field("packets-sent-lost", 5i32)
            .field("packets-retransmitted", 5i32)
            .field("packets-sent-dropped", 1i32)
            .field("bytes-sent", 250_000u64)
            .field("send-rate-mbps", 4.5f64)
            .field("snd-buf-level-ms", 30i32)
            .field("negotiated-latency-ms", 125i32)
            .field("bandwidth-mbps", 1000.0f64)
            .field("rtt-ms", 5.5f64)
            .build()
    }

    /// Build a GstStructure that looks like libsrt's stats for a caller-mode srtsrc.
    fn receiver_caller_structure() -> gst::Structure {
        gst::Structure::builder("application/x-srt-statistics")
            .field("packets-received", 2000i64)
            .field("packets-received-lost", 0i32)
            .field("packets-received-retransmitted", 12i32)
            .field("packets-received-dropped", 0i32)
            .field("bytes-received", 500_000u64)
            .field("receive-rate-mbps", 4.8f64)
            .field("recv-buf-level-ms", 80i32)
            .field("negotiated-latency-ms", 125i32)
            .field("bandwidth-mbps", 1000.0f64)
            .field("rtt-ms", 4.9f64)
            .build()
    }

    #[test]
    fn parses_sender_caller_fields() {
        init();
        let parsed = parse_caller_stats(&sender_caller_structure());

        // Sender fields populated.
        assert_eq!(parsed.packets_sent, Some(1000));
        assert_eq!(parsed.packets_sent_lost, Some(5));
        assert_eq!(parsed.packets_retransmitted, Some(5));
        assert_eq!(parsed.packets_sent_dropped, Some(1));
        assert_eq!(parsed.bytes_sent, Some(250_000));
        assert_eq!(parsed.send_rate_mbps, Some(4.5));
        assert_eq!(parsed.snd_buf_level_ms, Some(30));

        // Link metrics populated.
        assert_eq!(parsed.negotiated_latency_ms, Some(125));
        assert_eq!(parsed.bandwidth_mbps, Some(1000.0));
        assert_eq!(parsed.rtt_ms, Some(5.5));

        // Receiver fields stay None — that's the whole point of the split.
        assert!(parsed.packets_received.is_none());
        assert!(parsed.packets_received_lost.is_none());
        assert!(parsed.bytes_received.is_none());
        assert!(parsed.recv_rate_mbps.is_none());
        assert!(parsed.recv_buf_level_ms.is_none());
        assert!(parsed.packets_received_retransmitted.is_none());
        assert!(parsed.packets_received_dropped.is_none());
    }

    #[test]
    fn parses_receiver_caller_fields() {
        init();
        let parsed = parse_caller_stats(&receiver_caller_structure());

        // Receiver fields populated.
        assert_eq!(parsed.packets_received, Some(2000));
        assert_eq!(parsed.packets_received_lost, Some(0));
        assert_eq!(parsed.packets_received_retransmitted, Some(12));
        assert_eq!(parsed.packets_received_dropped, Some(0));
        assert_eq!(parsed.bytes_received, Some(500_000));
        assert_eq!(parsed.recv_rate_mbps, Some(4.8));
        assert_eq!(parsed.recv_buf_level_ms, Some(80));

        // Sender fields stay None.
        assert!(parsed.packets_sent.is_none());
        assert!(parsed.packets_sent_lost.is_none());
        assert!(parsed.packets_retransmitted.is_none());
        assert!(parsed.packets_sent_dropped.is_none());
        assert!(parsed.bytes_sent.is_none());
        assert!(parsed.send_rate_mbps.is_none());
        assert!(parsed.snd_buf_level_ms.is_none());
    }

    #[test]
    fn empty_structure_parses_to_default() {
        init();
        let empty = gst::Structure::new_empty("application/x-srt-statistics");
        let parsed = parse_caller_stats(&empty);
        assert!(parsed.address.is_none());
        assert!(parsed.packets_sent.is_none());
        assert!(parsed.packets_received.is_none());
        assert!(parsed.rtt_ms.is_none());
    }

    #[test]
    fn reads_callers_from_gst_array() {
        // Listener-mode srtsink puts per-caller stats in a `callers` value array
        // alongside top-level totals. The real plugin uses GValueArray (which
        // is !Send and therefore awkward to build through the typed Structure
        // builder in a test), but `read_callers_array` also handles gst::Array
        // and gst::List — and the downstream extraction logic is identical, so
        // we cover that path here. The GValueArray path is exercised by manual
        // integration testing against a running pipeline.
        init();
        let caller1 = sender_caller_structure();
        let caller2 = gst::Structure::builder("application/x-srt-statistics")
            .field("packets-sent", 50i64)
            .field("bytes-sent", 12_000u64)
            .field("rtt-ms", 2.1f64)
            .build();

        let callers_value = gst::Array::new([caller1.to_send_value(), caller2.to_send_value()]);
        let listener_stats = gst::Structure::builder("application/x-srt-statistics")
            .field("bytes-sent", 262_000u64)
            .field("callers", callers_value)
            .build();

        let callers = read_callers_array(&listener_stats).expect("callers parsed");
        assert_eq!(callers.len(), 2);
        assert_eq!(callers[0].packets_sent, Some(1000));
        assert_eq!(callers[0].rtt_ms, Some(5.5));
        assert_eq!(callers[1].packets_sent, Some(50));
        assert_eq!(callers[1].bytes_sent, Some(12_000));
    }

    #[test]
    fn empty_callers_array_does_not_fall_through_to_single_peer() {
        // Listener-mode element with no peer attached exposes `callers` as an
        // empty array. The previous behaviour fell through to single-peer
        // parsing and pushed a phantom caller using the top-level link metrics
        // (which libsrt populates eagerly before any handshake) — the inline
        // peer count in the graph then read "1p connected" on an idle listener.
        init();
        let callers_value = gst::Array::new(std::iter::empty::<gst::glib::SendValue>());
        let listener_stats = gst::Structure::builder("application/x-srt-statistics")
            .field("bandwidth-mbps", 1000.0f64)
            .field("negotiated-latency-ms", 125i32)
            .field("callers", callers_value)
            .build();

        // We can't easily build a real srtsrc element here, but the salient
        // logic lives in the parse branch. Drive it directly via the helpers.
        let callers = read_callers_array(&listener_stats).expect("callers field present");
        assert!(callers.is_empty(), "no peer attached → empty callers");

        // The fall-through path would have produced this phantom caller; the
        // production code must NOT execute it when `callers` is present.
        let phantom = parse_caller_stats(&listener_stats);
        assert!(
            has_any_data(&phantom),
            "top-level link metrics make has_any_data true — exactly the trap we are avoiding"
        );
        assert!(
            !caller_is_active(&phantom),
            "phantom has no packet/byte counters, so it would render as 'waiting' but still bump the peer count"
        );
    }

    #[test]
    fn reads_caller_address_from_inet_socket_address() {
        init();
        let inet = gio::InetAddress::from_string("198.51.100.7").expect("parsed");
        let sock = gio::InetSocketAddress::new(&inet, 9000);
        let s = gst::Structure::builder("application/x-srt-statistics")
            .field("caller-address", &sock)
            .field("packets-sent", 10i64)
            .build();

        let parsed = parse_caller_stats(&s);
        assert_eq!(parsed.address.as_deref(), Some("198.51.100.7:9000"));
        assert_eq!(parsed.packets_sent, Some(10));
    }

    #[test]
    fn integer_width_aliases_are_handled() {
        // libsrt fields use a mix of int32/int64/uint64 across versions and
        // plugin updates. The get_u64 helper should accept all of them.
        init();
        let s = gst::Structure::builder("application/x-srt-statistics")
            // sent as i32 (older versions used GInt for some counters)
            .field("packets-sent", 7i32)
            // received as u64
            .field("packets-received", 99u64)
            // negotiated-latency-ms as i32 (canonical)
            .field("negotiated-latency-ms", 250i32)
            .build();
        let parsed = parse_caller_stats(&s);
        assert_eq!(parsed.packets_sent, Some(7));
        assert_eq!(parsed.packets_received, Some(99));
        assert_eq!(parsed.negotiated_latency_ms, Some(250));
    }

    #[test]
    fn caller_is_active_counts_only_real_activity() {
        // The `connected` flag is the user-visible chip in the UI, so the
        // heuristic that backs it deserves explicit coverage. A caller with
        // packets or bytes flowing in either direction is active; one with
        // only link metrics filled in is not (libsrt populates rtt-ms /
        // bandwidth-mbps eagerly even before the first buffer).
        assert!(!caller_is_active(&SrtCallerStats::default()));

        let link_only = SrtCallerStats {
            rtt_ms: Some(2.0),
            bandwidth_mbps: Some(500.0),
            ..Default::default()
        };
        assert!(
            !caller_is_active(&link_only),
            "link-only metrics shouldn't imply activity"
        );

        let sent_some = SrtCallerStats {
            packets_sent: Some(10),
            ..link_only.clone()
        };
        assert!(caller_is_active(&sent_some));

        let recv_some = SrtCallerStats {
            bytes_received: Some(1),
            ..Default::default()
        };
        assert!(caller_is_active(&recv_some));
    }

    #[test]
    fn has_any_data_detects_populated_callers() {
        // Top-level fall-through path in `collect_element_stats` only inserts
        // a single-caller entry when the top-level structure has at least one
        // recognised field. This test guards that decision.
        assert!(!has_any_data(&SrtCallerStats::default()));

        let only_link = SrtCallerStats {
            rtt_ms: Some(1.0),
            ..Default::default()
        };
        assert!(has_any_data(&only_link));

        let only_bytes = SrtCallerStats {
            bytes_sent: Some(42),
            ..Default::default()
        };
        assert!(has_any_data(&only_bytes));

        let only_address = SrtCallerStats {
            address: Some("198.51.100.5:5000".to_string()),
            ..Default::default()
        };
        assert!(
            !has_any_data(&only_address),
            "address alone is not enough — we'd display an empty caller row"
        );
    }
}
