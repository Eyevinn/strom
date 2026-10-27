//! WHIP Output's video input: what it accepts, and how it refuses the rest.
//!
//! Video must arrive encoded (the output-block input rule): a `builtin.videoenc`
//! block in front of this one chooses codec, profile and bitrate, and disables
//! B-frames for RTP. This block takes the codecs WebRTC receivers decode:
//! H.264 in the profiles they support, VP8, and VP9 profile 0. Anything else
//! fails the flow with an element error that names what arrived and what to
//! change.
//!
//! Each video track starts with an `identity`, which the `video_in` pad feeds.
//! The decision is made in an EVENT_DOWNSTREAM probe on its src pad, from the
//! negotiated caps.
//!
//! On `whipsink` the identity's src pad stays unlinked until the probe accepts
//! the caps, and only then is linked to the chain for that codec. Linked
//! earlier, the caps query would reach a parser or payloader and refuse raw
//! video while the flow is still linking or negotiating, before this block can
//! say why.
//!
//! B-frames are not checked: they show only in the slices of each buffer, and
//! this block puts no probe on the buffer path. `builtin.videoenc` turns them
//! off for every encoder it builds.

use super::{element_id, make};
use crate::blocks::builtin::refusal::{accept_input, refuse_input, video_refusal};
use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info};

/// The first element of each video track, which its `video_in` pad feeds.
pub(super) const INPUT_ELEMENT: &str = "video_input";

/// The codecs this block sends, as the operator reads them.
const ACCEPTS: &str = "H.264, VP8 or VP9";

/// The H.264 profiles WebRTC receivers decode: 8-bit 4:2:0, High and below.
/// Browsers, and the servers that relay to them, have no decoder for 10-bit,
/// 4:2:2 or 4:4:4 H.264, and would accept the session and show nothing.
const WEBRTC_H264_PROFILES: &[&str] = &[
    "constrained-baseline",
    "baseline",
    "main",
    "high",
    "progressive-high",
    "constrained-high",
];

/// A video codec WHIP Output sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VideoCodec {
    H264,
    Vp8,
    Vp9,
}

impl VideoCodec {
    fn caps_name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "video/x-h264",
            VideoCodec::Vp8 => "video/x-vp8",
            VideoCodec::Vp9 => "video/x-vp9",
        }
    }
}

/// Which codec WHIP Output sends for these caps, or the operator-facing
/// reason it cannot send them.
///
/// `profile` is the caps' `profile` field. An absent profile is accepted:
/// nothing can be checked, and refusing would break every source that leaves
/// the field out.
pub(super) fn video_plan(caps_name: &str, profile: Option<&str>) -> Result<VideoCodec, String> {
    match caps_name {
        "video/x-h264" => match profile {
            Some(p) if !WEBRTC_H264_PROFILES.contains(&p) => Err(format!(
                "WHIP Output needs H.264 in the High, Main or Baseline profile, but its \
                 video input is {}, which WebRTC receivers do not decode. Set \
                 builtin.videoenc's profile to auto or high",
                p
            )),
            _ => Ok(VideoCodec::H264),
        },
        "video/x-vp8" => Ok(VideoCodec::Vp8),
        // Profile 0 is 8-bit 4:2:0. WebRTC receivers do not all decode the
        // others, and vp9enc picks the profile from the pixel format it gets.
        "video/x-vp9" => match profile {
            Some(p) if p != "0" => Err(format!(
                "WHIP Output needs VP9 in profile 0, but its video input is VP9 profile {}, \
                 which WebRTC receivers do not all decode. Feed the encoder 8-bit 4:2:0 \
                 video (I420 or NV12)",
                p
            )),
            _ => Ok(VideoCodec::Vp9),
        },
        other => Err(video_refusal("WHIP Output", ACCEPTS, other)),
    }
}

/// `whipsink`: the first element of each codec's chain on one video track.
pub(super) struct CodecChains {
    pub h264: gst::glib::WeakRef<gst::Element>,
    pub vp8: gst::glib::WeakRef<gst::Element>,
    pub vp9: gst::glib::WeakRef<gst::Element>,
}

impl CodecChains {
    fn first(&self, codec: VideoCodec) -> &gst::glib::WeakRef<gst::Element> {
        match codec {
            VideoCodec::H264 => &self.h264,
            VideoCodec::Vp8 => &self.vp8,
            VideoCodec::Vp9 => &self.vp9,
        }
    }
}

/// What the probe does once a video track's caps are accepted.
pub(super) enum OnAccept {
    /// `whipsink`: link the identity to the chain for the codec that arrived,
    /// which feeds that codec's payloader.
    LinkTo(CodecChains),
    /// `whipclientsink`: tell the sink which codec it carries (`video-caps`),
    /// and drop the caps fields webrtcsink would treat as a change it cannot
    /// make without renegotiating.
    TellWebrtcSink(gst::glib::WeakRef<gst::Element>),
}

/// H.264 caps fields removed before webrtcsink sees them.
///
/// The first four are ones webrtcsink's own parser chain does not take, as
/// WHEP Output finds for `whepserversink`. `level` goes because webrtcsink
/// refuses a caps change to a higher level than the session started with
/// ("Renegotiation is not supported") and the flow stops with not-negotiated:
/// a pre-encoded source that goes from 720p to 1080p would end the publish.
/// webrtcsink's `h264parse` puts the level back from the stream itself.
const H264_FIELDS_WEBRTCSINK_DROPS: &[&str] = &[
    "coded-picture-structure",
    "chroma-format",
    "bit-depth-luma",
    "bit-depth-chroma",
    "level",
];

/// Build the `identity` that starts video track `slot`, with the probe that
/// accepts or refuses what arrives.
pub(super) fn build_video_input(
    instance_id: &str,
    slot: usize,
    on_accept: OnAccept,
) -> Result<(String, gst::Element), BlockBuildError> {
    let id = element_id(instance_id, INPUT_ELEMENT, slot);
    let input = make("identity", &id)?;
    let src_pad = input
        .static_pad("src")
        .ok_or_else(|| BlockBuildError::ElementCreation("identity has no src pad".into()))?;

    let instance = instance_id.to_string();
    // Posted once per refusal: the dropped caps event is sent again with the
    // next buffer, and each would otherwise post the same error.
    let refused = AtomicBool::new(false);
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let caps = caps_event.caps_owned();
        let Some(structure) = caps.structure(0) else {
            return gst::PadProbeReturn::Ok;
        };
        let caps_name = structure.name().to_string();
        let profile = structure.get::<String>("profile").ok();
        let Some(input) = pad.parent_element() else {
            return gst::PadProbeReturn::Ok;
        };

        let plan = video_plan(&caps_name, profile.as_deref()).and_then(|codec| match &on_accept {
            OnAccept::LinkTo(chains) => codec_change(pad, chains, codec).map(|_| codec),
            OnAccept::TellWebrtcSink(_) => Ok(codec),
        });
        let codec = match plan {
            Ok(codec) => codec,
            Err(reason) => {
                if !refused.swap(true, Ordering::SeqCst) {
                    refuse_input(&input, &reason);
                }
                // Nothing downstream can take these caps. Dropping the event
                // keeps the refusal from coming back upstream as
                // not-negotiated, which would replace the reason above as the
                // flow's error.
                return gst::PadProbeReturn::Drop;
            }
        };
        if refused.swap(false, Ordering::SeqCst) {
            accept_input(&input);
        }
        info!(
            "WHIP Output {} (video {}): sending {} (profile {})",
            instance,
            slot,
            caps_name,
            profile.as_deref().unwrap_or("unstated")
        );

        match &on_accept {
            OnAccept::LinkTo(chains) => {
                if !pad.is_linked() {
                    let linked = chains
                        .first(codec)
                        .upgrade()
                        .and_then(|n| n.static_pad("sink"))
                        .map(|sink| pad.link(&sink));
                    if !matches!(linked, Some(Ok(_))) {
                        error!(
                            "WHIP Output {} (video {}): could not link the video input to \
                             the {} chain: {:?}",
                            instance, slot, caps_name, linked
                        );
                    }
                }
            }
            OnAccept::TellWebrtcSink(sink) => {
                if let Some(sink) = sink.upgrade() {
                    sink.set_property("video-caps", gst::Caps::builder(&caps_name).build());
                }
                if codec == VideoCodec::H264 {
                    let mut normalized = caps.copy();
                    if let Some(s) = normalized.make_mut().structure_mut(0) {
                        s.remove_fields(H264_FIELDS_WEBRTCSINK_DROPS.iter().copied());
                    }
                    info.data = Some(gst::PadProbeData::Event(gst::event::Caps::new(&normalized)));
                }
            }
        }
        gst::PadProbeReturn::Ok
    });

    Ok((id, input))
}

/// `whipsink`: refuse a codec other than the one the track already sends. The
/// published session carries the first codec, and WHIP has no way to change it.
fn codec_change(pad: &gst::Pad, chains: &CodecChains, codec: VideoCodec) -> Result<(), String> {
    let Some(peer) = pad.peer().and_then(|p| p.parent_element()) else {
        return Ok(());
    };
    let current = [VideoCodec::H264, VideoCodec::Vp8, VideoCodec::Vp9]
        .into_iter()
        .find(|c| chains.first(*c).upgrade().as_ref() == Some(&peer));
    match current {
        Some(current) if current != codec => Err(format!(
            "WHIP Output's video input changed from {} to {}, but a published WHIP \
             session cannot change codec. Restart the flow",
            current.caps_name(),
            codec.caps_name()
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_in_a_webrtc_profile_is_accepted() {
        for p in WEBRTC_H264_PROFILES {
            assert_eq!(video_plan("video/x-h264", Some(p)), Ok(VideoCodec::H264));
        }
        assert_eq!(video_plan("video/x-h264", None), Ok(VideoCodec::H264));
    }

    #[test]
    fn h264_outside_the_webrtc_profiles_is_refused_by_name() {
        for p in ["high-4:4:4", "high-4:2:2", "high-10"] {
            let reason = video_plan("video/x-h264", Some(p)).expect_err("refused");
            assert!(reason.contains(p), "{}", reason);
            assert!(reason.contains("builtin.videoenc's profile"), "{}", reason);
        }
    }

    #[test]
    fn raw_video_points_at_the_video_encoder_block() {
        let reason = video_plan("video/x-raw", None).expect_err("refused");
        assert!(reason.contains("WHIP Output"), "{}", reason);
        assert!(reason.contains("builtin.videoenc"), "{}", reason);
    }

    #[test]
    fn vp8_and_vp9_profile_0_are_accepted() {
        assert_eq!(video_plan("video/x-vp8", None), Ok(VideoCodec::Vp8));
        assert_eq!(video_plan("video/x-vp9", None), Ok(VideoCodec::Vp9));
        assert_eq!(video_plan("video/x-vp9", Some("0")), Ok(VideoCodec::Vp9));
    }

    #[test]
    fn vp9_outside_profile_0_is_refused_by_name() {
        for p in ["1", "2", "3"] {
            let reason = video_plan("video/x-vp9", Some(p)).expect_err("refused");
            assert!(reason.contains(&format!("profile {}", p)), "{}", reason);
        }
    }

    #[test]
    fn another_codec_is_named() {
        let reason = video_plan("video/x-h265", None).expect_err("refused");
        assert!(reason.contains("video/x-h265"), "{}", reason);
        assert!(reason.contains("H.264, VP8 or VP9"), "{}", reason);
    }
}
