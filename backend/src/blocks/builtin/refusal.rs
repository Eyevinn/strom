//! Refusing an input stream that an output block cannot carry.
//!
//! Output blocks decide what to do with a stream once its caps arrive, inside a
//! caps probe on their input `identity`. When the answer is "this cannot go
//! out", the operator has to be told why, in words that say what to change.

use gstreamer as gst;
use gstreamer::prelude::*;

/// Refuse the stream arriving at `input`, an output block's input `identity`.
///
/// Posts `reason` as an element error, which the flow's bus handler reports as
/// the flow's error. Then silences the refused stream: `identity` drops every
/// buffer from here on. Without that, the unlinked src pad would send
/// `not-linked` upstream, the source would post `Internal data stream error`
/// after us, and that line would replace `reason` as the error the flow shows.
///
/// Dropping inside `identity` returns `OK` upstream, so nothing else stops or
/// complains, and it costs no probe on the buffer path.
pub(crate) fn refuse_input(input: &gst::Element, reason: &str) {
    gst::element_error!(input, gst::StreamError::Format, ("{}", reason));
    if input.find_property("drop-probability").is_some() {
        input.set_property("drop-probability", 1.0f32);
    }
}

/// Undo the silencing half of [`refuse_input`], for a block that lets a later
/// usable Caps event replace a refused one. The error already posted stands.
pub(crate) fn accept_input(input: &gst::Element) {
    if input.find_property("drop-probability").is_some() {
        input.set_property("drop-probability", 0.0f32);
    }
}

/// The reason to give when an output block that needs encoded video gets
/// something else.
///
/// `accepts` names the codecs the block takes, as the operator reads them
/// (`"H.264 or H.265"`). Raw video points at `builtin.videoenc`, because an
/// output block never encodes video itself. Any other codec points at that
/// block's `codec` property, which is where the choice is made.
pub(crate) fn video_refusal(block: &str, accepts: &str, caps_name: &str) -> String {
    if caps_name == "video/x-raw" {
        format!(
            "{} needs {} video, but its video input is raw. Place a builtin.videoenc \
             block before it and link its encoded_out pad",
            block, accepts
        )
    } else {
        format!(
            "{} needs {} video, but its video input carries {}. Set builtin.videoenc's \
             codec to one of those",
            block, accepts, caps_name
        )
    }
}

/// The reason to give when an output block gets audio it cannot carry.
///
/// `accepts` names what the block takes. Raw audio only reaches this in a block
/// that does not encode audio yet, and points at `builtin.audioenc`.
pub(crate) fn audio_refusal(block: &str, accepts: &str, caps_name: &str) -> String {
    if caps_name == "audio/x-raw" {
        format!(
            "{} needs {} audio, but its audio input is raw. Place a builtin.audioenc \
             block before it and link its encoded_out pad",
            block, accepts
        )
    } else {
        format!(
            "{} needs {} audio, but its audio input carries {}. Set builtin.audioenc's \
             codec to one of those",
            block, accepts, caps_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_video_points_at_the_video_encoder_block() {
        let reason = video_refusal("Recorder", "H.264 or H.265", "video/x-raw");
        assert!(reason.contains("is raw"), "{}", reason);
        assert!(reason.contains("builtin.videoenc"), "{}", reason);
    }

    #[test]
    fn another_codec_is_named_and_points_at_the_codec_property() {
        let reason = video_refusal("Recorder", "H.264 or H.265", "video/x-vp9");
        assert!(reason.contains("video/x-vp9"), "{}", reason);
        assert!(reason.contains("builtin.videoenc's codec"), "{}", reason);
    }

    #[test]
    fn audio_refusal_names_what_arrived_and_the_fix() {
        let reason = audio_refusal("TAMS Output", "MP3, AAC, Opus or AC-3", "audio/x-flac");
        assert!(reason.contains("audio/x-flac"), "{}", reason);
        assert!(reason.contains("builtin.audioenc's codec"), "{}", reason);
    }
}
