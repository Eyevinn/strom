use super::*;
use crate::blocks::{BlockBuildContext, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom_types::block::ExposedProperty;
use strom_types::mixer::DEFAULT_INTERNAL_BUS_LATENCY_MS;
use strom_types::PropertyValue;

fn init_gst() {
    let _ = gst::init();
    let _ = gst_plugins_lsp::plugin_register_static();
}

/// GObject type name. `factory()` can SIGSEGV when static and LV2 plugins
/// coexist, so the tests identify elements the way `translate_property_for_element` does.
fn type_name(element: &gst::Element) -> &'static str {
    element.type_().name()
}

fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

// ---- Pure functions ----

#[test]
fn test_db_linear_conversions() {
    for (db, linear) in [
        (0.0, 1.0),
        (-6.0, 0.501_187),
        (-20.0, 0.1),
        (-60.0, 0.001),
        (6.0, 1.995_262),
    ] {
        assert!(
            (db_to_linear(db) - linear).abs() < 1e-6,
            "db_to_linear({db}) = {}, expected {linear}",
            db_to_linear(db)
        );
        assert!(
            (linear_to_db(linear) - db).abs() < 1e-4,
            "linear_to_db({linear}) = {}, expected {db}",
            linear_to_db(linear)
        );
    }
    assert_eq!(linear_to_db(0.0), -120.0, "silence floors at -120 dB");
    assert_eq!(
        linear_to_db(-1.0),
        -120.0,
        "negative gain floors at -120 dB"
    );
}

#[test]
fn test_parse_counts_default_and_clamp() {
    let string = |s: &str| PropertyValue::String(s.to_string());
    type Parse = fn(&HashMap<String, PropertyValue>) -> usize;
    let cases: &[(Parse, &str, Option<PropertyValue>, usize)] = &[
        (parse_num_channels, "num_channels", None, DEFAULT_CHANNELS),
        (parse_num_channels, "num_channels", Some(string("4")), 4),
        (
            parse_num_channels,
            "num_channels",
            Some(PropertyValue::UInt(3)),
            3,
        ),
        (
            parse_num_channels,
            "num_channels",
            Some(string("9999")),
            MAX_CHANNELS,
        ),
        (parse_num_channels, "num_channels", Some(string("0")), 1),
        (
            parse_num_channels,
            "num_channels",
            Some(string("abc")),
            DEFAULT_CHANNELS,
        ),
        (parse_num_aux_buses, "num_aux_buses", None, 0),
        (
            parse_num_aux_buses,
            "num_aux_buses",
            Some(string("999")),
            MAX_AUX_BUSES,
        ),
        (parse_num_groups, "num_groups", None, 0),
        (
            parse_num_groups,
            "num_groups",
            Some(string("999")),
            MAX_GROUPS,
        ),
    ];
    for (parse, key, value, expected) in cases {
        let map = match value {
            Some(v) => props(&[(key, v.clone())]),
            None => HashMap::new(),
        };
        assert_eq!(parse(&map), *expected, "{key} = {value:?}");
    }
}

#[test]
fn test_extract_level_values() {
    init_gst();
    // Read a real message from a real `level` element: a stereo sine at
    // amplitude 0.5 peaks at 20*log10(0.5) = -6.02 dB on both channels.
    let pipeline = gst::parse::launch(
        "audiotestsrc num-buffers=20 volume=0.5 ! \
         audio/x-raw,format=F32LE,channels=2 ! \
         level name=lvl interval=10000000 post-messages=true ! fakesink",
    )
    .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Element],
        )
        .expect("level posted a message");
    pipeline.set_state(gst::State::Null).unwrap();
    let structure = msg.structure().unwrap();
    assert_eq!(structure.name(), "level");

    let peak = extract_level_values(structure, "peak");
    assert_eq!(peak.len(), 2, "one value per channel: {peak:?}");
    for db in &peak {
        assert!((db - linear_to_db(0.5)).abs() < 0.1, "peak {db} dB");
    }
    assert_eq!(extract_level_values(structure, "rms").len(), 2);
    // A field that is missing, or is not a GValueArray, yields nothing.
    assert!(extract_level_values(structure, "missing").is_empty());
    assert!(extract_level_values(structure, "timestamp").is_empty());
}

// ---- Block definition ----

fn exposed<'a>(def: &'a strom_types::BlockDefinition, name: &str) -> &'a ExposedProperty {
    def.exposed_properties
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("missing property {name}"))
}

#[test]
fn test_mixer_definition_enabled_mappings() {
    let def = mixer_definition();
    for name in [
        "main_comp_enabled",
        "main_eq_enabled",
        "main_limiter_enabled",
        "ch1_gate_enabled",
        "ch1_comp_enabled",
        "ch1_eq_enabled",
    ] {
        let prop = exposed(&def, name);
        assert_eq!(prop.mapping.property_name, "enabled", "{name}");
        assert_eq!(prop.mapping.transform, None, "{name} needs no transform");
    }
}

#[test]
fn test_mixer_definition_db_to_linear_transforms() {
    let def = mixer_definition();
    for name in [
        "ch1_gate_threshold",
        "ch1_comp_threshold",
        "ch1_comp_makeup",
        "ch1_comp_knee",
        "main_comp_threshold",
        "main_comp_makeup",
    ] {
        assert_eq!(
            exposed(&def, name).mapping.transform.as_deref(),
            Some("db_to_linear"),
            "{name}"
        );
    }
}

#[test]
fn test_mixer_pfl_afl_are_transient() {
    // Solo state must not persist across pipeline restarts — see the
    // persist:false guard in state.rs::strip_transient_properties.
    let def = mixer_definition();

    let mut names: Vec<String> = Vec::new();
    for ch in 1..=4usize {
        for kind in ["pfl", "afl"] {
            names.push(format!("ch{}_{}", ch, kind));
        }
    }
    // Aux/group AFL are also pure solo state and follow the same rule.
    for aux in 1..=4usize {
        names.push(format!("aux{}_afl", aux));
    }
    for sg in 1..=4usize {
        names.push(format!("group{}_afl", sg));
    }

    for name in names {
        let prop = exposed(&def, &name);
        assert!(prop.live, "{} should be live", name);
        assert_eq!(
            prop.persist,
            Some(false),
            "{} must be marked persist: Some(false)",
            name
        );
    }
}

#[test]
fn test_is_solo_property_name_matches_pfl_and_afl() {
    use super::is_solo_property_name;
    // Channel PFL/AFL
    assert!(is_solo_property_name("ch1_pfl"));
    assert!(is_solo_property_name("ch12_afl"));
    // Aux/group AFL
    assert!(is_solo_property_name("aux1_afl"));
    assert!(is_solo_property_name("aux32_afl"));
    assert!(is_solo_property_name("group1_afl"));
    assert!(is_solo_property_name("group16_afl"));
    // Non-solo names must be rejected
    assert!(!is_solo_property_name("ch1_mute"));
    assert!(!is_solo_property_name("main_fader"));
    assert!(!is_solo_property_name("ch_pfl"));
    assert!(!is_solo_property_name("chA_pfl"));
    assert!(!is_solo_property_name("aux1_mute"));
    assert!(!is_solo_property_name("group1_mute"));
    // PFL only exists for channels today
    assert!(!is_solo_property_name("aux1_pfl"));
    assert!(!is_solo_property_name("group1_pfl"));
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
        let prop = exposed(&def, name);
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
        assert!(
            !exposed(&def, name).live,
            "{} must NOT be marked live: true",
            name
        );
    }
}

// ---- Element factories ----

#[test]
fn test_rust_backend_elements_take_their_settings() {
    init_gst();
    // lsp-rs is statically registered, so the rust backend never falls back
    // to identity. These asserts are unconditional on purpose.
    let gate = make_gate_element("t_gate_rs", true, -40.0, 5.0, 100.0, "rust").unwrap();
    assert_eq!(type_name(&gate), "LspRsGate");
    assert!(gate.property::<bool>("enabled"));
    assert_eq!(gate.property::<f32>("open-threshold"), -40.0);
    assert_eq!(gate.property::<f32>("close-threshold"), -40.0);
    assert_eq!(gate.property::<f32>("attack"), 5.0);
    assert_eq!(gate.property::<f32>("release"), 100.0);

    let gate_off = make_gate_element("t_gate_rs_off", false, -40.0, 5.0, 100.0, "rust").unwrap();
    assert!(!gate_off.property::<bool>("enabled"));

    let comp =
        make_compressor_element("t_comp_rs", true, -20.0, 4.0, 10.0, 100.0, 6.0, "rust").unwrap();
    assert_eq!(type_name(&comp), "LspRsCompressor");
    assert!(comp.property::<bool>("enabled"));
    assert_eq!(comp.property::<f32>("ratio"), 4.0);
    assert_eq!(comp.property::<f32>("attack"), 10.0);
    assert_eq!(comp.property::<f32>("release"), 100.0);

    let bands = [
        (1000.0, 3.0, 1.0),
        (2000.0, -3.0, 2.0),
        (4000.0, 0.0, 1.0),
        (8000.0, 6.0, 0.7),
    ];
    let eq = make_eq_element("t_eq_rs", true, &bands, "rust").unwrap();
    assert_eq!(type_name(&eq), "LspRsEqualizer");
    assert!(eq.property::<bool>("enabled"));
    for (i, (freq, gain_db, q)) in bands.iter().enumerate() {
        assert_eq!(
            eq.property::<f32>(&format!("band{i}-frequency")),
            *freq as f32
        );
        // The Rust EQ takes dB directly.
        assert_eq!(
            eq.property::<f32>(&format!("band{i}-gain")),
            *gain_db as f32
        );
        assert_eq!(eq.property::<f32>(&format!("band{i}-q")), *q as f32);
    }

    let lim = make_limiter_element("t_lim_rs", true, -3.0, "rust").unwrap();
    assert_eq!(type_name(&lim), "LspRsLimiter");
    assert!(lim.property::<bool>("enabled"));
    assert_eq!(lim.property::<f32>("threshold"), -3.0);
}

const LV2_FACTORIES: [&str; 4] = [
    "lsp-plug-in-plugins-lv2-gate-stereo",
    "lsp-plug-in-plugins-lv2-compressor-stereo",
    "lsp-plug-in-plugins-lv2-para-equalizer-x8-stereo",
    "lsp-plug-in-plugins-lv2-limiter-stereo",
];

fn lv2_elements() -> [gst::Element; 4] {
    let bands = [
        (1000.0, 0.0, 1.0),
        (2000.0, 3.0, 1.0),
        (4000.0, -3.0, 1.0),
        (8000.0, 0.0, 1.0),
    ];
    [
        make_gate_element("t_gate_lv2", true, -40.0, 5.0, 100.0, "lv2").unwrap(),
        make_compressor_element("t_comp_lv2", true, -20.0, 4.0, 10.0, 100.0, 0.0, "lv2").unwrap(),
        make_eq_element("t_eq_lv2", true, &bands, "lv2").unwrap(),
        make_limiter_element("t_lim_lv2", true, -3.0, "lv2").unwrap(),
    ]
}

#[test]
fn test_lv2_backend_falls_back_to_identity() {
    init_gst();
    // Without lsp-plugins-lv2 (as in CI) every LV2 stage must still build, as
    // a passthrough identity. With the plugins installed, it must be the
    // plugin and not the fallback.
    for (factory, element) in LV2_FACTORIES.iter().zip(lv2_elements()) {
        let installed = gst::ElementFactory::find(factory).is_some();
        assert_eq!(
            type_name(&element) == "GstIdentity",
            !installed,
            "{factory}: installed={installed}, built a {}",
            type_name(&element)
        );
    }
}

#[test]
#[ignore = "needs lsp-plugins-lv2, which CI does not install"]
fn test_lv2_backend_elements_take_their_settings() {
    init_gst();
    let [gate, comp, eq, _lim] = lv2_elements();
    assert!(gate.property::<bool>("enabled"));
    assert!(comp.property::<bool>("enabled"));
    // LV2 takes linear thresholds.
    let al: f32 = comp.property("al");
    assert!((al - db_to_linear(-20.0) as f32).abs() < 1e-3, "al = {al}");
    assert!(eq.property::<bool>("enabled"));
    assert!((eq.property::<f32>("f-0") - 1000.0).abs() < 1.0);
}

#[test]
fn test_make_hpf_element() {
    init_gst();
    // audiocheblimit is in gst-plugins-good, which CI installs.
    let on = make_hpf_element("t_hpf_on", true, 80.0).unwrap();
    assert_eq!(type_name(&on), "GstAudioChebLimit");
    assert_eq!(on.property::<f32>("cutoff"), 80.0);

    // Disabled leaves cutoff at 0, which puts the filter in passthrough.
    let off = make_hpf_element("t_hpf_off", false, 80.0).unwrap();
    assert_eq!(off.property::<f32>("cutoff"), 0.0);
}

// ---- Property translation (lsp-rs is statically registered) ----

fn lsp_rs(factory: &str) -> gst::Element {
    init_gst();
    gst::ElementFactory::make(factory)
        .build()
        .unwrap_or_else(|e| panic!("{factory} is statically registered: {e}"))
}

fn float(value: &PropertyValue) -> f64 {
    match value {
        PropertyValue::Float(v) => *v,
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn test_translate_gate_property() {
    let gate = lsp_rs("lsp-rs-gate");
    // gt (linear) -> open-threshold + close-threshold (dB): 0.1 linear = -20 dB
    let result = translate_property_for_element(&gate, "gt", &PropertyValue::Float(0.1));
    let names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["open-threshold", "close-threshold"]);
    for (name, value) in &result {
        assert!((float(value) + 20.0).abs() < 1e-6, "{name}: {value:?}");
    }
}

#[test]
fn test_translate_compressor_property() {
    let comp = lsp_rs("lsp-rs-compressor");
    for (lv2, rust) in [
        ("al", "threshold"),
        ("cr", "ratio"),
        ("at", "attack"),
        ("rt", "release"),
        ("mk", "makeup-gain"),
    ] {
        let result = translate_property_for_element(&comp, lv2, &PropertyValue::Float(0.5));
        assert_eq!(result.len(), 1, "{lv2}");
        assert_eq!(result[0].0, rust, "{lv2}");
        assert_eq!(float(&result[0].1), 0.5, "{lv2} passes the value through");
    }
    let result = translate_property_for_element(&comp, "enabled", &PropertyValue::Bool(true));
    assert!(result.is_empty(), "enabled should not need translation");
}

#[test]
fn test_translate_eq_property() {
    let eq = lsp_rs("lsp-rs-equalizer");

    let result = translate_property_for_element(&eq, "f-2", &PropertyValue::Float(1000.0));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "band2-frequency");
    assert_eq!(float(&result[0].1), 1000.0);

    // g-N (linear) -> bandN-gain (dB)
    let result = translate_property_for_element(&eq, "g-0", &PropertyValue::Float(0.1));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "band0-gain");
    assert!((float(&result[0].1) + 20.0).abs() < 1e-6);

    let result = translate_property_for_element(&eq, "q-3", &PropertyValue::Float(0.7));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "band3-q");
}

#[test]
fn test_translate_limiter_property() {
    let lim = lsp_rs("lsp-rs-limiter");
    // th (linear) -> threshold (dB)
    let result = translate_property_for_element(&lim, "th", &PropertyValue::Float(0.1));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "threshold");
    assert!((float(&result[0].1) + 20.0).abs() < 1e-6);
}

#[test]
fn test_translate_no_translation_for_other_elements() {
    init_gst();
    let elem = gst::ElementFactory::make("identity").build().unwrap();
    let result = translate_property_for_element(&elem, "gt", &PropertyValue::Float(0.1));
    assert!(
        result.is_empty(),
        "Should not translate properties for non-lsp-rs elements"
    );
}

// ---- MixerBuilder::build ----

const INSTANCE: &str = "mx";

/// Two channels, one aux, one group: small enough to read, and it exercises
/// every kind of bus the builder wires.
fn small_mixer_props(extra: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    let mut p = props(&[
        ("num_channels", PropertyValue::UInt(2)),
        ("num_aux_buses", PropertyValue::UInt(1)),
        ("num_groups", PropertyValue::UInt(1)),
        ("dsp_backend", PropertyValue::String("rust".to_string())),
    ]);
    p.extend(props(extra));
    p
}

struct Assembled {
    pipeline: gst::Pipeline,
    result: BlockBuildResult,
}

impl Assembled {
    fn element(&self, id: &str) -> &gst::Element {
        let full = format!("{INSTANCE}:{id}");
        self.result
            .elements
            .iter()
            .find(|(k, _)| *k == full)
            .map(|(_, e)| e)
            .unwrap_or_else(|| panic!("builder produced no element {full}"))
    }

    fn has_element(&self, id: &str) -> bool {
        let full = format!("{INSTANCE}:{id}");
        self.result.elements.iter().any(|(k, _)| *k == full)
    }

    /// Every element reachable downstream of `id` through linked pads.
    fn downstream(&self, id: &str) -> HashSet<String> {
        let prefix = format!("{INSTANCE}:");
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([self.element(id).clone()]);
        while let Some(element) = queue.pop_front() {
            for pad in element.src_pads() {
                let Some(next) = pad.peer().and_then(|p| p.parent_element()) else {
                    continue;
                };
                let name = next.name().to_string();
                if seen.insert(name.trim_start_matches(&prefix).to_string()) {
                    queue.push_back(next);
                }
            }
        }
        seen
    }
}

impl Drop for Assembled {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn resolve_pad(element: &gst::Element, name: &str) -> gst::Pad {
    element
        .static_pad(name)
        .or_else(|| element.pads().into_iter().find(|p| p.name() == name))
        .or_else(|| element.request_pad_simple(name))
        .unwrap_or_else(|| panic!("{} has no pad {name}", element.name()))
}

/// Build through the real builder and link the result the way the pipeline
/// manager does: named pads pad-to-pad, element refs by `Element::link`.
fn assemble(properties: &HashMap<String, PropertyValue>) -> Assembled {
    init_gst();
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = MixerBuilder
        .build(INSTANCE, properties, &ctx)
        .expect("mixer build");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<&str, &gst::Element> = HashMap::new();
    for (id, element) in &result.elements {
        assert_eq!(element.name(), id.as_str(), "element name matches its id");
        pipeline.add(element).expect("add element");
        assert!(by_id.insert(id, element).is_none(), "duplicate id {id}");
    }
    for (from, to) in &result.internal_links {
        let src = by_id
            .get(from.element_id.as_str())
            .unwrap_or_else(|| panic!("link source {} was never built", from.element_id));
        let dst = by_id
            .get(to.element_id.as_str())
            .unwrap_or_else(|| panic!("link sink {} was never built", to.element_id));
        match (&from.pad_name, &to.pad_name) {
            (Some(sp), Some(dp)) => {
                resolve_pad(src, sp)
                    .link(&resolve_pad(dst, dp))
                    .unwrap_or_else(|e| panic!("link {from:?} -> {to:?}: {e:?}"));
            }
            (None, Some(dp)) => src
                .link_pads(None, *dst, Some(dp.as_str()))
                .unwrap_or_else(|e| panic!("link {from:?} -> {to:?}: {e}")),
            (Some(sp), None) => src
                .link_pads(Some(sp.as_str()), *dst, None)
                .unwrap_or_else(|e| panic!("link {from:?} -> {to:?}: {e}")),
            (None, None) => src
                .link(*dst)
                .unwrap_or_else(|e| panic!("link {from:?} -> {to:?}: {e}")),
        };
    }
    Assembled { pipeline, result }
}

#[test]
fn test_build_wires_channels_aux_group_and_monitor() {
    let m = assemble(&small_mixer_props(&[]));

    // Every channel feeds main, the aux, the group and the solo bus; and main
    // and solo both reach the monitor output.
    for ch in 0..2 {
        let reach = m.downstream(&format!("convert_{ch}"));
        for bus in [
            "audiomixer",
            "main_out_tee",
            "aux0_mixer",
            "aux0_out_tee",
            "group0_mixer",
            "group0_out_tee",
            "solo_mixer",
            "monitor_out_tee",
        ] {
            assert!(reach.contains(bus), "convert_{ch} does not reach {bus}");
        }
        assert!(
            !reach.contains(&format!("convert_{}", 1 - ch)),
            "channels must not feed each other"
        );
    }

    // A group sums back into main and has its own AFL tap. An aux is a
    // separate send: it must not leak into main.
    let group = m.downstream("group0_out_tee");
    assert!(group.contains("audiomixer") && group.contains("solo_mixer"));
    let aux = m.downstream("aux0_out_tee");
    assert!(
        aux.contains("solo_mixer"),
        "aux AFL tap reaches the solo bus"
    );
    assert!(!aux.contains("main_out_tee"), "aux must not feed main");

    // Main feeds the monitor through its gate, never the other way round.
    assert!(m.downstream("main_out_tee").contains("monitor_out_tee"));
    assert!(!m.downstream("monitor_out_tee").contains("main_out_tee"));

    // Nothing was built past the configured counts.
    for absent in ["convert_2", "aux1_mixer", "group1_mixer"] {
        assert!(!m.has_element(absent), "{absent} should not exist");
    }

    // Every external pad points at an element and a pad the builder made.
    let pads = MixerBuilder
        .get_external_pads(&small_mixer_props(&[]))
        .unwrap();
    let outputs: Vec<&str> = pads.outputs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        outputs,
        ["main_out", "monitor_out", "aux_out_1", "group_out_1"]
    );
    assert_eq!(pads.inputs.len(), 2);
    for pad in pads.inputs.iter().chain(pads.outputs.iter()) {
        let element = m.element(&pad.internal_element_id);
        assert!(
            element.static_pad(&pad.internal_pad_name).is_some()
                || element.pad_template(&pad.internal_pad_name).is_some(),
            "{}: {} has no pad {}",
            pad.name,
            pad.internal_element_id,
            pad.internal_pad_name
        );
    }
}

/// Whether every `chN` / `auxN` / `groupN` / `grpN` index in a property name
/// is within the build's counts. The definition is generated for the largest
/// mixer, so this filters it down to the properties a small build has
/// elements for.
fn in_config(name: &str, channels: usize, aux: usize, groups: usize) -> bool {
    name.split('_').all(|token| {
        for (prefix, max) in [
            ("ch", channels),
            ("aux", aux),
            ("group", groups),
            ("grp", groups),
        ] {
            if let Some(n) = token
                .strip_prefix(prefix)
                .and_then(|n| n.parse::<usize>().ok())
            {
                return n >= 1 && n <= max;
            }
        }
        true
    })
}

#[test]
fn test_every_live_property_targets_a_built_element() {
    // The definition names element ids and property names as strings; the
    // builder names elements separately. A rename on either side silently
    // breaks live control, so resolve every in-range live property the way
    // update_element_property does: translation first, then the raw name.
    let m = assemble(&small_mixer_props(&[]));
    let def = mixer_definition();
    let mut checked = 0;
    for prop in &def.exposed_properties {
        if !prop.live || prop.mapping.element_id == "_block" || !in_config(&prop.name, 2, 1, 1) {
            continue;
        }
        assert!(
            m.has_element(&prop.mapping.element_id),
            "{} maps to {}, which the builder did not create",
            prop.name,
            prop.mapping.element_id
        );
        let element = m.element(&prop.mapping.element_id);
        let value = prop
            .default_value
            .clone()
            .unwrap_or(PropertyValue::Float(0.0));
        let translated =
            translate_property_for_element(element, &prop.mapping.property_name, &value);
        let targets: Vec<String> = if translated.is_empty() {
            vec![prop.mapping.property_name.clone()]
        } else {
            translated.into_iter().map(|(n, _)| n).collect()
        };
        for target in targets {
            assert!(
                element.find_property(&target).is_some(),
                "{} -> {}.{}: no such property on {}",
                prop.name,
                prop.mapping.element_id,
                target,
                type_name(element)
            );
        }
        checked += 1;
    }
    // Guard the filter itself: a broken in_config would check nothing.
    assert!(checked > 50, "only {checked} properties checked");
}

#[test]
fn test_build_applies_properties_to_elements() {
    let m = assemble(&small_mixer_props(&[
        ("ch1_gain", PropertyValue::Int(-20)),
        ("ch2_mute", PropertyValue::Bool(true)),
        ("group1_fader", PropertyValue::Float(0.5)),
        ("ch1_comp_knee", PropertyValue::Float(-30.0)),
        ("main_comp_knee", PropertyValue::Float(6.0)),
        ("ch1_to_grp1", PropertyValue::Bool(true)),
        ("ch1_pfl", PropertyValue::Bool(true)),
    ]));

    // An Int gain is accepted as dB and converted to linear.
    let gain = m.element("gain_0").property::<f64>("volume");
    assert!((gain - 0.1).abs() < 1e-6, "gain_0 volume {gain}");
    assert!(m.element("volume_1").property::<bool>("mute"));
    assert!(!m.element("volume_0").property::<bool>("mute"));
    assert_eq!(m.element("group0_volume").property::<f64>("volume"), 0.5);
    // The knee is clamped into the compressor's usable range at build time.
    assert_eq!(
        m.element("comp_0").property::<f32>("knee"),
        MIN_KNEE_LINEAR as f32
    );
    assert_eq!(m.element("main_comp").property::<f32>("knee"), 1.0);
    // Routing and solo gates are volume elements switched 0/1.
    assert_eq!(m.element("to_grp0_vol_0").property::<f64>("volume"), 1.0);
    assert_eq!(m.element("to_grp0_vol_1").property::<f64>("volume"), 0.0);
    assert_eq!(m.element("pfl_volume_0").property::<f64>("volume"), 1.0);
    assert_eq!(m.element("pfl_volume_1").property::<f64>("volume"), 0.0);
}

#[test]
fn test_built_mixer_passes_audio_to_main_out() {
    let m = assemble(&small_mixer_props(&[]));

    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    m.pipeline.add_many([&src, &sink]).unwrap();
    src.link_pads(None, m.element("convert_0"), Some("sink"))
        .unwrap();
    m.element("main_out_tee")
        .link_pads(Some("src_%u"), &sink, None)
        .unwrap();

    let buffers = Arc::new(AtomicUsize::new(0));
    let counter = buffers.clone();
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

    m.pipeline.set_state(gst::State::Playing).unwrap();
    let bus = m.pipeline.bus().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while buffers.load(Ordering::Relaxed) < 5 {
        assert!(Instant::now() < deadline, "no audio reached main_out");
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) {
            panic!("pipeline error: {msg:?}");
        }
    }
}

/// Minimum latency reported upstream of each named output tee, with live audio
/// playing into both channels. An unfed channel fails the query.
fn reported_latency(properties: &HashMap<String, PropertyValue>, tees: &[&str]) -> Vec<u64> {
    let m = assemble(properties);
    for ch in 0..2 {
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        m.pipeline.add(&src).unwrap();
        src.link_pads(None, m.element(&format!("convert_{ch}")), Some("sink"))
            .unwrap();
    }
    m.pipeline.set_state(gst::State::Playing).unwrap();
    // The query fails until every element upstream has reached PLAYING.
    let deadline = Instant::now() + Duration::from_secs(10);
    tees.iter()
        .map(|tee| {
            let pad = m.element(tee).static_pad("sink").unwrap();
            loop {
                let mut q = gst::query::Latency::new();
                if pad.peer_query(&mut q) {
                    let (live, min, _) = q.result();
                    assert!(live, "{tee} is live");
                    break min.mseconds();
                }
                assert!(Instant::now() < deadline, "latency query upstream of {tee}");
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .collect()
}

#[test]
fn test_monitor_bus_does_not_stack_block_latency() {
    // Monitor sums Main and Solo, and Solo sums the aux and group buses. Each
    // of those already waits the block latency, so Monitor must add only a
    // little on top of Main, or a linked monitor_out holds every sink in the
    // flow a further block latency.
    let latency = 100;
    let [main, aux, monitor] = reported_latency(
        &small_mixer_props(&[("latency", PropertyValue::UInt(latency))]),
        &["main_out_tee", "aux0_out_tee", "monitor_out_tee"],
    )[..] else {
        unreachable!()
    };
    assert!(aux >= latency, "aux waits the block latency: {aux} ms");
    assert!(
        monitor <= main + 3 * DEFAULT_INTERNAL_BUS_LATENCY_MS,
        "monitor {monitor} ms stacks latency on main {main} ms"
    );
}

#[test]
fn test_solo_keeps_block_latency_without_aux_or_group() {
    // With no aux or group bus, Solo's only inputs are the channels' PFL/AFL
    // taps, which need the same slack Main gives the channels.
    let latency = 100;
    let props = props(&[
        ("num_channels", PropertyValue::UInt(2)),
        ("num_aux_buses", PropertyValue::UInt(0)),
        ("num_groups", PropertyValue::UInt(0)),
        ("dsp_backend", PropertyValue::String("rust".to_string())),
        ("latency", PropertyValue::UInt(latency)),
    ]);
    let m = assemble(&props);
    let solo = m.element("solo_mixer").property::<u64>("latency");
    assert_eq!(solo, latency * 1_000_000);
    drop(m);
    let [main, monitor] = reported_latency(&props, &["main_out_tee", "monitor_out_tee"])[..] else {
        unreachable!()
    };
    assert!(
        monitor <= main + 3 * DEFAULT_INTERNAL_BUS_LATENCY_MS,
        "monitor {monitor} ms stacks latency on main {main} ms"
    );
}

#[test]
fn test_internal_bus_latency_property_sets_solo_and_monitor() {
    // The operator's override reaches both internal buses, and is capped at
    // the block latency so it can never stack more than the block itself.
    let latency = 100;
    for (requested, expected) in [(60, 60), (250, latency)] {
        let props = small_mixer_props(&[
            ("latency", PropertyValue::UInt(latency)),
            ("internal_bus_latency", PropertyValue::UInt(requested)),
        ]);
        let m = assemble(&props);
        for bus in ["solo_mixer", "monitor_mixer"] {
            let got = m.element(bus).property::<u64>("latency");
            assert_eq!(
                got,
                expected * 1_000_000,
                "{bus} latency with internal_bus_latency={requested}"
            );
        }
    }
}

#[test]
fn test_solo_does_not_stack_block_latency_with_aux_buses() {
    // Open Live's shape: aux buses and no group, so Main takes only channels
    // and does not hide what Solo adds. Solo sums the aux buses, which already
    // wait the block latency; Monitor sums Solo.
    let latency = 100;
    let [main, aux, monitor] = reported_latency(
        &small_mixer_props(&[
            ("num_groups", PropertyValue::UInt(0)),
            ("latency", PropertyValue::UInt(latency)),
        ]),
        &["main_out_tee", "aux0_out_tee", "monitor_out_tee"],
    )[..] else {
        unreachable!()
    };
    assert!(
        monitor <= main.max(aux) + 3 * DEFAULT_INTERNAL_BUS_LATENCY_MS,
        "monitor {monitor} ms stacks latency on main {main} ms / aux {aux} ms"
    );
}
