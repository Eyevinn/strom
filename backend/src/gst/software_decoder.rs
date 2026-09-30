//! Pick a video decoder for a stream: the host's best one for its exact caps
//! ([`decoders_for`]), or one that runs on the CPU ([`best_for`]).
//!
//! An autoplugger picks the highest-ranked decoder for a stream, which on a
//! Mac is VideoToolbox (`vtdec_hw`, primary + 1) for ProRes, among others. A
//! hardware decoder can advertise a format it then fails to decode: a Mac
//! whose VideoToolbox has no ProRes decoder (a virtual M1, for one) refuses
//! every frame, and nothing falls back. Where that is known, the caller asks
//! for a software decoder here instead.

use gstreamer as gst;
use gstreamer::prelude::*;

/// Whether `factory` decodes on dedicated hardware. GStreamer marks those
/// with `Hardware` in the element klass (VideoToolbox, NVDEC, VA-API,
/// D3D11/12, V4L2 stateless, ...).
pub fn is_hardware(factory: &gst::ElementFactory) -> bool {
    factory
        .metadata(gst::ELEMENT_METADATA_KLASS)
        .is_some_and(|klass| klass.split('/').any(|part| part == "Hardware"))
}

/// Video and image decoders.
fn picture_decoders() -> gst::ElementFactoryType {
    gst::ElementFactoryType::DECODER
        | gst::ElementFactoryType::MEDIA_VIDEO
        | gst::ElementFactoryType::MEDIA_IMAGE
}

/// Whether `caps` is an encoded picture format a decoder could take: not raw,
/// not audio, not text.
pub fn is_encoded_picture(caps: &gst::Caps) -> bool {
    caps.structure(0).is_some_and(|s| {
        let name = s.name();
        (name.starts_with("video/") || name.starts_with("image/")) && name != "video/x-raw"
    })
}

/// The highest-ranked video or image decoder for `caps` that is not a
/// hardware one.
pub fn best_for(caps: &gst::Caps) -> Option<gst::ElementFactory> {
    let mut candidates: Vec<gst::ElementFactory> =
        gst::ElementFactory::factories_with_type(picture_decoders(), gst::Rank::MARGINAL)
            .into_iter()
            .filter(|f| !is_hardware(f) && f.can_sink_any_caps(caps))
            .collect();
    candidates.sort_by_key(|f| std::cmp::Reverse(f.rank()));
    candidates.into_iter().next()
}

/// Video decoders that take all of `caps`, hardware or not, highest rank
/// first, as `decodebin` would rank them.
///
/// Pass the parser's output caps, which carry profile, chroma format and bit
/// depth: a hardware decoder whose templates lack the stream's profile (NVDEC,
/// VA or D3D for 4:2:2 or 10-bit H.264) is left out, and the stream falls to a
/// decoder that takes it. Filtering on the bare media type would pick the
/// hardware decoder and fail with not-negotiated.
pub fn decoders_for(caps: &gst::Caps) -> Vec<gst::ElementFactory> {
    let mut candidates: Vec<gst::ElementFactory> =
        gst::ElementFactory::factories_with_type(picture_decoders(), gst::Rank::MARGINAL)
            .into_iter()
            .filter(|f| f.can_sink_all_caps(caps))
            .collect();
    candidates.sort_by_key(|f| std::cmp::Reverse(f.rank()));
    candidates
}

/// Caps that make a `uridecodebin3` stop at `encoded` video instead of
/// decoding it, and decode everything else as it would by default
/// (`default_caps`, the element's own `caps` default).
pub fn stop_at(default_caps: &gst::Caps, encoded: &str) -> gst::Caps {
    let mut caps = default_caps.clone();
    caps.make_mut()
        .append_structure(gst::Structure::new_empty(encoded));
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No hardware decoder is ever picked, for any format a hardware decoder
    /// on this machine takes.
    #[test]
    fn never_picks_a_hardware_decoder() {
        gst::init().unwrap();
        let hardware: Vec<gst::ElementFactory> =
            gst::ElementFactory::factories_with_type(picture_decoders(), gst::Rank::NONE)
                .into_iter()
                .filter(is_hardware)
                .collect();
        for factory in &hardware {
            for template in factory.static_pad_templates() {
                if template.direction() != gst::PadDirection::Sink {
                    continue;
                }
                let caps = template.caps();
                for s in caps.iter() {
                    let one = gst::Caps::builder(s.name()).build();
                    if let Some(picked) = best_for(&one) {
                        assert!(
                            !is_hardware(&picked),
                            "{} picked for {}",
                            picked.name(),
                            s.name()
                        );
                    }
                }
            }
        }
    }

    /// ProRes, the format that showed the problem, has a software decoder.
    #[test]
    fn prores_has_a_software_decoder() {
        gst::init().unwrap();
        let picked = best_for(&gst::Caps::builder("video/x-prores").build())
            .expect("a software ProRes decoder (gst-libav)");
        assert!(!is_hardware(&picked), "{}", picked.name());
    }

    /// `decoders_for` returns only decoders that take the whole caps, best
    /// rank first. Guards the filter and the order only: which decoder wins
    /// on a host is up to its ranks.
    #[test]
    fn decoders_for_takes_the_caps_best_rank_first() {
        gst::init().unwrap();
        let caps: gst::Caps =
            "video/x-h264, stream-format=byte-stream, alignment=au, profile=high-4:2:2"
                .parse()
                .unwrap();
        let found = decoders_for(&caps);
        assert!(!found.is_empty(), "no H.264 decoder (gst-libav)");
        for f in &found {
            assert!(
                f.can_sink_all_caps(&caps),
                "{} does not take {caps}",
                f.name()
            );
        }
        assert!(
            found.windows(2).all(|w| w[0].rank() >= w[1].rank()),
            "not ordered by rank: {:?}",
            found
                .iter()
                .map(|f| (f.name().to_string(), f.rank()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn only_encoded_pictures_count() {
        gst::init().unwrap();
        for (caps, encoded) in [
            ("video/x-prores", true),
            ("image/png", true),
            ("video/x-h264", true),
            ("video/x-raw", false),
            ("audio/x-raw", false),
            ("audio/mpeg", false),
            ("text/x-raw", false),
        ] {
            let caps: gst::Caps = caps.parse().unwrap();
            assert_eq!(is_encoded_picture(&caps), encoded, "{caps}");
        }
    }

    #[test]
    fn stop_at_keeps_the_defaults_and_adds_the_encoded_format() {
        gst::init().unwrap();
        let default: gst::Caps = "video/x-raw(ANY); audio/x-raw(ANY)".parse().unwrap();
        let caps = stop_at(&default, "video/x-prores");
        assert_eq!(caps.size(), 3);
        assert!(default.is_subset(&caps));
        assert!(gst::Caps::builder("video/x-prores")
            .field("variant", "4444")
            .build()
            .is_subset(&caps));
    }
}
