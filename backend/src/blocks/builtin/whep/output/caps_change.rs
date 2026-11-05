//! Which mid-stream changes of an encoded video input WHEP Output can follow.
//!
//! `whepserversink` (webrtcsink) refuses a CAPS event on its input that would
//! need a new WebRTC negotiation, and only lets through changes of fields that
//! are not in the SDP: size, frame rate, colorimetry and the like. On top of
//! that it lets the H.264 profile and level go down, but never up: a level
//! rise is refused ("Incompatible H264 levels"), unless the `video-caps`
//! property names a maximum level. For H.265 there is no such rule, so any
//! change of level or tier is refused. A source that switches size mid-stream
//! (an HLS variant switch) makes the encoder pick a higher level, and the flow
//! then stops with `not-negotiated`.
//!
//! The block therefore removes the level (and the H.265 tier) from its input
//! caps before they reach the sink. webrtcsink reads neither from its input
//! caps for anything else: a viewer's session takes the level from the stream
//! itself, and browsers offer `level-asymmetry-allowed=1` and decode a higher
//! level in practice (see `webrtcsink_profile_filter`, which already strips the
//! level the viewer offered).
//!
//! Every other change webrtcsink refuses fails the flow here with an element
//! error that names what changed, instead of a bare `not-negotiated`.

use gstreamer as gst;
use gstreamer::prelude::*;

/// Fields removed from H.264 / H.265 input caps before they reach the sink.
///
/// h264parse adds `coded-picture-structure`, `chroma-format` and the bit
/// depths as it parses the stream, so they appear on a later CAPS event.
/// `level` (and `tier` for H.265) changes with the input's size and rate.
pub(super) fn stripped_fields(caps_name: &str) -> &'static [&'static str] {
    match caps_name {
        "video/x-h264" => &[
            "coded-picture-structure",
            "chroma-format",
            "bit-depth-luma",
            "bit-depth-chroma",
            "level",
        ],
        "video/x-h265" => &[
            "coded-picture-structure",
            "chroma-format",
            "bit-depth-luma",
            "bit-depth-chroma",
            "level",
            "tier",
        ],
        _ => &[],
    }
}

/// `caps` with [`stripped_fields`] removed, or `None` when nothing is removed.
pub(super) fn normalize(caps: &gst::CapsRef) -> Option<gst::Caps> {
    let s = caps.structure(0)?;
    let fields = stripped_fields(s.name().as_str());
    if !fields.iter().any(|f| s.has_field(*f)) {
        return None;
    }
    let mut new_caps = caps.to_owned();
    if let Some(s) = new_caps.make_mut().structure_mut(0) {
        s.remove_fields(fields.iter().copied());
    }
    Some(new_caps)
}

/// H.264 profiles in the order webrtcsink ranks them: a later one may not
/// follow an earlier one mid-stream.
const H264_PROFILE_ORDER: &[&str] = &[
    "constrained-baseline",
    "baseline",
    "main",
    "constrained-high",
    "high",
    "high-10",
    "high-4:2:2",
    "high-4:4:4",
];

/// Fields of any video caps that webrtcsink lets change without a new session.
const VIDEO_ALLOWED_CHANGES: &[&str] = &[
    "width",
    "height",
    "framerate",
    "pixel-aspect-ratio",
    "colorimetry",
    "chroma-site",
    "interlace-mode",
];

const ENCODED_VIDEO: &[&str] = &[
    "video/x-h264",
    "video/x-h265",
    "video/x-vp8",
    "video/x-vp9",
    "video/x-av1",
];

/// Why webrtcsink would refuse a change of the block's encoded video input
/// from `old` to `new` (both after [`normalize`]), or `None` when it follows
/// it. Mirrors webrtcsink's `input_caps_change_allowed`. Raw video is not
/// judged here: the block's input bridge may convert it before the sink.
pub(super) fn refused_change(old: &gst::CapsRef, new: &gst::CapsRef) -> Option<String> {
    let (old_s, new_s) = (old.structure(0)?, new.structure(0)?);
    let (old_name, new_name) = (old_s.name().as_str(), new_s.name().as_str());
    if !ENCODED_VIDEO.contains(&old_name) && !ENCODED_VIDEO.contains(&new_name) {
        return None;
    }
    if old_name != new_name {
        return Some(format!(
            "WHEP Output cannot change its video input from {old_name} to {new_name} \
             mid-stream: a WebRTC session keeps the codec it started with. Keep the \
             codec upstream the same, or restart the flow."
        ));
    }

    let mut old_s = old_s.to_owned();
    let mut new_s = new_s.to_owned();
    if old_name == "video/x-h264" {
        let rank = |s: &gst::Structure| {
            s.get::<&str>("profile")
                .ok()
                .and_then(|p| H264_PROFILE_ORDER.iter().position(|o| *o == p))
        };
        if let (Some(o), Some(n)) = (rank(&old_s), rank(&new_s)) {
            if n > o {
                return Some(format!(
                    "WHEP Output cannot raise the H.264 profile of its video input from \
                     {} to {} mid-stream: viewers were set up for the first. Set a fixed \
                     profile on the encoder upstream (the Video Encoder's Profile \
                     property), or restart the flow.",
                    H264_PROFILE_ORDER[o], H264_PROFILE_ORDER[n]
                ));
            }
        }
        old_s.remove_fields(["profile", "level"]);
        new_s.remove_fields(["profile", "level"]);
    }
    for s in [&mut old_s, &mut new_s] {
        s.remove_fields(VIDEO_ALLOWED_CHANGES.iter().copied());
        s.remove_field("codec_data");
        if matches!(old_name, "video/x-vp8" | "video/x-vp9") {
            s.remove_field("profile");
        }
        // A missing multiview mode is mono.
        if s.get::<&str>("multiview-mode")
            .map_or(true, |m| m == "mono")
        {
            s.remove_fields(["multiview-mode", "multiview-flags"]);
        }
    }
    if old_s == new_s {
        return None;
    }

    let mut changed: Vec<String> = Vec::new();
    for (field, value) in old_s.iter() {
        let serialized = |v: &gst::glib::SendValue| v.serialize().ok().map(|s| s.to_string());
        if new_s.value(field).ok().and_then(serialized) != serialized(value) {
            changed.push(field.to_string());
        }
    }
    for (field, _) in new_s.iter() {
        if !old_s.has_field(field) && !changed.iter().any(|c| c == field.as_str()) {
            changed.push(field.to_string());
        }
    }
    let fields = changed.join(", ");
    Some(format!(
        "WHEP Output cannot follow a change of {fields} in its {old_name} input \
         mid-stream (from {old_s} to {new_s}): without a new WebRTC session, webrtcsink \
         only follows changes of size and frame rate. Keep {fields} the same upstream, \
         or restart the flow."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(s: &str) -> gst::Caps {
        let _ = gst::init();
        s.parse().expect("caps")
    }

    fn normalized(s: &str) -> gst::Caps {
        let c = caps(s);
        normalize(&c).unwrap_or(c)
    }

    #[test]
    fn level_and_tier_are_stripped() {
        let h264 = normalized(
            "video/x-h264, stream-format=byte-stream, alignment=au, profile=high, level=(string)4.2",
        );
        let s = h264.structure(0).unwrap();
        assert!(!s.has_field("level"));
        assert_eq!(s.get::<&str>("profile").unwrap(), "high");

        let h265 = normalized(
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, tier=main, level=(string)5.1",
        );
        let s = h265.structure(0).unwrap();
        assert!(!s.has_field("level") && !s.has_field("tier"));
        assert_eq!(s.get::<&str>("profile").unwrap(), "main");

        assert!(normalize(&caps("video/x-vp9, profile=(string)0")).is_none());
    }

    /// The HLS variant switch: 640x360p25 level 3 to 1080p50 level 4.2.
    #[test]
    fn a_size_and_level_rise_is_followed() {
        let old = normalized(
            "video/x-h264, stream-format=byte-stream, alignment=au, profile=high, \
             level=(string)3, width=640, height=360, framerate=25/1",
        );
        let new = normalized(
            "video/x-h264, stream-format=byte-stream, alignment=au, profile=high, \
             level=(string)4.2, width=1920, height=1080, framerate=50/1",
        );
        assert_eq!(refused_change(&old, &new), None);

        let old = normalized(
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, tier=main, \
             level=(string)3, width=640, height=360, framerate=25/1",
        );
        let new = normalized(
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, tier=main, \
             level=(string)4.1, width=1920, height=1080, framerate=50/1",
        );
        assert_eq!(refused_change(&old, &new), None);
    }

    #[test]
    fn an_h264_profile_rise_is_refused_with_the_property_that_fixes_it() {
        let old = caps("video/x-h264, stream-format=byte-stream, alignment=au, profile=main");
        let new = caps("video/x-h264, stream-format=byte-stream, alignment=au, profile=high");
        let err = refused_change(&old, &new).expect("refused");
        assert!(err.contains("from main to high"), "{err}");
        assert!(err.contains("Profile property"), "{err}");
        // Going down is followed.
        assert_eq!(refused_change(&new, &old), None);
    }

    #[test]
    fn other_changes_are_refused_with_the_fields_named() {
        let old = caps("video/x-h265, stream-format=byte-stream, alignment=au, profile=main");
        let new = caps("video/x-h265, stream-format=byte-stream, alignment=au, profile=main-10");
        let err = refused_change(&old, &new).expect("refused");
        assert!(err.contains("change of profile"), "{err}");

        let old = caps("video/x-h264, stream-format=byte-stream, alignment=au");
        let new = caps("video/x-h264, stream-format=avc, alignment=au");
        let err = refused_change(&old, &new).expect("refused");
        assert!(err.contains("change of stream-format"), "{err}");

        let err = refused_change(&old, &caps("video/x-vp9")).expect("refused");
        assert!(err.contains("from video/x-h264 to video/x-vp9"), "{err}");
    }

    #[test]
    fn raw_video_is_not_judged() {
        let old = caps("video/x-raw, format=NV12, width=640, height=360");
        let new = caps("video/x-raw, format=I420, width=1920, height=1080");
        assert_eq!(refused_change(&old, &new), None);
    }
}
