//! What the SRT output blocks share around their `srtsink`.

use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::element::ElementPadRef;
use tracing::info;

/// Whether `srtsink` dials out (caller or rendezvous) rather than listens.
///
/// Reads the `mode` the sink parsed from its `uri`, so call it after setting
/// the URI. A URI with no host is opened as a listener whatever its `mode`
/// says, which [`strom_types::srt_uri::parse_srt_uri`] tells.
pub(crate) fn dials_out(srtsink: &gst::Element) -> bool {
    let mode = srtsink.property_value("mode");
    if gst::glib::EnumValue::from_value(&mode).is_some_and(|(_, v)| v.nick() == "listener") {
        return false;
    }
    let uri: String = srtsink.property("uri");
    strom_types::srt_uri::parse_srt_uri(&uri)
        .is_some_and(|e| e.mode != strom_types::api::SrtMode::Listener)
}

/// Link the muxer's `src` pad to `srtsink`, through a leaky queue when the
/// sink dials out.
///
/// An `srtsink` in caller mode with `auto-reconnect` reconnects inside
/// `render`, on the streaming thread, with blocking connect attempts of about
/// 3 s each, for as long as it has no peer. Strom links one block output to
/// several blocks through a `tee` without queues, so a sink that blocks there
/// holds up everything upstream: a live mixer stalls, every other output stops,
/// and when a peer comes back the backlog bursts out and SRT inputs that were
/// not read in the meantime break (#1259). Through the SRT relay every output
/// is a caller, and having no peer is the normal state.
///
/// The queue gives the sink a thread of its own and drops the oldest data
/// while the sink is blocked (`leaky=downstream`), so an output without a peer
/// only loses its own data. This is the documented requirement the queue rule
/// in CLAUDE.md asks for. Its other limits:
///
/// - `max-size-time` stays at its 1 s default. It bounds the stale data that
///   goes out first when a peer connects. A connected sink holds a buffer only
///   until its clock time (`sync=true`), which is far below that.
/// - `max-size-buffers=0` (default 200). The muxers emit one 1316-byte buffer
///   per 7 TS packets, so 200 buffers is ~100-250 ms at broadcast bitrates,
///   less than a syncing sink may legitimately hold data. The queue would then
///   drop media while the output is connected.
/// - `max-size-bytes` stays at its 10 MB default, the bound if a muxer ever
///   sends buffers without timestamps (then the time level reads 0).
///
/// A listener does not block without a peer as long as `wait-for-connection`
/// is false (the default); with it true the operator asked for blocking. So a
/// listener keeps its direct link and no extra thread.
pub(crate) fn link_mux_to_srtsink(
    instance_id: &str,
    mux_id: &str,
    sink_id: &str,
    srtsink: &gst::Element,
    elements: &mut Vec<(String, gst::Element)>,
    internal_links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<(), BlockBuildError> {
    if !dials_out(srtsink) {
        internal_links.push((
            ElementPadRef::pad(mux_id, "src"),
            ElementPadRef::pad(sink_id, "sink"),
        ));
        return Ok(());
    }

    let queue_id = format!("{}:srtsink_queue", instance_id);
    let queue = gst::ElementFactory::make("queue")
        .name(&queue_id)
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 0u32)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("srtsink queue: {}", e)))?;
    info!(
        "SRT output {}: caller mode, srtsink behind a leaky queue so a missing peer only drops this output's data (#1259)",
        instance_id
    );

    internal_links.push((
        ElementPadRef::pad(mux_id, "src"),
        ElementPadRef::pad(&queue_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&queue_id, "src"),
        ElementPadRef::pad(sink_id, "sink"),
    ));
    elements.push((queue_id, queue));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srtsink(uri: &str) -> gst::Element {
        gst::init().unwrap();
        let sink = gst::ElementFactory::make("srtsink")
            .build()
            .expect("srtsink");
        sink.set_property("uri", uri);
        sink
    }

    #[test]
    fn dials_out_follows_the_uri() {
        let cases = [
            ("srt://127.0.0.1:5000?mode=caller", true),
            ("srt://127.0.0.1:5000", true),
            ("srt://127.0.0.1:5000?mode=rendezvous", true),
            ("srt://127.0.0.1:5000?mode=listener", false),
            ("srt://:5000?mode=listener", false),
            ("srt://:5000", false),
            // No host: GStreamer opens a listener whatever mode says.
            ("srt://:5000?mode=caller", false),
            ("srt://0.0.0.0:5000", true),
        ];
        for (uri, expected) in cases {
            assert_eq!(dials_out(&srtsink(uri)), expected, "{uri}");
        }
    }
}
