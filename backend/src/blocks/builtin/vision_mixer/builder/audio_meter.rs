//! Audio metering chains: per-input + a dedicated PGM branch.
//!
//! Each chain is `queue_audio_{i} → audioconvert → level_audio_{i} → fakesink_audio_{i}`.
//! The external pads (`audio_in_{i}` / `pgm_audio_in`) target the queue sinks,
//! so the chains are self-contained and don't need caps negotiation up-front.

use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::{element::ElementPadRef, FlowId};
use tracing::trace;

use super::super::elements;
use super::super::overlay::VisionMixerOverlayState;
use super::PipelineParams;
use crate::blocks::BlockBuildError;
use crate::events::EventBroadcaster;

/// Append audio metering chains to the pipeline: one per video input plus a
/// dedicated PGM audio branch.
pub(super) fn append_audio_meter_chains(
    p: &PipelineParams,
    elems: &mut Vec<(String, gst::Element)>,
    links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<(), BlockBuildError> {
    for i in 0..p.num_inputs {
        let q_id = p.id(&format!("queue_audio_{}", i));
        let conv_id = p.id(&format!("audioconvert_audio_{}", i));
        let level_id = p.id(&format!("level_audio_{}", i));
        let sink_id = p.id(&format!("fakesink_audio_{}", i));

        elems.push((q_id.clone(), elements::make_queue(&q_id)?));
        elems.push((
            conv_id.clone(),
            elements::make_element("audioconvert", &conv_id)?,
        ));
        elems.push((level_id.clone(), elements::make_level(&level_id)?));
        elems.push((sink_id.clone(), elements::make_meter_fakesink(&sink_id)?));

        links.push((
            ElementPadRef::pad(&q_id, "src"),
            ElementPadRef::pad(&conv_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&conv_id, "src"),
            ElementPadRef::pad(&level_id, "sink"),
        ));
        links.push((
            ElementPadRef::pad(&level_id, "src"),
            ElementPadRef::pad(&sink_id, "sink"),
        ));
    }

    // PGM audio branch.
    let q_id = p.id("queue_audio_pgm");
    let conv_id = p.id("audioconvert_audio_pgm");
    let level_id = p.id("level_audio_pgm");
    let sink_id = p.id("fakesink_audio_pgm");

    elems.push((q_id.clone(), elements::make_queue(&q_id)?));
    elems.push((
        conv_id.clone(),
        elements::make_element("audioconvert", &conv_id)?,
    ));
    elems.push((level_id.clone(), elements::make_level(&level_id)?));
    elems.push((sink_id.clone(), elements::make_meter_fakesink(&sink_id)?));

    links.push((
        ElementPadRef::pad(&q_id, "src"),
        ElementPadRef::pad(&conv_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&conv_id, "src"),
        ElementPadRef::pad(&level_id, "sink"),
    ));
    links.push((
        ElementPadRef::pad(&level_id, "src"),
        ElementPadRef::pad(&sink_id, "sink"),
    ));

    Ok(())
}

/// Build the bus message handler that forwards `level` element messages into
/// the overlay state so VU meters can be rendered.
///
/// The handler owns an `Arc<VisionMixerOverlayState>` — never a `gst::Element`
/// or `gst::Pipeline` — so it doesn't create a reference cycle.
pub(super) fn build_meter_bus_handler(
    instance_id: &str,
    overlay_state: Arc<VisionMixerOverlayState>,
) -> crate::blocks::BusMessageConnectFn {
    let input_level_prefix = format!("{}:level_audio_", instance_id);
    let pgm_level_name = format!("{}:level_audio_pgm", instance_id);

    Box::new(
        move |bus: &gst::Bus,
              _flow_id: FlowId,
              _events: EventBroadcaster|
              -> gst::glib::SignalHandlerId {
            bus.add_signal_watch();
            let state = overlay_state;
            bus.connect_message(None, move |_bus, msg| {
                use gst::MessageView;
                let MessageView::Element(element_msg) = msg.view() else {
                    return;
                };
                let Some(s) = element_msg.structure() else {
                    return;
                };
                if s.name() != "level" {
                    return;
                }
                let Some(src) = msg.src() else { return };
                let src_name = src.name();

                let peak = extract_level_array(s, "peak");
                let decay = extract_level_array(s, "decay");
                if peak.is_empty() {
                    return;
                }
                let peak_db = max_f64(&peak);
                // decay may be missing on some level implementations; fall back
                // to peak so the tick still tracks something sensible.
                let decay_db = if decay.is_empty() {
                    peak_db
                } else {
                    max_f64(&decay)
                };

                if src_name == pgm_level_name.as_str() {
                    trace!(
                        "vision_mixer PGM meter peak={:.1} decay={:.1}",
                        peak_db,
                        decay_db
                    );
                    state.set_pgm_levels(peak_db, decay_db);
                    return;
                }
                if let Some(rest) = src_name.strip_prefix(input_level_prefix.as_str()) {
                    if let Ok(idx) = rest.parse::<usize>() {
                        trace!(
                            "vision_mixer input {} meter peak={:.1} decay={:.1}",
                            idx,
                            peak_db,
                            decay_db
                        );
                        state.set_input_levels(idx, peak_db, decay_db);
                    }
                }
            })
        },
    )
}

fn extract_level_array(s: &gst::StructureRef, field: &str) -> Vec<f64> {
    use gstreamer::glib;
    s.get::<glib::ValueArray>(field)
        .map(|arr| arr.iter().filter_map(|v| v.get::<f64>().ok()).collect())
        .unwrap_or_default()
}

fn max_f64(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}
