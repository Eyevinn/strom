use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::{FlowId, StromEvent};
use tracing::{debug, trace};

/// Extract f64 values from a GValueArray field in a GStreamer structure.
pub(super) fn extract_level_values(structure: &gst::StructureRef, field_name: &str) -> Vec<f64> {
    use gstreamer::glib;

    if let Ok(array) = structure.get::<glib::ValueArray>(field_name) {
        array.iter().filter_map(|v| v.get::<f64>().ok()).collect()
    } else {
        Vec::new()
    }
}

/// Connect message handler for all level elements in this mixer block.
pub(super) fn connect_mixer_meter_handler(
    bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
    instance_id: String,
) -> gst::glib::SignalHandlerId {
    use gst::MessageView;

    debug!(
        "Connecting mixer meter handler for flow {} instance {}",
        flow_id, instance_id
    );

    bus.add_signal_watch();

    let level_prefix = format!("{}:level_", instance_id);
    let main_level_id = format!("{}:main_level", instance_id);
    let monitor_level_id = format!("{}:monitor_level", instance_id);
    let aux_level_prefix = format!("{}:aux", instance_id);
    let group_level_prefix = format!("{}:group", instance_id);

    bus.connect_message(None, move |_bus, msg| {
        if let MessageView::Element(element_msg) = msg.view() {
            if let Some(s) = element_msg.structure() {
                if s.name() == "level" {
                    if let Some(source) = msg.src() {
                        let source_name = source.name().to_string();

                        let rms = extract_level_values(s, "rms");
                        let peak = extract_level_values(s, "peak");
                        let decay = extract_level_values(s, "decay");

                        if rms.is_empty() {
                            return;
                        }

                        // Check if this is the main level meter
                        if source_name == main_level_id {
                            trace!("Mixer main meter: rms={:?}, peak={:?}", rms, peak);
                            let element_id = format!("{}:meter:main", instance_id);
                            events.broadcast(StromEvent::MeterData {
                                flow_id,
                                element_id,
                                rms,
                                peak,
                                decay,
                            });
                            return;
                        }

                        // Check if this is the monitor bus level meter
                        if source_name == monitor_level_id {
                            trace!("Mixer monitor meter: rms={:?}, peak={:?}", rms, peak);
                            let element_id = format!("{}:meter:monitor", instance_id);
                            events.broadcast(StromEvent::MeterData {
                                flow_id,
                                element_id,
                                rms,
                                peak,
                                decay,
                            });
                            return;
                        }

                        // Check if this is an aux level meter
                        // Format: "instance_id:auxN_level"
                        if source_name.starts_with(&aux_level_prefix)
                            && source_name.contains("_level")
                        {
                            // Extract aux number from "auxN_level"
                            if let Some(aux_part) =
                                source_name.strip_prefix(&format!("{}:aux", instance_id))
                            {
                                if let Some(aux_num_str) = aux_part.strip_suffix("_level") {
                                    if let Ok(aux_num) = aux_num_str.parse::<usize>() {
                                        trace!(
                                            "Mixer aux{} meter: rms={:?}, peak={:?}",
                                            aux_num + 1,
                                            rms,
                                            peak
                                        );
                                        let element_id =
                                            format!("{}:meter:aux{}", instance_id, aux_num + 1);
                                        events.broadcast(StromEvent::MeterData {
                                            flow_id,
                                            element_id,
                                            rms,
                                            peak,
                                            decay,
                                        });
                                        return;
                                    }
                                }
                            }
                        }

                        // Check if this is a group level meter
                        // Format: "instance_id:groupN_level"
                        if source_name.starts_with(&group_level_prefix)
                            && source_name.contains("_level")
                        {
                            if let Some(sg_part) =
                                source_name.strip_prefix(&format!("{}:group", instance_id))
                            {
                                if let Some(sg_num_str) = sg_part.strip_suffix("_level") {
                                    if let Ok(sg_num) = sg_num_str.parse::<usize>() {
                                        trace!(
                                            "Mixer group{} meter: rms={:?}, peak={:?}",
                                            sg_num + 1,
                                            rms,
                                            peak
                                        );
                                        let element_id =
                                            format!("{}:meter:group{}", instance_id, sg_num + 1);
                                        events.broadcast(StromEvent::MeterData {
                                            flow_id,
                                            element_id,
                                            rms,
                                            peak,
                                            decay,
                                        });
                                        return;
                                    }
                                }
                            }
                        }

                        // Check if this is a channel level meter
                        if !source_name.starts_with(&level_prefix) {
                            return;
                        }

                        // Extract channel number from element name
                        // Format: "instance_id:level_N" -> extract N
                        let channel_str = source_name.strip_prefix(&level_prefix).unwrap_or("0");
                        let channel_num: usize = channel_str.parse().unwrap_or(0) + 1;

                        trace!(
                            "Mixer meter ch{}: rms={:?}, peak={:?}",
                            channel_num,
                            rms,
                            peak
                        );

                        // Use element_id format that frontend can parse
                        // Format: "block_id:meter:N" for channel N
                        let element_id = format!("{}:meter:{}", instance_id, channel_num);

                        events.broadcast(StromEvent::MeterData {
                            flow_id,
                            element_id,
                            rms,
                            peak,
                            decay,
                        });
                    }
                }
            }
        }
    })
}
