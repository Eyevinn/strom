//! Which mid-stream changes of an encoded video input WHEP Output can follow.
//!
//! `whepserversink` (webrtcsink) refuses a CAPS event on its input that would
//! need a new WebRTC negotiation, and only lets through changes of fields that
//! are not in the SDP: size, frame rate, colorimetry and the like. On top of
//! that it lets the H.264 profile and level go down, but never up: a level
//! rise is refused ("Incompatible H264 levels"), unless the `video-caps`
//! property names a maximum level. For H.265 and AV1 there is no such rule, so
//! any change of level or tier is refused. A source that switches size
//! mid-stream (an HLS variant switch) makes the encoder pick a higher level,
//! and the flow then stops with `not-negotiated`.
//!
//! The block therefore removes the level (and the H.265 / AV1 tier) from its
//! input caps before they reach the sink. webrtcsink reads neither from its
//! input caps for anything else: a viewer's session takes the level from the
//! stream itself, and browsers offer `level-asymmetry-allowed=1` and decode a
//! higher level in practice (see `webrtcsink_profile_filter`, which already
//! strips the level the viewer offered).
//!
//! webrtcsink also refuses a field that appears on a later CAPS event, though
//! that is a refinement, not a change: the parsers (h264parse, h265parse,
//! vp9parse, av1parse) send caps first and add `chroma-format`, the bit depths
//! and the like once they have parsed the first frame. The block keeps such a
//! field away from the sink, and keeps a field that disappears at its earlier
//! value, so the sink sees the caps it started with.
//!
//! What remains is a field present before and after with another value. Where
//! webrtcsink refuses that, the flow fails here with an element error that
//! names what changed, instead of a bare `not-negotiated`.

use gstreamer as gst;
use gstreamer::prelude::*;

/// Fields removed from encoded video input caps before they reach the sink:
/// the level (and tier), which change with the input's size and rate.
pub(super) fn stripped_fields(caps_name: &str) -> &'static [&'static str] {
    match caps_name {
        "video/x-h264" => &["level"],
        "video/x-h265" | "video/x-av1" => &["level", "tier"],
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

/// Whether webrtcsink ignores `field` of `caps_name` when it compares caps,
/// or the downstream chain needs it as it comes (`codec_data`).
fn ignored_field(caps_name: &str, field: &str) -> bool {
    VIDEO_ALLOWED_CHANGES.contains(&field)
        || field == "codec_data"
        || (caps_name == "video/x-h264" && matches!(field, "profile" | "level"))
        || (matches!(caps_name, "video/x-vp8" | "video/x-vp9") && field == "profile")
}

/// Follows the CAPS events of one video input of the sink.
#[derive(Default)]
pub(super) struct CapsFollower {
    /// The last incoming caps, after [`normalize`].
    incoming: Option<gst::Caps>,
    /// The last caps given to the sink.
    sent: Option<gst::Caps>,
}

impl CapsFollower {
    /// The caps to give the sink for `incoming`, or why webrtcsink would
    /// refuse them.
    ///
    /// The caps are `incoming` with [`stripped_fields`] removed and, after a
    /// previous CAPS event of the same codec, refined fields kept at what the
    /// sink already has: a field the sink did not get is removed, and one it
    /// got that `incoming` lacks keeps its earlier value. Fields webrtcsink
    /// ignores pass as they come. A change is a field present in the previous
    /// and in this CAPS event with another value (see [`refused_change`]).
    pub(super) fn next(&mut self, incoming: &gst::CapsRef) -> Result<gst::Caps, String> {
        let normalized = normalize(incoming).unwrap_or_else(|| incoming.to_owned());
        if let Some(reason) = self
            .incoming
            .as_ref()
            .and_then(|previous| refused_change(previous, &normalized))
        {
            return Err(reason);
        }
        let mut caps = normalized.clone();
        if let Some(sent) = self.sent.as_ref() {
            refine(sent, &mut caps);
        }
        self.incoming = Some(normalized);
        self.sent = Some(caps.clone());
        Ok(caps)
    }
}

/// Keep the fields of `caps` that webrtcsink compares at what `sent` has.
fn refine(sent: &gst::CapsRef, caps: &mut gst::Caps) {
    let Some(old_s) = sent.structure(0) else {
        return;
    };
    let Some(new_s) = caps.make_mut().structure_mut(0) else {
        return;
    };
    let name = new_s.name().to_string();
    if !ENCODED_VIDEO.contains(&name.as_str()) || old_s.name() != name.as_str() {
        return;
    }
    let added: Vec<String> = new_s
        .fields()
        .filter(|f| !old_s.has_field(*f) && !ignored_field(&name, f))
        .map(|f| f.to_string())
        .collect();
    new_s.remove_fields(added.iter().map(String::as_str));
    for (field, value) in old_s.iter() {
        if !new_s.has_field(field) && !ignored_field(&name, field) {
            new_s.set_value(field, value.clone());
        }
    }
}

/// Why webrtcsink would refuse a change of the block's encoded video input
/// from `old` to `new` (both after [`normalize`]), or `None` when it follows
/// it. Mirrors webrtcsink's `input_caps_change_allowed`, except that only a
/// field present in both with another value counts as a change:
/// [`CapsFollower`] keeps a field that appears or disappears away from the
/// sink. Raw video is
/// not judged here: the block's input bridge may convert it before the sink.
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

    if old_name == "video/x-h264" {
        let rank = |s: &gst::StructureRef| {
            s.get::<&str>("profile")
                .ok()
                .and_then(|p| H264_PROFILE_ORDER.iter().position(|o| *o == p))
        };
        if let (Some(o), Some(n)) = (rank(old_s), rank(new_s)) {
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
    }

    let serialized = |v: &gst::glib::SendValue| v.serialize().ok().map(|s| s.to_string());
    let mut changed: Vec<String> = old_s
        .iter()
        .filter(|(field, _)| {
            !ignored_field(old_name, field)
                && !matches!(field.as_str(), "multiview-mode" | "multiview-flags")
        })
        .filter_map(|(field, value)| {
            let new_value = new_s.value(field).ok()?;
            (serialized(new_value) != serialized(value)).then(|| field.to_string())
        })
        .collect();
    if multiview_changed(old_s, new_s) {
        changed.push("multiview-mode".to_owned());
    }
    if changed.is_empty() {
        return None;
    }
    let fields = changed.join(", ");
    Some(format!(
        "WHEP Output cannot follow a change of {fields} in its {old_name} input \
         mid-stream (from {old} to {new}): without a new WebRTC session, webrtcsink \
         only follows changes of size and frame rate. Keep {fields} the same upstream, \
         or restart the flow."
    ))
}

/// Whether the multiview mode changed, a missing one counting as mono.
fn multiview_changed(old: &gst::StructureRef, new: &gst::StructureRef) -> bool {
    let mode = |s: &gst::StructureRef| {
        s.get::<&str>("multiview-mode")
            .map(str::to_owned)
            .unwrap_or_else(|_| "mono".to_owned())
    };
    mode(old) != mode(new)
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

    /// Feed `events` through a [`CapsFollower`] as the probe does; the caps
    /// the sink got and the first refusal.
    fn run(events: &[&str]) -> (Vec<gst::Caps>, Option<String>) {
        let mut follower = CapsFollower::default();
        let mut sent: Vec<gst::Caps> = Vec::new();
        for e in events {
            match follower.next(&caps(e)) {
                Ok(c) => sent.push(c),
                Err(reason) => return (sent, Some(reason)),
            }
        }
        (sent, None)
    }

    /// The caps vp9parse sent in the flow that failed at start: the second
    /// event adds chroma-format and the bit depths.
    #[test]
    fn a_field_added_by_vp9parse_is_not_a_change() {
        let (sent, refusal) = run(&[
            "video/x-vp9, profile=(string)0, width=640, height=360, framerate=25/1, \
             alignment=super-frame, parsed=true, codec-alpha=false",
            "video/x-vp9, profile=(string)0, width=640, height=360, framerate=25/1, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8, \
             parsed=true, alignment=super-frame, codec-alpha=false",
            "video/x-vp9, profile=(string)0, width=1920, height=1080, framerate=50/1, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8, \
             parsed=true, alignment=super-frame, codec-alpha=false",
        ]);
        assert_eq!(refusal, None);
        let last = sent.last().unwrap().structure(0).unwrap();
        assert!(!last.has_field("chroma-format"), "{last}");
        assert_eq!(last.get::<i32>("width").unwrap(), 1920);
        // What the sink sees stays equal to what it started with, apart from
        // what webrtcsink ignores.
        for c in &sent[1..] {
            let mut a = sent[0].structure(0).unwrap().to_owned();
            let mut b = c.structure(0).unwrap().to_owned();
            for s in [&mut a, &mut b] {
                s.remove_fields(VIDEO_ALLOWED_CHANGES.iter().copied());
            }
            assert_eq!(a, b);
        }
    }

    /// av1parse adds fields the same way, and the AV1 level rises with size.
    #[test]
    fn av1_refinements_and_a_level_rise_are_followed() {
        let (sent, refusal) = run(&[
            "video/x-av1, stream-format=obu-stream, alignment=tu, width=640, height=360",
            "video/x-av1, stream-format=obu-stream, alignment=tu, width=640, height=360, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8, \
             parsed=true, profile=main, level=(string)2.1, tier=main",
            "video/x-av1, stream-format=obu-stream, alignment=tu, width=1920, height=1080, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8, \
             parsed=true, profile=main, level=(string)4.1, tier=main",
        ]);
        assert_eq!(refusal, None);
        let last = sent.last().unwrap().structure(0).unwrap();
        assert!(
            !last.has_field("level") && !last.has_field("tier"),
            "{last}"
        );
    }

    #[test]
    fn a_field_added_by_h264parse_is_not_a_change() {
        let (_, refusal) = run(&[
            "video/x-h264, stream-format=byte-stream, alignment=au, profile=high, level=(string)3",
            "video/x-h264, stream-format=byte-stream, alignment=au, profile=high, level=(string)3, \
             coded-picture-structure=frame, chroma-format=4:2:0, bit-depth-luma=(uint)8, \
             bit-depth-chroma=(uint)8",
            "video/x-h264, stream-format=byte-stream, alignment=au, profile=high, \
             level=(string)4.2, coded-picture-structure=frame, chroma-format=4:2:0, \
             bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8, width=1920, height=1080",
        ]);
        assert_eq!(refusal, None);
    }

    #[test]
    fn a_field_that_disappears_keeps_its_value() {
        let (sent, refusal) = run(&[
            "video/x-vp9, chroma-format=4:2:0, alignment=super-frame",
            "video/x-vp9, alignment=super-frame",
        ]);
        assert_eq!(refusal, None);
        let s = sent[1].structure(0).unwrap();
        assert_eq!(s.get::<&str>("chroma-format").unwrap(), "4:2:0");
    }

    #[test]
    fn a_changed_value_is_a_change() {
        let (_, refusal) = run(&[
            "video/x-vp9, alignment=super-frame",
            "video/x-vp9, alignment=super-frame, chroma-format=4:2:0, bit-depth-luma=(uint)8",
            "video/x-vp9, alignment=super-frame, chroma-format=4:4:4, bit-depth-luma=(uint)8",
        ]);
        let err = refusal.expect("refused");
        assert!(err.contains("change of chroma-format"), "{err}");

        let (_, refusal) = run(&[
            "video/x-vp9, alignment=super-frame, chroma-format=4:2:0, bit-depth-luma=(uint)8",
            "video/x-vp9, alignment=super-frame, chroma-format=4:4:4, bit-depth-luma=(uint)10",
        ]);
        let err = refusal.expect("refused");
        assert!(
            err.contains("change of chroma-format, bit-depth-luma"),
            "{err}"
        );

        let (_, refusal) = run(&[
            "video/x-av1, stream-format=obu-stream, alignment=tu, profile=main",
            "video/x-av1, stream-format=obu-stream, alignment=tu, profile=high",
        ]);
        assert!(refusal.expect("refused").contains("change of profile"));
    }
}
