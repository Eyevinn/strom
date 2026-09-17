//! A separate jitterbuffer latency for the audio of a WebRTC receive session.
//!
//! `webrtcbin` gives every `rtpjitterbuffer` the same `latency`. A lost packet
//! holds back everything behind it until it is retransmitted or its deadline,
//! the latency, passes. Video is retransmitted, so a longer latency recovers
//! frames. Opus is not retransmitted, and the jitterbuffer's requests for it go
//! unanswered, so for audio a longer latency only lengthens the stall.
//!
//! With BUNDLE, audio and video share one RTP session, so a jitterbuffer's
//! media is unknown when it is created. The payload type of its first packet,
//! mapped through the jitterbuffer's own `request-pt-map` signal, tells.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info};

/// Sets `latency` on `jitterbuffer` to `audio_latency_ms` if its stream turns
/// out to be audio. Other media keep the latency the jitterbuffer already has.
///
/// The probe fires on the first buffer (or buffer list) only and removes
/// itself, so it never runs on the steady-state data path.
pub fn set_audio_latency_on_first_packet(jitterbuffer: &gst::Element, audio_latency_ms: u32) {
    let Some(sink) = jitterbuffer.static_pad("sink") else {
        return;
    };
    let jitterbuffer_weak = jitterbuffer.downgrade();
    sink.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, info| {
            let Some(jitterbuffer) = jitterbuffer_weak.upgrade() else {
                return gst::PadProbeReturn::Remove;
            };
            let payload_type = match &info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => rtp_payload_type(buffer),
                Some(gst::PadProbeData::BufferList(list)) => list.get(0).and_then(rtp_payload_type),
                _ => None,
            };
            let Some(payload_type) = payload_type else {
                return gst::PadProbeReturn::Remove;
            };
            let caps = jitterbuffer
                .emit_by_name::<Option<gst::Caps>>("request-pt-map", &[&u32::from(payload_type)]);
            let media = caps
                .as_ref()
                .and_then(|c| c.structure(0))
                .and_then(|s| s.get::<&str>("media").ok());
            if media == Some("audio") {
                jitterbuffer.set_property("latency", audio_latency_ms);
                info!(
                    "WHIP Input: audio jitterbuffer {} latency set to {}ms",
                    jitterbuffer.name(),
                    audio_latency_ms
                );
            } else {
                debug!(
                    "WHIP Input: jitterbuffer {} carries {:?} (pt {}), latency left unchanged",
                    jitterbuffer.name(),
                    media,
                    payload_type
                );
            }
            gst::PadProbeReturn::Remove
        },
    );
}

/// RTP payload type from the second byte of the fixed header.
fn rtp_payload_type(buffer: &gst::BufferRef) -> Option<u8> {
    let map = buffer.map_readable().ok()?;
    (map.len() >= 12).then(|| map[1] & 0x7f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIO_PT: u8 = 111;
    const VIDEO_PT: u8 = 96;

    fn rtp_packet(payload_type: u8, seq: u16) -> gst::Buffer {
        let mut data = vec![0u8; 20];
        data[0] = 0x80;
        data[1] = payload_type;
        data[2..4].copy_from_slice(&seq.to_be_bytes());
        data[8..12].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        let mut buffer = gst::Buffer::from_mut_slice(data);
        buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        buffer
    }

    /// A jitterbuffer at 400 ms whose pt map says `AUDIO_PT` is Opus audio
    /// and `VIDEO_PT` is H.264 video, as webrtcbin's would.
    fn jitterbuffer() -> gst::Element {
        gst::init().unwrap();
        let jb = gst::ElementFactory::make("rtpjitterbuffer")
            .property("latency", 400u32)
            .build()
            .expect("rtpjitterbuffer (gst-plugins-good) is required");
        jb.connect("request-pt-map", false, |values| {
            let pt = values[1].get::<u32>().unwrap();
            let caps = if pt == u32::from(AUDIO_PT) {
                gst::Caps::builder("application/x-rtp")
                    .field("media", "audio")
                    .field("clock-rate", 48000i32)
                    .field("encoding-name", "OPUS")
                    .build()
            } else {
                gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("clock-rate", 90000i32)
                    .field("encoding-name", "H264")
                    .build()
            };
            Some(caps.to_value())
        });
        jb
    }

    fn push_first_packet(jb: &gst::Element, payload_type: u8) {
        let src = gst::Pad::builder(gst::PadDirection::Src)
            .name("src")
            .build();
        src.set_active(true).unwrap();
        src.link(&jb.static_pad("sink").unwrap()).unwrap();
        jb.set_state(gst::State::Paused).unwrap();
        src.push_event(gst::event::StreamStart::new("test"));
        src.push_event(gst::event::Caps::new(
            &gst::Caps::builder("application/x-rtp").build(),
        ));
        src.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()));
        let _ = src.push(rtp_packet(payload_type, 1));
    }

    #[test]
    fn audio_jitterbuffer_takes_the_audio_latency() {
        let jb = jitterbuffer();
        set_audio_latency_on_first_packet(&jb, 60);
        push_first_packet(&jb, AUDIO_PT);
        assert_eq!(jb.property::<u32>("latency"), 60);
        jb.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn video_jitterbuffer_keeps_its_latency() {
        let jb = jitterbuffer();
        set_audio_latency_on_first_packet(&jb, 60);
        push_first_packet(&jb, VIDEO_PT);
        assert_eq!(jb.property::<u32>("latency"), 400);
        jb.set_state(gst::State::Null).unwrap();
    }
}
