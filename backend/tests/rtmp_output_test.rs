//! Regression tests for `builtin.rtmp_output`.
//!
//! Three properties, each of which was wrong in a first draft of the block and
//! each of which is silent when it breaks:
//!
//! 1. **The sink is not async.** Block-built elements bypass `add_element`, so
//!    nothing sets `async=false` for them. A sink that waits to preroll holds
//!    the whole pipeline out of PLAYING, and the flow simply never starts.
//! 2. **Nothing links to `flvmux` at build time.** An aggregator sink pad
//!    requested for an input that never carries data means the muxer never
//!    aggregates and the sink receives nothing, which looks like a dead server
//!    rather than a graph mistake.
//! 3. **The declared properties are the ones the block reads.** A property read
//!    but never declared is settable only by hand-editing flow JSON and is
//!    invisible in the UI.
//!
//! The codec decision each pad probe makes is covered directly, through
//! `video_plan` and `audio_plan`, which exist as separate functions so the
//! branch logic can be tested without a pipeline.
//!
//! **The element construction and pad linking those decisions lead to are
//! covered in `rtmp_output_pipeline_test.rs`, not here.** An earlier draft of
//! this comment claimed that needed a reachable RTMP server and used it to
//! justify leaving the probes untested. That was wrong: the probes fire on the
//! CAPS event, so the chains are built whether or not anything is listening, and
//! the whole pipeline suite runs in about a tenth of a second against a closed
//! port. Nothing in *this* file reaches the probes, so read the two together.

use std::collections::HashMap;
use strom::blocks::builtin::rtmp::{
    audio_plan, libav_package_hint, parse_rtmp_location, redact_location, rtmp_missing_message,
    rtmp_package_hint, video_plan, AudioPlan, RtmpOutputBuilder,
};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare image.
const REQUIRED: &[&str] = &[
    "rtmp2sink",
    "flvmux",
    "h264parse",
    "aacparse",
    "avenc_aac",
    "identity",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    // Before the factory lookup, not after: ElementFactory::find panics on an
    // uninitialised GStreamer, and this runs before build() would have done it.
    gst::init().expect("gst init");
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    eprintln!(
        "skipping: missing GStreamer elements: {}",
        missing.join(", ")
    );
    false
}

fn build(properties: HashMap<String, PropertyValue>) -> strom::blocks::BlockBuildResult {
    gst::init().expect("gst init");
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    RtmpOutputBuilder
        .build("rtmp0", &properties, &ctx)
        .expect("rtmp_output should build")
}

fn element<'a>(result: &'a strom::blocks::BlockBuildResult, suffix: &str) -> &'a gst::Element {
    result
        .elements
        .iter()
        .find(|(id, _)| id.ends_with(suffix))
        .map(|(_, e)| e)
        .unwrap_or_else(|| panic!("no element ending in {}", suffix))
}

/// `rtmp2sink` parses `location` and re-serialises it, and that drops the
/// default port on at least GStreamer 1.28, so `rtmp://h:1935/p` reads back as
/// `rtmp://h/p`. Compare through this rather than pinning one version's
/// normalisation: the runtime image is on 1.26 and CI must not depend on which
/// of the two behaviours it has.
fn without_default_port(url: &str) -> String {
    url.replace(":1935/", "/")
}

#[test]
fn the_sink_is_not_async() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    let sink = element(&result, ":rtmp_sink");
    assert!(
        !sink.property::<bool>("async"),
        "the RTMP sink is async, so it will wait to preroll and hold the pipeline \
         out of PLAYING. Block-built elements bypass add_element, so this has to \
         be set here"
    );
    assert!(
        sink.property::<bool>("qos"),
        "qos should be on so the sink can report back pressure upstream"
    );
}

#[test]
fn nothing_is_linked_to_the_muxer_at_build_time() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    assert_eq!(
        result.internal_links.len(),
        1,
        "only flvmux -> sink may be linked statically. A mux sink pad requested \
         for an input that never carries data means the muxer never aggregates: \
         {:?}",
        result.internal_links
    );
    let (from, to) = &result.internal_links[0];
    assert!(from.element_id.ends_with(":rtmp_flvmux"));
    assert!(to.element_id.ends_with(":rtmp_sink"));
}

#[test]
fn both_inputs_exist_and_are_identities() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    for suffix in [":rtmp_video_input", ":rtmp_audio_input"] {
        let input = element(&result, suffix);
        assert_eq!(
            input.factory().map(|f| f.name().to_string()).as_deref(),
            Some("identity"),
            "{} should be an identity, so a probe can insert the real chain",
            suffix
        );
    }
}

#[test]
fn the_location_default_is_used_when_absent_or_blank() {
    if !plugins_available() {
        return;
    }
    for properties in [
        HashMap::new(),
        HashMap::from([(
            "location".to_string(),
            PropertyValue::String("   ".to_string()),
        )]),
    ] {
        let result = build(properties);
        assert_eq!(
            without_default_port(&element(&result, ":rtmp_sink").property::<String>("location")),
            without_default_port(strom_types::DEFAULT_RTMP_LOCATION),
            "a blank location should fall back to the default rather than \
             publishing to an empty URL"
        );
    }
}

#[test]
fn the_location_and_sync_properties_reach_the_sink() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::from([
        (
            // A non-default port on purpose: 1935 is the one `rtmp2sink`
            // normalises away, so using it here would mean the suite never
            // asserts that a port reaches the sink at all.
            "location".to_string(),
            PropertyValue::String("rtmp://192.0.2.10:1936/live/key".to_string()),
        ),
        ("sync".to_string(), PropertyValue::Bool(false)),
    ]));
    let sink = element(&result, ":rtmp_sink");
    assert_eq!(
        sink.property::<String>("location"),
        "rtmp://192.0.2.10:1936/live/key",
        "a non-default port must survive verbatim into the sink"
    );
    assert!(!sink.property::<bool>("sync"));
}

#[test]
fn the_muxer_is_streamable() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    assert!(
        element(&result, ":rtmp_flvmux").property::<bool>("streamable"),
        "the non-streamable form rewrites the header at end of file, and a live \
         stream has no end"
    );
}

/// The declared set is exactly `location` and `sync`, both named here.
///
/// The name says "the declared set" rather than "every property the block
/// reads" on purpose: this test cannot enumerate the reads, it checks a list
/// written by hand. The teeth come from composition. The length assertion
/// catches a property declared with no reader, and
/// `the_location_and_sync_properties_reach_the_sink` proves both of these are
/// read, so adding a third read without declaring it fails the length check as
/// soon as anyone declares it, and reaches the UI as a bug otherwise.
#[test]
fn the_declared_property_set_is_exactly_location_and_sync() {
    // No GStreamer needed: this is about the definition, not the pipeline.
    let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
    let declared: Vec<&str> = definition
        .exposed_properties
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    for name in ["location", "sync"] {
        assert!(
            declared.contains(&name),
            "the block reads {} but does not declare it, so it is settable only \
             by hand-editing flow JSON and invisible in the UI. Declared: {:?}",
            name,
            declared
        );
    }
    assert_eq!(
        declared.len(),
        2,
        "a declared property with no reader is the mirror of the same defect. \
         Declared: {:?}",
        declared
    );
}

#[test]
fn every_mapping_names_an_element_the_block_builds() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
    for property in &definition.exposed_properties {
        let suffix = format!(":{}", property.mapping.element_id);
        assert!(
            result
                .elements
                .iter()
                .any(|(id, _)| id.ends_with(suffix.as_str())),
            "property {} maps to element {}, which the block does not build, so a \
             runtime update would silently go nowhere",
            property.name,
            property.mapping.element_id
        );
    }
}

#[test]
fn every_declared_pad_names_an_element_the_block_builds() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
    for pad in &definition.external_pads.inputs {
        let suffix = format!(":{}", pad.internal_element_id);
        assert!(
            result
                .elements
                .iter()
                .any(|(id, _)| id.ends_with(suffix.as_str())),
            "pad {} points at element {}, which the block does not build, so the \
             graph would fail to link",
            pad.name,
            pad.internal_element_id
        );
    }
    assert!(
        definition.external_pads.outputs.is_empty(),
        "an output block has no outputs"
    );
}

#[test]
fn the_missing_plugin_message_names_what_to_install() {
    // Runs on a host with the element present, which is why the message is a
    // separate function from the check.
    let message = rtmp_missing_message();
    assert!(message.contains("rtmp2sink"), "{}", message);
    assert!(message.contains(rtmp_package_hint()), "{}", message);
}

// ---------------------------------------------------------------------------
// The codec decision, which is the half of the probe logic that can be tested
// without a pipeline. Every arm is here, because the refusals are what an
// operator sees when a flow is wired wrongly and a wrong message costs more
// than a wrong element: it sends them to fix the wrong block.
// ---------------------------------------------------------------------------

/// The AAC encoder is built inside a pad probe, so a missing one is a log line
/// and a silent loss of audio rather than a refusal to build. The message has to
/// name the package, and it is a different package from the sink's.
#[test]
fn the_aac_encoder_hint_names_a_different_package_than_the_sink() {
    let hint = libav_package_hint();
    assert!(!hint.is_empty());
    assert_ne!(
        hint,
        rtmp_package_hint(),
        "avenc_aac ships in gst-libav, not in gst-plugins-bad, so pointing an \
         operator at the sink's package would send them to the wrong place"
    );
}

// --- rtmps and the location contract ---------------------------------------
//
// RTMPS needs no configuration beyond the scheme in the URL, so the thing worth
// testing is not that TLS works but that nothing silently downgrades it and that
// credentials in the URL never reach a log.

/// `127.0.0.1.evil.example` resolves somewhere else entirely and used to be
/// treated as loopback by a prefix match, suppressing the plaintext warning.
#[test]
fn a_hostile_hostname_that_starts_like_loopback_is_not_loopback() {
    let parsed = parse_rtmp_location("rtmp://127.0.0.1.evil.example/live/key").expect("accepted");
    assert_eq!(parsed.host, "127.0.0.1.evil.example");
    assert!(!parsed.tls);
}

#[test]
fn rtmps_is_recognised_as_tls() {
    let parsed = parse_rtmp_location("rtmps://example.com/live/key").expect("rtmps accepted");
    assert!(parsed.tls);
    assert_eq!(parsed.host, "example.com");
}

#[test]
fn rtmp_is_recognised_as_plaintext() {
    let parsed = parse_rtmp_location("rtmp://example.com:1935/live/key").expect("rtmp accepted");
    assert!(!parsed.tls);
    assert_eq!(
        parsed.host, "example.com",
        "the port must not be part of the host"
    );
}

#[test]
fn the_default_location_is_accepted_and_is_loopback_plaintext() {
    let parsed = parse_rtmp_location(strom_types::DEFAULT_RTMP_LOCATION).expect("default accepted");
    assert!(!parsed.tls);
    assert_eq!(parsed.host, "127.0.0.1");
}

#[test]
fn an_ipv6_host_keeps_its_address_and_loses_its_port() {
    let parsed = parse_rtmp_location("rtmps://[2001:db8::1]:443/live/key").expect("ipv6 accepted");
    assert!(parsed.tls);
    assert_eq!(parsed.host, "2001:db8::1");
}

/// The case that matters: `rtmp2sink` accepts any string and re-serialises what
/// it could parse, so this would otherwise become `rtmp:/`, a plaintext
/// connection to nowhere, with no error anywhere.
#[test]
fn a_location_with_no_scheme_is_refused_rather_than_silently_becoming_plaintext() {
    let err = parse_rtmp_location("not-a-url").expect_err("must be refused");
    assert!(err.contains("rtmps://"), "{}", err);
}

#[test]
fn an_unknown_scheme_is_refused_and_names_the_alternatives() {
    let err = parse_rtmp_location("https://example.com/live/key").expect_err("must be refused");
    assert!(err.contains("https"), "{}", err);
    assert!(err.contains("rtmps://"), "{}", err);
}

#[test]
fn a_location_with_no_host_is_refused() {
    parse_rtmp_location("rtmps:///live/key").expect_err("must be refused");
}

/// Credentials belong to the sink's own `username` and `password` properties,
/// which it fills from the URL. The raw string is what a block would naturally
/// log, and it is the one that still has them in it.
#[test]
fn credentials_never_survive_into_something_loggable() {
    let raw = "rtmps://alice:s3cret@example.com/live/key";
    let redacted = redact_location(raw);
    assert!(!redacted.contains("s3cret"), "{}", redacted);
    assert!(!redacted.contains("alice"), "{}", redacted);
    assert!(redacted.contains("example.com/live/"), "{}", redacted);
    // And the parsed form carries only the safe spelling.
    let parsed = parse_rtmp_location(raw).expect("accepted");
    assert!(!parsed.redacted.contains("s3cret"), "{}", parsed.redacted);
    assert_eq!(parsed.host, "example.com");
}

/// The stream key is the second secret in an RTMP URL and for most servers it is
/// the whole authorisation, so it is masked even when there are no credentials.
/// The host and application survive, because a log line still has to identify
/// which output it belongs to.
#[test]
fn the_stream_key_is_masked_even_with_no_credentials() {
    let redacted = redact_location("rtmps://example.com/live/key");
    assert!(!redacted.ends_with("/key"), "{}", redacted);
    assert!(
        redacted.starts_with("rtmps://example.com/live/"),
        "{}",
        redacted
    );
}

/// A token in a query string is the same secret by another route.
#[test]
fn a_query_string_is_masked_because_it_may_carry_a_token() {
    let redacted = redact_location("rtmps://example.com/live/key?token=SECRET");
    assert!(!redacted.contains("SECRET"), "{}", redacted);
    assert!(!redacted.contains("/key?"), "{}", redacted);
}

/// A bare host has no path, so there is nothing to mask and nothing to invent.
#[test]
fn a_location_with_no_path_is_left_alone() {
    assert_eq!(
        redact_location("rtmps://example.com"),
        "rtmps://example.com"
    );
}

/// A password containing an `@` would defeat a naive split on the first one.
#[test]
fn redaction_handles_an_at_sign_inside_the_password() {
    let redacted = redact_location("rtmp://user:p@ss@example.com/live/key");
    assert!(!redacted.contains("p@ss"), "{}", redacted);
    assert!(redacted.contains("example.com/live/"), "{}", redacted);
}

/// The input that defeated the first version of `redact_location`. An unencoded
/// `/` in a password meant the authority could not be isolated, and the function
/// returned its input unchanged, so the whole credential went into an `info!`.
#[test]
fn a_slash_inside_the_password_does_not_defeat_redaction() {
    let redacted = redact_location("rtmp://user:p/ss@host/live/key");
    assert!(!redacted.contains("p/ss"), "{}", redacted);
    assert!(redacted.contains("host/live/"), "{}", redacted);
}

/// The refusal path leaked too: with only one slash there is no `://`, so the old
/// redaction returned the raw string, and it was embedded in the error message
/// that the pipeline builder logs and the API returns.
#[test]
fn a_credentialed_url_with_a_mistyped_scheme_is_refused_without_echoing_the_secret() {
    let raw = "rtmp:/user:pass@host/live/key";
    assert!(
        !redact_location(raw).contains("pass"),
        "{}",
        redact_location(raw)
    );
    let err = parse_rtmp_location(raw).expect_err("must be refused");
    assert!(
        !err.contains("pass"),
        "the refusal message leaks the password: {}",
        err
    );
}

#[test]
fn redaction_of_an_unparseable_string_never_returns_it_unchanged() {
    // The secret is always the part BEFORE the last '@'; whatever follows is the
    // host and must survive, which is why the host names here are unremarkable.
    for (raw, secret) in [
        ("user:pass@host", "pass"),
        ("rtmp:/user:topsecret@host/live/key", "topsecret"),
        ("://user:pw@h", "pw"),
        ("user:p/w@host/live", "p/w"),
        ("user:p@w@host/live", "p@w"),
    ] {
        let redacted = redact_location(raw);
        assert!(
            !redacted.contains(secret),
            "input {:?} redacted to {:?}, which still contains {:?}",
            raw,
            redacted,
            secret
        );
    }
}

/// A stray space around a pasted URL used to pass the parser as TLS while the
/// sink quietly reduced it to `rtmp:/`, so the log asserted encryption on an
/// output that published nowhere.
#[test]
fn a_whitespace_padded_location_is_trimmed_before_the_sink_sees_it() {
    let parsed = parse_rtmp_location("  rtmps://example.com/live/key  ").expect("accepted");
    assert_eq!(parsed.location, "rtmps://example.com/live/key");
    assert!(parsed.tls);
}

#[test]
fn the_trimmed_location_is_what_reaches_the_sink() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::from([(
        "location".to_string(),
        PropertyValue::String("  rtmps://192.0.2.10/live/key  ".to_string()),
    )]));
    let sink = element(&result, ":rtmp_sink");
    assert_eq!(
        sink.property::<String>("location"),
        "rtmps://192.0.2.10/live/key",
        "an untrimmed location would reach rtmp2sink as rtmp:/ with the scheme fallen back"
    );
}

/// The block must refuse rather than build an output whose sink silently holds a
/// degenerate location. Verified against the real element, not a prediction.
#[test]
fn a_location_the_sink_cannot_parse_is_refused_at_build_time() {
    if !plugins_available() {
        return;
    }
    gst::init().expect("gst init");
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let props = HashMap::from([(
        "location".to_string(),
        PropertyValue::String("rtmp://user:p/ss@192.0.2.10/live/key".to_string()),
    )]);
    let message = match RtmpOutputBuilder.build("rtmp0", &props, &ctx) {
        Ok(_) => panic!("a location rtmp2sink reduces to rtmp:/ must not build"),
        Err(e) => e.to_string(),
    };
    assert!(
        !message.contains("p/ss"),
        "the refusal leaks the password: {}",
        message
    );
    assert!(
        message.contains("%2F"),
        "should hint at percent-encoding: {}",
        message
    );
}

#[test]
fn an_rtmps_location_reaches_the_sink_and_sets_its_scheme() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::from([(
        "location".to_string(),
        PropertyValue::String("rtmps://192.0.2.10/live/key".to_string()),
    )]));
    let sink = element(&result, ":rtmp_sink");
    assert_eq!(
        sink.property::<String>("location"),
        "rtmps://192.0.2.10/live/key"
    );
    // The sink's own scheme enum, set from the URL rather than by us. Compared
    // as an integer because the generated Rust enum is not re-exported here;
    // `gst-inspect-1.0 rtmp2sink` documents 0 = rtmp, 1 = rtmps.
    let scheme = sink.property_value("scheme");
    assert_eq!(
        scheme
            .transform::<i32>()
            .ok()
            .and_then(|v| v.get::<i32>().ok()),
        Some(1),
        "an rtmps:// location must leave the sink's scheme on rtmps, got {:?}",
        scheme
    );
    // Certificate validation must stay strict. This block exposes no way to
    // weaken it, so a flow gets the sink's secure default: validate-all, which
    // is 0x7f. If that ever reads as anything else, someone has added a switch.
    let flags = sink.property_value("tls-validation-flags");
    assert_eq!(
        flags
            .transform::<u32>()
            .ok()
            .and_then(|v| v.get::<u32>().ok()),
        Some(0x7f),
        "tls-validation-flags should still be validate-all, got {:?}",
        flags
    );
}

#[test]
fn h264_video_is_accepted() {
    assert_eq!(video_plan("video/x-h264"), Ok(()));
}

#[test]
fn raw_video_is_refused_and_names_the_encoder_block() {
    let message = video_plan("video/x-raw").expect_err("raw video must be refused");
    assert!(
        message.contains("builtin.videoenc"),
        "the refusal must name the block to add, got: {}",
        message
    );
}

#[test]
fn other_video_codecs_are_refused_and_named() {
    let message = video_plan("video/x-vp8").expect_err("VP8 must be refused");
    assert!(
        message.contains("video/x-vp8"),
        "the refusal must name what arrived, got: {}",
        message
    );
}

#[test]
fn raw_audio_is_encoded_in_the_block() {
    assert_eq!(audio_plan("audio/x-raw", 0, 0), Ok(AudioPlan::Encode));
}

#[test]
fn aac_audio_is_parsed_only() {
    assert_eq!(audio_plan("audio/mpeg", 4, 0), Ok(AudioPlan::Parse));
    assert_eq!(audio_plan("audio/mpeg", 2, 0), Ok(AudioPlan::Parse));
}

/// MPEG-1 layer 3 is MP3, which FLV does carry. The block refuses it, and the
/// message must not claim FLV cannot: that would send an operator to change a
/// container setting that is not the problem.
#[test]
fn mp3_is_refused_without_blaming_the_container() {
    let message = audio_plan("audio/mpeg", 1, 3).expect_err("MP3 must be refused");
    assert!(
        message.contains("MP3"),
        "the refusal must name MP3, got: {}",
        message
    );
    assert!(
        !message.contains("FLV cannot"),
        "FLV does carry MP3, so the refusal must not blame the container: {}",
        message
    );
}

/// MPEG-1 layers 1 and 2 are a different case from MP3: `flvmux` accepts only
/// layer 3, so here the container really is the reason.
#[test]
fn mpeg1_layer_two_is_refused_as_a_container_limit() {
    let message = audio_plan("audio/mpeg", 1, 2).expect_err("MPEG-1 layer 2 must be refused");
    assert!(
        message.contains("layer 2"),
        "the refusal must name the layer, got: {}",
        message
    );
}

#[test]
fn other_audio_codecs_are_refused_and_named() {
    let message = audio_plan("audio/x-opus", 0, 0).expect_err("Opus must be refused");
    assert!(
        message.contains("audio/x-opus"),
        "the refusal must name what arrived, got: {}",
        message
    );
}
