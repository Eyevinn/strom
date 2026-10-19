//! WHIP Output's video input: what it accepts, and how it refuses the rest.
//!
//! Video must arrive encoded (the output-block input rule): a `builtin.videoenc`
//! block in front of this one chooses codec, profile and bitrate, and disables
//! B-frames for RTP. This block takes H.264 in the profiles WebRTC receivers
//! decode. Anything else fails the flow with an element error that names what
//! arrived and what to change.
//!
//! Each video track starts with an `identity`, which the `video_in` pad feeds.
//! The decision is made in an EVENT_DOWNSTREAM probe on its src pad, from the
//! negotiated caps.
//!
//! On `whipsink` the identity's src pad stays unlinked until the probe accepts
//! the caps, and only then is linked to `h264parse`. Linked earlier, the caps
//! query would reach `h264parse` and refuse raw video while the flow is still
//! linking or negotiating, before this block can say why.

use super::{element_id, make};
use crate::blocks::builtin::refusal::{accept_input, refuse_input, video_refusal};
use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info};

/// The first element of each video track, which its `video_in` pad feeds.
pub(super) const INPUT_ELEMENT: &str = "video_input";

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

/// Whether WHIP Output can send video with these caps, or the operator-facing
/// reason it cannot.
///
/// `profile` is the caps' `profile` field. An absent profile is accepted:
/// nothing can be checked, and refusing would break every source that leaves
/// the field out.
pub(super) fn video_plan(caps_name: &str, profile: Option<&str>) -> Result<(), String> {
    match caps_name {
        "video/x-h264" => match profile {
            Some(p) if !WEBRTC_H264_PROFILES.contains(&p) => Err(format!(
                "WHIP Output needs H.264 in the High, Main or Baseline profile, but its \
                 video input is {}, which WebRTC receivers do not decode. Set \
                 builtin.videoenc's profile to auto or high",
                p
            )),
            _ => Ok(()),
        },
        other => Err(video_refusal("WHIP Output", "H.264", other)),
    }
}

/// What the probe does once a video track's caps are accepted.
#[derive(Clone)]
pub(super) enum OnAccept {
    /// `whipsink`: link the identity to this element, `h264parse`, which
    /// feeds `rtph264pay`.
    LinkTo(gst::glib::WeakRef<gst::Element>),
    /// `whipclientsink`: tell the sink which codec it carries (`video-caps`),
    /// and drop the H.264 caps fields webrtcsink's own parser chain does not
    /// take, as WHEP Output does for `whepserversink`.
    TellWebrtcSink(gst::glib::WeakRef<gst::Element>),
}

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

        if let Err(reason) = video_plan(&caps_name, profile.as_deref()) {
            if !refused.swap(true, Ordering::SeqCst) {
                refuse_input(&input, &reason);
            }
            // Nothing downstream can take these caps. Dropping the event keeps
            // the refusal from coming back upstream as not-negotiated, which
            // would replace the reason above as the flow's error.
            return gst::PadProbeReturn::Drop;
        }
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

        if let OnAccept::LinkTo(next) = &on_accept {
            if !pad.is_linked() {
                let linked = next
                    .upgrade()
                    .and_then(|n| n.static_pad("sink"))
                    .map(|sink| pad.link(&sink));
                if !matches!(linked, Some(Ok(_))) {
                    error!(
                        "WHIP Output {} (video {}): could not link the video input to \
                         h264parse: {:?}",
                        instance, slot, linked
                    );
                }
            }
        }
        if let OnAccept::TellWebrtcSink(sink) = &on_accept {
            if let Some(sink) = sink.upgrade() {
                sink.set_property("video-caps", gst::Caps::builder(&caps_name).build());
            }
            let mut normalized = caps.copy();
            if let Some(s) = normalized.make_mut().structure_mut(0) {
                s.remove_fields([
                    "coded-picture-structure",
                    "chroma-format",
                    "bit-depth-luma",
                    "bit-depth-chroma",
                ]);
            }
            info.data = Some(gst::PadProbeData::Event(gst::event::Caps::new(&normalized)));
        }
        gst::PadProbeReturn::Ok
    });

    Ok((id, input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_in_a_webrtc_profile_is_accepted() {
        for p in WEBRTC_H264_PROFILES {
            assert_eq!(video_plan("video/x-h264", Some(p)), Ok(()));
        }
        assert_eq!(video_plan("video/x-h264", None), Ok(()));
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
    fn another_codec_is_named() {
        let reason = video_plan("video/x-vp9", None).expect_err("refused");
        assert!(reason.contains("video/x-vp9"), "{}", reason);
        assert!(reason.contains("H.264"), "{}", reason);
    }
}
