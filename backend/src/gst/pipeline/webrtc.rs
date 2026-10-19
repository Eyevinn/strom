use super::PipelineManager;
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, trace, warn};

impl PipelineManager {
    /// Query the latency of the pipeline.
    /// Returns (min_latency_ns, max_latency_ns, live) if query succeeds.
    ///
    /// This method tries multiple approaches to get meaningful latency:
    /// 1. Pipeline-level latency query (best for live pipelines)
    /// 2. If pipeline returns 0, try querying individual sink elements
    pub fn query_latency(&self) -> Option<(u64, u64, bool)> {
        let mut query = gst::query::Latency::new();

        if self.pipeline.query(&mut query) {
            let (live, min, max) = query.result();
            let min_ns = min.nseconds();
            let max_ns = max.map_or(u64::MAX, |t| t.nseconds());
            trace!(
                "Pipeline '{}' latency query: live={}, min={}ns, max={}ns",
                self.flow_name,
                live,
                min_ns,
                max_ns
            );

            // If pipeline is live and has meaningful latency, use it
            if live && min_ns > 0 {
                return Some((min_ns, max_ns, live));
            }

            // For non-live pipelines or if latency is 0, try to get latency from sink elements
            // This gives more useful information for streaming pipelines
            let sink_latency = self.query_sink_latency();
            if let Some((sink_min, sink_max)) = sink_latency {
                if sink_min > 0 {
                    trace!(
                        "Pipeline '{}' using sink latency: min={}ns, max={}ns",
                        self.flow_name,
                        sink_min,
                        sink_max
                    );
                    return Some((sink_min, sink_max, live));
                }
            }

            // Return pipeline values even if 0 (user sees it's not live)
            Some((min_ns, max_ns, live))
        } else {
            trace!(
                "Pipeline '{}' latency query failed (may not be in playing state)",
                self.flow_name
            );
            None
        }
    }

    /// Query latency from sink elements in the pipeline.
    /// This is useful for non-live pipelines where the pipeline-level query returns 0.
    fn query_sink_latency(&self) -> Option<(u64, u64)> {
        let mut total_latency: u64 = 0;

        // Iterate over all elements and find sinks (elements with sink pads but no src pads)
        for (element_id, element) in &self.elements {
            // Check if this is a sink element by looking at pads
            let has_sink_pad = element.static_pad("sink").is_some()
                || element
                    .iterate_sink_pads()
                    .into_iter()
                    .flatten()
                    .next()
                    .is_some();
            let has_src_pad = element.static_pad("src").is_some()
                || element
                    .iterate_src_pads()
                    .into_iter()
                    .flatten()
                    .next()
                    .is_some();

            // True sinks have sink pads but no source pads
            let is_sink = has_sink_pad && !has_src_pad;

            if is_sink {
                // Try to query latency directly on the sink
                let mut sink_query = gst::query::Latency::new();
                if element.query(&mut sink_query) {
                    let (live, min, max) = sink_query.result();
                    let min_ns = min.nseconds();
                    let max_ns = max.map_or(u64::MAX, |t| t.nseconds());
                    debug!(
                        "Sink element '{}' latency: live={}, min={}ns, max={}ns",
                        element_id, live, min_ns, max_ns
                    );
                    if min_ns > 0 {
                        total_latency = total_latency.max(min_ns);
                    }
                }

                // For audio sinks, try to get the latency-time property (in microseconds)
                if element.has_property("latency-time") {
                    let latency_us = element.property::<i64>("latency-time");
                    let latency_ns = (latency_us * 1000) as u64;
                    debug!(
                        "Audio sink '{}' latency-time: {}us ({}ns)",
                        element_id, latency_us, latency_ns
                    );
                    if latency_ns > 0 {
                        total_latency = total_latency.max(latency_ns);
                    }
                }

                // Try buffer-time property as well (typically 2x latency-time)
                if element.has_property("buffer-time") {
                    let buffer_us = element.property::<i64>("buffer-time");
                    let buffer_ns = (buffer_us * 1000) as u64;
                    debug!(
                        "Audio sink '{}' buffer-time: {}us ({}ns)",
                        element_id, buffer_us, buffer_ns
                    );
                    // Don't use buffer-time directly as it's the full buffer, not latency
                }
            }

            // Also check queue elements for their current level/latency
            let factory_name = element.factory().map(|f| f.name().to_string());
            if (factory_name.as_deref() == Some("queue")
                || factory_name.as_deref() == Some("queue2"))
                && element.has_property("current-level-time")
            {
                let level_ns = element.property::<u64>("current-level-time");
                debug!("Queue '{}' current-level-time: {}ns", element_id, level_ns);
                // Queue level contributes to latency
                total_latency = total_latency.saturating_add(level_ns);
            }
        }

        if total_latency > 0 {
            Some((total_latency, total_latency))
        } else {
            None
        }
    }

    /// Get WebRTC statistics from all webrtcbin elements in the pipeline.
    /// Searches for webrtcbin elements (including those nested in bins like whepclientsrc/whipclientsink)
    /// and collects their stats using the "get-stats" action signal.
    pub fn get_webrtc_stats(&self) -> strom_types::api::WebRtcStats {
        use strom_types::api::{WebRtcConnectionStats, WebRtcStats};

        let mut stats = WebRtcStats::default();
        let total_start = std::time::Instant::now();

        // Find all webrtcbin elements in the pipeline
        let find_start = std::time::Instant::now();
        let webrtcbins = self.find_webrtcbin_elements();
        let find_elapsed = find_start.elapsed();
        debug!(
            "get_webrtc_stats: Found {} webrtcbin element(s) in {:?}",
            webrtcbins.len(),
            find_elapsed
        );

        for (name, webrtcbin) in webrtcbins {
            let element_start = std::time::Instant::now();
            trace!("get_webrtc_stats: Getting stats from webrtcbin: {}", name);

            let mut conn_stats = WebRtcConnectionStats::default();

            // First check if ICE connection is established - skip if not ready
            // This avoids blocking on promise.wait() for webrtcbins that aren't connected
            let ice_start = std::time::Instant::now();
            let ice_state = self.get_ice_connection_state(&webrtcbin);
            debug!(
                "get_webrtc_stats: ICE state for {}: {:?} (took {:?})",
                name,
                ice_state,
                ice_start.elapsed()
            );

            // Only get detailed stats if we have a reasonable ICE state
            let should_get_stats = match ice_state.as_deref() {
                Some("connected") | Some("completed") | Some("checking") => true,
                Some("new") => {
                    // New state - webrtcbin exists but connection not started
                    // Still try to get basic stats
                    true
                }
                Some("failed") | Some("disconnected") | Some("closed") => {
                    warn!(
                        "get_webrtc_stats: Skipping stats for {} - ICE state: {:?}",
                        name, ice_state
                    );
                    false
                }
                _ => {
                    // Unknown state - try anyway but be cautious
                    true
                }
            };

            if should_get_stats {
                // Call the get-stats action signal
                // The signal takes:
                // - GstPad* (optional, NULL for all stats)
                // - GstPromise* (to receive the stats)
                // Returns void, stats come via the promise
                let pad_none: Option<&gst::Pad> = None;
                let promise = gst::Promise::new();
                let signal_start = std::time::Instant::now();
                webrtcbin.emit_by_name::<()>("get-stats", &[&pad_none, &promise]);

                // Wait for the promise with a timeout using interrupt from another thread.
                // The timeout thread is detached — calling interrupt() on an already-resolved
                // promise is a no-op, so there is no need to join.
                let promise_clone = promise.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    promise_clone.interrupt();
                });

                let promise_result = promise.wait();

                let promise_elapsed = signal_start.elapsed();
                debug!(
                    "get_webrtc_stats: Promise for {} resolved in {:?} (result: {:?})",
                    name, promise_elapsed, promise_result
                );

                match promise_result {
                    gst::PromiseResult::Replied => {
                        if let Some(reply) = promise.get_reply() {
                            // The reply is a GstStructure containing the stats
                            trace!(
                                "get_webrtc_stats: Got reply structure with {} fields: {}",
                                reply.n_fields(),
                                reply.name()
                            );
                            // Log all field names
                            for i in 0..reply.n_fields() {
                                if let Some(field_name) = reply.nth_field_name(i) {
                                    trace!("get_webrtc_stats: Field [{}]: {}", i, field_name);
                                }
                            }
                            // Convert StructureRef to owned Structure for parsing
                            let parse_start = std::time::Instant::now();
                            conn_stats = self.parse_webrtc_stats_structure(&reply.to_owned());
                            debug!(
                                "get_webrtc_stats: Parsed stats for {} in {:?} - ICE: {:?}, inbound_rtp: {}, outbound_rtp: {}",
                                name,
                                parse_start.elapsed(),
                                conn_stats.ice_candidates.is_some(),
                                conn_stats.inbound_rtp.len(),
                                conn_stats.outbound_rtp.len()
                            );
                        } else {
                            trace!(
                                "get_webrtc_stats: No stats in promise reply from webrtcbin: {}",
                                name
                            );
                        }
                    }
                    gst::PromiseResult::Interrupted => {
                        debug!(
                            "get_webrtc_stats: Promise timed out (interrupted) for webrtcbin: {}",
                            name
                        );
                    }
                    gst::PromiseResult::Expired => {
                        trace!("get_webrtc_stats: Promise expired for webrtcbin: {}", name);
                    }
                    gst::PromiseResult::Pending => {
                        trace!(
                            "get_webrtc_stats: Promise still pending for webrtcbin: {}",
                            name
                        );
                    }
                    _ => {
                        info!(
                            "get_webrtc_stats: Unknown promise result for webrtcbin: {}",
                            name
                        );
                    }
                }
            }

            // Also try to get basic element properties as fallback/additional info
            if let Some(ice_state_str) = ice_state {
                if conn_stats.ice_candidates.is_none() {
                    conn_stats.ice_candidates = Some(strom_types::api::IceCandidateStats {
                        state: Some(ice_state_str),
                        ..Default::default()
                    });
                }
            }

            debug!(
                "get_webrtc_stats: Element {} total: {:?}",
                name,
                element_start.elapsed()
            );
            stats.connections.insert(name, conn_stats);
        }

        debug!(
            "get_webrtc_stats: Returning {} connection(s), total time: {:?}",
            stats.connections.len(),
            total_start.elapsed()
        );
        stats
    }

    /// Find all webrtcbin elements in the pipeline, including those nested in bins.
    fn find_webrtcbin_elements(&self) -> Vec<(String, gst::Element)> {
        let mut results = Vec::new();

        debug!(
            "find_webrtcbin_elements: Searching {} elements in elements map",
            self.elements.len()
        );

        // Check direct elements
        for (name, element) in &self.elements {
            let factory_name = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            debug!(
                "find_webrtcbin_elements: Checking element '{}' (factory: {})",
                name, factory_name
            );

            if factory_name == "webrtcbin" {
                debug!("find_webrtcbin_elements: Found direct webrtcbin: {}", name);
                results.push((name.clone(), element.clone()));
            }

            // Check if element is a bin (like whepclientsrc, whipclientsink)
            // and search inside it recursively
            if element.is::<gst::Bin>() {
                let bin = element.clone().downcast::<gst::Bin>().unwrap();
                debug!(
                    "find_webrtcbin_elements: Element '{}' is a Bin, searching children",
                    name
                );

                // Use iterate_recurse to find all nested elements
                let iter = bin.iterate_recurse();
                for child_elem in iter.into_iter().flatten() {
                    let child_factory = child_elem
                        .factory()
                        .map(|f| f.name().to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    if child_factory == "webrtcbin" {
                        let child_name = format!("{}:{}", name, child_elem.name());
                        debug!(
                            "find_webrtcbin_elements: Found nested webrtcbin: {}",
                            child_name
                        );
                        results.push((child_name, child_elem));
                    }
                }
            }
        }

        // Also search the pipeline directly in case elements were added dynamically
        // This handles webrtcbins created by webrtcsink/whepserversink for each consumer
        debug!("find_webrtcbin_elements: Also searching pipeline directly");
        let pipeline_iter = self.pipeline.iterate_recurse();
        for elem in pipeline_iter.into_iter().flatten() {
            let factory_name = elem
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            if factory_name == "webrtcbin" {
                let elem_name = elem.name().to_string();
                // Check if we already have this element
                let already_found = results.iter().any(|(_, e)| e.name() == elem.name());
                if !already_found {
                    // Try to find the block_id by traversing up the parent hierarchy
                    // webrtcsink creates webrtcbin inside nested bins, so we need to
                    // find which registered element this webrtcbin belongs to
                    let qualified_name = self
                        .find_block_prefix_for_element(&elem)
                        .unwrap_or(elem_name.clone());
                    debug!(
                        "find_webrtcbin_elements: Found webrtcbin in pipeline: {} (qualified: {})",
                        elem_name, qualified_name
                    );
                    results.push((qualified_name, elem));
                }
            }
        }

        // Also include dynamically registered webrtcbins (from webrtcsink/whepserversink consumer-added)
        // These are stored separately because they're created in separate session pipelines
        if let Ok(store) = self.dynamic_webrtcbins.lock() {
            for (block_id, consumers) in store.iter() {
                for (consumer_id, webrtcbin) in consumers {
                    let qualified_name =
                        format!("{}:session_{}:{}", block_id, consumer_id, webrtcbin.name());
                    let already_found = results.iter().any(|(_, e)| e.name() == webrtcbin.name());
                    if !already_found {
                        debug!(
                            "find_webrtcbin_elements: Found dynamic webrtcbin: {} (consumer: {})",
                            qualified_name, consumer_id
                        );
                        results.push((qualified_name, webrtcbin.clone()));
                    }
                }
            }
        }

        debug!(
            "find_webrtcbin_elements: Found {} webrtcbin element(s)",
            results.len()
        );
        results
    }

    /// Find the block prefix (block_id:element_type) for a dynamically created element.
    ///
    /// This traverses up the parent hierarchy to find which registered element (block)
    /// contains this element. Used to properly name webrtcbin elements created by
    /// webrtcsink/whepserversink for frontend filtering.
    ///
    /// Returns the qualified name in format "block_id:parent_element:element_name"
    /// or None if no registered parent is found.
    fn find_block_prefix_for_element(&self, element: &gst::Element) -> Option<String> {
        let elem_name = element.name().to_string();

        // Walk up the parent chain to find a registered element
        let mut current: Option<gst::Object> = element.parent();
        let mut path_parts: Vec<String> = vec![elem_name.clone()];

        while let Some(parent) = current {
            // Check if parent is an Element
            if let Ok(parent_elem) = parent.clone().downcast::<gst::Element>() {
                let parent_name = parent_elem.name().to_string();

                // Check if this parent is a registered element in our elements map
                for (registered_name, registered_elem) in &self.elements {
                    if registered_elem.name() == parent_elem.name() {
                        // Found a match! Build the qualified name
                        // Format: "registered_name:path_to_webrtcbin"
                        path_parts.reverse();
                        let qualified = format!("{}:{}", registered_name, path_parts.join(":"));
                        debug!(
                            "find_block_prefix_for_element: {} belongs to registered element {}, qualified name: {}",
                            elem_name, registered_name, qualified
                        );
                        return Some(qualified);
                    }
                }

                // Add this parent to the path and continue up
                path_parts.push(parent_name);
            }

            // Move to the next parent
            current = parent.parent();
        }

        // No registered parent found
        debug!(
            "find_block_prefix_for_element: No registered parent found for {}",
            elem_name
        );
        None
    }

    /// Parse a GstStructure containing WebRTC stats into our typed format.
    fn parse_webrtc_stats_structure(
        &self,
        structure: &gst::Structure,
    ) -> strom_types::api::WebRtcConnectionStats {
        use strom_types::api::{IceCandidateStats, WebRtcConnectionStats};

        let mut conn_stats = WebRtcConnectionStats::default();

        trace!(
            "parse_webrtc_stats_structure: Parsing structure '{}' with {} fields",
            structure.name(),
            structure.n_fields()
        );

        // Log ALL field names and their types for debugging
        trace!("=== RAW WEBRTC STATS STRUCTURE ===");
        for (field_name, value) in structure.iter() {
            let type_name = value.type_().name();
            trace!("  Field: '{}' (type: {})", field_name, type_name);

            // If it's a nested structure, log its contents too
            if let Ok(nested) = value.get::<gst::Structure>() {
                trace!("    Nested structure '{}' fields:", nested.name());
                for (nested_field, nested_value) in nested.iter() {
                    let nested_type = nested_value.type_().name();
                    // Try to get the actual value for common types
                    let value_str = if let Ok(s) = nested_value.get::<String>() {
                        format!("\"{}\"", s)
                    } else if let Ok(s) = nested_value.get::<&str>() {
                        format!("\"{}\"", s)
                    } else if let Ok(n) = nested_value.get::<u64>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<i64>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<u32>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<i32>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<f64>() {
                        format!("{:.6}", n)
                    } else if let Ok(b) = nested_value.get::<bool>() {
                        format!("{}", b)
                    } else {
                        format!("<{}>", nested_type)
                    };
                    trace!("      {}: {} = {}", nested_field, nested_type, value_str);
                }
            }
        }
        trace!("=== END RAW STATS ===");

        // WebRTC stats structure contains nested structures for each stat type
        // The field NAME indicates the type (e.g., "rtp-inbound-stream-stats_1234")

        // Iterate over all fields in the structure
        for (field_name, value) in structure.iter() {
            let field_str = field_name.to_string();

            // Try to get as nested structure
            if let Ok(nested) = value.get::<gst::Structure>() {
                // Determine type from field name prefix
                if field_str.starts_with("rtp-inbound-stream-stats") {
                    debug!(
                        "parse_webrtc_stats_structure: Found inbound RTP stats: {}",
                        field_str
                    );
                    conn_stats
                        .inbound_rtp
                        .push(self.parse_rtp_stats(&nested, true));
                } else if field_str.starts_with("rtp-outbound-stream-stats") {
                    debug!(
                        "parse_webrtc_stats_structure: Found outbound RTP stats: {}",
                        field_str
                    );
                    conn_stats
                        .outbound_rtp
                        .push(self.parse_rtp_stats(&nested, false));
                } else if field_str.starts_with("ice-candidate-local") {
                    debug!(
                        "parse_webrtc_stats_structure: Found local ICE candidate: {}",
                        field_str
                    );
                    if conn_stats.ice_candidates.is_none() {
                        conn_stats.ice_candidates = Some(IceCandidateStats::default());
                    }
                    if let Some(ref mut ice) = conn_stats.ice_candidates {
                        if let Ok(candidate_type) = nested.get::<&str>("candidate-type") {
                            ice.local_candidate_type = Some(candidate_type.to_string());
                        }
                        if let Ok(address) = nested.get::<&str>("address") {
                            ice.local_address = Some(address.to_string());
                        }
                        if let Ok(port) = nested.get::<u32>("port") {
                            ice.local_port = Some(port);
                        }
                        if let Ok(protocol) = nested.get::<&str>("protocol") {
                            ice.local_protocol = Some(protocol.to_string());
                        }
                    }
                } else if field_str.starts_with("ice-candidate-remote") {
                    debug!(
                        "parse_webrtc_stats_structure: Found remote ICE candidate: {}",
                        field_str
                    );
                    if conn_stats.ice_candidates.is_none() {
                        conn_stats.ice_candidates = Some(IceCandidateStats::default());
                    }
                    if let Some(ref mut ice) = conn_stats.ice_candidates {
                        if let Ok(candidate_type) = nested.get::<&str>("candidate-type") {
                            ice.remote_candidate_type = Some(candidate_type.to_string());
                        }
                        if let Ok(address) = nested.get::<&str>("address") {
                            ice.remote_address = Some(address.to_string());
                        }
                        if let Ok(port) = nested.get::<u32>("port") {
                            ice.remote_port = Some(port);
                        }
                        if let Ok(protocol) = nested.get::<&str>("protocol") {
                            ice.remote_protocol = Some(protocol.to_string());
                        }
                    }
                } else if field_str.starts_with("ice-candidate-pair") {
                    debug!(
                        "parse_webrtc_stats_structure: Found ICE candidate pair: {}",
                        field_str
                    );
                    // Candidate pair stats - this is where we can get connection state
                    if conn_stats.ice_candidates.is_none() {
                        conn_stats.ice_candidates = Some(IceCandidateStats::default());
                    }
                    // Note: GStreamer webrtcbin doesn't expose ICE state in stats
                    // We get it from the ice-connection-state property instead
                } else if field_str.starts_with("transport-stats") {
                    debug!(
                        "parse_webrtc_stats_structure: Found transport stats: {}",
                        field_str
                    );
                    let mut transport = strom_types::api::TransportStats::default();
                    if let Ok(bytes) = nested.get::<u64>("bytes-sent") {
                        transport.bytes_sent = Some(bytes);
                    }
                    if let Ok(bytes) = nested.get::<u64>("bytes-received") {
                        transport.bytes_received = Some(bytes);
                    }
                    if let Ok(packets) = nested.get::<u64>("packets-sent") {
                        transport.packets_sent = Some(packets);
                    }
                    if let Ok(packets) = nested.get::<u64>("packets-received") {
                        transport.packets_received = Some(packets);
                    }
                    conn_stats.transport = Some(transport);
                } else if field_str.starts_with("codec-stats") || field_str.starts_with("codec_") {
                    debug!(
                        "parse_webrtc_stats_structure: Found codec stats: {}",
                        field_str
                    );
                    let mut codec = strom_types::api::CodecStats::default();
                    if let Ok(mime) = nested.get::<&str>("mime-type") {
                        codec.mime_type = Some(mime.to_string());
                    }
                    if let Ok(clock_rate) = nested.get::<u32>("clock-rate") {
                        codec.clock_rate = Some(clock_rate);
                    }
                    if let Ok(pt) = nested.get::<u32>("payload-type") {
                        codec.payload_type = Some(pt);
                    }
                    if let Ok(channels) = nested.get::<u32>("channels") {
                        codec.channels = Some(channels);
                    }
                    conn_stats.codecs.push(codec);
                }
            }
        }

        debug!(
            "parse_webrtc_stats_structure: Done - ICE: {:?}, inbound: {}, outbound: {}, codecs: {}, transport: {:?}",
            conn_stats.ice_candidates.is_some(),
            conn_stats.inbound_rtp.len(),
            conn_stats.outbound_rtp.len(),
            conn_stats.codecs.len(),
            conn_stats.transport.is_some()
        );
        conn_stats
    }

    /// Parse RTP stream stats from a GstStructure.
    fn parse_rtp_stats(
        &self,
        structure: &gst::Structure,
        inbound: bool,
    ) -> strom_types::api::RtpStreamStats {
        use strom_types::api::RtpStreamStats;

        let mut stats = RtpStreamStats::default();

        // SSRC
        if let Ok(ssrc) = structure.get::<u32>("ssrc") {
            stats.ssrc = Some(ssrc);
        }

        // Media type
        if let Ok(media_type) = structure.get::<&str>("media-type") {
            stats.media_type = Some(media_type.to_string());
        } else if let Ok(kind) = structure.get::<&str>("kind") {
            stats.media_type = Some(kind.to_string());
        }

        // Codec
        if let Ok(codec) = structure.get::<&str>("codec-id") {
            stats.codec = Some(codec.to_string());
        }

        // Bytes
        if inbound {
            if let Ok(bytes) = structure.get::<u64>("bytes-received") {
                stats.bytes = Some(bytes);
            }
        } else if let Ok(bytes) = structure.get::<u64>("bytes-sent") {
            stats.bytes = Some(bytes);
        }

        // Packets
        if inbound {
            if let Ok(packets) = structure.get::<u64>("packets-received") {
                stats.packets = Some(packets);
            }
            // Packets lost (signed for inbound)
            if let Ok(lost) = structure.get::<i64>("packets-lost") {
                stats.packets_lost = Some(lost);
            } else if let Ok(lost) = structure.get::<i32>("packets-lost") {
                stats.packets_lost = Some(lost as i64);
            }
            // Jitter
            if let Ok(jitter) = structure.get::<f64>("jitter") {
                stats.jitter = Some(jitter);
            }
        } else if let Ok(packets) = structure.get::<u64>("packets-sent") {
            stats.packets = Some(packets);
        }

        // RTT (round-trip time) - try top level first
        if let Ok(rtt) = structure.get::<f64>("round-trip-time") {
            stats.round_trip_time = Some(rtt);
        }

        // Bitrate (if available) - try top level first
        if let Ok(bitrate) = structure.get::<u64>("bitrate") {
            stats.bitrate = Some(bitrate);
        }

        // Parse nested gst-rtpsource-stats for additional fields
        // This contains packets-lost, bitrate, and round-trip time
        if let Ok(rtp_source_stats) = structure.get::<gst::Structure>("gst-rtpsource-stats") {
            debug!(
                "parse_rtp_stats: Found nested gst-rtpsource-stats with {} fields",
                rtp_source_stats.n_fields()
            );

            // Packets lost (only for inbound)
            if inbound && stats.packets_lost.is_none() {
                // Try packets-lost first (cumulative)
                if let Ok(lost) = rtp_source_stats.get::<i32>("packets-lost") {
                    // -1 means unknown/not calculated, only set if we have a real value
                    if lost >= 0 {
                        stats.packets_lost = Some(lost as i64);
                    }
                }
                // Try sent-rb-packetslost (from receiver report we sent)
                if stats.packets_lost.is_none() {
                    if let Ok(lost) = rtp_source_stats.get::<i32>("sent-rb-packetslost") {
                        if lost >= 0 {
                            stats.packets_lost = Some(lost as i64);
                        }
                    }
                }
                // If we have sent-rb data (sent-rb=true) but packets-lost is still -1,
                // check sent-rb-fractionlost (0-255 representing 0-100% loss)
                // If fraction lost is 0, we can assume 0 packets lost
                if stats.packets_lost.is_none() {
                    if let Ok(sent_rb) = rtp_source_stats.get::<bool>("sent-rb") {
                        if sent_rb {
                            if let Ok(fraction) =
                                rtp_source_stats.get::<u32>("sent-rb-fractionlost")
                            {
                                // If fraction lost is 0, no recent packet loss
                                // Set packets_lost to 0 to indicate healthy stream
                                if fraction == 0 {
                                    stats.packets_lost = Some(0);
                                }
                            }
                        }
                    }
                }
            }

            // Bitrate from nested structure
            if stats.bitrate.is_none() {
                if let Ok(bitrate) = rtp_source_stats.get::<u64>("bitrate") {
                    if bitrate > 0 {
                        stats.bitrate = Some(bitrate);
                    }
                }
            }

            // Fraction lost (0-255 scale, convert to 0.0-1.0)
            if inbound {
                if let Ok(fraction) = rtp_source_stats.get::<u32>("sent-rb-fractionlost") {
                    // 0-255 maps to 0.0-1.0 (0-100% loss)
                    let fraction_pct = fraction as f64 / 255.0;
                    stats.fraction_lost = Some(fraction_pct);
                }
            }

            // Round-trip time from RTCP receiver reports
            if stats.round_trip_time.is_none() {
                // Try rb-round-trip first (from received receiver reports)
                if let Ok(rtt_ticks) = rtp_source_stats.get::<u32>("rb-round-trip") {
                    if rtt_ticks > 0 {
                        // rb-round-trip is in 1/65536 seconds units
                        let rtt_seconds = rtt_ticks as f64 / 65536.0;
                        stats.round_trip_time = Some(rtt_seconds);
                    }
                }
                // Also try sent-rb-dlsr and sent-rb-lsr to calculate RTT
                // RTT = now - LSR - DLSR (when we have the values)
                // For now, just use rb-round-trip if available
            }
        }

        stats
    }

    /// Get ICE connection state from webrtcbin element.
    fn get_ice_connection_state(&self, webrtcbin: &gst::Element) -> Option<String> {
        // Try to get ice-connection-state property
        if webrtcbin.has_property("ice-connection-state") {
            let state_value = webrtcbin.property_value("ice-connection-state");
            // The state is an enum, try to get it as a string
            if let Ok(state_int) = state_value.get::<i32>() {
                // Map enum values to strings
                let state_str = match state_int {
                    0 => "new",
                    1 => "checking",
                    2 => "connected",
                    3 => "completed",
                    4 => "failed",
                    5 => "disconnected",
                    6 => "closed",
                    _ => "unknown",
                };
                return Some(state_str.to_string());
            }
        }
        None
    }
}
