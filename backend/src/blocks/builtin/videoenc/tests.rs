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

/// Initialize GStreamer and GPU detection for tests
fn init_gst() {
    let _ = gst::init();
    // Initialize GPU detection (required for video conversion mode)
    crate::gpu::detect_gpu_capabilities();
}

/// Check if an encoder is available on the system
fn is_encoder_available(encoder_name: &str) -> bool {
    let registry = gst::Registry::get();
    registry
        .find_feature(encoder_name, gst::ElementFactory::static_type())
        .is_some()
}

/// Get list of available encoders from a list
fn get_available_encoders(encoder_names: &[&str]) -> Vec<String> {
    encoder_names
        .iter()
        .filter(|name| is_encoder_available(name))
        .map(|s| s.to_string())
        .collect()
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
        encoders.contains(&"nvautogpuh264enc"),
        "Should include nvautogpuh264enc"
    );
    assert!(encoders.contains(&"nvh264enc"), "Should include nvh264enc");
    assert!(
        encoders.contains(&"qsvh264enc"),
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
    let nv_pos = h264_hw.iter().position(|&e| e == "nvautogpuh264enc");
    let qsv_pos = h264_hw.iter().position(|&e| e == "qsvh264enc");

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
    let result = select_encoder(Codec::H264, EncoderPreference::SoftwareOnly);

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
fn test_encoder_selection_with_fallback() {
    init_gst();

    // Auto mode with fallback should find something
    let result = select_encoder(Codec::H264, EncoderPreference::Auto);

    assert!(
        result.is_ok(),
        "Should find at least one H.264 encoder with fallback enabled"
    );

    if let Ok(encoder) = result {
        println!("Selected encoder: {}", encoder);
    }
}

#[test]
fn test_get_codec_caps_string() {
    assert_eq!(
        get_codec_caps_string(Codec::H264),
        "video/x-h264,alignment=au"
    );
    assert_eq!(
        get_codec_caps_string(Codec::H265),
        "video/x-h265,alignment=au"
    );
    assert_eq!(get_codec_caps_string(Codec::AV1), "video/x-av1");
    assert_eq!(get_codec_caps_string(Codec::VP9), "video/x-vp9");
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
    );

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
        );

        // Verify bitrate
        let bitrate: u32 = encoder.property("bitrate");
        assert_eq!(bitrate, 4000, "Bitrate should be set for {}", encoder_name);

        // Verify rate control property exists and is set correctly
        let rc_property = if encoder_name.starts_with("nvautogpu") {
            "rate-control"
        } else {
            "rc-mode"
        };

        assert!(
            encoder.has_property(rc_property),
            "{} should have {} property",
            encoder_name,
            rc_property
        );
    }
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
        );

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
            );

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
        );

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

/// Test all available encoders on the system
#[test]
fn test_all_available_encoders() {
    init_gst();

    let all_encoders = vec![
        // H.264
        "nvautogpuh264enc",
        "nvh264enc",
        "qsvh264enc",
        "vah264enc",
        "amfh264enc",
        "x264enc",
        // H.265
        "nvautogpuh265enc",
        "nvh265enc",
        "qsvh265enc",
        "vah265enc",
        "amfh265enc",
        "x265enc",
        // AV1
        "nvautogpuav1enc",
        "nvav1enc",
        "qsvav1enc",
        "vaav1enc",
        "amfav1enc",
        "svtav1enc",
        "av1enc",
        // VP9
        "qsvvp9enc",
        "vavp9enc",
        "vp9enc",
    ];

    let available = get_available_encoders(&all_encoders);

    println!("Available encoders on this system:");
    for encoder in &available {
        println!("  - {}", encoder);
    }

    // Test that we can create each available encoder
    for encoder_name in &available {
        let encoder = gst::ElementFactory::make(encoder_name)
            .build()
            .unwrap_or_else(|_| panic!("Should create {}", encoder_name));

        // Test basic property setting
        set_encoder_properties(
            &encoder,
            encoder_name,
            4000,
            "medium",
            "zerolatency",
            RateControl::VBR,
            60,
        );

        println!("✓ {} configured successfully", encoder_name);
    }

    assert!(
        !available.is_empty(),
        "At least one encoder should be available on any system"
    );
}

/// Integration test: Build a complete video encoder block
#[test]
fn test_build_video_encoder_block() {
    init_gst();

    let builder = VideoEncBuilder;
    let mut properties = HashMap::new();

    // Use default codec (h264)
    properties.insert(
        "quality_preset".to_string(),
        PropertyValue::String("fast".to_string()),
    );
    properties.insert("bitrate".to_string(), PropertyValue::UInt(5000));

    let ctx = BlockBuildContext::new();
    let result = builder.build("test_block", &properties, &ctx);

    match result {
        Ok(block_result) => {
            assert_eq!(
                block_result.elements.len(),
                4,
                "Should create 4 elements (videoconvert, encoder, parser, capsfilter)"
            );
            assert_eq!(
                block_result.internal_links.len(),
                3,
                "Should create 3 internal links"
            );
            println!("✓ Video encoder block built successfully");

            // Verify elements have correct names
            let element_ids: Vec<_> = block_result.elements.iter().map(|(id, _)| id).collect();
            assert!(element_ids.iter().any(|id| id.contains("videoconvert")));
            assert!(element_ids.iter().any(|id| id.contains("encoder")));
            assert!(element_ids.iter().any(|id| id.contains("parser")));
            assert!(element_ids.iter().any(|id| id.contains("capsfilter")));
        }
        Err(e) => {
            panic!("Failed to build video encoder block: {:?}", e);
        }
    }
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

    let ctx = BlockBuildContext::new();
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
