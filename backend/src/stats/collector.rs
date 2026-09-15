//! Statistics collector for running pipelines.

use crate::stats::rtp::{
    collect_all_jitterbuffer_stats, collect_rtp_jitterbuffer_stats, find_jitterbuffers_in_bin,
    jitterbuffer_media_kind,
};
use crate::whip_session_manager::WhipSessionManager;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::time::{SystemTime, UNIX_EPOCH};
use strom_types::block::BlockInstance;
use strom_types::stats::{BlockStats, FlowStats, Statistic};
use strom_types::Flow;
use strom_types::PropertyValue;
use tracing::{debug, trace, warn};

/// Collector for pipeline statistics.
pub struct StatsCollector;

impl StatsCollector {
    /// Collect statistics for a running flow.
    ///
    /// `whip_sessions` is needed because WHIP Input's receive path is not in
    /// `pipeline`: each publisher gets its own session pipeline.
    pub fn collect_flow_stats(
        pipeline: &gst::Pipeline,
        flow: &Flow,
        whip_sessions: &WhipSessionManager,
    ) -> FlowStats {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        let mut block_stats = Vec::new();

        // Collect stats for each block in the flow
        for block in &flow.blocks {
            if let Some(stats) = Self::collect_block_stats(pipeline, block, whip_sessions) {
                block_stats.push(stats);
            }
        }

        FlowStats {
            flow_id: flow.id,
            flow_name: flow.name.clone(),
            block_stats,
            collected_at: now,
        }
    }

    /// Collect statistics for a specific block.
    fn collect_block_stats(
        pipeline: &gst::Pipeline,
        block: &BlockInstance,
        whip_sessions: &WhipSessionManager,
    ) -> Option<BlockStats> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        // Determine what kind of stats to collect based on block definition
        let stats = match block.block_definition_id.as_str() {
            "builtin.aes67_input" => Self::collect_aes67_input_stats(pipeline, &block.id),
            "builtin.whip_input" => Self::collect_whip_input_stats(block, whip_sessions),
            "builtin.aes67_output" => {
                // AES67 output doesn't have jitterbuffer stats, could add other stats later
                vec![]
            }
            "builtin.meter" => {
                // Meter block stats could be added here
                vec![]
            }
            _ => {
                // Unknown block type - no stats available
                vec![]
            }
        };

        if stats.is_empty() {
            return None;
        }

        Some(BlockStats {
            block_instance_id: block.id.clone(),
            block_definition_id: block.block_definition_id.clone(),
            block_name: block
                .name
                .clone()
                .unwrap_or_else(|| block.block_definition_id.clone()),
            stats,
            collected_at: now,
        })
    }

    /// Collect statistics for AES67 Input block (RTP jitterbuffer stats).
    fn collect_aes67_input_stats(pipeline: &gst::Pipeline, instance_id: &str) -> Vec<Statistic> {
        let mut all_stats = Vec::new();

        // Find the sdpdemux element for this block
        let sdpdemux_name = format!("{}:sdpdemux", instance_id);
        if let Some(sdpdemux) = pipeline.by_name(&sdpdemux_name) {
            debug!("Found sdpdemux element: {}", sdpdemux_name);

            // Cast sdpdemux to Bin to search for jitterbuffers
            if let Ok(bin) = sdpdemux.dynamic_cast::<gst::Bin>() {
                let jb_stats = collect_all_jitterbuffer_stats(&bin);
                let jb_count = jb_stats.len();
                trace!("Found {} jitterbuffer(s) in {}", jb_count, sdpdemux_name);

                for (jb_name, stats) in jb_stats {
                    debug!("Jitterbuffer '{}' stats: {:?}", jb_name, stats);
                    // Add stats with jitterbuffer name prefix for multi-stream support
                    for mut stat in stats.to_statistics() {
                        if jb_count > 1 {
                            stat.id = format!("{}_{}", jb_name, stat.id);
                            stat.metadata.display_name =
                                format!("{} ({})", stat.metadata.display_name, jb_name);
                        }
                        all_stats.push(stat);
                    }
                }
            } else {
                warn!("Failed to cast sdpdemux to Bin: {}", sdpdemux_name);
            }
        } else {
            warn!("Could not find sdpdemux element: {}", sdpdemux_name);
        }

        all_stats
    }

    /// Collect statistics for a WHIP Input block, one set per connected seat.
    ///
    /// Each publisher runs in its own session pipeline, so the jitterbuffers
    /// are per session rather than under the block's own elements. A seat's
    /// audio and video buffer independently, hence the `slot<n>_<medium>_`
    /// prefix.
    fn collect_whip_input_stats(
        block: &BlockInstance,
        whip_sessions: &WhipSessionManager,
    ) -> Vec<Statistic> {
        let Some(endpoint_id) = whip_endpoint_id(block) else {
            debug!(
                "WHIP Input '{}' has no endpoint_id yet, so it has no sessions to report",
                block.id
            );
            return Vec::new();
        };

        let mut all_stats = Vec::new();
        for session in whip_sessions.sessions_for_endpoint(&endpoint_id) {
            let jitterbuffers = find_jitterbuffers_in_bin(session.pipeline.upcast_ref());
            trace!(
                "WHIP Input '{}': slot {} (session '{}') has {} jitterbuffer(s)",
                block.id,
                session.slot,
                session.resource_id,
                jitterbuffers.len()
            );

            for jb in &jitterbuffers {
                let Some(stats) = collect_rtp_jitterbuffer_stats(jb) else {
                    continue;
                };
                // A buffer whose stream has not been linked yet has no medium
                // to report. Its element name keeps it apart from its
                // siblings and, unlike their order, does not move between
                // polls.
                let medium = jitterbuffer_media_kind(jb).unwrap_or_else(|| jb.name().to_string());
                let prefix = format!("slot{}_{}", session.slot, medium);

                for mut stat in stats.to_statistics() {
                    stat.id = format!("{}_{}", prefix, stat.id);
                    stat.metadata.display_name = format!(
                        "{} (slot {} {})",
                        stat.metadata.display_name, session.slot, medium
                    );
                    all_stats.push(stat);
                }
            }
        }

        all_stats
    }
}

/// The endpoint a WHIP Input block is actually serving.
///
/// `runtime_data` carries the id the running pipeline registered, which is what
/// the session manager is keyed on. The property is the fallback, and is unset
/// on a block left to generate its own id.
fn whip_endpoint_id(block: &BlockInstance) -> Option<String> {
    if let Some(id) = block
        .runtime_data
        .as_ref()
        .and_then(|d| d.get("whip_endpoint_id"))
    {
        return Some(id.clone());
    }
    match block.properties.get("endpoint_id") {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whip_session_manager::NewWhipSession;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use strom_types::stats::StatValue;

    /// A session pipeline shaped like a real one for stats purposes: a
    /// jitterbuffer sitting where whipserversrc's rtpbin would put it.
    /// The name the fixture's jitterbuffer carries, which is also the label its
    /// stats get: an unlinked buffer has no medium to report.
    const JB_NAME: &str = "seat-jitterbuffer";

    fn session_pipeline_with_jitterbuffer(latency_ms: u32) -> gst::Pipeline {
        let pipeline = gst::Pipeline::new();
        let inner = gst::Bin::with_name("rtpbin-stand-in");
        let jb = gst::ElementFactory::make("rtpjitterbuffer")
            .name(JB_NAME)
            .property("latency", latency_ms)
            .build()
            .expect("rtpjitterbuffer is in gst-plugins-good");
        inner.add(&jb).unwrap();
        pipeline.add(&inner).unwrap();
        pipeline
    }

    fn register_seat(
        mgr: &WhipSessionManager,
        endpoint_id: &str,
        slot: usize,
        latency_ms: u32,
        port: u16,
    ) {
        assert!(mgr.register_session(NewWhipSession {
            resource_id: format!("{}-resource-{}", endpoint_id, slot),
            port,
            element: gst::ElementFactory::make("fakesrc").build().unwrap(),
            session_pipeline: session_pipeline_with_jitterbuffer(latency_ms),
            endpoint_id: endpoint_id.to_string(),
            slot,
            cleanup_sent: Arc::new(AtomicBool::new(false)),
        }));
    }

    fn whip_flow(endpoint_id: Option<&str>) -> Flow {
        let runtime = match endpoint_id {
            Some(id) => format!(r#", "runtime_data": {{"whip_endpoint_id": "{}"}}"#, id),
            None => String::new(),
        };
        serde_json::from_str(&format!(
            r#"{{
                "id": "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
                "name": "seat stats",
                "blocks": [{{
                    "id": "whip_p1",
                    "block_definition_id": "builtin.whip_input",
                    "name": "WHIP p1",
                    "properties": {{}},
                    "position": {{"x": 0.0, "y": 0.0}}{}
                }}]
            }}"#,
            runtime
        ))
        .expect("flow fixture")
    }

    fn stat<'a>(stats: &'a [Statistic], id: &str) -> Option<&'a Statistic> {
        stats.iter().find(|s| s.id == id)
    }

    #[test]
    fn whip_input_reports_the_jitterbuffer_of_each_connected_seat() {
        let _ = gst::init();
        let mgr = WhipSessionManager::new();
        register_seat(&mgr, "p1", 0, 150, 50000);
        register_seat(&mgr, "p1", 1, 150, 50001);
        // A seat on a different endpoint must not show up under this block.
        register_seat(&mgr, "p2", 0, 400, 50002);

        let flow = whip_flow(Some("p1"));
        let stats = StatsCollector::collect_flow_stats(&gst::Pipeline::new(), &flow, &mgr);

        let block = stats
            .block_stats
            .iter()
            .find(|b| b.block_instance_id == "whip_p1")
            .expect("the WHIP block reports stats while a seat is publishing");

        for slot in [0, 1] {
            let id = format!("slot{}_{}_latency_ms", slot, JB_NAME);
            let found = stat(&block.stats, &id).unwrap_or_else(|| {
                panic!(
                    "slot {} reports its configured buffer; ids were {:?}",
                    slot,
                    block.stats.iter().map(|s| &s.id).collect::<Vec<_>>()
                )
            });
            assert!(matches!(found.value, StatValue::Gauge(150)));
        }
        assert!(
            !block
                .stats
                .iter()
                .any(|s| matches!(s.value, StatValue::Gauge(400))),
            "a seat on another endpoint leaked into this block's stats"
        );
    }

    #[test]
    fn whip_input_with_no_seat_reports_nothing() {
        let _ = gst::init();
        let mgr = WhipSessionManager::new();
        let flow = whip_flow(Some("p1"));

        let stats = StatsCollector::collect_flow_stats(&gst::Pipeline::new(), &flow, &mgr);
        assert!(
            stats.block_stats.is_empty(),
            "an endpoint with no publisher must not report a phantom seat"
        );
    }

    #[test]
    fn whip_input_falls_back_to_the_endpoint_id_property() {
        let _ = gst::init();
        let mgr = WhipSessionManager::new();
        register_seat(&mgr, "p9", 0, 80, 50003);

        let mut flow = whip_flow(None);
        flow.blocks[0].properties.insert(
            "endpoint_id".to_string(),
            PropertyValue::String("p9".to_string()),
        );

        let stats = StatsCollector::collect_flow_stats(&gst::Pipeline::new(), &flow, &mgr);
        assert_eq!(stats.block_stats.len(), 1);
    }
}
