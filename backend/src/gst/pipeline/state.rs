use super::PipelineManager;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::flow::ThreadPriorityStatus;
use strom_types::{FlowId, PipelineState};
use tracing::info;

impl PipelineManager {
    /// Get the current cached state of the pipeline.
    pub fn get_state(&self) -> PipelineState {
        *self.cached_state.read().unwrap()
    }

    /// Get the flow ID this pipeline manages.
    pub fn flow_id(&self) -> FlowId {
        self.flow_id
    }

    /// Get runtime dynamic pads that have been auto-linked to tees.
    /// Returns a map of element_id -> {pad_name -> tee_element_name}
    /// These are pads that appeared at runtime without defined links.
    pub fn get_dynamic_pads(&self) -> HashMap<String, HashMap<String, String>> {
        self.dynamic_pad_tees
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Get the thread priority status for this pipeline.
    /// Returns None if pipeline hasn't been started yet.
    pub fn get_thread_priority_status(&self) -> Option<ThreadPriorityStatus> {
        self.thread_priority_state
            .as_ref()
            .map(|state| state.get_status())
    }

    /// Get the clock synchronization status for this pipeline.
    pub fn get_clock_sync_status(&self) -> strom_types::flow::ClockSyncStatus {
        use strom_types::flow::{ClockSyncStatus, GStreamerClockType};

        match self.properties.clock_type {
            GStreamerClockType::Ptp => {
                // For PTP clocks, use the stored PTP clock reference for accurate sync status
                // This ensures consistency with get_ptp_info()
                if let Some(ref ptp_clock) = self.ptp_clock {
                    if ptp_clock.is_synced() {
                        ClockSyncStatus::Synced
                    } else {
                        ClockSyncStatus::NotSynced
                    }
                } else {
                    // No PTP clock stored (shouldn't happen if properly configured)
                    ClockSyncStatus::NotSynced
                }
            }
            GStreamerClockType::Ntp => {
                // For NTP clocks, use the is_synced() method from gst::Clock trait
                // Note: NTP clock not fully implemented yet, so this is best-effort
                if let Some(clock) = self.pipeline.clock() {
                    if clock.is_synced() {
                        ClockSyncStatus::Synced
                    } else {
                        ClockSyncStatus::NotSynced
                    }
                } else {
                    ClockSyncStatus::NotSynced
                }
            }
            _ => {
                // For other clock types (Monotonic, Realtime, PipelineDefault),
                // sync status is not applicable - these are local clocks
                ClockSyncStatus::Unknown
            }
        }
    }

    /// Get detailed PTP clock information.
    /// Returns None if the pipeline is not using a PTP clock.
    pub fn get_ptp_info(&self) -> Option<strom_types::flow::PtpInfo> {
        use strom_types::flow::PtpInfo;

        // Use the stored PTP clock reference
        let ptp_clock = self.ptp_clock.as_ref()?;

        // Get the actual domain from the running clock (not from saved properties)
        let actual_domain = ptp_clock.domain() as u8;
        let gm_id = ptp_clock.grandmaster_clock_id();
        let master_id = ptp_clock.master_clock_id();

        // Only include IDs if they're non-zero (indicating valid data)
        let grandmaster_clock_id = if gm_id != 0 {
            Some(PtpInfo::format_clock_id(gm_id))
        } else {
            None
        };
        let master_clock_id = if master_id != 0 {
            Some(PtpInfo::format_clock_id(master_id))
        } else {
            None
        };

        // Check if configured domain differs from running domain
        let configured_domain = self.properties.ptp_domain.unwrap_or(0);
        let restart_needed = configured_domain != actual_domain;

        // Get statistics from the stored stats
        let stats = self.ptp_stats.read().ok().and_then(|guard| guard.clone());

        // Get sync status directly from the PTP clock
        let synced = ptp_clock.is_synced();

        Some(PtpInfo {
            domain: actual_domain,
            synced,
            grandmaster_clock_id,
            master_clock_id,
            restart_needed,
            stats,
        })
    }

    /// Get the underlying GStreamer pipeline (for debugging).
    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    /// Get the GStreamer element for a strom element_id (standalone elements only).
    pub fn find_gst_element(&self, element_id: &str) -> Option<&gst::Element> {
        self.elements.get(element_id)
    }

    /// Get all GStreamer elements belonging to a block ID.
    /// Returns elements whose key starts with "block_id:".
    pub fn find_block_elements(&self, block_id: &str) -> Vec<(&str, &gst::Element)> {
        let prefix = format!("{}:", block_id);
        self.elements
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }

    /// Look up the strom element_id for a GStreamer element name.
    ///
    /// GStreamer element names are set by us to match element_ids during
    /// construction, but some internal elements (auto-inserted tees, block
    /// sub-elements) may differ. Returns `None` if there is no match.
    pub fn element_id_for_gst_name<'a>(&'a self, gst_name: &'a str) -> Option<&'a str> {
        // The elements map is keyed by element_id and the GStreamer element
        // name is set to the element_id during construction, so a simple
        // key lookup usually works.
        if self.elements.contains_key(gst_name) {
            return Some(gst_name);
        }
        // Fallback: linear scan comparing GStreamer element names
        for (id, el) in &self.elements {
            if el.name().as_str() == gst_name {
                return Some(id.as_str());
            }
        }
        None
    }

    /// Set the thread registry for tracking streaming threads.
    ///
    /// This should be called before start() to enable thread CPU monitoring.
    pub fn set_thread_registry(&mut self, registry: crate::thread_registry::ThreadRegistry) {
        self.thread_registry = Some(registry);
    }

    /// Set the assigned CPU set for SingleCore affinity.
    ///
    /// This should be called before start() so the thread priority handler
    /// can pin threads to the correct physical core. The vec contains all
    /// logical CPUs (hyperthreads) of the physical core.
    pub fn set_assigned_cpus(&mut self, cpus: Option<Vec<usize>>) {
        self.assigned_cpus = cpus;
    }

    /// Get the probe manager (read-only access).
    pub fn probe_manager(&self) -> &crate::gst::buffer_age_probe::ProbeManager {
        &self.probe_manager
    }

    /// Get WHEP endpoints registered by blocks in this pipeline.
    pub fn whep_endpoints(&self) -> &[crate::blocks::WhepEndpointInfo] {
        &self.whep_endpoints
    }

    /// Get WHIP endpoints registered by blocks in this pipeline.
    pub fn whip_endpoints(&self) -> &[crate::blocks::WhipEndpointInfo] {
        &self.whip_endpoints
    }

    /// Take WHIP endpoint configs for session manager registration.
    /// This consumes the configs (they are moved to the session manager).
    pub fn take_whip_endpoint_configs(
        &mut self,
    ) -> Vec<(String, crate::whip_session_manager::WhipEndpointConfig)> {
        std::mem::take(&mut self.whip_endpoint_configs)
    }

    /// Get a weak reference to this pipeline's GStreamer pipeline.
    pub fn pipeline_weak(&self) -> gst::glib::WeakRef<gst::Pipeline> {
        use gstreamer::prelude::*;
        self.pipeline.downgrade()
    }

    /// Generate a DOT graph of the pipeline for debugging.
    /// Returns the DOT graph content as a string.
    pub fn generate_dot_graph(&self) -> String {
        use gst::prelude::*;

        info!("Generating DOT graph for pipeline: {}", self.flow_name);

        // Use GStreamer's debug graph dump functionality
        // The details level determines how much information is included
        let dot = self
            .pipeline
            .debug_to_dot_data(gst::DebugGraphDetails::all());

        wrap_dot_labels(&dot, 80)
    }

    /// Get debug information about the pipeline.
    /// Provides timing, clock, latency, and state information for troubleshooting.
    pub fn get_debug_info(&self) -> strom_types::api::FlowDebugInfo {
        use gst::prelude::*;
        use strom_types::api::FlowDebugInfo;
        use strom_types::flow::GStreamerClockType;

        // Get pipeline clock
        let clock = self.pipeline.clock();

        // Get base_time and clock_time
        let base_time = self.pipeline.base_time();
        let clock_time: Option<gst::ClockTime> = clock.as_ref().map(|c| c.time());

        // Calculate running_time = clock_time - base_time
        let running_time: Option<gst::ClockTime> = match (clock_time, base_time) {
            (Some(ct), Some(bt)) if ct >= bt => Some(ct - bt),
            _ => None,
        };

        // Get clock type description
        let clock_type = match self.properties.clock_type {
            GStreamerClockType::Ptp => Some("PTP".to_string()),
            GStreamerClockType::Monotonic => Some("Monotonic".to_string()),
            GStreamerClockType::Realtime => Some("Realtime".to_string()),
            GStreamerClockType::Ntp => Some("NTP".to_string()),
        };

        // Get PTP grandmaster if using PTP clock
        let ptp_grandmaster = self.ptp_clock.as_ref().and_then(|ptp| {
            let gm_id = ptp.grandmaster_clock_id();
            if gm_id != 0 {
                Some(strom_types::flow::PtpInfo::format_clock_id(gm_id))
            } else {
                None
            }
        });

        // Get pipeline state
        let (_, state, _) = self.pipeline.state(gst::ClockTime::ZERO);
        let pipeline_state = Some(format!("{:?}", state));

        // Query latency
        let (latency_min_ns, latency_max_ns, is_live) = self
            .query_latency()
            .map(|(min, max, live)| (Some(min), Some(max), Some(live)))
            .unwrap_or((None, None, None));

        // Count elements
        let element_count = Some(self.elements.len() as u32);

        // Helper to format nanoseconds as duration
        fn format_duration(ns: u64) -> String {
            let secs = ns as f64 / 1_000_000_000.0;
            if secs < 1.0 {
                format!("{:.2} ms", secs * 1000.0)
            } else if secs < 60.0 {
                format!("{:.3} s", secs)
            } else if secs < 3600.0 {
                format!("{:.1} min", secs / 60.0)
            } else {
                format!("{:.2} h", secs / 3600.0)
            }
        }

        // Format latency
        let latency_formatted = match (latency_min_ns, latency_max_ns) {
            (Some(min), Some(max)) if min == max => Some(format_duration(min)),
            (Some(min), Some(max)) => Some(format!(
                "{} - {}",
                format_duration(min),
                format_duration(max)
            )),
            _ => None,
        };

        FlowDebugInfo {
            flow_id: self.flow_id,
            flow_name: self.flow_name.clone(),
            pipeline_state,
            is_live,
            base_time_ns: base_time.map(|t| t.nseconds()),
            clock_time_ns: clock_time.map(|t| t.nseconds()),
            running_time_ns: running_time.map(|t| t.nseconds()),
            running_time_formatted: running_time.map(|t| format_duration(t.nseconds())),
            clock_type,
            ptp_grandmaster,
            latency_min_ns,
            latency_max_ns,
            latency_formatted,
            element_count,
        }
    }
}

/// Wrap long property values inside DOT graph labels so that graphviz
/// produces narrower nodes. GStreamer DOT labels use `\n` as line separator
/// inside quoted `label="..."` strings.
///
/// Strategy:
/// - `caps=` values are split at `;` (caps alternatives) and then at `, `
///   if individual alternatives still exceed `max_width`.
/// - All other property values that exceed `max_width` are truncated with `…`.
fn wrap_dot_labels(dot: &str, max_width: usize) -> String {
    let mut result = String::with_capacity(dot.len());
    let mut remaining = dot;

    while let Some(label_start) = remaining.find("label=\"") {
        // Copy everything before this label
        let before_label = &remaining[..label_start];
        result.push_str(before_label);
        result.push_str("label=\"");
        remaining = &remaining[label_start + 7..]; // skip past label="

        // Find the closing quote (not preceded by backslash)
        let label_end = find_closing_quote(remaining);
        let label_content = &remaining[..label_end];
        remaining = &remaining[label_end..]; // keep the closing "

        // Edge labels (caps between elements) contain negotiated caps info
        // that is very useful to see in full — don't truncate them.
        // Edges have "->" before the label, nodes don't.
        let is_edge = before_label
            .rfind('\n')
            .map(|nl| before_label[nl..].contains("->"))
            .unwrap_or_else(|| before_label.contains("->"));

        // Also skip truncation for the legend node (Element-States key).
        let is_legend = label_content.contains("Element-States:");

        if is_edge || is_legend {
            result.push_str(label_content);
        } else {
            // Split label into logical property lines, respecting escaped quotes.
            // GStreamer DOT labels use \n to separate properties, but property
            // values can contain escaped quotes with \n inside them, e.g.:
            //   pem=\"-----BEGIN CERT-----\\nMIIC...\\n-----END CERT-----\\n\"
            // We must treat such a quoted value as part of one property line.
            let lines = split_dot_label_lines(label_content);
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    result.push_str("\\n");
                }
                result.push_str(&wrap_dot_property_line(line, max_width));
            }
        }
    }

    // Copy the remainder
    result.push_str(remaining);
    result
}

/// Find the position of the closing `"` for a DOT label, accounting for
/// escaped quotes (`\"`).
fn find_closing_quote(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            return i;
        }
        i += 1;
    }
    s.len()
}

/// Split DOT label content into logical property lines.
///
/// Naive splitting on `\n` breaks when a property value contains escaped
/// quotes with `\n` inside, e.g. `pem=\"...\\n...\\n\"`. This function
/// tracks whether we are inside an escaped-quote string and only splits
/// on `\n` that are outside such strings.
fn split_dot_label_lines(label: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let bytes = label.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut in_escaped_quote = false;

    while i < len {
        // Check for escaped quote: \"
        if i + 1 < len && bytes[i] == b'\\' && bytes[i + 1] == b'"' {
            in_escaped_quote = !in_escaped_quote;
            current.push('\\');
            current.push('"');
            i += 2;
            continue;
        }

        // Check for \n (literal backslash + n in the string)
        if i + 1 < len && bytes[i] == b'\\' && bytes[i + 1] == b'n' {
            if in_escaped_quote {
                // Inside a quoted value — keep the \n as-is
                current.push('\\');
                current.push('n');
            } else {
                // Property separator — start a new line
                lines.push(current);
                current = String::new();
            }
            i += 2;
            continue;
        }

        current.push(bytes[i] as char);
        i += 1;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Truncate a single property line from a DOT label if it exceeds max_width.
fn wrap_dot_property_line(line: &str, max_width: usize) -> String {
    if line.len() <= max_width {
        return line.to_string();
    }
    truncate_str(line, max_width)
}

/// Truncate a string to `max_len` characters, appending `…` if shortened.
///
/// Handles DOT escape sequences correctly:
/// - Won't cut between `\` and the next character (would break escape sequences)
/// - Removes embedded `\n` so graphviz doesn't create extra line breaks
/// - Closes unclosed escaped quotes (`\"`) so the DOT label stays valid
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    // Reserve 3 bytes for the UTF-8 ellipsis character '…'
    let mut end = max_len.saturating_sub(3);
    // Don't cut in the middle of a UTF-8 char
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // Don't cut between \ and the next character (would break escape sequences)
    if end > 0 && s.as_bytes()[end - 1] == b'\\' {
        end -= 1;
    }

    // Remove embedded \n sequences that graphviz would interpret as line breaks.
    // This also handles \\n (literal backslash + newline in DOT) which is still
    // a line break — the remaining \ is harmless in the displayed label.
    let truncated = s[..end].replace("\\n", " ");

    // Count escaped quotes (\") to detect unclosed quoted values.
    // If we cut inside a quoted value like pem=\"...long..., the opening \"
    // has no matching close, corrupting the DOT label structure.
    let mut escaped_quotes = 0usize;
    let bytes = truncated.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
            escaped_quotes += 1;
            i += 2;
        } else {
            i += 1;
        }
    }

    if escaped_quotes % 2 == 1 {
        // Unclosed escaped quote — close it before the ellipsis
        format!("{}\\\"…", truncated)
    } else {
        format!("{}…", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_label_unchanged() {
        let dot = r#"node [label="GstElement\nname\n[>]"];"#;
        assert_eq!(wrap_dot_labels(dot, 80), dot);
    }

    #[test]
    fn test_caps_truncated() {
        let dot = r#"label="GstCapsFilter\ncapsfilter4\n[>]\ncaps=video/x-h264, parsed=(boolean)true; video/x-h264, alignment=(string)au; video/x-h264, parsed=(boolean)true""#;
        let result = wrap_dot_labels(dot, 80);
        // Caps line should be truncated to max_width
        assert!(result.contains("caps=video/x-h264, parsed=(boolean)true; video/x-h264"));
        assert!(result.contains("…"));
    }

    #[test]
    fn test_long_non_caps_property_truncated() {
        let long_cert = format!("certificate={}", "A".repeat(200));
        let dot = format!(r#"label="Element\nname\n{}""#, long_cert);
        let result = wrap_dot_labels(&dot, 80);
        assert!(result.contains("…"));
        assert!(!result.contains(&"A".repeat(200)));
    }

    #[test]
    fn test_no_label_unchanged() {
        let dot = r#"digraph { a -> b; }"#;
        assert_eq!(wrap_dot_labels(dot, 80), dot);
    }

    #[test]
    fn test_escaped_quotes_in_label() {
        let dot = r#"label="foo=\"bar\"\nname""#;
        let result = wrap_dot_labels(dot, 80);
        assert!(result.contains(r#"foo=\"bar\""#));
    }

    #[test]
    fn test_realistic_capsfilter_truncated() {
        let caps = "caps=video/x-h264, stream-format=(string){ avc, avc3, byte-stream }, \
            alignment=(string)au, profile=(string){ high, main }; \
            video/x-h264, alignment=(string)au";
        let dot = format!(r#"label="GstCapsFilter\ncapsfilter4\n[>]\n{}""#, caps);
        let result = wrap_dot_labels(&dot, 80);

        // The caps line should be truncated, not wrapped
        let label_start = result.find("label=\"").unwrap() + 7;
        let label_end = find_closing_quote(&result[label_start..]);
        let label = &result[label_start..label_start + label_end];
        let lines: Vec<&str> = label.split("\\n").collect();
        // Should still be 4 lines (type, name, state, caps) — not more
        assert_eq!(lines.len(), 4, "Expected 4 lines, got {:?}", lines);
        assert!(lines[3].len() <= 80);
        assert!(lines[3].ends_with('…'));
    }

    #[test]
    fn test_extensions_property_truncated() {
        let extensions = format!(
            "extensions=< (GstRTPHeaderExtensionTWCC) twcc, {} >",
            "(GstRTPHeaderExtensionColorspace) colorspace, ".repeat(10)
        );
        let dot = format!(r#"label="GstRtpBin\nrtpbin\n[>]\n{}""#, extensions);
        let result = wrap_dot_labels(&dot, 80);
        assert!(result.contains("…"), "Long extensions should be truncated");
    }

    #[test]
    fn test_multiple_labels_in_dot() {
        let dot = r#"node1 [label="Short\nlabel"]; node2 [label="Long\ncaps=a/b; c/d; e/f, x=(int)1, y=(int)2, z=(int)3, w=(int)4, v=(int)5, u=(int)6, t=(int)7"];"#;
        let result = wrap_dot_labels(dot, 40);
        // Both labels should be processed
        assert!(result.contains("Short"));
        assert!(result.contains("caps=a/b"));
        // The long caps line should be truncated
        assert!(result.contains("…"));
    }

    #[test]
    fn test_pem_certificate_truncated() {
        // Simulate GStreamer DOT format: pem value uses escaped quotes and
        // contains literal \n inside the quoted string
        let dot = r#"label="GstDtlsDec\ndtlsdec1\n[>]\npem=\"-----BEGIN CERTIFICATE-----\\nMIICpzCCAY+gAwIBAgIJA\\nAAAABBBBCCCC\\n-----END CERTIFICATE-----\\n\"\npeer-pem=\"-----BEGIN CERTIFICATE-----\\nMIIBFjCBvaADAgEC\\n-----END CERTIFICATE-----\\n\"\nconnection-state=connected""#;

        let result = wrap_dot_labels(dot, 80);

        // pem= should be truncated to one short line, not expanded into many
        assert!(result.contains("pem="), "pem property should still exist");
        assert!(
            result.contains("connection-state=connected"),
            "Properties after pem should survive"
        );

        // Count lines — should be type + name + state + pem(truncated) +
        // peer-pem(truncated) + connection-state = 6
        let label_start = result.find("label=\"").unwrap() + 7;
        let label_end = find_closing_quote(&result[label_start..]);
        let label = &result[label_start..label_start + label_end];
        let lines: Vec<&str> = label.split("\\n").collect();
        assert_eq!(lines.len(), 6, "Expected 6 lines, got {:?}", lines);
        // The pem line must be truncated
        assert!(lines[3].len() <= 80, "pem line too long: {}", lines[3]);
        assert!(lines[3].contains("…"));
    }

    #[test]
    fn test_split_dot_label_lines_respects_escaped_quotes() {
        // \n inside escaped quotes should NOT split
        let label = r#"name\npem=\"cert\\ndata\\nmore\"\nstate=ok"#;
        let lines = split_dot_label_lines(label);
        assert_eq!(lines.len(), 3, "Got {:?}", lines);
        assert_eq!(lines[0], "name");
        assert!(lines[1].starts_with("pem="));
        assert!(lines[1].contains(r#"\\n"#), "Inner \\n should be preserved");
        assert_eq!(lines[2], "state=ok");
    }

    #[test]
    fn test_truncate_closes_unclosed_escaped_quote() {
        // Truncating inside an escaped quoted value must close the quote
        // to prevent corrupting the DOT label structure.
        let line = r#"pem=\"-----BEGIN CERTIFICATE-----\\nMIICpzCCAY+gAwIBAgIJA\\nAAAABBBBCCCC\\n-----END CERTIFICATE-----\\n\""#;
        let result = truncate_str(line, 60);
        assert!(result.contains("…"), "Should be truncated");
        // Must contain a closing \" before the ellipsis
        assert!(
            result.ends_with("\\\"…"),
            "Unclosed escaped quote should be closed, got: {}",
            result
        );
    }

    #[test]
    fn test_truncate_no_extra_close_when_quotes_balanced() {
        // When escaped quotes are balanced, don't add an extra closing quote
        let line =
            r#"key=\"short\" and more padding to exceed the limit for truncation testing here"#;
        let result = truncate_str(line, 60);
        assert!(result.contains("…"), "Should be truncated");
        assert!(
            !result.ends_with("\\\"…"),
            "Balanced quotes should not get extra close, got: {}",
            result
        );
    }

    #[test]
    fn test_edge_label_caps_not_truncated() {
        // Edge labels contain negotiated caps — these must not be truncated
        let caps = "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, framerate=(fraction)25/1, multiview-mode=(string)mono";
        let dot = format!(r#"element0:src -> element1:sink [label="{}"];"#, caps);
        let result = wrap_dot_labels(&dot, 80);
        // The full caps string should be preserved
        assert!(
            result.contains(caps),
            "Edge caps should not be truncated, got: {}",
            result
        );
        assert!(!result.contains('…'), "Edge label should not have ellipsis");
    }

    #[test]
    fn test_node_label_still_truncated() {
        // Node labels should still be truncated as before
        let long_prop = format!("property={}", "x".repeat(200));
        let dot = format!(
            r#"element0 [label="GstElement\nname\n[>]\n{}"];"#,
            long_prop
        );
        let result = wrap_dot_labels(&dot, 80);
        assert!(result.contains('…'), "Node property should be truncated");
        assert!(!result.contains(&"x".repeat(200)));
    }

    #[test]
    fn test_mixed_nodes_and_edges() {
        let long_caps = format!("video/x-raw, {}", "key=(int)1, ".repeat(20));
        let long_prop = format!("extensions={}", "A".repeat(200));
        let dot = format!(
            r#"n1 [label="Type\nname\n[>]\n{}"]; n1:src -> n2:sink [label="{}"]; n2 [label="Type2\nname2"];"#,
            long_prop, long_caps
        );
        let result = wrap_dot_labels(&dot, 80);
        // Node property should be truncated
        assert!(
            result.contains("extensions=A"),
            "Node property should exist"
        );
        assert!(
            !result.contains(&"A".repeat(200)),
            "Node property should be truncated"
        );
        // Edge caps should be preserved in full
        assert!(result.contains(&long_caps), "Edge caps should be preserved");
    }

    #[test]
    fn test_legend_not_truncated() {
        let legend =
            "Element-States: [~] void-pending, [0] null, [-] ready, [=] paused, [>] playing";
        let dot = format!(r#"legend [label="{}"];"#, legend);
        let result = wrap_dot_labels(&dot, 80);
        assert!(
            result.contains(legend),
            "Legend should not be truncated, got: {}",
            result
        );
    }

    #[test]
    fn test_truncate_does_not_cut_on_backslash() {
        // Should not cut between \ and the next character.
        // Build a string where byte at position (max_len - 3 - 1) is a backslash.
        // With max_len=20, end starts at 17, so byte 16 must be '\'.
        let line = r"aaaaaaaaaaaaaaaa\bcccccccccc";
        assert_eq!(line.as_bytes()[16], b'\\');
        let result = truncate_str(line, 20);
        assert!(result.contains('…'));
        // Should truncate to 16 bytes (before the backslash), not 17
        assert_eq!(result, "aaaaaaaaaaaaaaaa…");
    }
}
