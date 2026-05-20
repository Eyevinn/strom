use super::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::PropertyType, PropertyValue};

fn init_gst() {
    let _ = gst::init();
    let _ = gst_plugins_lsp::plugin_register_static();
}

fn is_element_available(name: &str) -> bool {
    gst::ElementFactory::make(name).build().is_ok()
}

// ---- Pure function tests (no GStreamer needed) ----

#[test]
fn test_db_to_linear_unity() {
    let result = db_to_linear(0.0);
    assert!((result - 1.0).abs() < 1e-10, "0 dB should be 1.0 linear");
}

#[test]
fn test_db_to_linear_minus_6() {
    let result = db_to_linear(-6.0);
    assert!(
        (result - 0.5012).abs() < 0.001,
        "-6 dB should be ~0.501, got {}",
        result
    );
}

#[test]
fn test_db_to_linear_minus_20() {
    let result = db_to_linear(-20.0);
    assert!(
        (result - 0.1).abs() < 1e-10,
        "-20 dB should be 0.1, got {}",
        result
    );
}

#[test]
fn test_db_to_linear_minus_60() {
    let result = db_to_linear(-60.0);
    assert!(
        (result - 0.001).abs() < 1e-10,
        "-60 dB should be 0.001, got {}",
        result
    );
}

#[test]
fn test_db_to_linear_plus_6() {
    let result = db_to_linear(6.0);
    assert!(
        (result - 1.9953).abs() < 0.001,
        "+6 dB should be ~1.995, got {}",
        result
    );
}

#[test]
fn test_parse_num_channels_default() {
    let props = HashMap::new();
    assert_eq!(parse_num_channels(&props), DEFAULT_CHANNELS);
}

#[test]
fn test_parse_num_channels_from_string() {
    let mut props = HashMap::new();
    props.insert(
        "num_channels".to_string(),
        PropertyValue::String("4".to_string()),
    );
    assert_eq!(parse_num_channels(&props), 4);
}

#[test]
fn test_parse_num_channels_clamped() {
    let mut props = HashMap::new();
    props.insert(
        "num_channels".to_string(),
        PropertyValue::String("9999".to_string()),
    );
    assert_eq!(parse_num_channels(&props), MAX_CHANNELS);

    props.insert(
        "num_channels".to_string(),
        PropertyValue::String("0".to_string()),
    );
    assert_eq!(parse_num_channels(&props), 1);
}

#[test]
fn test_parse_num_aux_buses_default() {
    let props = HashMap::new();
    assert_eq!(parse_num_aux_buses(&props), 0);
}

#[test]
fn test_parse_num_aux_buses_clamped() {
    let mut props = HashMap::new();
    props.insert(
        "num_aux_buses".to_string(),
        PropertyValue::String("999".to_string()),
    );
    assert_eq!(parse_num_aux_buses(&props), MAX_AUX_BUSES);
}

#[test]
fn test_parse_num_groups_default() {
    let props = HashMap::new();
    assert_eq!(parse_num_groups(&props), 0);
}

#[test]
fn test_get_float_prop_default() {
    let props = HashMap::new();
    assert_eq!(get_float_prop(&props, "volume", 0.5), 0.5);
}

#[test]
fn test_get_float_prop_value() {
    let mut props = HashMap::new();
    props.insert("volume".to_string(), PropertyValue::Float(0.75));
    assert_eq!(get_float_prop(&props, "volume", 0.5), 0.75);
}

#[test]
fn test_get_float_prop_from_int() {
    let mut props = HashMap::new();
    props.insert("volume".to_string(), PropertyValue::Int(3));
    assert_eq!(get_float_prop(&props, "volume", 0.5), 3.0);
}

#[test]
fn test_get_bool_prop_default() {
    let props = HashMap::new();
    assert!(!get_bool_prop(&props, "mute", false));
    assert!(get_bool_prop(&props, "mute", true));
}

#[test]
fn test_get_bool_prop_value() {
    let mut props = HashMap::new();
    props.insert("mute".to_string(), PropertyValue::Bool(true));
    assert!(get_bool_prop(&props, "mute", false));
}

#[test]
fn test_get_string_prop_default() {
    let props = HashMap::new();
    assert_eq!(get_string_prop(&props, "mode", "pfl"), "pfl");
}

#[test]
fn test_get_string_prop_value() {
    let mut props = HashMap::new();
    props.insert("mode".to_string(), PropertyValue::String("afl".to_string()));
    assert_eq!(get_string_prop(&props, "mode", "pfl"), "afl");
}

#[test]
fn test_comp_knee_db_to_linear_in_range() {
    // Default knee -6 dB should map to ~0.5 (within LSP range 0.0631..1.0)
    let kn = db_to_linear(-6.0).clamp(0.0631, 1.0);
    assert!(kn > 0.49 && kn < 0.52, "Knee -6dB = {}, expected ~0.5", kn);

    // 0 dB should map to 1.0 (max)
    let kn = db_to_linear(0.0).clamp(0.0631, 1.0);
    assert!((kn - 1.0).abs() < 1e-6, "Knee 0dB = {}, expected 1.0", kn);

    // -24 dB should map to ~0.063 (near min)
    let kn = db_to_linear(-24.0).clamp(0.0631, 1.0);
    assert!(kn >= 0.0631, "Knee -24dB = {}, should be >= 0.0631", kn);

    // +6 dB would exceed max, should clamp to 1.0
    let kn = db_to_linear(6.0).clamp(0.0631, 1.0);
    assert!((kn - 1.0).abs() < 1e-6, "Knee +6dB should clamp to 1.0");
}

// ---- Property mapping tests ----

#[test]
fn test_mixer_definition_has_bypass_mappings() {
    let def = mixer_definition();
    let bypass_props = [
        "main_comp_enabled",
        "main_eq_enabled",
        "main_limiter_enabled",
    ];
    for prop_name in &bypass_props {
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == *prop_name)
            .unwrap_or_else(|| panic!("Missing property: {}", prop_name));
        assert_eq!(
            prop.mapping.property_name, "enabled",
            "{} should map to 'enabled', got '{}'",
            prop_name, prop.mapping.property_name
        );
        assert_eq!(
            prop.mapping.transform, None,
            "{} should have no transform",
            prop_name
        );
    }
}

#[test]
fn test_mixer_definition_channel_bypass_mappings() {
    let def = mixer_definition();
    // Check that per-channel gate/comp/eq enabled properties map to bypass
    for suffix in &["gate_enabled", "comp_enabled", "eq_enabled"] {
        let prop_name = format!("ch1_{}", suffix);
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == prop_name)
            .unwrap_or_else(|| panic!("Missing property: {}", prop_name));
        assert_eq!(
            prop.mapping.property_name, "enabled",
            "{} should map to 'enabled', got '{}'",
            prop_name, prop.mapping.property_name
        );
        assert_eq!(
            prop.mapping.transform, None,
            "{} should have no transform",
            prop_name
        );
    }
}

#[test]
fn test_mixer_definition_no_gate_range_property() {
    let def = mixer_definition();
    // There should be no gate range exposed property (LSP doesn't support it)
    let gate_range = def
        .exposed_properties
        .iter()
        .find(|p| p.name.contains("gate_range"));
    assert!(
        gate_range.is_none(),
        "Gate range property should not exist (LSP has no settable range)"
    );
}

#[test]
fn test_mixer_definition_comp_knee_defaults() {
    let def = mixer_definition();
    let knee = def
        .exposed_properties
        .iter()
        .find(|p| p.name == "ch1_comp_knee")
        .expect("Missing ch1_comp_knee");
    match &knee.default_value {
        Some(PropertyValue::Float(v)) => assert!(
            (*v - (-6.0)).abs() < 1e-6,
            "Knee default should be -6.0 dB, got {}",
            v
        ),
        other => panic!("Knee default should be Float(-6.0), got {:?}", other),
    }
    assert_eq!(
        knee.mapping.transform,
        Some("db_to_linear".to_string()),
        "Knee should have db_to_linear transform"
    );
}

#[test]
fn test_mixer_definition_db_to_linear_transforms() {
    let def = mixer_definition();
    // Properties that should have db_to_linear transform
    let db_props = [
        "ch1_gate_threshold",
        "ch1_comp_threshold",
        "ch1_comp_makeup",
        "ch1_comp_knee",
        "main_comp_threshold",
        "main_comp_makeup",
    ];
    for prop_name in &db_props {
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == *prop_name)
            .unwrap_or_else(|| panic!("Missing property: {}", prop_name));
        assert_eq!(
            prop.mapping.transform,
            Some("db_to_linear".to_string()),
            "{} should have 'db_to_linear' transform, got {:?}",
            prop_name,
            prop.mapping.transform
        );
    }
}

#[test]
fn test_mixer_definition_channel_count() {
    let def = mixer_definition();
    // Default is 8 channels, should have properties for ch1..ch8
    let ch8_fader = def
        .exposed_properties
        .iter()
        .find(|p| p.name == "ch8_fader");
    assert!(
        ch8_fader.is_some(),
        "Should have ch8_fader for default 8 channels"
    );
}

#[test]
fn test_mixer_definition_aux_group_outputs() {
    let def = mixer_definition();
    // Should have main, Monitor, aux, and group output pads
    let pads = &def.external_pads;
    assert!(
        pads.outputs.iter().any(|p| p.name == "main_out"),
        "Should have main_out pad"
    );
    assert!(
        pads.outputs.iter().any(|p| p.name == "monitor_out"),
        "Should have monitor_out pad"
    );
}

#[test]
fn test_mixer_pfl_afl_are_transient() {
    // Solo state must not persist across pipeline restarts — see the
    // persist:false guard in state.rs::strip_transient_properties.
    let def = mixer_definition();
    for ch in 1..=4usize {
        for kind in ["pfl", "afl"] {
            let name = format!("ch{}_{}", ch, kind);
            let prop = def
                .exposed_properties
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("missing {}", name));
            assert!(prop.live, "{} should be live", name);
            assert_eq!(
                prop.persist,
                Some(false),
                "{} must be marked persist: Some(false)",
                name
            );
        }
    }
}

#[test]
fn test_parse_solo_property_name_matches_pfl_and_afl() {
    use super::parse_solo_property_name;
    assert_eq!(parse_solo_property_name("ch1_pfl"), Some(1));
    assert_eq!(parse_solo_property_name("ch12_afl"), Some(12));
    assert_eq!(parse_solo_property_name("ch1_mute"), None);
    assert_eq!(parse_solo_property_name("main_fader"), None);
    assert_eq!(parse_solo_property_name("ch_pfl"), None);
    assert_eq!(parse_solo_property_name("chA_pfl"), None);
}

#[test]
fn test_mixer_mute_maps_to_gstvolume_mute_property() {
    // Mute is implemented via GstVolume's native `mute` property, not the
    // legacy "volume = 0 if muted" trick. The mapping must point at the
    // corresponding volume element with property_name == "mute" so the new
    // block-properties endpoint can write through.
    let def = mixer_definition();
    let cases: &[(&str, &str)] = &[
        ("main_mute", "main_volume"),
        ("ch1_mute", "volume_0"),
        ("ch3_mute", "volume_2"),
        ("group1_mute", "group0_volume"),
        ("aux1_mute", "aux0_volume"),
    ];
    for (name, expected_element) in cases {
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == *name)
            .unwrap_or_else(|| panic!("missing {}", name));
        assert_eq!(
            prop.mapping.element_id, *expected_element,
            "{} should map to {}",
            name, expected_element
        );
        assert_eq!(
            prop.mapping.property_name, "mute",
            "{} should target GstVolume.mute",
            name
        );
        assert!(
            prop.mapping.transform.is_none(),
            "{} needs no transform",
            name
        );
        assert!(prop.live, "{} must be live", name);
    }
}

#[test]
fn test_mixer_config_properties_not_live() {
    // Construction-time block parameters cannot be live-applied: they decide
    // how the builder wires up the pipeline. The block-property endpoint
    // depends on this flag to give a meaningful error message instead of
    // silently failing in the `_block` branch.
    let def = mixer_definition();
    for name in ["num_channels", "dsp_backend", "num_aux_buses", "num_groups"] {
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("missing {}", name));
        assert!(!prop.live, "{} must NOT be marked live: true", name);
    }
}

// ---- GStreamer element tests (conditional on plugin availability) ----

#[test]
fn test_make_gate_element_lsp() {
    init_gst();
    if !is_element_available("lsp-plug-in-plugins-lv2-gate-stereo") {
        println!("LSP gate not available, skipping");
        return;
    }
    let gate = make_gate_element("test_gate", true, -40.0, 5.0, 100.0, "lv2");
    assert!(gate.is_ok(), "Should create gate element");
    let gate = gate.unwrap();

    // Verify bypass property was set (enabled=true means bypass=false)
    if gate.find_property("enabled").is_some() {
        let enabled_val: bool = gate.property("enabled");
        assert!(enabled_val, "Gate enabled=true should set enabled=true");
    }
}

#[test]
fn test_make_gate_element_disabled() {
    init_gst();
    if !is_element_available("lsp-plug-in-plugins-lv2-gate-stereo") {
        println!("LSP gate not available, skipping");
        return;
    }
    let gate = make_gate_element("test_gate_off", false, -40.0, 5.0, 100.0, "lv2");
    assert!(gate.is_ok());
    let gate = gate.unwrap();

    if gate.find_property("enabled").is_some() {
        let enabled_val: bool = gate.property("enabled");
        assert!(!enabled_val, "Gate enabled=false should set enabled=false");
    }
}

#[test]
fn test_make_compressor_element_lsp() {
    init_gst();
    if !is_element_available("lsp-plug-in-plugins-lv2-compressor-stereo") {
        println!("LSP compressor not available, skipping");
        return;
    }
    let comp = make_compressor_element("test_comp", true, -20.0, 4.0, 10.0, 100.0, 0.0, "lv2");
    assert!(comp.is_ok(), "Should create compressor element");
    let comp = comp.unwrap();

    if comp.find_property("enabled").is_some() {
        let enabled_val: bool = comp.property("enabled");
        assert!(enabled_val, "Comp enabled=true should set enabled=true");
    }

    // Verify threshold was converted to linear
    if comp.find_property("al").is_some() {
        let al: f32 = comp.property("al");
        let expected = db_to_linear(-20.0) as f32;
        assert!(
            (al - expected).abs() < 0.001,
            "Threshold -20dB: expected {}, got {}",
            expected,
            al
        );
    }
}

#[test]
fn test_make_eq_element_lsp() {
    init_gst();
    if !is_element_available("lsp-plug-in-plugins-lv2-para-equalizer-x8-stereo") {
        println!("LSP EQ not available, skipping");
        return;
    }
    let bands = [
        (1000.0, 0.0, 1.0),
        (2000.0, 3.0, 1.0),
        (4000.0, -3.0, 1.0),
        (8000.0, 0.0, 1.0),
    ];
    let eq = make_eq_element("test_eq", true, &bands, "lv2");
    assert!(eq.is_ok(), "Should create EQ element");
    let eq = eq.unwrap();

    if eq.find_property("enabled").is_some() {
        let enabled_val: bool = eq.property("enabled");
        assert!(enabled_val, "EQ enabled=true should set enabled=true");
    }

    // Verify first band frequency
    if eq.find_property("f-0").is_some() {
        let f0: f32 = eq.property("f-0");
        assert!(
            (f0 - 1000.0).abs() < 1.0,
            "Band 0 freq should be 1000, got {}",
            f0
        );
    }
}

#[test]
fn test_make_limiter_element_lsp() {
    init_gst();
    if !is_element_available("lsp-plug-in-plugins-lv2-limiter-stereo") {
        println!("LSP limiter not available, skipping");
        return;
    }
    let lim = make_limiter_element("test_lim", true, -3.0, "lv2");
    assert!(lim.is_ok(), "Should create limiter element");
}

#[test]
fn test_make_hpf_element() {
    init_gst();
    if !is_element_available("audiocheblimit") && !is_element_available("audiowsinclimit") {
        println!("No HPF element available, skipping");
        return;
    }
    let hpf = make_hpf_element("test_hpf", true, 80.0);
    assert!(hpf.is_ok(), "Should create HPF element");
}

#[test]
fn test_make_hpf_element_disabled_uses_passthrough() {
    init_gst();
    if !is_element_available("audiocheblimit") {
        println!("audiocheblimit not available, skipping");
        return;
    }
    let hpf = make_hpf_element("test_hpf_off", false, 80.0);
    assert!(hpf.is_ok());
    let hpf = hpf.unwrap();

    if hpf.find_property("cutoff").is_some() {
        let cutoff: f32 = hpf.property("cutoff");
        assert!(
            cutoff.abs() < 0.1,
            "Disabled HPF should have cutoff=0 (passthrough), got {}",
            cutoff
        );
    }
}

#[test]
fn test_make_audiomixer() {
    init_gst();
    if !is_element_available("audiomixer") {
        println!("audiomixer not available, skipping");
        return;
    }
    let mixer = make_audiomixer("test_mixer", true, 30, 30);
    assert!(mixer.is_ok(), "Should create audiomixer: {:?}", mixer.err());
}

#[test]
fn test_make_gate_fallback_to_identity() {
    init_gst();
    // If LSP is available this just tests normal path, but it shouldn't panic
    let gate = make_gate_element("test_gate_fb", true, -40.0, 5.0, 100.0, "lv2");
    assert!(
        gate.is_ok(),
        "Gate should succeed (LSP or identity fallback)"
    );
}

#[test]
fn test_make_compressor_fallback_to_identity() {
    init_gst();
    let comp = make_compressor_element("test_comp_fb", true, -20.0, 4.0, 10.0, 100.0, 0.0, "lv2");
    assert!(
        comp.is_ok(),
        "Compressor should succeed (LSP or identity fallback)"
    );
}

#[test]
fn test_make_eq_fallback_to_identity() {
    init_gst();
    let bands = [
        (100.0, 0.0, 1.0),
        (1000.0, 0.0, 1.0),
        (5000.0, 0.0, 1.0),
        (10000.0, 0.0, 1.0),
    ];
    let eq = make_eq_element("test_eq_fb", true, &bands, "lv2");
    assert!(eq.is_ok(), "EQ should succeed (LSP or identity fallback)");
}

#[test]
fn test_extract_level_values_empty() {
    init_gst();
    let structure = gst::Structure::builder("level").build();
    let values = extract_level_values(structure.as_ref(), "peak");
    assert!(
        values.is_empty(),
        "Should return empty vec for missing field"
    );
}

// ---- Rust backend (lsp-plugins-rs) tests ----

#[test]
fn test_make_gate_element_rust() {
    init_gst();
    let gate = make_gate_element("test_gate_rs", true, -40.0, 5.0, 100.0, "rust");
    assert!(
        gate.is_ok(),
        "Should create gate element (rust or fallback): {:?}",
        gate.err()
    );
    let gate = gate.unwrap();
    // Use find_property to check element type (factory() can SIGSEGV in test context)
    if gate.find_property("open-threshold").is_some() {
        let enabled_val: bool = gate.property("enabled");
        assert!(enabled_val, "Gate should be enabled");
        let thresh: f32 = gate.property("open-threshold");
        assert!(
            (thresh - (-40.0)).abs() < 0.1,
            "Threshold should be -40 dB, got {}",
            thresh
        );
    }
}

#[test]
fn test_make_gate_element_rust_disabled() {
    init_gst();
    let gate = make_gate_element("test_gate_rs_off", false, -40.0, 5.0, 100.0, "rust");
    assert!(gate.is_ok());
    let gate = gate.unwrap();
    if gate.find_property("open-threshold").is_some() {
        let enabled_val: bool = gate.property("enabled");
        assert!(!enabled_val, "Gate should be disabled");
    }
}

#[test]
fn test_make_compressor_element_rust() {
    init_gst();
    let comp = make_compressor_element("test_comp_rs", true, -20.0, 4.0, 10.0, 100.0, 6.0, "rust");
    assert!(
        comp.is_ok(),
        "Should create compressor element (rust or fallback): {:?}",
        comp.err()
    );
    let comp = comp.unwrap();
    if comp.find_property("ratio").is_some() {
        let enabled_val: bool = comp.property("enabled");
        assert!(enabled_val, "Compressor should be enabled");
        let ratio: f32 = comp.property("ratio");
        assert!(
            (ratio - 4.0).abs() < 0.1,
            "Ratio should be 4.0, got {}",
            ratio
        );
    }
}

#[test]
fn test_make_eq_element_rust() {
    init_gst();
    let bands = [
        (1000.0, 3.0, 1.0),
        (2000.0, -3.0, 2.0),
        (4000.0, 0.0, 1.0),
        (8000.0, 6.0, 0.7),
    ];
    let eq = make_eq_element("test_eq_rs", true, &bands, "rust");
    assert!(
        eq.is_ok(),
        "Should create EQ element (rust or fallback): {:?}",
        eq.err()
    );
    let eq = eq.unwrap();
    if eq.find_property("band0-frequency").is_some() {
        let enabled_val: bool = eq.property("enabled");
        assert!(enabled_val, "EQ should be enabled");
        let f0: f32 = eq.property("band0-frequency");
        assert!(
            (f0 - 1000.0).abs() < 1.0,
            "Band 0 freq should be 1000, got {}",
            f0
        );
        // Rust EQ gain is dB directly
        let g0: f32 = eq.property("band0-gain");
        assert!(
            (g0 - 3.0).abs() < 0.1,
            "Band 0 gain should be 3.0 dB, got {}",
            g0
        );
    }
}

#[test]
fn test_make_limiter_element_rust() {
    init_gst();
    let lim = make_limiter_element("test_lim_rs", true, -3.0, "rust");
    assert!(
        lim.is_ok(),
        "Should create limiter element (rust or fallback): {:?}",
        lim.err()
    );
    let lim = lim.unwrap();
    if lim.find_property("lookahead").is_some() {
        let enabled_val: bool = lim.property("enabled");
        assert!(enabled_val, "Limiter should be enabled");
        let thresh: f32 = lim.property("threshold");
        assert!(
            (thresh - (-3.0)).abs() < 0.1,
            "Threshold should be -3 dB, got {}",
            thresh
        );
    }
}

// ---- Property translation tests ----

#[test]
fn test_linear_to_db() {
    assert!(
        (linear_to_db(1.0) - 0.0).abs() < 1e-6,
        "1.0 linear should be 0 dB"
    );
    assert!(
        (linear_to_db(0.1) - (-20.0)).abs() < 1e-6,
        "0.1 linear should be -20 dB"
    );
}

#[test]
fn test_linear_to_db_zero() {
    let result = linear_to_db(0.0);
    assert!(
        result <= -120.0,
        "0.0 linear should be <= -120 dB, got {}",
        result
    );
}

#[test]
fn test_translate_gate_property() {
    init_gst();
    if !is_element_available("lsp-rs-gate") {
        println!("lsp-rs-gate not available, skipping translation test");
        return;
    }
    let gate = gst::ElementFactory::make("lsp-rs-gate")
        .name("translate_test_gate")
        .build()
        .unwrap();

    // gt (linear) -> open-threshold + close-threshold (dB): 0.1 linear = -20 dB
    let result = translate_property_for_element(&gate, "gt", &PropertyValue::Float(0.1));
    assert_eq!(
        result.len(),
        2,
        "gt should translate to both open-threshold and close-threshold"
    );
    assert_eq!(result[0].0, "open-threshold");
    assert_eq!(result[1].0, "close-threshold");
    for (name, value) in &result {
        if let PropertyValue::Float(v) = value {
            assert!(
                (v - (-20.0)).abs() < 0.1,
                "0.1 linear should translate to -20 dB for {}, got {}",
                name,
                v
            );
        } else {
            panic!("Expected Float value for {}", name);
        }
    }
}

#[test]
fn test_translate_compressor_property() {
    init_gst();
    if !is_element_available("lsp-rs-compressor") {
        println!("lsp-rs-compressor not available, skipping translation test");
        return;
    }
    let comp = gst::ElementFactory::make("lsp-rs-compressor")
        .name("translate_test_comp")
        .build()
        .unwrap();

    // al -> threshold (both linear, no value change)
    let result = translate_property_for_element(&comp, "al", &PropertyValue::Float(0.1));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "threshold");

    // cr -> ratio
    let result = translate_property_for_element(&comp, "cr", &PropertyValue::Float(4.0));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "ratio");

    // enabled -> no translation needed
    let result = translate_property_for_element(&comp, "enabled", &PropertyValue::Bool(true));
    assert!(result.is_empty(), "enabled should not need translation");
}

#[test]
fn test_translate_eq_property() {
    init_gst();
    if !is_element_available("lsp-rs-equalizer") {
        println!("lsp-rs-equalizer not available, skipping translation test");
        return;
    }
    let eq = gst::ElementFactory::make("lsp-rs-equalizer")
        .name("translate_test_eq")
        .build()
        .unwrap();

    // f-0 -> band0-frequency
    let result = translate_property_for_element(&eq, "f-0", &PropertyValue::Float(1000.0));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "band0-frequency");

    // g-0 (linear) -> band0-gain (dB)
    let result = translate_property_for_element(&eq, "g-0", &PropertyValue::Float(1.0));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "band0-gain");
    if let PropertyValue::Float(v) = &result[0].1 {
        assert!(
            v.abs() < 0.1,
            "1.0 linear should translate to 0 dB, got {}",
            v
        );
    }
}

#[test]
fn test_translate_limiter_property() {
    init_gst();
    if !is_element_available("lsp-rs-limiter") {
        println!("lsp-rs-limiter not available, skipping translation test");
        return;
    }
    let lim = gst::ElementFactory::make("lsp-rs-limiter")
        .name("translate_test_lim")
        .build()
        .unwrap();

    // th (linear) -> threshold (dB)
    let result = translate_property_for_element(&lim, "th", &PropertyValue::Float(0.1));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "threshold");
    if let PropertyValue::Float(v) = &result[0].1 {
        assert!(
            (v - (-20.0)).abs() < 0.1,
            "0.1 linear should translate to -20 dB, got {}",
            v
        );
    }
}

#[test]
fn test_translate_no_translation_for_lv2() {
    init_gst();
    // For LV2 elements (or any non-lsp-rs element), translation should return None
    if let Ok(elem) = gst::ElementFactory::make("identity")
        .name("translate_test_identity")
        .build()
    {
        let result = translate_property_for_element(&elem, "gt", &PropertyValue::Float(0.1));
        assert!(
            result.is_empty(),
            "Should not translate properties for non-lsp-rs elements"
        );
    }
}

#[test]
fn test_mixer_definition_has_dsp_backend_property() {
    let def = mixer_definition();
    let dsp_prop = def
        .exposed_properties
        .iter()
        .find(|p| p.name == "dsp_backend");
    assert!(dsp_prop.is_some(), "Should have dsp_backend property");
    let dsp_prop = dsp_prop.unwrap();
    match &dsp_prop.default_value {
        Some(PropertyValue::String(s)) => {
            assert_eq!(s, "rust", "Default should be rust, got {}", s);
        }
        other => panic!("Expected String(\"rust\"), got {:?}", other),
    }
    match &dsp_prop.property_type {
        PropertyType::Enum { values } => {
            assert_eq!(values.len(), 2);
            assert_eq!(values[0].value, "rust");
            assert_eq!(values[1].value, "lv2");
        }
        other => panic!("Expected Enum type, got {:?}", other),
    }
}
