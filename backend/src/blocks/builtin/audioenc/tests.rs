//! Tests for the audio encoder block.
//!
//! Encoder selection and caps are pure functions and are tested unconditionally.
//! The build test needs a real GStreamer element, so it is skipped when no encoder
//! for the codec is installed.

use super::*;
use gstreamer as gst;

fn init_gst() {
    let _ = gst::init();
}

fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn test_codec_parsing_default() {
    let codec = parse_codec(&HashMap::new()).expect("should have a default codec");
    assert_eq!(codec, Codec::Aac, "default codec should be AAC");
}

#[test]
fn test_codec_parsing_valid() {
    for (s, expected) in [
        ("aac", Codec::Aac),
        ("opus", Codec::Opus),
        ("mp3", Codec::Mp3),
        ("ac3", Codec::Ac3),
    ] {
        let p = props(&[("codec", PropertyValue::String(s.to_string()))]);
        assert_eq!(parse_codec(&p).expect("should parse"), expected);
    }
}

#[test]
fn test_codec_parsing_invalid() {
    let p = props(&[("codec", PropertyValue::String("flac".to_string()))]);
    assert!(parse_codec(&p).is_err(), "unknown codec should error");
}

#[test]
fn test_every_codec_has_a_parser_and_caps() {
    init_gst();
    for codec in [Codec::Aac, Codec::Opus, Codec::Mp3, Codec::Ac3] {
        let caps_str = get_codec_caps_string(codec);
        assert!(
            caps_str.parse::<gst::Caps>().is_ok(),
            "caps for {:?} should parse: {}",
            codec,
            caps_str
        );
        assert!(
            !get_encoder_priority_list(codec).is_empty(),
            "{:?} should have at least one candidate encoder",
            codec
        );
    }
}

#[test]
fn test_aac_caps_leave_stream_format_open() {
    init_gst();

    // aacparse converts between raw and adts. Pinning either here would break the
    // muxers that want the other, so the field must stay absent.
    let caps: gst::Caps = get_codec_caps_string(Codec::Aac)
        .parse()
        .expect("aac caps should parse");
    let s = caps.structure(0).expect("caps should have a structure");
    assert!(
        s.get::<String>("stream-format").is_err(),
        "AAC output caps must not pin stream-format"
    );
}

#[test]
fn test_bitrate_default_and_override() {
    assert_eq!(parse_bitrate(&HashMap::new()), DEFAULT_BITRATE_KBPS);
    let p = props(&[("bitrate", PropertyValue::UInt(192))]);
    assert_eq!(parse_bitrate(&p), 192);
}

#[test]
fn test_sample_rate_parsing() {
    assert_eq!(
        parse_sample_rate(&HashMap::new()).expect("default should parse"),
        Some(48000),
        "default sample rate should be 48 kHz"
    );

    let p = props(&[("sample_rate", PropertyValue::String("source".to_string()))]);
    assert_eq!(
        parse_sample_rate(&p).expect("source should parse"),
        None,
        "\"source\" should leave the rate unconstrained"
    );

    let p = props(&[("sample_rate", PropertyValue::String("44100".to_string()))]);
    assert_eq!(parse_sample_rate(&p).expect("should parse"), Some(44100));

    let p = props(&[("sample_rate", PropertyValue::String("banana".to_string()))]);
    assert!(parse_sample_rate(&p).is_err(), "garbage should error");
}

#[test]
fn test_channels_default_is_source() {
    assert_eq!(
        parse_channels(&HashMap::new()).expect("default should parse"),
        None
    );
    let p = props(&[("channels", PropertyValue::String("2".to_string()))]);
    assert_eq!(parse_channels(&p).expect("should parse"), Some(2));
}

#[test]
fn test_raw_caps_omit_unset_fields() {
    init_gst();

    let caps = build_raw_caps(None, None);
    let s = caps.structure(0).expect("caps should have a structure");
    assert!(s.get::<i32>("rate").is_err(), "rate should be absent");
    assert!(
        s.get::<i32>("channels").is_err(),
        "channels should be absent"
    );

    let caps = build_raw_caps(Some(48000), Some(2));
    let s = caps.structure(0).expect("caps should have a structure");
    assert_eq!(s.get::<i32>("rate").expect("rate should be set"), 48000);
    assert_eq!(s.get::<i32>("channels").expect("channels should be set"), 2);
}

#[test]
fn test_opus_rejects_illegal_sample_rate_at_build_time() {
    init_gst();

    let p = props(&[
        ("codec", PropertyValue::String("opus".to_string())),
        ("sample_rate", PropertyValue::String("44100".to_string())),
    ]);
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let err = match AudioEncBuilder.build("test:audioenc", &p, &ctx) {
        Err(e) => e,
        Ok(_) => panic!("Opus at 44.1 kHz should be rejected"),
    };

    let msg = format!("{:?}", err);
    assert!(
        msg.contains("44100"),
        "error should name the offending rate, got: {}",
        msg
    );
}

#[test]
fn test_build_produces_the_full_chain() {
    init_gst();

    // Only run where an AAC encoder actually exists.
    if select_encoder(Codec::Aac).is_err() {
        eprintln!("skipping: no AAC encoder available");
        return;
    }

    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = AudioEncBuilder
        .build("test:audioenc", &HashMap::new(), &ctx)
        .expect("default AAC build should succeed");

    let ids: Vec<&str> = result.elements.iter().map(|(id, _)| id.as_str()).collect();
    for expected in [
        "test:audioenc:audioconvert",
        "test:audioenc:audioresample",
        "test:audioenc:rate_caps",
        "test:audioenc:encoder",
        "test:audioenc:parser",
        "test:audioenc:capsfilter",
    ] {
        assert!(
            ids.contains(&expected),
            "chain should contain {}, got {:?}",
            expected,
            ids
        );
    }

    assert_eq!(
        result.internal_links.len(),
        5,
        "six elements should be joined by five links"
    );
}

#[test]
fn test_definition_pads_point_at_real_elements() {
    // The external pads name elements by their un-prefixed id. If the build renames
    // one, links into and out of the block silently stop resolving.
    let def = audioenc_definition();
    assert_eq!(def.id, "builtin.audioenc");

    let input = &def.external_pads.inputs[0];
    assert_eq!(input.internal_element_id, "audioconvert");
    assert_eq!(input.media_type, MediaType::Audio);

    let output = &def.external_pads.outputs[0];
    assert_eq!(output.internal_element_id, "capsfilter");
    assert_eq!(output.media_type, MediaType::Audio);
}
