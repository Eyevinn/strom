//! An `srt_uri` that GStreamer would read differently from how it is written
//! fails the SRT block's build with a clear message, for all four SRT
//! blocks. `latency=` in the URI only warns: the block's own Latency
//! property is what the element ends up with. Each test builds the real block.

use crate::common;

use std::collections::HashMap;
use strom::blocks::builtin::get_builder;
use strom::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult};
use strom_types::PropertyValue;

use gstreamer::prelude::*;

/// The SRT blocks and the element in each that carries the URI.
/// The EFP blocks exist only with the `efp` feature, which CI builds with.
const BLOCKS: &[(&str, &str)] = &[
    ("builtin.mpegtssrt_input", "srtsrc"),
    ("builtin.mpegtssrt_output", "srtsink"),
    #[cfg(feature = "efp")]
    ("builtin.efpsrt_input", "srtsrc"),
    #[cfg(feature = "efp")]
    ("builtin.efpsrt_output", "srtsink"),
];

fn build(
    definition_id: &str,
    properties: &[(&str, PropertyValue)],
) -> Result<BlockBuildResult, BlockBuildError> {
    let properties: HashMap<String, PropertyValue> = properties
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let ctx = BlockBuildContext::new(vec![], "all".to_string())
        .with_srt_relay(Some("127.0.0.1:9000".to_string()));
    get_builder(definition_id)
        .unwrap_or_else(|| panic!("no builder for {definition_id}"))
        .build("srt_block", &properties, &ctx)
}

fn uri(srt_uri: &str) -> (&'static str, PropertyValue) {
    ("srt_uri", PropertyValue::String(srt_uri.to_string()))
}

fn element<'a>(result: &'a BlockBuildResult, factory: &str) -> &'a gstreamer::Element {
    result
        .elements
        .iter()
        .map(|(_, e)| e)
        .find(|e| e.factory().is_some_and(|f| f.name() == factory))
        .unwrap_or_else(|| panic!("block has no {factory}"))
}

#[test]
fn a_uri_gstreamer_would_misread_fails_the_build() {
    common::init_static_plugins();
    for (srt_uri, expected) in [
        (
            "srt://:5000?mode=listener&PassPhrase=0123456789",
            "'PassPhrase' is not read by GStreamer: use 'passphrase'",
        ),
        (
            "srt://:5000?mode=Listener",
            "mode 'Listener' is not read by GStreamer: use 'listener'",
        ),
        (
            "srt://:5000?mode=listener&passphrase=0123456789%zz",
            "%-escape",
        ),
        ("srt://127.0.0.1:5000?streamid=#!::r=cam1,m=publish", "'#'"),
        ("srt+relay://cam1?passphrase=a#bcdefghijk", "'#'"),
        (
            "srt://:5000?mode=listener&passphrase=short",
            "10 to 80 bytes",
        ),
        ("srt://:5000?mode=listener&pbkeylen=20", "pbkeylen"),
        ("SRT://:5000?mode=listener", "lower case"),
        (
            "srt://:5000?mode=listener&passphrase=0123456789&passphrase=9876543210",
            "more than once",
        ),
    ] {
        for (definition_id, _) in BLOCKS {
            let err = match build(definition_id, &[uri(srt_uri)]) {
                Ok(_) => panic!("{definition_id} built with {srt_uri}"),
                Err(e) => e.to_string(),
            };
            assert!(
                err.contains("srt_uri") && err.contains(expected),
                "{definition_id}, {srt_uri}: {err}"
            );
            // The passphrase never shows up in the message.
            assert!(!err.contains("0123456789"), "{err}");
        }
    }
}

#[test]
fn latency_in_the_uri_builds_and_the_block_latency_wins() {
    common::init_static_plugins();
    for (definition_id, factory) in BLOCKS {
        let result = build(
            definition_id,
            &[
                uri("  srt://:5000?mode=listener&latency=900 "),
                ("latency", PropertyValue::Int(240)),
            ],
        )
        .unwrap_or_else(|e| panic!("{definition_id}: {e}"));
        let element = element(&result, factory);
        let latency: i32 = element.property("latency");
        assert_eq!(latency, 240, "{definition_id}");
        // The URI reached the element trimmed.
        let set: String = element.property("uri");
        assert!(set.starts_with("srt://:5000"), "{definition_id}: {set}");
    }
}

#[test]
fn a_well_formed_encoded_uri_builds() {
    common::init_static_plugins();
    for (definition_id, factory) in BLOCKS {
        let result = build(
            definition_id,
            &[uri(
                "srt://127.0.0.1:5000?mode=caller&streamid=%23!%3A%3Ar%3Dcam1\
                 &passphrase=a%26b%23c%25d0123&pbkeylen=32&rcvlatency=200",
            )],
        )
        .unwrap_or_else(|e| panic!("{definition_id}: {e}"));
        element(&result, factory);
    }
}
