//! Sends the payload type a WHEP viewer offered when the WHEP proxy had to
//! renumber it for webrtcsink.
//!
//! Chrome and Safari offer H.265 on payload types below 96, which
//! `rtph265pay` cannot produce (its src template is `payload=[96,127]`). The
//! proxy moves them into [96, 127] in the offer, maps the answer back, and
//! names the moves in a session attribute of the offer: [`PT_MAP_ATTRIBUTE`],
//! `a=x-strom-pt-map:<renumbered>=<offered> ...`. webrtcsink negotiates the
//! renumbered payload type and its payloader stamps it on every packet, so the
//! browser receives a payload type it never offered and binds no decoder.
//!
//! For each viewer, this module reads the attribute from webrtcbin's remote
//! description and:
//! - rewrites the payload type in the RTP header right after the payloader,
//!   back to the offered one;
//! - adds the offered payload types to `rtprtxsend`'s maps, so retransmission
//!   still finds its history for the packets and sends RTX on the offered RTX
//!   payload type.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_webrtc as gst_webrtc;
use tracing::{debug, info};

/// Session-level SDP attribute the WHEP proxy adds to an offer whose payload
/// types it renumbered.
pub const PT_MAP_ATTRIBUTE: &str = "x-strom-pt-map";

/// Marks a payload type the table does not rewrite.
const KEEP: u8 = 0xff;

/// Renumbered payload type -> offered payload type, indexed by the 7-bit
/// payload type. Copied into each closure: no locks or lookups per packet.
type PtTable = [u8; 128];

/// Restore the offered payload types for every viewer of `whepserversink`.
pub(crate) fn install(whepserversink: &gst::Element) {
    whepserversink.connect("payloader-setup", false, |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let payloader = values[3].get::<gst::Element>().unwrap();
        if let Some(webrtcbin) = session_webrtcbin(&payloader) {
            if let Some(table) = remote_pt_table(&webrtcbin) {
                info!(
                    "WHEP Output: viewer {} offered payload types the proxy renumbered ({}), \
                     sending the offered ones from {}",
                    consumer_id,
                    describe(&table),
                    payloader.name()
                );
                restore_on_payloader(&payloader, table);
                restore_on_rtxsend(&webrtcbin, table);
            }
        }
        // Let the other handlers and webrtcsink's defaults run.
        Some(false.to_value())
    });
}

/// The webrtcbin of the session pipeline `payloader` is in. `None` in a
/// discovery pipeline, which has none.
pub(crate) fn session_webrtcbin(payloader: &gst::Element) -> Option<gst::Element> {
    let mut top: gst::Object = payloader.clone().upcast();
    while let Some(parent) = top.parent() {
        top = parent;
    }
    let bin = top.downcast::<gst::Bin>().ok()?;
    // webrtcsink is still adding elements to the session pipeline from other
    // threads; `find` resyncs and restarts when the bin changes under it.
    bin.iterate_elements()
        .find(|e| factory_name(&e).as_deref() == Some("webrtcbin"))
}

fn factory_name(element: &gst::Element) -> Option<glib::GString> {
    element.factory().map(|f| f.name())
}

/// The rewrite table from `webrtcbin`'s remote description, or `None` when
/// the offer carries no [`PT_MAP_ATTRIBUTE`].
fn remote_pt_table(webrtcbin: &gst::Element) -> Option<PtTable> {
    let desc =
        webrtcbin.property::<Option<gst_webrtc::WebRTCSessionDescription>>("remote-description")?;
    parse_pt_map(desc.sdp().attribute_val(PT_MAP_ATTRIBUTE)?)
}

/// Parse `105=49 113=50` into a table; `None` if it names nothing usable.
fn parse_pt_map(value: &str) -> Option<PtTable> {
    let mut table = [KEEP; 128];
    let mut any = false;
    for pair in value.split_whitespace() {
        let Some((from, to)) = pair.split_once('=') else {
            continue;
        };
        let (Ok(from), Ok(to)) = (from.parse::<u8>(), to.parse::<u8>()) else {
            continue;
        };
        if from < 128 && to < 128 {
            table[from as usize] = to;
            any = true;
        }
    }
    any.then_some(table)
}

fn describe(table: &PtTable) -> String {
    table
        .iter()
        .enumerate()
        .filter(|(_, to)| **to != KEEP)
        .map(|(from, to)| format!("{}->{}", from, to))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rewrite the payload type of each packet leaving `payloader`.
///
/// BUFFER probe, so it fires per packet: it reads and writes one byte of the
/// RTP header through the copied table. The payloader's packets are its own
/// (refcount 1, header memory it just allocated), so making them writable
/// copies nothing.
fn restore_on_payloader(payloader: &gst::Element, table: PtTable) {
    let Some(src) = payloader.static_pad("src") else {
        return;
    };
    src.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_, info| {
            match info.data.as_mut() {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    rewrite_pt(buffer.make_mut(), &table);
                }
                Some(gst::PadProbeData::BufferList(list)) => {
                    list.make_mut().foreach_mut(|mut buffer, _| {
                        rewrite_pt(buffer.make_mut(), &table);
                        std::ops::ControlFlow::Continue(Some(buffer))
                    });
                }
                _ => {}
            }
            gst::PadProbeReturn::Ok
        },
    );
}

/// Rewrite the payload type in an RTP packet's header through `table`,
/// keeping the marker bit.
fn rewrite_pt(buffer: &mut gst::BufferRef, table: &PtTable) {
    let mut head = [0u8; 2];
    if buffer.copy_to_slice(0, &mut head).is_err() || head[0] >> 6 != 2 {
        return;
    }
    let to = table[(head[1] & 0x7f) as usize];
    if to != KEEP {
        let _ = buffer.copy_from_slice(1, &[(head[1] & 0x80) | to]);
    }
}

/// Teach every `rtprtxsend` in `webrtcbin`, now and later, the offered
/// payload types.
fn restore_on_rtxsend(webrtcbin: &gst::Element, table: PtTable) {
    let Some(bin) = webrtcbin.downcast_ref::<gst::Bin>() else {
        return;
    };
    for_each_element(bin, |element| watch_rtxsend(element, table));
    // webrtcbin creates its RTX senders when rtpbin asks for them. The
    // handler only holds the table: the element arrives as an argument.
    bin.connect("deep-element-added", false, move |values| {
        if let Ok(element) = values[2].get::<gst::Element>() {
            watch_rtxsend(&element, table);
            // A bin added whole (webrtcbin's aux sender bin) announces only
            // itself.
            if let Some(added) = element.downcast_ref::<gst::Bin>() {
                for_each_element(added, |child| watch_rtxsend(child, table));
            }
        }
        None
    });
}

/// Call `f` on every element in `bin`, recursively. Restarts when the bin
/// changes while it is walked; an element may then be seen twice, which
/// [`watch_rtxsend`]'s patching tolerates.
fn for_each_element(bin: &gst::Bin, mut f: impl FnMut(&gst::Element)) {
    let mut iter = bin.iterate_recurse();
    while let Err(gst::IteratorError::Resync) = iter.foreach(|element| f(&element)) {
        iter.resync();
    }
}

/// Patch `element`'s maps now and whenever webrtcbin sets them again.
fn watch_rtxsend(element: &gst::Element, table: PtTable) {
    if factory_name(element).as_deref() != Some("rtprtxsend") {
        return;
    }
    patch_rtxsend(element, &table);
    element.connect_notify(Some("payload-type-map"), move |element, _| {
        patch_rtxsend(element, &table)
    });
}

/// Add an entry for the offered payload type next to each renumbered one in
/// `payload-type-map` (media -> RTX) and `clock-rate-map`. Setting the map
/// notifies again, and that call finds nothing to add.
fn patch_rtxsend(rtxsend: &gst::Element, table: &PtTable) {
    let map_pt = |pt: u8| match table.get(pt as usize) {
        Some(&to) if to != KEEP => to,
        _ => pt,
    };
    for property in ["payload-type-map", "clock-rate-map"] {
        let Some(map) = rtxsend.property::<Option<gst::Structure>>(property) else {
            continue;
        };
        if let Some(patched) = patched_map(&map, property == "payload-type-map", &map_pt) {
            debug!(
                "WHEP Output: {} {} {} -> {}",
                rtxsend.name(),
                property,
                map,
                patched
            );
            rtxsend.set_property(property, &patched);
        }
    }
}

/// `map` with an entry added for each renumbered key, or `None` if it has
/// them all. `values_are_pts` maps the value too (the RTX payload type).
fn patched_map(
    map: &gst::StructureRef,
    values_are_pts: bool,
    map_pt: &impl Fn(u8) -> u8,
) -> Option<gst::Structure> {
    let mut patched = map.to_owned();
    let mut changed = false;
    for (field, value) in map.iter() {
        let Ok(key) = field.parse::<u8>() else {
            continue;
        };
        let new_key = map_pt(key);
        if new_key == key {
            continue;
        }
        let Ok(value) = value.get::<u32>() else {
            continue;
        };
        let new_value = if values_are_pts {
            u8::try_from(value).map_or(value, |v| map_pt(v) as u32)
        } else {
            value
        };
        let new_field = new_key.to_string();
        if patched.get::<u32>(new_field.as_str()).ok() != Some(new_value) {
            patched.set(new_field.as_str(), new_value);
            changed = true;
        }
    }
    changed.then_some(patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp_packet(pt: u8, marker: bool) -> gst::Buffer {
        gst::init().unwrap();
        let mut data = vec![0u8; 16];
        data[0] = 0x80;
        data[1] = (if marker { 0x80 } else { 0 }) | pt;
        gst::Buffer::from_mut_slice(data)
    }

    fn pt_of(buffer: &gst::BufferRef) -> (u8, bool) {
        let mut head = [0u8; 2];
        buffer.copy_to_slice(0, &mut head).unwrap();
        (head[1] & 0x7f, head[1] & 0x80 != 0)
    }

    #[test]
    fn parses_the_proxy_attribute() {
        let table = parse_pt_map("105=49 113=50").unwrap();
        assert_eq!(table[105], 49);
        assert_eq!(table[113], 50);
        assert_eq!(table[96], KEEP);
        assert!(parse_pt_map("").is_none());
        assert!(parse_pt_map("bogus 300=1").is_none());
    }

    #[test]
    fn rewrites_the_payload_type_and_keeps_the_marker() {
        let table = parse_pt_map("105=49").unwrap();
        let mut packet = rtp_packet(105, true);
        rewrite_pt(packet.make_mut(), &table);
        assert_eq!(pt_of(&packet), (49, true));

        let mut other = rtp_packet(96, false);
        rewrite_pt(other.make_mut(), &table);
        assert_eq!(pt_of(&other), (96, false));
    }

    #[test]
    fn rtxsend_maps_gain_the_offered_payload_types() {
        gst::init().unwrap();
        let table = parse_pt_map("105=49 113=50").unwrap();
        let map_pt = |pt: u8| match table[pt as usize] {
            KEEP => pt,
            to => to,
        };
        let pt_map = gst::Structure::builder("application/x-rtp-pt-map")
            .field("105", 113u32)
            .field("96", 97u32)
            .build();
        let patched = patched_map(&pt_map, true, &map_pt).unwrap();
        assert_eq!(patched.get::<u32>("49").unwrap(), 50);
        assert_eq!(patched.get::<u32>("105").unwrap(), 113);
        assert_eq!(patched.get::<u32>("96").unwrap(), 97);
        // Patching again finds nothing to add.
        assert!(patched_map(&patched, true, &map_pt).is_none());

        let clock_map = gst::Structure::builder("application/x-rtp-clock-rate-map")
            .field("105", 90000u32)
            .build();
        let patched = patched_map(&clock_map, false, &map_pt).unwrap();
        assert_eq!(patched.get::<u32>("49").unwrap(), 90000);
    }

    /// The probe rewrites what a real `rtph265pay` sends, and `rtprtxsend`
    /// answers a retransmission request for a rewritten packet on the offered
    /// RTX payload type. Without the map patch rtprtxsend keeps no history
    /// for payload type 49 and sends nothing.
    #[test]
    fn rtph265pay_output_and_rtx_use_the_offered_payload_types() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        gst::init().unwrap();
        for factory in ["rtph265pay", "rtprtxsend", "x265enc", "h265parse"] {
            assert!(
                gst::ElementFactory::find(factory).is_some(),
                "{factory} is missing"
            );
        }
        let table = parse_pt_map("105=49 113=50").unwrap();
        let pipeline = gst::parse::launch(
            "videotestsrc is-live=true ! video/x-raw,width=320,height=240,framerate=30/1 ! \
             x265enc tune=zerolatency speed-preset=ultrafast key-int-max=5 ! h265parse ! \
             rtph265pay name=pay pt=105 mtu=400 aggregate-mode=zero-latency ! \
             rtprtxsend name=rtx max-size-packets=1000 ! appsink name=sink sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let pay = pipeline.by_name("pay").unwrap();
        let rtx = pipeline.by_name("rtx").unwrap();
        // What webrtcbin sets for the renumbered offer.
        rtx.set_property(
            "payload-type-map",
            gst::Structure::builder("application/x-rtp-pt-map")
                .field("105", 113u32)
                .build(),
        );
        restore_on_payloader(&pay, table);
        watch_rtxsend(&rtx, table);

        // Count the RTX packets rtprtxsend sends on the offered RTX type.
        let rtx_seen = Arc::new(AtomicU32::new(0));
        let seen = rtx_seen.clone();
        rtx.static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(b)) = info.data.as_ref() {
                    let mut h = [0u8; 2];
                    if b.copy_to_slice(0, &mut h).is_ok() && h[1] & 0x7f == 50 {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                }
                gst::PadProbeReturn::Ok
            });

        let sink = pipeline
            .by_name("sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut media_pts = std::collections::BTreeSet::new();
        let mut last = None;
        for _ in 0..30 {
            let sample = sink
                .try_pull_sample(gst::ClockTime::from_seconds(5))
                .expect("RTP packet");
            let buffer = sample.buffer().unwrap();
            let mut head = [0u8; 12];
            buffer.copy_to_slice(0, &mut head).unwrap();
            media_pts.insert(head[1] & 0x7f);
            let seq = u16::from_be_bytes([head[2], head[3]]);
            let ssrc = u32::from_be_bytes([head[8], head[9], head[10], head[11]]);
            last = Some((ssrc, seq));
        }
        assert_eq!(media_pts.into_iter().collect::<Vec<_>>(), vec![49]);

        let (ssrc, seq) = last.unwrap();
        let request = gst::event::CustomUpstream::new(
            gst::Structure::builder("GstRTPRetransmissionRequest")
                .field("seqnum", seq as u32)
                .field("ssrc", ssrc)
                .build(),
        );
        assert!(rtx.static_pad("src").unwrap().send_event(request));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while rtx_seen.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            let _ = sink.try_pull_sample(gst::ClockTime::from_mseconds(50));
        }
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            rtx_seen.load(Ordering::SeqCst) > 0,
            "rtprtxsend sent no RTX on payload type 50"
        );
    }
}
