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
//! The parsers (h264parse, h265parse, vp9parse, av1parse) send caps first and
//! complete them (`chroma-format`, the bit depths, the profile) once they have
//! parsed the first frame, before they push it. The block holds its input's
//! first caps back until the first frame arrives and then gives the sink the
//! caps as they are by then, so the session starts from complete caps
//! ([`install`]).
//!
//! After that, a field that appears is kept away from the sink when it does
//! not change what a decoder has to handle (`coded-picture-structure`, say, or
//! `chroma-format=4:2:0` where 4:2:0 was implied), and a field that disappears
//! keeps the value the sink has. A field that describes the stream (profile,
//! chroma format, bit depth) and changes, or appears with a value other than
//! the one implied at the start, is a change. Where webrtcsink cannot follow
//! a change, the flow fails here with an element error that names what
//! changed, instead of a bare `not-negotiated`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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

fn is_encoded_video(caps: &gst::CapsRef) -> bool {
    caps.structure(0)
        .is_some_and(|s| ENCODED_VIDEO.contains(&s.name().as_str()))
}

/// Whether `field` of `caps_name` passes to the sink as it comes and is not
/// judged as a change: webrtcsink ignores it when it compares caps, the
/// downstream chain needs it as it comes (`codec_data`), or a rule of its
/// own judges it (the H.264 profile, see [`refused_change`]).
fn ignored_field(caps_name: &str, field: &str) -> bool {
    VIDEO_ALLOWED_CHANGES.contains(&field)
        || field == "codec_data"
        || (caps_name == "video/x-h264" && field == "profile")
        || (caps_name == "video/x-vp8" && field == "profile")
}

/// For a field that describes what a decoder has to handle, the value the
/// stream has while its caps do not name it. `None` for other fields.
fn implied_value(caps_name: &str, field: &str) -> Option<&'static str> {
    match (caps_name, field) {
        (_, "chroma-format") => Some("4:2:0"),
        (_, "bit-depth-luma" | "bit-depth-chroma") => Some("8"),
        ("video/x-h265" | "video/x-av1", "profile") => Some("main"),
        ("video/x-vp9", "profile") => Some("0"),
        _ => None,
    }
}

/// A caps field value as text: a string as it is, anything else serialized.
fn value_text(value: &gst::glib::SendValue) -> Option<String> {
    value
        .get::<String>()
        .ok()
        .or_else(|| value.serialize().ok().map(|s| s.to_string()))
}

/// What the input probe does with a CAPS event.
#[derive(Debug, PartialEq)]
pub(super) enum Next {
    /// Give the sink these caps.
    Forward(gst::Caps),
    /// webrtcsink cannot follow the change: fail the flow with this message
    /// and drop the event.
    Refuse(String),
    /// The flow already failed on a refused change: drop the event without
    /// another error.
    Drop,
}

/// Follows the CAPS events of one video input of the sink.
#[derive(Default)]
pub(super) struct CapsFollower {
    /// The last caps given to the sink.
    sent: Option<gst::Caps>,
    /// A change was refused; the flow is failing.
    failed: bool,
}

impl CapsFollower {
    /// What to give the sink for `incoming`.
    ///
    /// The caps are `incoming` with [`stripped_fields`] removed and, after
    /// earlier caps of the same codec, refined against what the sink already
    /// has: a field that appears without changing what a decoder handles is
    /// removed, and one that disappears keeps the value the sink has. Fields
    /// webrtcsink ignores pass as they come. A change is judged against the
    /// caps the sink has (see [`refused_change`]).
    pub(super) fn next(&mut self, incoming: &gst::CapsRef) -> Next {
        if self.failed {
            return Next::Drop;
        }
        let mut caps = normalize(incoming).unwrap_or_else(|| incoming.to_owned());
        if let Some(sent) = self.sent.as_ref() {
            if let Some(reason) = refused_change(sent, &caps) {
                self.failed = true;
                return Next::Refuse(reason);
            }
            refine(sent, &mut caps);
        }
        self.sent = Some(caps.clone());
        Next::Forward(caps)
    }
}

/// Keep the fields of `caps` that webrtcsink compares at what `sent` has:
/// remove the ones `sent` lacks, add back the ones `caps` lacks.
/// [`refused_change`] has already refused a field that appears with a value
/// that changes the stream.
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

/// Why WHEP Output cannot follow a change of its encoded video input from
/// `sent`, the caps its sink has, to `new` (after [`normalize`]), or `None`
/// when it can. Mirrors webrtcsink's `input_caps_change_allowed` for a field
/// present in both, and on top of that refuses a field that describes the
/// stream and appears with a value other than the one implied before
/// ([`implied_value`]). A field that disappears is no change: the sink keeps
/// its value. Raw video is not judged here: the block's input bridge may
/// convert it before the sink.
pub(super) fn refused_change(sent: &gst::CapsRef, new: &gst::CapsRef) -> Option<String> {
    let (old_s, new_s) = (sent.structure(0)?, new.structure(0)?);
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

    let judged = |field: &str| {
        !ignored_field(old_name, field) && !matches!(field, "multiview-mode" | "multiview-flags")
    };
    let mut changed: Vec<String> = old_s
        .iter()
        .filter(|(field, _)| judged(field))
        .filter_map(|(field, value)| {
            let new_value = new_s.value(field).ok()?;
            (value_text(new_value) != value_text(value)).then(|| field.to_string())
        })
        .collect();
    changed.extend(
        new_s
            .iter()
            .filter(|(field, _)| !old_s.has_field(*field) && judged(field))
            .filter_map(|(field, value)| {
                let implied = implied_value(new_name, field)?;
                (value_text(value).as_deref() != Some(implied)).then(|| field.to_string())
            }),
    );
    if multiview_changed(old_s, new_s) {
        changed.push("multiview-mode".to_owned());
    }
    if changed.is_empty() {
        return None;
    }
    let fields = changed.join(", ");
    Some(format!(
        "WHEP Output cannot follow a change of {fields} in its {old_name} input \
         mid-stream (from {sent} to {new}): a WebRTC session keeps the stream \
         format it started with and only follows changes of size and frame rate. \
         Keep {fields} the same upstream, or restart the flow."
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

/// State of one video input's probes.
#[derive(Default)]
struct InputState {
    /// The first encoded video caps were held back.
    holding: AtomicBool,
    /// The first data went through: caps are no longer held.
    released: AtomicBool,
    follower: Mutex<CapsFollower>,
}

/// Watch the CAPS events on `pad`, a WHEP Output video input in front of the
/// sink: hold the first encoded video caps back until the first frame, keep
/// the caps the sink gets to what it can follow, and fail the flow with an
/// element error on a change it cannot.
///
/// A held event stays on `pad` as its sticky caps (a newer one replaces it),
/// and so does every sticky event after it (segment, tags): GStreamer marks a
/// sticky event a probe dropped as pending. The first frame releases them, in
/// order.
///
/// The event probe fires per event. The release probe is a BUFFER probe that
/// removes itself on the first frame, so it costs nothing per frame.
pub(super) fn install(pad: &gst::Pad) {
    let state = Arc::new(InputState::default());

    let events = state.clone();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let released = events.released.load(Ordering::Acquire);
        let caps = match event.view() {
            gst::EventView::Caps(caps_event) => caps_event.caps_owned(),
            _ if released || !events.holding.load(Ordering::Acquire) => {
                return gst::PadProbeReturn::Ok;
            }
            // A gap stands in for a frame: release what is held first.
            gst::EventView::Gap(_) => {
                release(&events, pad);
                return gst::PadProbeReturn::Ok;
            }
            gst::EventView::Eos(_) => {
                events.released.store(true, Ordering::Release);
                return gst::PadProbeReturn::Ok;
            }
            // Stays pending behind the held caps.
            _ if event.is_sticky() => return gst::PadProbeReturn::Drop,
            _ => return gst::PadProbeReturn::Ok,
        };
        if !released && is_encoded_video(&caps) {
            events.holding.store(true, Ordering::Release);
            return gst::PadProbeReturn::Drop;
        }
        let next = events
            .follower
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next(&caps);
        match next {
            Next::Forward(forward) => {
                if forward != caps {
                    info.data = Some(gst::PadProbeData::Event(gst::event::Caps::new(&forward)));
                }
                gst::PadProbeReturn::Ok
            }
            Next::Refuse(reason) => {
                if let Some(element) = pad.parent_element() {
                    gst::element_error!(element, gst::StreamError::Format, ("{}", reason));
                }
                gst::PadProbeReturn::Drop
            }
            Next::Drop => gst::PadProbeReturn::Drop,
        }
    });

    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |pad, _| {
            release(&state, pad);
            gst::PadProbeReturn::Remove
        },
    );
}

/// Stop holding, and send the sticky events held on `pad` (the latest caps,
/// then the segment and the rest) on to the sink, in order. Pushing an event
/// already stored on the pad sends the pending ones up to it and nothing
/// else, so the events the sink already has are not sent twice.
fn release(state: &InputState, pad: &gst::Pad) {
    if state.released.swap(true, Ordering::AcqRel) || !state.holding.load(Ordering::Acquire) {
        return;
    }
    let mut held = Vec::new();
    pad.sticky_events_foreach(|event| {
        if event.type_() != gst::EventType::Eos {
            held.push(event.clone());
        }
        std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
    });
    for event in held {
        pad.push_event(event);
    }
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
                Next::Forward(c) => sent.push(c),
                Next::Refuse(reason) => return (sent, Some(reason)),
                Next::Drop => panic!("dropped without a refusal"),
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

    /// A field the sink has that disappears keeps its value at the sink, so
    /// when it comes back with another value that is a change, judged
    /// against what the sink has, not against the caps just before.
    #[test]
    fn present_then_missing_then_different_is_a_change() {
        let (sent, refusal) = run(&[
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8",
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main",
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, \
             chroma-format=4:2:0, bit-depth-luma=(uint)10, bit-depth-chroma=(uint)10",
        ]);
        assert_eq!(sent.len(), 2);
        let err = refusal.expect("refused");
        assert!(
            err.contains("change of bit-depth-luma, bit-depth-chroma"),
            "{err}"
        );
    }

    /// A field that describes the stream and first appears mid-stream is a
    /// change unless it has the value implied before.
    #[test]
    fn a_describing_field_that_appears_with_another_value_is_a_change() {
        let (_, refusal) = run(&[
            "video/x-h265, stream-format=byte-stream, alignment=au",
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main-10",
        ]);
        assert!(refusal.expect("refused").contains("change of profile"));

        let (_, refusal) = run(&[
            "video/x-vp9, alignment=super-frame, profile=(string)0",
            "video/x-vp9, alignment=super-frame, profile=(string)2",
        ]);
        assert!(refusal.expect("refused").contains("change of profile"));

        // The implied value is no change, and the sink does not see it.
        let (sent, refusal) = run(&[
            "video/x-h265, stream-format=byte-stream, alignment=au",
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8",
        ]);
        assert_eq!(refusal, None);
        assert!(!sent[1].structure(0).unwrap().has_field("profile"));
    }

    /// H.265 Main 10 from the start is a session like any other; a later
    /// switch to Main is a change.
    #[test]
    fn h265_main_10_follows_a_size_change_and_refuses_a_profile_change() {
        let main10 = "video/x-h265, stream-format=byte-stream, alignment=au, \
                      profile=main-10, tier=main, chroma-format=4:2:0, \
                      bit-depth-luma=(uint)10, bit-depth-chroma=(uint)10";
        let (sent, refusal) = run(&[
            &format!("{main10}, level=(string)3, width=640, height=360"),
            &format!("{main10}, level=(string)4.1, width=1920, height=1080"),
        ]);
        assert_eq!(refusal, None);
        let s = sent[1].structure(0).unwrap();
        assert_eq!(s.get::<&str>("profile").unwrap(), "main-10");
        assert!(!s.has_field("level") && !s.has_field("tier"));

        let (_, refusal) = run(&[
            main10,
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8",
        ]);
        let err = refusal.expect("refused");
        assert!(
            err.contains("change of profile, bit-depth-luma, bit-depth-chroma"),
            "{err}"
        );
    }

    #[test]
    fn after_a_refusal_events_are_dropped_without_another_error() {
        let mut follower = CapsFollower::default();
        let a = caps("video/x-vp9, alignment=super-frame, chroma-format=4:2:0");
        let b = caps("video/x-vp9, alignment=super-frame, chroma-format=4:4:4");
        assert!(matches!(follower.next(&a), Next::Forward(_)));
        assert!(matches!(follower.next(&b), Next::Refuse(_)));
        assert_eq!(follower.next(&b), Next::Drop);
        assert_eq!(follower.next(&a), Next::Drop);
    }

    /// What reached the peer of the watched pad.
    #[derive(Debug, PartialEq)]
    enum Got {
        Caps(String),
        Segment,
        Buffer,
    }

    /// An `identity` in a pipeline whose src pad carries [`install`], linked
    /// to a pad that records what it gets. Push through `identity`'s sink pad.
    struct Harness {
        pipeline: gst::Pipeline,
        input: gst::Pad,
        /// Kept alive here: a pad holds no reference to its peer.
        _peer: gst::Pad,
        got: Arc<Mutex<Vec<Got>>>,
    }

    impl Harness {
        fn new() -> Self {
            let _ = gst::init();
            let pipeline = gst::Pipeline::new();
            let identity = gst::ElementFactory::make("identity").build().unwrap();
            pipeline.add(&identity).unwrap();
            let src = identity.static_pad("src").unwrap();
            install(&src);
            let got = Arc::new(Mutex::new(Vec::new()));
            let (on_event, on_chain) = (got.clone(), got.clone());
            let peer = gst::Pad::builder(gst::PadDirection::Sink)
                .event_function(move |_, _, event| {
                    match event.view() {
                        gst::EventView::Caps(c) => on_event
                            .lock()
                            .unwrap()
                            .push(Got::Caps(c.caps().to_string())),
                        gst::EventView::Segment(_) => on_event.lock().unwrap().push(Got::Segment),
                        _ => {}
                    }
                    true
                })
                // Takes any caps, as the sink would take these.
                .query_function(|pad, parent, query| match query.view_mut() {
                    gst::QueryViewMut::AcceptCaps(q) => {
                        q.set_result(true);
                        true
                    }
                    _ => gst::Pad::query_default(pad, parent, query),
                })
                .chain_function(move |_, _, _| {
                    on_chain.lock().unwrap().push(Got::Buffer);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build();
            peer.set_active(true).unwrap();
            src.link(&peer).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            let input = identity.static_pad("sink").unwrap();
            input.send_event(gst::event::StreamStart::new("caps-change-test"));
            Self {
                pipeline,
                input,
                _peer: peer,
                got,
            }
        }

        fn caps(&self, caps_str: &str) {
            self.input
                .send_event(gst::event::Caps::new(&caps(caps_str)));
        }

        fn segment(&self) {
            let segment = gst::FormattedSegment::<gst::ClockTime>::new();
            self.input.send_event(gst::event::Segment::new(&segment));
        }

        fn buffer(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
            self.input.chain(gst::Buffer::with_size(4).unwrap())
        }

        fn got(&self) -> Vec<Got> {
            std::mem::take(&mut *self.got.lock().unwrap())
        }

        fn errors(&self) -> Vec<String> {
            let bus = self.pipeline.bus().unwrap();
            std::iter::from_fn(|| bus.pop_filtered(&[gst::MessageType::Error]))
                .filter_map(|m| match m.view() {
                    gst::MessageView::Error(e) => Some(e.error().to_string()),
                    _ => None,
                })
                .collect()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    /// vp9parse sends caps, completes them once it has parsed the first
    /// frame, and pushes that frame. The sink starts from the complete caps,
    /// before the segment and the frame, and a later size change keeps them.
    /// Fails if the first caps are not held: the sink then starts from caps
    /// without chroma-format and never gets it.
    #[test]
    fn the_sink_starts_from_the_caps_the_parser_completes() {
        let h = Harness::new();
        h.caps("video/x-vp9, profile=(string)0, width=640, height=360, parsed=true");
        h.segment();
        assert_eq!(h.got(), vec![], "caps and segment are held");
        h.caps(
            "video/x-vp9, profile=(string)0, width=640, height=360, parsed=true, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8",
        );
        h.buffer().unwrap();
        let got = h.got();
        assert_eq!(got.len(), 3, "{got:?}");
        let Got::Caps(first) = &got[0] else {
            panic!("the sink got {got:?} before caps");
        };
        assert!(first.contains("chroma-format"), "{first}");
        assert_eq!(&got[1..], &[Got::Segment, Got::Buffer]);

        h.caps(
            "video/x-vp9, profile=(string)0, width=1920, height=1080, parsed=true, \
             chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8",
        );
        h.buffer().unwrap();
        let got = h.got();
        assert!(
            matches!(&got[..], [Got::Caps(c), Got::Buffer] if c.contains("1920") && c.contains("chroma-format")),
            "{got:?}"
        );
        assert_eq!(h.errors(), Vec::<String>::new());
    }

    /// A change the sink cannot follow posts one named element error and the
    /// event is dropped; the same caps again, as GStreamer resends a dropped
    /// sticky event before each frame, post no new error.
    #[test]
    fn a_refused_change_posts_one_error_and_is_dropped() {
        let h = Harness::new();
        let start = "video/x-h265, stream-format=byte-stream, alignment=au, profile=main, \
                     chroma-format=4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8";
        h.caps(start);
        h.segment();
        h.buffer().unwrap();
        assert_eq!(h.got().len(), 3);

        h.caps(
            "video/x-h265, stream-format=byte-stream, alignment=au, profile=main-10, \
             chroma-format=4:2:0, bit-depth-luma=(uint)10, bit-depth-chroma=(uint)10",
        );
        let _ = h.buffer();
        let _ = h.buffer();
        let got = h.got();
        assert!(
            got.iter().all(|g| !matches!(g, Got::Caps(_))),
            "the refused caps reached the sink: {got:?}"
        );
        let errors = h.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("change of profile"), "{errors:?}");
    }
}
