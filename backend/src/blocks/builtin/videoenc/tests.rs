//! Tests for video encoder block
//!
//! These tests verify:
//! 1. Encoder selection logic (unit tests)
//! 2. Property setting compatibility (integration tests)
//! 3. Codec and preference handling
//!
//! Tests are conditional - they only test encoders available on the system.

use super::*;
use gstreamer as gst;

/// Initialize GStreamer for tests
fn init_gst() {
    let _ = gst::init();
}

/// Check if an encoder is available on the system
fn is_encoder_available(encoder_name: &str) -> bool {
    let registry = gst::Registry::get();
    registry
        .find_feature(encoder_name, gst::ElementFactory::static_type())
        .is_some()
}

/// Like `is_encoder_available`, but a skip is a failure when CI sets
/// `STROM_REQUIRE_GST_PLUGINS`: a test that skips passes green and guards nothing.
fn require_element(element_name: &str) -> bool {
    if is_encoder_available(element_name) {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but {} is missing",
        element_name
    );
    println!("{} not available, skipping test", element_name);
    false
}

#[test]
fn test_codec_parsing_default() {
    let properties = HashMap::new();
    let codec = parse_codec(&properties).expect("Should have default codec");
    assert_eq!(codec, Codec::H264, "Default codec should be H.264");
}

#[test]
fn test_codec_parsing_valid() {
    let test_cases = vec![
        ("h264", Codec::H264),
        ("h265", Codec::H265),
        ("av1", Codec::AV1),
        ("vp9", Codec::VP9),
    ];

    for (codec_str, expected) in test_cases {
        let mut properties = HashMap::new();
        properties.insert(
            "codec".to_string(),
            PropertyValue::String(codec_str.to_string()),
        );
        let codec = parse_codec(&properties).expect("Should parse valid codec");
        assert_eq!(
            codec, expected,
            "Codec {} should parse correctly",
            codec_str
        );
    }
}

#[test]
fn test_codec_parsing_invalid() {
    let mut properties = HashMap::new();
    properties.insert(
        "codec".to_string(),
        PropertyValue::String("invalid".to_string()),
    );
    assert!(
        parse_codec(&properties).is_err(),
        "Invalid codec should return error"
    );
}

#[test]
fn test_encoder_preference_parsing() {
    let test_cases = vec![
        (None, EncoderPreference::Auto),
        (Some("auto"), EncoderPreference::Auto),
        (Some("hardware"), EncoderPreference::HardwareOnly),
        (Some("software"), EncoderPreference::SoftwareOnly),
        (Some("invalid"), EncoderPreference::Auto), // Falls back to auto
    ];

    for (input, expected) in test_cases {
        let mut properties = HashMap::new();
        if let Some(pref) = input {
            properties.insert(
                "encoder_preference".to_string(),
                PropertyValue::String(pref.to_string()),
            );
        }
        let preference = parse_encoder_preference(&properties);
        assert_eq!(
            preference, expected,
            "Preference {:?} should parse to {:?}",
            input, expected
        );
    }
}

#[test]
fn test_rate_control_parsing() {
    let test_cases = vec![
        (None, RateControl::VBR),
        (Some("vbr"), RateControl::VBR),
        (Some("cbr"), RateControl::CBR),
        (Some("cqp"), RateControl::CQP),
        (Some("invalid"), RateControl::VBR), // Falls back to VBR
    ];

    for (input, expected) in test_cases {
        let mut properties = HashMap::new();
        if let Some(rc) = input {
            properties.insert(
                "rate_control".to_string(),
                PropertyValue::String(rc.to_string()),
            );
        }
        let rate_control = parse_rate_control(&properties);
        assert_eq!(
            rate_control, expected,
            "Rate control {:?} should parse to {:?}",
            input, expected
        );
    }
}

#[test]
fn test_hardware_encoder_list_h264() {
    let encoders = get_hardware_encoder_list(Codec::H264);
    assert!(
        !encoders.is_empty(),
        "Should have H.264 hardware encoders in list"
    );
    assert!(
        encoders.contains(&Candidate::Named("nvautogpuh264enc")),
        "Should include nvautogpuh264enc"
    );
    assert!(
        encoders.contains(&Candidate::Named("nvh264enc")),
        "Should include nvh264enc"
    );
    assert!(
        encoders.contains(&Candidate::Named("qsvh264enc")),
        "Should include qsvh264enc"
    );
}

#[test]
fn test_software_encoder_list() {
    let h264 = get_software_encoder_list(Codec::H264);
    assert_eq!(h264, vec!["x264enc"], "H.264 software encoder");

    let h265 = get_software_encoder_list(Codec::H265);
    assert_eq!(h265, vec!["x265enc"], "H.265 software encoder");

    let av1 = get_software_encoder_list(Codec::AV1);
    assert!(av1.contains(&"svtav1enc"), "AV1 should include svtav1enc");
    assert!(av1.contains(&"av1enc"), "AV1 should include av1enc");

    let vp9 = get_software_encoder_list(Codec::VP9);
    assert_eq!(vp9, vec!["vp9enc"], "VP9 software encoder");
}

#[test]
fn test_encoder_priority_order() {
    init_gst();

    // H.264 hardware priority: NVIDIA should come before Intel/VA-API
    let h264_hw = get_hardware_encoder_list(Codec::H264);
    let nv_pos = h264_hw
        .iter()
        .position(|&e| e == Candidate::Named("nvautogpuh264enc"));
    let qsv_pos = h264_hw
        .iter()
        .position(|&e| e == Candidate::Named("qsvh264enc"));

    if let (Some(nv), Some(qsv)) = (nv_pos, qsv_pos) {
        assert!(
            nv < qsv,
            "NVIDIA encoder should have higher priority than QSV"
        );
    }
}

#[test]
fn test_encoder_selection_software_only() {
    init_gst();

    // Software-only should only try software encoders
    let result = select_encoder(Codec::H264, EncoderPreference::SoftwareOnly, Profile::Auto);

    match result {
        Ok(encoder) => {
            assert_eq!(
                encoder, "x264enc",
                "Software-only should select x264enc for H.264"
            );
        }
        Err(_) => {
            // x264enc might not be installed, that's OK for this test
            println!("x264enc not available on system, test skipped");
        }
    }
}

#[test]
fn test_get_codec_caps_string() {
    assert_eq!(
        get_codec_caps_string(Codec::H264, Profile::None),
        "video/x-h264,alignment=au"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H264, Profile::Baseline),
        "video/x-h264,alignment=au,profile=baseline"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H264, Profile::High),
        "video/x-h264,alignment=au,profile=high"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H264, Profile::High444),
        "video/x-h264,alignment=au,profile=high-4:4:4"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H265, Profile::Main422_10),
        "video/x-h265,alignment=au,profile=main-422-10"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H265, Profile::None),
        "video/x-h265,alignment=au"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H265, Profile::Main),
        "video/x-h265,alignment=au,profile=main"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H265, Profile::Main10),
        "video/x-h265,alignment=au,profile=main-10"
    );
    assert_eq!(
        get_codec_caps_string(Codec::AV1, Profile::None),
        "video/x-av1"
    );
    assert_eq!(
        get_codec_caps_string(Codec::VP9, Profile::None),
        "video/x-vp9"
    );
}

/// `Auto` is the default, so this is what an operator who sets no profile gets.
/// It has to resolve per codec: "high" is not an H.265 profile name and pinning
/// it on an h265 capsfilter fails to negotiate.
#[test]
fn test_get_codec_caps_string_auto_resolves_per_codec() {
    assert_eq!(
        get_codec_caps_string(Codec::H264, Profile::Auto),
        "video/x-h264,alignment=au,profile=high"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H265, Profile::Auto),
        "video/x-h265,alignment=au,profile=main"
    );
    // AV1 and VP9 have no profile field on their caps at all.
    assert_eq!(
        get_codec_caps_string(Codec::AV1, Profile::Auto),
        "video/x-av1"
    );
    assert_eq!(
        get_codec_caps_string(Codec::VP9, Profile::Auto),
        "video/x-vp9"
    );

    // The default must reach the capsfilter as a pinned profile, not as an
    // empty one: this is the assertion that fails if the default is reverted.
    assert_eq!(
        get_codec_caps_string(Codec::H264, Profile::default()),
        "video/x-h264,alignment=au,profile=high"
    );
}

#[test]
fn test_parse_profile_invalid_value_falls_back_to_default() {
    let mut props = HashMap::new();
    props.insert(
        "profile".to_string(),
        PropertyValue::String("garbage".to_string()),
    );
    assert_eq!(parse_profile(&props), Profile::default());

    // Not a GStreamer profile name, and close enough to a real one to be a
    // plausible typo.
    props.insert(
        "profile".to_string(),
        PropertyValue::String("high-4:2:0".to_string()),
    );
    assert_eq!(parse_profile(&props), Profile::default());
}

#[test]
fn test_parse_profile_auto_is_the_default() {
    let mut props = HashMap::new();
    props.insert(
        "profile".to_string(),
        PropertyValue::String("auto".to_string()),
    );
    assert_eq!(parse_profile(&props), Profile::Auto);
    assert_eq!(Profile::default(), Profile::Auto);

    // "none" stays reachable: free negotiation is still on offer, it is just
    // no longer what you get by not choosing.
    props.insert(
        "profile".to_string(),
        PropertyValue::String("none".to_string()),
    );
    assert_eq!(parse_profile(&props), Profile::None);
}

#[test]
fn test_parse_profile_missing_falls_back_to_default() {
    let props = HashMap::new();
    assert_eq!(parse_profile(&props), Profile::default());
}

#[test]
fn test_parse_profile_valid_value() {
    let mut props = HashMap::new();
    props.insert(
        "profile".to_string(),
        PropertyValue::String("main-422-10".to_string()),
    );
    assert_eq!(parse_profile(&props), Profile::Main422_10);
}

/// Test that we can create and configure available encoders without panicking
#[test]
fn test_encoder_property_setting_x264() {
    init_gst();

    if !is_encoder_available("x264enc") {
        println!("x264enc not available, skipping test");
        return;
    }

    let encoder = gst::ElementFactory::make("x264enc")
        .build()
        .expect("Should create x264enc");

    // Test that we can set properties without panicking
    set_encoder_properties(
        &encoder,
        "x264enc",
        4000,
        "medium",
        "zerolatency",
        RateControl::VBR,
        60,
    )
    .expect("valid properties apply");

    // Verify bitrate was set
    let bitrate: u32 = encoder.property("bitrate");
    assert_eq!(bitrate, 4000, "Bitrate should be set correctly");
}

/// Test NVIDIA encoder property setting (conditional)
#[test]
fn test_encoder_property_setting_nvenc() {
    init_gst();

    // Try different NVIDIA encoder variants
    let nv_encoders = ["nvautogpuh264enc", "nvh264enc"];
    let available: Vec<_> = nv_encoders
        .iter()
        .filter(|&e| is_encoder_available(e))
        .collect();

    if available.is_empty() {
        println!("No NVIDIA encoders available, skipping test");
        return;
    }

    for encoder_name in available {
        println!("Testing NVIDIA encoder: {}", encoder_name);

        let encoder = gst::ElementFactory::make(encoder_name)
            .build()
            .unwrap_or_else(|_| panic!("Should create {}", encoder_name));

        // Test property setting without panicking
        set_encoder_properties(
            &encoder,
            encoder_name,
            4000,
            "medium",
            "zerolatency",
            RateControl::VBR,
            60,
        )
        .expect("valid properties apply");

        // Verify bitrate
        let bitrate: u32 = encoder.property("bitrate");
        assert_eq!(bitrate, 4000, "Bitrate should be set for {}", encoder_name);

        // The rate control property is `rate-control` or `rc-mode`, depending
        // on the encoder and the GStreamer build: `nvautogpuh264enc` has
        // `rc-mode` in the Windows 1.28.8 build (#1166). The block takes
        // whichever exists, so the encoder must have one of them.
        assert!(
            ["rate-control", "rc-mode"]
                .iter()
                .any(|p| encoder.has_property(p)),
            "{} has neither rate-control nor rc-mode",
            encoder_name
        );
    }
}

/// #769: the v4l2 branch multiplied the client's kbps by 1000 in plain `u32`
/// arithmetic, so a bitrate above 4_294_967 panicked in a debug build before any
/// property was set. `identity` has none of the properties the branch touches,
/// so only the arithmetic runs.
#[test]
fn test_v4l2_bitrate_past_u32_bits_per_second_does_not_panic() {
    init_gst();

    let encoder = gst::ElementFactory::make("identity")
        .build()
        .expect("Should create identity");

    set_encoder_properties(
        &encoder,
        "v4l2h264enc",
        5_000_000,
        "medium",
        "zerolatency",
        RateControl::VBR,
        60,
    )
    .expect("a bitrate past u32 bits/s must not fail the v4l2 branch");
}

/// #769: the NVENC branch set `preset` and a rate-control property picked by
/// name prefix, both unguarded, so an `nv*` encoder exposing other names could
/// not build at all. `x264enc` stands in for such an element: it has `bitrate`
/// but none of `preset`, `rc-mode` or `rate-control`.
#[test]
fn test_nvenc_branch_skips_properties_the_element_lacks() {
    init_gst();

    if !require_element("x264enc") {
        return;
    }

    let encoder = gst::ElementFactory::make("x264enc")
        .build()
        .expect("Should create x264enc");
    for name in ["preset", "rc-mode", "rate-control"] {
        assert!(
            !encoder.has_property(name),
            "the stand-in must lack {} for this test to mean anything",
            name
        );
    }

    set_encoder_properties(
        &encoder,
        "nvd3d11h264enc",
        4000,
        "medium",
        "zerolatency",
        RateControl::VBR,
        60,
    )
    .expect("a property the element lacks must be skipped, not fail the build");

    let bitrate: u32 = encoder.property("bitrate");
    assert_eq!(bitrate, 4000, "Bitrate should still be set");
}

/// #769: vp9enc's `target-bitrate` is in bits/s, derived from the client's
/// kbps. A bitrate whose bits/s form is past the element's range was refused
/// over a number the operator never supplied; it is clamped to the range.
#[test]
fn test_vp9_derived_target_bitrate_is_clamped_to_its_range() {
    init_gst();

    if !require_element("vp9enc") {
        return;
    }

    let encoder = gst::ElementFactory::make("vp9enc")
        .build()
        .expect("Should create vp9enc");
    let pspec = encoder
        .find_property("target-bitrate")
        .expect("vp9enc has target-bitrate");
    let max = pspec
        .downcast_ref::<gst::glib::ParamSpecInt>()
        .expect("vp9enc target-bitrate is a gint")
        .maximum();

    // 2_200_000 kbps is 2.2e9 bits/s, past gint's maximum.
    set_encoder_properties(
        &encoder,
        "vp9enc",
        2_200_000,
        "medium",
        "zerolatency",
        RateControl::VBR,
        60,
    )
    .expect("a derived value past its target's range must be clamped, not refused");

    // vp9enc keeps the rate in whole kbps (libvpx's `rc_target_bitrate`), so
    // the clamped maximum reads back rounded down to a multiple of 1000.
    let target: i32 = encoder.property("target-bitrate");
    assert!(
        target <= max && target > max - 1000,
        "target-bitrate should be clamped to its maximum ({}), got {}",
        max,
        target
    );
}

/// Test GOP size property compatibility
#[test]
fn test_gop_size_properties() {
    init_gst();

    // Test x264enc (uses key-int-max)
    if is_encoder_available("x264enc") {
        let encoder = gst::ElementFactory::make("x264enc")
            .build()
            .expect("Should create x264enc");

        assert!(
            encoder.has_property("key-int-max"),
            "x264enc should have key-int-max property"
        );

        set_encoder_properties(
            &encoder,
            "x264enc",
            4000,
            "medium",
            "zerolatency",
            RateControl::VBR,
            60,
        )
        .expect("valid properties apply");

        // x264enc's key-int-max is u32 (guint), not i32 (gint)
        let gop: u32 = encoder.property("key-int-max");
        assert_eq!(gop, 60, "GOP size should be set correctly for x264enc");
    }

    // Test NVIDIA encoders (use gop-size)
    for encoder_name in &["nvautogpuh264enc", "nvh264enc"] {
        if is_encoder_available(encoder_name) {
            let encoder = gst::ElementFactory::make(encoder_name)
                .build()
                .unwrap_or_else(|_| panic!("Should create {}", encoder_name));

            assert!(
                encoder.has_property("gop-size"),
                "{} should have gop-size property",
                encoder_name
            );

            set_encoder_properties(
                &encoder,
                encoder_name,
                4000,
                "medium",
                "zerolatency",
                RateControl::VBR,
                60,
            )
            .expect("valid properties apply");

            let gop: i32 = encoder.property("gop-size");
            assert_eq!(
                gop, 60,
                "GOP size should be set correctly for {}",
                encoder_name
            );
        }
    }
}

/// Test that type casting works correctly (u32 -> i32 for GOP size)
#[test]
fn test_gop_size_type_casting() {
    init_gst();

    if !is_encoder_available("x264enc") {
        println!("x264enc not available, skipping test");
        return;
    }

    let encoder = gst::ElementFactory::make("x264enc")
        .build()
        .expect("Should create x264enc");

    // Test with various GOP sizes including edge cases
    for gop_value in &[0u32, 1, 30, 60, 120, 300] {
        set_encoder_properties(
            &encoder,
            "x264enc",
            4000,
            "medium",
            "zerolatency",
            RateControl::VBR,
            *gop_value,
        )
        .expect("valid properties apply");

        if *gop_value > 0 {
            // x264enc's key-int-max is u32 (guint), not i32
            let set_gop: u32 = encoder.property("key-int-max");
            assert_eq!(
                set_gop, *gop_value,
                "GOP size should be set correctly for x264enc"
            );
        }
    }
}

/// Test quality preset mapping
#[test]
fn test_quality_preset_mappings() {
    // x264/x265 enum values (0=none, 1=ultrafast, ..., 6=medium, ..., 9=veryslow)
    assert_eq!(map_quality_preset_x264_enum("ultrafast"), 1);
    assert_eq!(map_quality_preset_x264_enum("fast"), 5);
    assert_eq!(map_quality_preset_x264_enum("medium"), 6);
    assert_eq!(map_quality_preset_x264_enum("slow"), 7);
    assert_eq!(map_quality_preset_x264_enum("veryslow"), 9);
    assert_eq!(map_quality_preset_x264_enum("invalid"), 6); // default to medium

    // NVENC enum values (8=p1/fastest, 11=p4/medium, 14=p7/slowest)
    assert_eq!(map_quality_preset_nvenc_enum("ultrafast"), 8); // p1
    assert_eq!(map_quality_preset_nvenc_enum("fast"), 10); // p3
    assert_eq!(map_quality_preset_nvenc_enum("medium"), 11); // p4 (default)
    assert_eq!(map_quality_preset_nvenc_enum("slow"), 13); // p6
    assert_eq!(map_quality_preset_nvenc_enum("veryslow"), 14); // p7

    // Intel QSV (1=best quality, 7=fastest)
    assert_eq!(map_quality_preset_qsv("ultrafast"), 7);
    assert_eq!(map_quality_preset_qsv("fast"), 5);
    assert_eq!(map_quality_preset_qsv("medium"), 4);
    assert_eq!(map_quality_preset_qsv("slow"), 2);
    assert_eq!(map_quality_preset_qsv("veryslow"), 1);
}

/// Test that block respects encoder preference
#[test]
fn test_block_encoder_preference() {
    init_gst();

    // Only test if we have x264enc available
    if !is_encoder_available("x264enc") {
        println!("x264enc not available, skipping test");
        return;
    }

    let builder = VideoEncBuilder;
    let mut properties = HashMap::new();

    properties.insert(
        "codec".to_string(),
        PropertyValue::String("h264".to_string()),
    );
    properties.insert(
        "encoder_preference".to_string(),
        PropertyValue::String("software".to_string()),
    );

    let ctx = BlockBuildContext::new(
        vec!["stun:stun.l.google.com:19302".to_string()],
        "all".to_string(),
    );
    let result = builder.build("test_software", &properties, &ctx);
    assert!(
        result.is_ok(),
        "Should successfully build with software preference"
    );

    // The encoder element should be x264enc
    if let Ok(block_result) = result {
        let encoder_elem = block_result
            .elements
            .iter()
            .find(|(id, _)| id.contains("encoder"))
            .map(|(_, elem)| elem);

        if let Some(encoder) = encoder_elem {
            // x264enc has the "speed-preset" property
            assert!(
                encoder.has_property("speed-preset"),
                "Software encoder should be x264enc with speed-preset property"
            );
        }
    }
}

/// #1163: on Windows with a Core Ultra GPU and an Intel driver older than
/// 32.0.101.9150, the QSV encoders corrupt the process heap, so once
/// `demote_qsv_plugin` has run the block never selects one. With 9150 or
/// later, or on other GPUs, they keep their rank. Elsewhere QSV keeps its rank
/// and this checks nothing.
///
/// The independent check: when the `qsv` plugin registered features, DXGI
/// must have found the Intel adapter behind them, with a real device id. The
/// rank checks reuse `qsv_at_risk`, so they show the demotion was applied to
/// every feature of the qsv plugin (decoders and per-device ones included)
/// and set the refusal, not that the decision is right; the unit tests in
/// `gst::intel_qsv` cover that.
#[test]
fn qsv_encoders_follow_the_intel_driver_on_windows() {
    use crate::gst::intel_qsv::{self, QsvMode};

    init_gst();
    crate::gst::demote_qsv_plugin();
    if !cfg!(windows) {
        return;
    }
    let features = intel_qsv::qsv_plugin_features();
    let scan = intel_qsv::intel_adapters();
    let names: Vec<String> = features.iter().map(|f| f.name().to_string()).collect();
    eprintln!("QSV plugin features {names:?}, adapter scan {scan:?}");
    if features.is_empty() {
        return;
    }
    assert!(
        !scan.intel.is_empty(),
        "qsv registered {names:?} but DXGI found no Intel adapter: {scan:?}"
    );
    for adapter in &scan.intel {
        assert_ne!(
            adapter.device_id, 0,
            "Intel adapter without a device id: {scan:?}"
        );
    }
    let demote = match intel_qsv::qsv_mode_from_env() {
        QsvMode::Off => true,
        QsvMode::On => false,
        QsvMode::Auto => intel_qsv::qsv_at_risk(&scan.intel).is_some(),
    };
    assert_eq!(
        intel_qsv::qsv_blocked(),
        demote,
        "qsv_blocked() on {scan:?}"
    );
    for factory in &features {
        let name = factory.name();
        assert_eq!(
            crate::gst::qsv_refusal(&name).is_some(),
            demote,
            "{name} refusal on {scan:?}"
        );
        if demote {
            assert_eq!(
                factory.rank(),
                gst::Rank::NONE,
                "{name} kept its rank on {scan:?}"
            );
        } else if crate::gst::QSV_ENCODERS.contains(&name.as_str()) {
            assert_ne!(
                factory.rank(),
                gst::Rank::NONE,
                "{name} was demoted on {scan:?}"
            );
        }
    }
    if !demote {
        return;
    }
    for codec in [Codec::H264, Codec::H265, Codec::AV1, Codec::VP9] {
        for preference in [EncoderPreference::Auto, EncoderPreference::HardwareOnly] {
            if let Ok(encoder) = select_encoder(codec, preference, Profile::Auto) {
                assert!(
                    !encoder.starts_with("qsv"),
                    "{codec:?} {preference:?} selected {encoder}"
                );
            }
        }
    }
}

// --- Encoder selection over factory descriptors -------------------------------
//
// These run on every platform: `choose_encoder` decides over descriptors, so
// factories the host lacks (Media Foundation, NVENC) are stood in for.

use super::mediafoundation::{
    map_quality_preset_mf, mf_encoder_settings, mf_encoder_settings_with, mf_kbit_from_kbps,
    mf_max_bitrate, mf_rc_mode_nick, mf_rc_mode_nick_with, MfSetting, MfValue, MF_CQP_QP,
    MF_CQP_QUALITY, MF_VBR_RC_MODE,
};
use super::selection::{FactoryInfo, SrcFormat};

const HW_KLASS: &str = "Codec/Encoder/Video/Hardware";
const SW_KLASS: &str = "Codec/Encoder/Video";

/// A descriptor whose source template has `media` with no profile field.
fn factory(name: &str, plugin: &str, klass: &str, rank: gst::Rank, media: &str) -> FactoryInfo {
    FactoryInfo {
        name: name.to_string(),
        plugin: Some(plugin.to_string()),
        klass: klass.to_string(),
        rank,
        src_formats: vec![SrcFormat {
            media_type: media.to_string(),
            profiles: None,
        }],
    }
}

fn mf(name: &str, klass: &str, rank: gst::Rank, media: &str) -> FactoryInfo {
    factory(name, MEDIA_FOUNDATION_PLUGIN, klass, rank, media)
}

/// `info` with its source template listing only `profiles`.
fn with_profiles(mut info: FactoryInfo, profiles: &[&str]) -> FactoryInfo {
    for format in &mut info.src_formats {
        format.profiles = Some(profiles.iter().map(|p| p.to_string()).collect());
    }
    info
}

const H264: &str = "video/x-h264";
/// The rank the plugin gives a non-default device: one below the default.
fn lower() -> gst::Rank {
    gst::Rank::SECONDARY - 1
}

/// H.264 with the block's default profile pin (`high`).
fn h264_output(profile: Profile) -> EncoderOutput {
    EncoderOutput {
        media_type: H264,
        profile: pinned_profile(Codec::H264, profile),
    }
}

fn choose_h264(factories: &[FactoryInfo]) -> Option<String> {
    choose_encoder(
        &get_hardware_encoder_list(Codec::H264),
        &h264_output(Profile::Auto),
        factories,
    )
}

/// Hardware list, then the software fallback, as `select_encoder` in Auto.
fn choose_h264_auto(factories: &[FactoryInfo], profile: Profile) -> Option<String> {
    let wanted = h264_output(profile);
    choose_encoder(&get_hardware_encoder_list(Codec::H264), &wanted, factories).or_else(|| {
        choose_encoder(
            &named(&get_software_encoder_list(Codec::H264)),
            &wanted,
            factories,
        )
    })
}

/// The profiles GStreamer 1.28's `mediafoundation` plugin can list for an
/// H.264 MFT (`gstmfvideoencoder.cpp`: high, main, baseline, plus
/// constrained-baseline next to baseline), and x264enc's template.
const MF_H264_ALL_PROFILES: [&str; 4] = ["high", "main", "constrained-baseline", "baseline"];
const X264_PROFILES: [&str; 10] = [
    "high-4:4:4",
    "high-4:2:2",
    "high-10",
    "high",
    "main",
    "baseline",
    "constrained-baseline",
    "high-4:4:4-intra",
    "high-4:2:2-intra",
    "high-10-intra",
];

/// An MFT whose device lists only some profiles is skipped for a pinned
/// profile it lacks, and selection falls through to x264. Fails if the
/// profile filter is removed from `choose_encoder`.
#[test]
fn encoder_that_cannot_produce_the_pinned_profile_is_skipped() {
    let factories = [
        with_profiles(
            mf("mfh264enc", HW_KLASS, gst::Rank::SECONDARY, H264),
            &["high", "main"],
        ),
        with_profiles(
            factory("x264enc", "x264", SW_KLASS, gst::Rank::PRIMARY, H264),
            &X264_PROFILES,
        ),
    ];
    // WebRTC pins constrained-baseline
    assert_eq!(
        choose_h264_auto(&factories, Profile::ConstrainedBaseline).as_deref(),
        Some("x264enc")
    );
    // High is listed: the MFT is used
    assert_eq!(
        choose_h264_auto(&factories, Profile::Auto).as_deref(),
        Some("mfh264enc")
    );
    // No pin: any profile will do
    assert_eq!(
        choose_h264_auto(&factories, Profile::None).as_deref(),
        Some("mfh264enc")
    );
}

/// Profiles no MFT lists (4:2:2, 4:4:4, 10-bit H.264) go to x264, even with a
/// full MF profile list; the filter is generic, so a vendor encoder that
/// lacks the profile is skipped the same way.
#[test]
fn profiles_the_mft_cannot_make_fall_through_to_x264() {
    let factories = [
        with_profiles(
            factory("nvh264enc", "nvcodec", HW_KLASS, gst::Rank::PRIMARY, H264),
            &["main", "high", "constrained-baseline", "baseline"],
        ),
        with_profiles(
            mf("mfh264enc", HW_KLASS, gst::Rank::SECONDARY, H264),
            &MF_H264_ALL_PROFILES,
        ),
        with_profiles(
            factory("x264enc", "x264", SW_KLASS, gst::Rank::PRIMARY, H264),
            &X264_PROFILES,
        ),
    ];
    for profile in [Profile::High444, Profile::High422, Profile::High10] {
        assert_eq!(
            choose_h264_auto(&factories, profile).as_deref(),
            Some("x264enc"),
            "{profile:?}"
        );
    }
    assert_eq!(
        choose_h264_auto(&factories, Profile::ConstrainedBaseline).as_deref(),
        Some("nvh264enc")
    );
}

/// A template without a profile field, or with no structures (ANY), counts
/// as able to produce any profile.
#[test]
fn template_without_profiles_is_compatible() {
    let wanted = h264_output(Profile::ConstrainedBaseline);
    assert!(mf("mfh264enc", HW_KLASS, gst::Rank::SECONDARY, H264).can_produce(&wanted));
    let mut any = mf("mfh264enc", HW_KLASS, gst::Rank::SECONDARY, H264);
    any.src_formats.clear();
    assert!(any.can_produce(&wanted));
    let other_codec = with_profiles(
        mf("mfh265enc", HW_KLASS, gst::Rank::SECONDARY, "video/x-h265"),
        &["main"],
    );
    assert!(!other_codec.can_produce(&wanted));
}

/// The Media Foundation entry for each codec that has one sits after every
/// vendor-native hardware encoder (NVENC, QSV, AMF) and is not a software
/// fallback. AV1 has none: the plugin registers no AV1 encoder.
#[test]
fn media_foundation_follows_the_vendor_native_encoders() {
    for (codec, has_mf) in [
        (Codec::H264, true),
        (Codec::H265, true),
        (Codec::VP9, true),
        (Codec::AV1, false),
    ] {
        let hw = get_hardware_encoder_list(codec);
        let mf_pos = hw
            .iter()
            .position(|&e| e == Candidate::Plugin(MEDIA_FOUNDATION_PLUGIN));
        assert_eq!(mf_pos.is_some(), has_mf, "{codec:?}");
        let Some(mf_pos) = mf_pos else { continue };
        for (pos, candidate) in hw.iter().enumerate() {
            let Candidate::Named(name) = candidate else {
                continue;
            };
            if ["nv", "qsv", "amf"].iter().any(|p| name.starts_with(p)) {
                assert!(
                    pos < mf_pos,
                    "{codec:?}: {name} must come before Media Foundation"
                );
            }
        }
        assert!(!get_software_encoder_list(codec).contains(&MEDIA_FOUNDATION_PLUGIN));
    }
}

/// A host with only a Media Foundation hardware MFT encodes in hardware.
#[test]
fn hardware_mft_is_selected_without_a_vendor_encoder() {
    let factories = [mf("mfh264enc", HW_KLASS, gst::Rank::SECONDARY, H264)];
    assert_eq!(choose_h264(&factories).as_deref(), Some("mfh264enc"));
}

/// A vendor-native encoder wins over the MFT for the same GPU.
#[test]
fn vendor_native_encoder_is_preferred_over_the_mft() {
    let factories = [
        mf("mfh264enc", HW_KLASS, gst::Rank::SECONDARY, H264),
        factory("nvh264enc", "nvcodec", HW_KLASS, gst::Rank::PRIMARY, H264),
    ];
    assert_eq!(choose_h264(&factories).as_deref(), Some("nvh264enc"));
}

/// Microsoft's software MFT is registered under the same names and rank as a
/// hardware one; only the klass tells them apart. It is never selected, even
/// when it ranks first. Fails if the Hardware-klass check is removed.
#[test]
fn software_mft_is_never_selected() {
    // No hardware MFT: the software one takes the default name.
    let only_software = [mf("mfh264enc", SW_KLASS, gst::Rank::SECONDARY, H264)];
    assert_eq!(choose_h264(&only_software), None);

    let both = [
        mf("mfh264enc", SW_KLASS, gst::Rank::SECONDARY, H264),
        mf("mfh264device1enc", HW_KLASS, lower(), H264),
    ];
    assert_eq!(choose_h264(&both).as_deref(), Some("mfh264device1enc"));
}

/// A factory of rank NONE (disabled through `GST_PLUGIN_FEATURE_RANK`) is
/// skipped, named or Media Foundation. Fails if the rank-NONE skip is removed.
#[test]
fn disabled_encoders_are_skipped() {
    let factories = [
        factory("nvh264enc", "nvcodec", HW_KLASS, gst::Rank::NONE, H264),
        mf("mfh264enc", HW_KLASS, gst::Rank::NONE, H264),
        mf("mfh264device1enc", HW_KLASS, lower(), H264),
    ];
    assert_eq!(choose_h264(&factories).as_deref(), Some("mfh264device1enc"));
}

/// Media Foundation devices are tried by rank (the default device ranks one
/// above the rest), whatever order the registry lists them in.
#[test]
fn media_foundation_devices_are_tried_by_rank() {
    let factories = [
        mf("mfh264device1enc", HW_KLASS, lower(), H264),
        mf("mfh264enc", HW_KLASS, gst::Rank::SECONDARY, H264),
    ];
    assert_eq!(choose_h264(&factories).as_deref(), Some("mfh264enc"));
}

/// Media Foundation encoders are found by plugin and output caps, not name: a
/// feature of another plugin named like one is ignored, an MFT for another
/// codec is ignored, and an MFT with any name is found.
#[test]
fn media_foundation_encoders_are_found_by_plugin_and_caps() {
    let factories = [
        factory("mfh264enc", "other", HW_KLASS, gst::Rank::SECONDARY, H264),
        mf("mfh265enc", HW_KLASS, gst::Rank::SECONDARY, "video/x-h265"),
    ];
    assert_eq!(choose_h264(&factories), None);

    let factories = [mf("somemftenc", HW_KLASS, gst::Rank::SECONDARY, H264)];
    assert_eq!(choose_h264(&factories).as_deref(), Some("somemftenc"));
}

/// The real registry: a named candidate resolves through `registry_snapshot`.
#[test]
fn registry_snapshot_reads_factory_metadata() {
    init_gst();
    if !require_element("x264enc") {
        return;
    }
    let snapshot = registry_snapshot(&[
        Candidate::Named("x264enc"),
        Candidate::Plugin(MEDIA_FOUNDATION_PLUGIN),
    ]);
    let x264 = snapshot
        .iter()
        .find(|f| f.name == "x264enc")
        .expect("x264enc in the snapshot");
    assert_eq!(x264.plugin.as_deref(), Some("x264"));
    assert!(!x264.is_hardware());
    // The profile list is read from the registry's template caps
    let h264 = x264
        .src_formats
        .iter()
        .find(|f| f.media_type == H264)
        .expect("x264enc produces H.264");
    let profiles = h264.profiles.as_ref().expect("x264enc lists its profiles");
    assert!(profiles.iter().any(|p| p == "high"), "{profiles:?}");
    assert!(
        profiles.iter().any(|p| p == "constrained-baseline"),
        "{profiles:?}"
    );
    assert_eq!(
        choose_encoder(
            &[Candidate::Named("x264enc")],
            &h264_output(Profile::Auto),
            &snapshot
        )
        .as_deref(),
        Some("x264enc")
    );
}

#[test]
fn media_foundation_is_identified_by_plugin() {
    assert!(is_media_foundation_plugin(Some("mediafoundation")));
    assert!(!is_media_foundation_plugin(Some("x264")));
    assert!(!is_media_foundation_plugin(None));
}

// --- Media Foundation property mapping ----------------------------------------

/// `rc-mode` nicks per codec, from GStreamer's `sys/mediafoundation`.
const MF_H264_RC: [&str; 4] = ["cbr", "pcvbr", "uvbr", "qvbr"];
const MF_H265_RC: [&str; 2] = ["cbr", "qvbr"];

#[test]
fn media_foundation_rate_control_mapping() {
    for nicks in [&MF_H264_RC[..], &MF_H265_RC[..]] {
        assert_eq!(mf_rc_mode_nick(RateControl::CBR, nicks), Some("cbr"));
        assert_eq!(mf_rc_mode_nick(RateControl::CQP, nicks), Some("qvbr"));
    }
    // VBR: peak-constrained on H.264; H.265/VP9 have no pcvbr and use cbr
    assert_eq!(
        mf_rc_mode_nick(RateControl::VBR, &MF_H264_RC),
        Some("pcvbr")
    );
    assert_eq!(mf_rc_mode_nick(RateControl::VBR, &MF_H265_RC), Some("cbr"));
    assert_eq!(mf_rc_mode_nick(RateControl::CBR, &[]), None);
    assert_eq!(mf_rc_mode_nick(RateControl::CQP, &["cbr"]), None);
}

#[test]
fn media_foundation_quality_preset_mapping() {
    assert_eq!(map_quality_preset_mf("ultrafast"), 0);
    assert_eq!(map_quality_preset_mf("fast"), 33);
    assert_eq!(map_quality_preset_mf("medium"), 50);
    assert_eq!(map_quality_preset_mf("slow"), 67);
    assert_eq!(map_quality_preset_mf("veryslow"), 100);
    assert_eq!(map_quality_preset_mf("unknown"), 50);
}

/// The plugin multiplies `bitrate` by 1024; the block's kbps is 1000 bits.
#[test]
fn media_foundation_bitrate_is_converted_to_1024_bit_kbit() {
    assert_eq!(mf_kbit_from_kbps(4000), 3906); // 3906.25
    assert_eq!(mf_kbit_from_kbps(1024), 1000);
    assert_eq!(mf_kbit_from_kbps(100), 98); // 97.66
    assert_eq!(mf_kbit_from_kbps(0), 0); // clamped to the range (min 1) when set
}

/// The shared burst policy, in explicit units, and what NVENC gets from it:
/// the same values as before it was factored out.
#[test]
fn burst_limits_units() {
    let limits = BurstLimits::for_bitrate_kbps(4000);
    assert_eq!(limits.peak_bits_per_sec, 4_800_000);
    assert_eq!(limits.vbv_bits, 2_000_000);
    // NVENC: max-bitrate kbit/s, vbv-buffer-size kbits
    assert_eq!(limits.peak_bits_per_sec / 1000, 4000 * 12 / 10);
    assert_eq!(limits.vbv_bits / 1000, 4000 / 2);
    // Media Foundation: vbv-buffer-size bytes
    assert_eq!(limits.vbv_bits / 8, 250_000);
}

fn settings_map(settings: &[MfSetting]) -> HashMap<&'static str, MfValue> {
    settings.iter().map(|s| (s.name, s.value)).collect()
}

fn derived(value: u64) -> MfValue {
    MfValue::Number { value, clamp: true }
}

fn fixed(value: u64) -> MfValue {
    MfValue::Number {
        value,
        clamp: false,
    }
}

/// An `mfh264enc` with every optional property: what each block property
/// becomes, in the element's units.
#[test]
fn media_foundation_h264_property_mapping() {
    let all = |_: &str| true;
    let map = settings_map(&mf_encoder_settings(
        all,
        &MF_H264_RC,
        4000,
        "slow",
        "zerolatency",
        RateControl::VBR,
    ));
    assert_eq!(map.get("bitrate"), Some(&derived(3906)), "1024-bit kbit/s");
    assert_eq!(map.get("rc-mode"), Some(&MfValue::Nick("pcvbr")));
    // 1.2 x 4000 kbps = 4_800_000 bits/s = 4687.5 1024-bit kbit/s
    assert_eq!(map.get("max-bitrate"), Some(&derived(4688)));
    assert_eq!(map.get("vbv-buffer-size"), Some(&derived(250_000)), "bytes");
    assert_eq!(map.get("quality-vs-speed"), Some(&fixed(67)));
    assert_eq!(map.get("low-latency"), Some(&MfValue::Bool(true)));
    assert_eq!(map.get("bframes"), Some(&fixed(0)));

    let map = settings_map(&mf_encoder_settings(
        all,
        &MF_H264_RC,
        4000,
        "medium",
        "film",
        RateControl::CBR,
    ));
    assert_eq!(map.get("rc-mode"), Some(&MfValue::Nick("cbr")));
    assert!(map.contains_key("vbv-buffer-size"));
    assert_eq!(map.get("low-latency"), Some(&MfValue::Bool(false)));
}

/// CQP never gets a VBV bound, whatever rc-mode the element resolves to:
/// `qvbr`, no `rc-mode` property, or no matching nick.
#[test]
fn media_foundation_cqp_never_sets_vbv() {
    let all = |_: &str| true;
    let no_rc = |name: &str| name != "rc-mode";
    for (has, nicks) in [
        (&all as &dyn Fn(&str) -> bool, &MF_H264_RC[..]),
        (&no_rc, &[][..]),
        (&all, &["cbr"][..]),
    ] {
        let map = settings_map(&mf_encoder_settings(
            has,
            nicks,
            4000,
            "medium",
            "zerolatency",
            RateControl::CQP,
        ));
        assert!(!map.contains_key("vbv-buffer-size"), "{nicks:?}");
    }
}

/// The plugin installs most properties only when the MFT supports them. An
/// MFT with only `bitrate` gets only `bitrate`, and nothing it lacks.
#[test]
fn media_foundation_settings_skip_properties_the_mft_lacks() {
    let only_bitrate = |name: &str| name == "bitrate";
    let map = settings_map(&mf_encoder_settings(
        only_bitrate,
        &[],
        4000,
        "slow",
        "zerolatency",
        RateControl::VBR,
    ));
    assert_eq!(map, HashMap::from([("bitrate", derived(3906))]));

    // VP9: no vbv-buffer-size, no bframes
    let vp9 = |name: &str| {
        [
            "bitrate",
            "rc-mode",
            "max-bitrate",
            "quality-vs-speed",
            "gop-size",
            "low-latency",
        ]
        .contains(&name)
    };
    let map = settings_map(&mf_encoder_settings(
        vp9,
        &MF_H265_RC,
        4000,
        "medium",
        "zerolatency",
        RateControl::VBR,
    ));
    assert_eq!(map.get("rc-mode"), Some(&MfValue::Nick("cbr")));
    assert!(!map.contains_key("vbv-buffer-size"));
    assert!(!map.contains_key("bframes"));
    assert_eq!(map.get("quality-vs-speed"), Some(&fixed(50)));
}

/// Media Foundation takes the keyframe interval as `gop-size`.
#[test]
fn keyframe_interval_goes_to_gop_size_on_media_foundation() {
    let mf = |name: &str| ["bitrate", "gop-size", "low-latency"].contains(&name);
    assert_eq!(keyframe_interval_property(mf), Some("gop-size"));
    let x264 = |name: &str| ["bitrate", "key-int-max"].contains(&name);
    assert_eq!(keyframe_interval_property(x264), Some("key-int-max"));
    assert_eq!(keyframe_interval_property(|_: &str| false), None);
}

/// The Media Foundation settings apply to a real element without failing on
/// the properties it lacks. `x264enc` stands in: it has `bitrate` and
/// `bframes`, and none of `rc-mode`, `quality-vs-speed`, `low-latency` or
/// `vbv-buffer-size`.
#[test]
fn media_foundation_settings_skip_properties_the_element_lacks() {
    init_gst();
    if !require_element("x264enc") {
        return;
    }
    let encoder = gst::ElementFactory::make("x264enc")
        .build()
        .expect("Should create x264enc");
    for name in [
        "rc-mode",
        "quality-vs-speed",
        "low-latency",
        "vbv-buffer-size",
    ] {
        assert!(
            !encoder.has_property(name),
            "the stand-in must lack {} for this test to mean anything",
            name
        );
    }
    set_mf_encoder_properties(
        &encoder,
        "mfh264enc",
        4000,
        "medium",
        "zerolatency",
        RateControl::VBR,
    )
    .expect("a property the element lacks must be skipped, not fail the build");
    let bitrate: u32 = encoder.property("bitrate");
    assert_eq!(bitrate, 3906, "converted to 1024-bit kbit/s");
}

/// The Media Foundation branch is chosen by plugin, not name: an `x264enc`
/// handed in under a Media Foundation name does not get the MF settings
/// (which would set its bitrate to the 1024-bit conversion).
#[test]
fn media_foundation_branch_is_chosen_by_plugin_not_name() {
    init_gst();
    if !require_element("x264enc") {
        return;
    }
    let encoder = gst::ElementFactory::make("x264enc")
        .build()
        .expect("Should create x264enc");
    set_encoder_properties(
        &encoder,
        "mfh264enc",
        4000,
        "medium",
        "zerolatency",
        RateControl::VBR,
        60,
    )
    .expect("settings apply");
    let bitrate: u32 = encoder.property("bitrate");
    assert_ne!(bitrate, 3906, "the MF branch ran for a non-MF element");
}

/// CQP (`qvbr`) sets the quality target: `qp` where the MFT has it, else
/// `quality` (the plugin installs only one). Other modes set neither.
#[test]
fn media_foundation_cqp_sets_the_quality_target() {
    let with_qp = |name: &str| ["bitrate", "rc-mode", "qp"].contains(&name);
    let map = settings_map(&mf_encoder_settings(
        with_qp,
        &MF_H264_RC,
        4000,
        "medium",
        "zerolatency",
        RateControl::CQP,
    ));
    assert_eq!(map.get("rc-mode"), Some(&MfValue::Nick("qvbr")));
    assert_eq!(map.get("qp"), Some(&fixed(MF_CQP_QP)));
    assert!(!map.contains_key("quality"));
    assert!(
        map.contains_key("bitrate"),
        "kept: written to the media type"
    );

    let with_quality = |name: &str| ["bitrate", "rc-mode", "quality"].contains(&name);
    let map = settings_map(&mf_encoder_settings(
        with_quality,
        &MF_H264_RC,
        4000,
        "medium",
        "zerolatency",
        RateControl::CQP,
    ));
    assert_eq!(map.get("quality"), Some(&fixed(MF_CQP_QUALITY)));
    assert!(!map.contains_key("qp"));

    let all = |_: &str| true;
    for rc in [RateControl::CBR, RateControl::VBR] {
        let map = settings_map(&mf_encoder_settings(
            all,
            &MF_H264_RC,
            4000,
            "medium",
            "zerolatency",
            rc,
        ));
        assert!(
            !map.contains_key("qp") && !map.contains_key("quality"),
            "{rc:?}"
        );
    }
}

/// VBR on H.265/VP9 (no `pcvbr`) is `cbr` with no peak cap; with the VBR
/// mode set to `cbr`, H.264 gets `cbr` and no cap either. `max-bitrate`
/// follows `pcvbr` only.
#[test]
fn media_foundation_max_bitrate_follows_pcvbr_only() {
    assert_eq!(MF_VBR_RC_MODE, "pcvbr");
    assert_eq!(mf_max_bitrate(1000), 1172); // 1_200_000 / 1024
    let all = |_: &str| true;
    let hevc = settings_map(&mf_encoder_settings(
        all,
        &MF_H265_RC,
        4000,
        "medium",
        "zerolatency",
        RateControl::VBR,
    ));
    assert_eq!(hevc.get("rc-mode"), Some(&MfValue::Nick("cbr")));
    assert!(!hevc.contains_key("max-bitrate"));

    let h264_cbr = settings_map(&mf_encoder_settings_with(
        all,
        &MF_H264_RC,
        4000,
        "medium",
        "zerolatency",
        RateControl::VBR,
        "cbr",
    ));
    assert_eq!(h264_cbr.get("rc-mode"), Some(&MfValue::Nick("cbr")));
    assert!(!h264_cbr.contains_key("max-bitrate"));
    assert_eq!(
        mf_rc_mode_nick_with(RateControl::VBR, &MF_H264_RC, "cbr"),
        Some("cbr")
    );
}

/// Run `videotestsrc` through the real block into a `fakesink` and return the
/// caps on the block's output. `videotestsrc` negotiates `Y444` when the
/// encoder accepts it, which is the 4:4:4 input of #1176.
fn encode_test_pattern(codec: &str, profile: &str) -> gst::Caps {
    let mut properties = HashMap::new();
    properties.insert("codec".to_string(), PropertyValue::String(codec.into()));
    properties.insert(
        "encoder_preference".to_string(),
        PropertyValue::String("software".into()),
    );
    properties.insert("profile".to_string(), PropertyValue::String(profile.into()));
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = VideoEncBuilder
        .build("enc", &properties, &ctx)
        .expect("build the block");

    let pipeline = gst::Pipeline::new();
    let by_id: HashMap<&str, &gst::Element> = result
        .elements
        .iter()
        .map(|(id, element)| (id.as_str(), element))
        .collect();
    for element in by_id.values() {
        pipeline.add(*element).expect("add element");
    }
    for (from, to) in &result.internal_links {
        let src = by_id[from.element_id.as_str()]
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("source pad");
        let sink = by_id[to.element_id.as_str()]
            .static_pad(to.pad_name.as_deref().unwrap_or("sink"))
            .expect("sink pad");
        src.link(&sink).expect("internal link");
    }
    let source = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 3)
        .build()
        .unwrap();
    let source_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320)
                .field("height", 240)
                .build(),
        )
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink").build().unwrap();
    pipeline.add_many([&source, &source_caps, &sink]).unwrap();
    gst::Element::link_many([&source, &source_caps, by_id["enc:input"]]).unwrap();
    by_id["enc:capsfilter"].link(&sink).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("EOS or error within 30 s");
    if let gst::MessageView::Error(err) = message.view() {
        pipeline.set_state(gst::State::Null).unwrap();
        panic!(
            "{} pipeline failed: {} ({:?})",
            codec,
            err.error(),
            err.debug()
        );
    }
    let caps = by_id["enc:capsfilter"]
        .static_pad("src")
        .unwrap()
        .current_caps()
        .expect("output caps");
    pipeline.set_state(gst::State::Null).unwrap();
    caps
}

/// #1176: with the default profile, a 4:4:4 input into VP9 comes out as
/// profile 0, which WHIP Output and WebRTC receivers take. Fails if the
/// 4:2:0 input constraint in front of the encoder is removed (profile 1).
#[test]
fn vp9_from_444_input_is_profile_0_by_default() {
    init_gst();
    if !require_element("vp9enc") || !require_element("vp9parse") {
        return;
    }
    let caps = encode_test_pattern("vp9", "auto");
    let profile = caps.structure(0).unwrap().get::<String>("profile");
    assert_eq!(profile.as_deref(), Ok("0"), "output caps {caps}");
}

/// The counterpart of the test above: with profile "none" the encoder still
/// follows the input, so the test pattern really is 4:4:4 there.
#[test]
fn vp9_profile_none_follows_the_input() {
    init_gst();
    if !require_element("vp9enc") || !require_element("vp9parse") {
        return;
    }
    let caps = encode_test_pattern("vp9", "none");
    let profile = caps.structure(0).unwrap().get::<String>("profile");
    assert_ne!(profile.as_deref(), Ok("0"), "output caps {caps}");
}

/// The input constraint keeps only 8-bit 4:2:0 in system memory, and leaves
/// GPU-memory structures of the encoder's template alone (#1104). The
/// template has the shape the hardware encoders register.
#[test]
fn base_profile_constraint_keeps_gpu_memory() {
    init_gst();
    let template: gst::Caps =
        "video/x-raw(memory:CUDAMemory), format=(string){ NV12, P010_10LE, Y444 }; \
         video/x-raw, format=(string){ NV12, P010_10LE, Y444 }"
            .parse()
            .unwrap();
    let caps = constrain_to_base_profile(&template).expect("NV12 is 4:2:0");
    assert_eq!(caps.size(), 2, "{caps}");
    assert!(
        caps.features(0).unwrap().contains("memory:CUDAMemory"),
        "{caps}"
    );
    assert_eq!(caps.structure(0), template.structure(0), "{caps}");
    let formats: Vec<String> = caps
        .structure(1)
        .unwrap()
        .get::<gst::List>("format")
        .unwrap()
        .iter()
        .map(|v| v.get::<String>().unwrap())
        .collect();
    assert_eq!(formats, ["NV12"], "{caps}");
}

/// An encoder with no 8-bit 4:2:0 input in system memory gets no constraint,
/// rather than a filter that can never negotiate.
#[test]
fn base_profile_constraint_skips_encoders_without_420() {
    init_gst();
    let template: gst::Caps = "video/x-raw, format=(string){ Y444, P010_10LE }"
        .parse()
        .unwrap();
    assert!(constrain_to_base_profile(&template).is_none());
}

/// libaom's `av1enc` takes `Y444` as well, so AV1 needs the same constraint;
/// without it a 4:4:4 input gives AV1 High.
#[test]
fn av1enc_input_is_limited_to_420() {
    init_gst();
    if !is_encoder_available("av1enc") {
        println!("av1enc not available, skipping test");
        return;
    }
    let encoder = gst::ElementFactory::make("av1enc").build().unwrap();
    let caps =
        base_profile_input_caps(Codec::AV1, Profile::Auto, &encoder).expect("av1enc takes I420");
    assert!(!caps.to_string().contains("Y444"), "{caps}");
    assert!(base_profile_input_caps(Codec::AV1, Profile::None, &encoder).is_none());
}

/// The profile list offers each named profile only for its own codec, and
/// the two policies (auto, none) for every codec. The property itself stays
/// visible for every codec: on VP9 and AV1 "none" lifts the 8-bit 4:2:0
/// input limit that "auto" applies.
#[test]
fn profile_options_carry_their_codec_condition() {
    let def = videoenc_definition();
    let profile = def
        .exposed_properties
        .iter()
        .find(|p| p.name == "profile")
        .expect("profile property");
    assert!(profile.visible_when.is_none());
    let PropertyType::Enum { values } = &profile.property_type else {
        panic!("profile is not an enum");
    };
    let codec_of = |value: &str| -> Option<Vec<String>> {
        let ev = values.iter().find(|v| v.value == value).unwrap();
        ev.visible_when.as_ref().map(|c| {
            assert_eq!(c.property, "codec");
            c.one_of
                .iter()
                .map(|v| match v {
                    PropertyValue::String(s) => s.clone(),
                    other => panic!("{other:?}"),
                })
                .collect()
        })
    };
    assert_eq!(codec_of("auto"), None);
    assert_eq!(codec_of("none"), None);
    assert_eq!(codec_of("high"), Some(vec!["h264".to_string()]));
    assert_eq!(codec_of("high-4:4:4"), Some(vec!["h264".to_string()]));
    assert_eq!(
        codec_of("main"),
        Some(vec!["h264".to_string(), "h265".to_string()])
    );
    assert_eq!(codec_of("main-422-10"), Some(vec!["h265".to_string()]));
    // Every condition names a codec the codec property offers.
    let codec = def
        .exposed_properties
        .iter()
        .find(|p| p.name == "codec")
        .unwrap();
    let PropertyType::Enum { values: codecs } = &codec.property_type else {
        panic!("codec is not an enum");
    };
    for v in values {
        for c in codec_of(&v.value).unwrap_or_default() {
            assert!(codecs.iter().any(|ev| ev.value == c), "{c}");
        }
    }

    // Offered per codec, as the UI filters them.
    let offered = |codec: &str| -> Vec<String> {
        let props = HashMap::from([(
            "codec".to_string(),
            PropertyValue::String(codec.to_string()),
        )]);
        values
            .iter()
            .filter(|v| v.is_visible(&props, &def.exposed_properties))
            .map(|v| v.value.clone())
            .collect()
    };
    assert_eq!(offered("vp9"), ["auto", "none"]);
    assert!(offered("h264").contains(&"high".to_string()));
    assert!(!offered("h264").contains(&"main-10".to_string()));
    assert!(offered("h265").contains(&"main-10".to_string()));
    assert!(!offered("h265").contains(&"high".to_string()));
}
