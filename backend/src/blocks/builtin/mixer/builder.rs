use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, FlowId, MediaType, PropertyValue};
use tracing::{debug, info, warn};

use super::elements::*;
use super::metering::connect_mixer_meter_handler;
use super::properties::*;
use super::{METER_INTERVAL_NS, MIN_KNEE_LINEAR};
use strom_types::mixer::{DEFAULT_LATENCY_MS, DEFAULT_MIN_UPSTREAM_LATENCY_MS};

/// Mixer block builder.
pub struct MixerBuilder;

impl BlockBuilder for MixerBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_channels = parse_num_channels(properties);
        let num_aux_buses = parse_num_aux_buses(properties);
        let num_groups = parse_num_groups(properties);
        // Create input pads dynamically
        let inputs = (0..num_channels)
            .map(|i| ExternalPad {
                name: format!("input_{}", i + 1),
                label: Some(format!("A{}", i)),
                media_type: MediaType::Audio,
                internal_element_id: format!("convert_{}", i),
                internal_pad_name: "sink".to_string(),
            })
            .collect();

        // Output pads - point to output tees so unconnected outputs don't cause
        // NOT_LINKED flow errors. Each output tee has allow-not-linked=true.
        let mut outputs = vec![
            // Main stereo output
            ExternalPad {
                name: "main_out".to_string(),
                label: Some("Main".to_string()),
                media_type: MediaType::Audio,
                internal_element_id: "main_out_tee".to_string(),
                internal_pad_name: "src_%u".to_string(),
            },
            // PFL output (always present)
            ExternalPad {
                name: "pfl_out".to_string(),
                label: Some("PFL".to_string()),
                media_type: MediaType::Audio,
                internal_element_id: "pfl_out_tee".to_string(),
                internal_pad_name: "src_%u".to_string(),
            },
        ];

        // Add aux outputs
        for aux in 0..num_aux_buses {
            outputs.push(ExternalPad {
                name: format!("aux_out_{}", aux + 1),
                label: Some(format!("Aux{}", aux + 1)),
                media_type: MediaType::Audio,
                internal_element_id: format!("aux{}_out_tee", aux),
                internal_pad_name: "src_%u".to_string(),
            });
        }

        // Add group outputs
        for sg in 0..num_groups {
            outputs.push(ExternalPad {
                name: format!("group_out_{}", sg + 1),
                label: Some(format!("Grp{}", sg + 1)),
                media_type: MediaType::Audio,
                internal_element_id: format!("group{}_out_tee", sg),
                internal_pad_name: "src_%u".to_string(),
            });
        }

        Some(ExternalPads { inputs, outputs })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building Mixer block instance: {}", instance_id);

        let num_channels = parse_num_channels(properties);
        let num_aux_buses = parse_num_aux_buses(properties);
        let num_groups = parse_num_groups(properties);
        let dsp_backend = get_string_prop(properties, "dsp_backend", "rust");
        if dsp_backend != "rust" && dsp_backend != "lv2" {
            warn!(
                "Unrecognized dsp_backend '{}', falling back to 'rust'",
                dsp_backend
            );
        }
        let dsp_backend = if dsp_backend == "rust" || dsp_backend == "lv2" {
            dsp_backend
        } else {
            "rust"
        };
        let solo_mode_afl = get_string_prop(properties, "solo_mode", "pfl") == "afl";
        info!(
            "Mixer config: {} channels, {} aux buses, {} groups, solo={}, dsp={}",
            num_channels,
            num_aux_buses,
            num_groups,
            if solo_mode_afl { "afl" } else { "pfl" },
            dsp_backend,
        );

        let mut elements = Vec::new();
        let mut internal_links = Vec::new();

        // Mixer aggregator settings
        let force_live = get_bool_prop(properties, "force_live", true);
        let latency_ms = get_u64_prop(properties, "latency", DEFAULT_LATENCY_MS);
        let min_upstream_latency_ms = get_u64_prop(
            properties,
            "min_upstream_latency",
            DEFAULT_MIN_UPSTREAM_LATENCY_MS,
        );

        // ========================================================================
        // Create main audiomixer
        // ========================================================================
        let mixer_id = format!("{}:audiomixer", instance_id);
        let audiomixer =
            make_audiomixer(&mixer_id, force_live, latency_ms, min_upstream_latency_ms)?;
        elements.push((mixer_id.clone(), audiomixer.clone()));

        // ========================================================================
        // Main bus processing: comp → EQ → limiter
        // ========================================================================
        let main_comp_enabled = get_bool_prop(properties, "main_comp_enabled", false);
        let main_comp_threshold = get_float_prop(properties, "main_comp_threshold", -20.0);
        let main_comp_ratio = get_float_prop(properties, "main_comp_ratio", 4.0);
        let main_comp_attack = get_float_prop(properties, "main_comp_attack", 10.0);
        let main_comp_release = get_float_prop(properties, "main_comp_release", 100.0);
        let main_comp_makeup = get_float_prop(properties, "main_comp_makeup", 0.0);
        let main_comp_knee = get_float_prop(properties, "main_comp_knee", -6.0);

        let main_comp_id = format!("{}:main_comp", instance_id);
        let main_comp = make_compressor_element(
            &main_comp_id,
            main_comp_enabled,
            main_comp_threshold,
            main_comp_ratio,
            main_comp_attack,
            main_comp_release,
            main_comp_makeup,
            dsp_backend,
        )?;
        // Set knee - Rust backend uses "knee" (linear), LV2 uses "kn" (linear)
        if main_comp.find_property("knee").is_some() {
            let kn_val = db_to_linear(main_comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
            main_comp.set_property("knee", kn_val);
        } else if main_comp.find_property("kn").is_some() {
            let kn_val = db_to_linear(main_comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
            main_comp.set_property("kn", kn_val);
        }
        elements.push((main_comp_id.clone(), main_comp));

        let main_eq_enabled = get_bool_prop(properties, "main_eq_enabled", false);
        let main_eq_bands = [
            (
                get_float_prop(properties, "main_eq1_freq", 80.0),
                get_float_prop(properties, "main_eq1_gain", 0.0),
                get_float_prop(properties, "main_eq1_q", 1.0),
            ),
            (
                get_float_prop(properties, "main_eq2_freq", 400.0),
                get_float_prop(properties, "main_eq2_gain", 0.0),
                get_float_prop(properties, "main_eq2_q", 1.0),
            ),
            (
                get_float_prop(properties, "main_eq3_freq", 2000.0),
                get_float_prop(properties, "main_eq3_gain", 0.0),
                get_float_prop(properties, "main_eq3_q", 1.0),
            ),
            (
                get_float_prop(properties, "main_eq4_freq", 8000.0),
                get_float_prop(properties, "main_eq4_gain", 0.0),
                get_float_prop(properties, "main_eq4_q", 1.0),
            ),
        ];
        let main_eq_id = format!("{}:main_eq", instance_id);
        let main_eq = make_eq_element(&main_eq_id, main_eq_enabled, &main_eq_bands, dsp_backend)?;
        elements.push((main_eq_id.clone(), main_eq));

        let main_limiter_enabled = get_bool_prop(properties, "main_limiter_enabled", false);
        let main_limiter_threshold = get_float_prop(properties, "main_limiter_threshold", -3.0);
        let main_limiter_id = format!("{}:main_limiter", instance_id);
        let main_limiter = make_limiter_element(
            &main_limiter_id,
            main_limiter_enabled,
            main_limiter_threshold,
            dsp_backend,
        )?;
        elements.push((main_limiter_id.clone(), main_limiter));

        // Main output volume (master fader)
        let main_volume_id = format!("{}:main_volume", instance_id);
        let main_volume = gst::ElementFactory::make("volume")
            .name(&main_volume_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main volume: {}", e)))?;

        // Set main fader from properties, respecting mute state
        let main_fader = properties
            .get("main_fader")
            .and_then(|v| match v {
                PropertyValue::Float(f) => Some(*f),
                _ => None,
            })
            .unwrap_or(1.0);
        let main_mute = get_bool_prop(properties, "main_mute", false);
        let effective_main_volume = if main_mute { 0.0 } else { main_fader };
        main_volume.set_property("volume", effective_main_volume);
        elements.push((main_volume_id.clone(), main_volume));

        // Main level meter (for main mix metering)
        let main_level_id = format!("{}:main_level", instance_id);
        let main_level = gst::ElementFactory::make("level")
            .name(&main_level_id)
            .property("interval", METER_INTERVAL_NS) // 100ms
            .property("post-messages", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main level: {}", e)))?;
        elements.push((main_level_id.clone(), main_level));

        // Main output tee (allow-not-linked so unconnected main_out doesn't stall pipeline)
        let main_out_tee_id = format!("{}:main_out_tee", instance_id);
        let main_out_tee = gst::ElementFactory::make("tee")
            .name(&main_out_tee_id)
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("main_out_tee: {}", e)))?;
        elements.push((main_out_tee_id.clone(), main_out_tee));

        // Link: mixer → main_comp → main_eq → main_limiter → main_volume → main_level → main_out_tee
        internal_links.push((
            ElementPadRef::pad(&mixer_id, "src"),
            ElementPadRef::pad(&main_comp_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_comp_id, "src"),
            ElementPadRef::pad(&main_eq_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_eq_id, "src"),
            ElementPadRef::pad(&main_limiter_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_limiter_id, "src"),
            ElementPadRef::pad(&main_volume_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_volume_id, "src"),
            ElementPadRef::pad(&main_level_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&main_level_id, "src"),
            ElementPadRef::pad(&main_out_tee_id, "sink"),
        ));

        // ========================================================================
        // Create PFL (Pre-Fader Listen) bus with master level
        // ========================================================================
        let pfl_mixer_id = format!("{}:pfl_mixer", instance_id);
        let pfl_mixer = make_audiomixer(
            &pfl_mixer_id,
            force_live,
            latency_ms,
            min_upstream_latency_ms,
        )?;
        elements.push((pfl_mixer_id.clone(), pfl_mixer));

        // PFL master volume
        let pfl_master_vol_id = format!("{}:pfl_master_vol", instance_id);
        let pfl_master_level = get_float_prop(properties, "pfl_level", 1.0);
        let pfl_master_vol = gst::ElementFactory::make("volume")
            .name(&pfl_master_vol_id)
            .property("volume", pfl_master_level)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("pfl master vol: {}", e)))?;
        elements.push((pfl_master_vol_id.clone(), pfl_master_vol));

        let pfl_level_id = format!("{}:pfl_level", instance_id);
        let pfl_level = gst::ElementFactory::make("level")
            .name(&pfl_level_id)
            .property("interval", METER_INTERVAL_NS)
            .property("post-messages", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("pfl_level: {}", e)))?;
        elements.push((pfl_level_id.clone(), pfl_level));

        // PFL output tee (allow-not-linked so unconnected pfl_out doesn't stall pipeline)
        let pfl_out_tee_id = format!("{}:pfl_out_tee", instance_id);
        let pfl_out_tee = gst::ElementFactory::make("tee")
            .name(&pfl_out_tee_id)
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("pfl_out_tee: {}", e)))?;
        elements.push((pfl_out_tee_id.clone(), pfl_out_tee));

        // Link: pfl_mixer → pfl_master_vol → pfl_level → pfl_out_tee
        internal_links.push((
            ElementPadRef::pad(&pfl_mixer_id, "src"),
            ElementPadRef::pad(&pfl_master_vol_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&pfl_master_vol_id, "src"),
            ElementPadRef::pad(&pfl_level_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&pfl_level_id, "src"),
            ElementPadRef::pad(&pfl_out_tee_id, "sink"),
        ));

        // ========================================================================
        // Create Aux buses
        // ========================================================================
        for aux in 0..num_aux_buses {
            let aux_mixer_id = format!("{}:aux{}_mixer", instance_id, aux);
            let aux_mixer = make_audiomixer(
                &aux_mixer_id,
                force_live,
                latency_ms,
                min_upstream_latency_ms,
            )?;
            elements.push((aux_mixer_id.clone(), aux_mixer));

            let aux_fader = get_float_prop(properties, &format!("aux{}_fader", aux + 1), 1.0);
            let aux_mute = get_bool_prop(properties, &format!("aux{}_mute", aux + 1), false);
            let aux_volume_val = if aux_mute { 0.0 } else { aux_fader };

            let aux_volume_id = format!("{}:aux{}_volume", instance_id, aux);
            let aux_volume = gst::ElementFactory::make("volume")
                .name(&aux_volume_id)
                .property("volume", aux_volume_val)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("aux{}_volume: {}", aux, e))
                })?;
            elements.push((aux_volume_id.clone(), aux_volume));

            let aux_level_id = format!("{}:aux{}_level", instance_id, aux);
            let aux_level = gst::ElementFactory::make("level")
                .name(&aux_level_id)
                .property("interval", METER_INTERVAL_NS)
                .property("post-messages", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("aux{}_level: {}", aux, e))
                })?;
            elements.push((aux_level_id.clone(), aux_level));

            // Aux output tee (allow-not-linked so unconnected aux_out doesn't stall pipeline)
            let aux_out_tee_id = format!("{}:aux{}_out_tee", instance_id, aux);
            let aux_out_tee = gst::ElementFactory::make("tee")
                .name(&aux_out_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("aux{}_out_tee: {}", aux, e))
                })?;
            elements.push((aux_out_tee_id.clone(), aux_out_tee));

            // Link: aux_mixer → aux_volume → aux_level → aux_out_tee
            internal_links.push((
                ElementPadRef::pad(&aux_mixer_id, "src"),
                ElementPadRef::pad(&aux_volume_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&aux_volume_id, "src"),
                ElementPadRef::pad(&aux_level_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&aux_level_id, "src"),
                ElementPadRef::pad(&aux_out_tee_id, "sink"),
            ));
        }

        // ========================================================================
        // Create Groups
        // ========================================================================
        for sg in 0..num_groups {
            let sg_mixer_id = format!("{}:group{}_mixer", instance_id, sg);
            let sg_mixer = make_audiomixer(
                &sg_mixer_id,
                force_live,
                latency_ms,
                min_upstream_latency_ms,
            )?;
            elements.push((sg_mixer_id.clone(), sg_mixer));

            let sg_fader = get_float_prop(properties, &format!("group{}_fader", sg + 1), 1.0);
            let sg_mute = get_bool_prop(properties, &format!("group{}_mute", sg + 1), false);
            let sg_volume_val = if sg_mute { 0.0 } else { sg_fader };

            let sg_volume_id = format!("{}:group{}_volume", instance_id, sg);
            let sg_volume = gst::ElementFactory::make("volume")
                .name(&sg_volume_id)
                .property("volume", sg_volume_val)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_volume: {}", sg, e))
                })?;
            elements.push((sg_volume_id.clone(), sg_volume));

            let sg_level_id = format!("{}:group{}_level", instance_id, sg);
            let sg_level = gst::ElementFactory::make("level")
                .name(&sg_level_id)
                .property("interval", METER_INTERVAL_NS)
                .property("post-messages", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_level: {}", sg, e))
                })?;
            elements.push((sg_level_id.clone(), sg_level));

            // Group output tee - allows both external output AND feeding main mixer
            // Also prevents NOT_LINKED when group_out isn't connected externally.
            let sg_out_tee_id = format!("{}:group{}_out_tee", instance_id, sg);
            let sg_out_tee = gst::ElementFactory::make("tee")
                .name(&sg_out_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_out_tee: {}", sg, e))
                })?;
            elements.push((sg_out_tee_id.clone(), sg_out_tee));

            // Queue between group tee and main mixer (isolates scheduling)
            let sg_to_main_queue_id = format!("{}:group{}_to_main_queue", instance_id, sg);
            let sg_to_main_queue = gst::ElementFactory::make("queue")
                .name(&sg_to_main_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("group{}_to_main_queue: {}", sg, e))
                })?;
            elements.push((sg_to_main_queue_id.clone(), sg_to_main_queue));

            // Link: group_mixer → group_volume → group_level → group_out_tee
            //        group_out_tee → queue → main audiomixer
            internal_links.push((
                ElementPadRef::pad(&sg_mixer_id, "src"),
                ElementPadRef::pad(&sg_volume_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_volume_id, "src"),
                ElementPadRef::pad(&sg_level_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_level_id, "src"),
                ElementPadRef::pad(&sg_out_tee_id, "sink"),
            ));
            // One branch from tee feeds the main mixer
            internal_links.push((
                ElementPadRef::element(&sg_out_tee_id),
                ElementPadRef::pad(&sg_to_main_queue_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&sg_to_main_queue_id, "src"),
                ElementPadRef::element(&mixer_id), // Request pad from main audiomixer
            ));
        }

        // ========================================================================
        // Create per-channel processing
        // ========================================================================
        for ch in 0..num_channels {
            let ch_num = ch + 1; // 1-indexed for display

            // Get channel properties
            let gain_db = get_float_prop(properties, &format!("ch{}_gain", ch_num), 0.0);

            let pan = properties
                .get(&format!("ch{}_pan", ch_num))
                .and_then(|v| match v {
                    PropertyValue::Float(f) => Some(*f),
                    _ => None,
                })
                .unwrap_or(0.0);

            let fader = properties
                .get(&format!("ch{}_fader", ch_num))
                .and_then(|v| match v {
                    PropertyValue::Float(f) => Some(*f),
                    _ => None,
                })
                .unwrap_or(1.0); // Default 0 dB (unity)

            let mute = properties
                .get(&format!("ch{}_mute", ch_num))
                .and_then(|v| match v {
                    PropertyValue::Bool(b) => Some(*b),
                    _ => None,
                })
                .unwrap_or(false);

            // audioconvert (ensure proper format for processing)
            let convert_id = format!("{}:convert_{}", instance_id, ch);
            let convert = gst::ElementFactory::make("audioconvert")
                .name(&convert_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audioconvert ch{}: {}", ch_num, e))
                })?;
            elements.push((convert_id.clone(), convert));

            // capsfilter to ensure F32LE stereo format for LV2 plugins
            let caps_id = format!("{}:caps_{}", instance_id, ch);
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("channels", 2i32)
                .field("layout", "interleaved")
                .build();
            let capsfilter = gst::ElementFactory::make("capsfilter")
                .name(&caps_id)
                .property("caps", &caps)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("capsfilter ch{}: {}", ch_num, e))
                })?;
            elements.push((caps_id.clone(), capsfilter));

            // ----------------------------------------------------------------
            // Input gain stage
            // ----------------------------------------------------------------
            let gain_id = format!("{}:gain_{}", instance_id, ch);
            let gain_linear = db_to_linear(gain_db);
            let gain_elem = gst::ElementFactory::make("volume")
                .name(&gain_id)
                .property("volume", gain_linear)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("gain ch{}: {}", ch_num, e))
                })?;
            elements.push((gain_id.clone(), gain_elem));

            // ----------------------------------------------------------------
            // HPF (High-Pass Filter)
            // ----------------------------------------------------------------
            let hpf_enabled =
                get_bool_prop(properties, &format!("ch{}_hpf_enabled", ch_num), false);
            let hpf_freq = get_float_prop(properties, &format!("ch{}_hpf_freq", ch_num), 80.0);

            let hpf_id = format!("{}:hpf_{}", instance_id, ch);
            let hpf = make_hpf_element(&hpf_id, hpf_enabled, hpf_freq)?;
            elements.push((hpf_id.clone(), hpf));

            // ----------------------------------------------------------------
            // Gate (LSP Gate Stereo with fallback)
            // ----------------------------------------------------------------
            let gate_enabled =
                get_bool_prop(properties, &format!("ch{}_gate_enabled", ch_num), false);
            let gate_threshold =
                get_float_prop(properties, &format!("ch{}_gate_threshold", ch_num), -40.0);
            let gate_attack = get_float_prop(properties, &format!("ch{}_gate_attack", ch_num), 5.0);
            let gate_release =
                get_float_prop(properties, &format!("ch{}_gate_release", ch_num), 100.0);
            let gate_id = format!("{}:gate_{}", instance_id, ch);
            let gate = make_gate_element(
                &gate_id,
                gate_enabled,
                gate_threshold,
                gate_attack,
                gate_release,
                dsp_backend,
            )?;
            elements.push((gate_id.clone(), gate));

            // ----------------------------------------------------------------
            // Compressor (LSP Compressor Stereo with fallback)
            // ----------------------------------------------------------------
            let comp_enabled =
                get_bool_prop(properties, &format!("ch{}_comp_enabled", ch_num), false);
            let comp_threshold =
                get_float_prop(properties, &format!("ch{}_comp_threshold", ch_num), -20.0);
            let comp_ratio = get_float_prop(properties, &format!("ch{}_comp_ratio", ch_num), 4.0);
            let comp_attack =
                get_float_prop(properties, &format!("ch{}_comp_attack", ch_num), 10.0);
            let comp_release =
                get_float_prop(properties, &format!("ch{}_comp_release", ch_num), 100.0);
            let comp_makeup = get_float_prop(properties, &format!("ch{}_comp_makeup", ch_num), 0.0);
            let comp_knee = get_float_prop(properties, &format!("ch{}_comp_knee", ch_num), -6.0);

            let comp_id = format!("{}:comp_{}", instance_id, ch);
            let compressor = make_compressor_element(
                &comp_id,
                comp_enabled,
                comp_threshold,
                comp_ratio,
                comp_attack,
                comp_release,
                comp_makeup,
                dsp_backend,
            )?;
            // Set knee - Rust backend uses "knee" (linear), LV2 uses "kn" (linear)
            // kn range: 0.0631..1.0 (linear gain, default ~0.5 = -6dB)
            if compressor.find_property("knee").is_some() {
                let kn_val = db_to_linear(comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
                compressor.set_property("knee", kn_val);
            } else if compressor.find_property("kn").is_some() {
                let kn_val = db_to_linear(comp_knee).clamp(MIN_KNEE_LINEAR, 1.0) as f32;
                compressor.set_property("kn", kn_val);
            }
            elements.push((comp_id.clone(), compressor));

            // ----------------------------------------------------------------
            // EQ (LSP Parametric Equalizer x8 Stereo with fallback)
            // ----------------------------------------------------------------
            let eq_enabled = get_bool_prop(properties, &format!("ch{}_eq_enabled", ch_num), false);

            let eq_defaults: [(f64, f64); 4] =
                [(80.0, 1.0), (400.0, 1.0), (2000.0, 1.0), (8000.0, 1.0)];
            let eq_bands: [(f64, f64, f64); 4] = std::array::from_fn(|band| {
                let (def_freq, def_q) = eq_defaults[band];
                let freq = get_float_prop(
                    properties,
                    &format!("ch{}_eq{}_freq", ch_num, band + 1),
                    def_freq,
                );
                let gain = get_float_prop(
                    properties,
                    &format!("ch{}_eq{}_gain", ch_num, band + 1),
                    0.0,
                );
                let q =
                    get_float_prop(properties, &format!("ch{}_eq{}_q", ch_num, band + 1), def_q);
                (freq, gain, q)
            });

            let eq_id = format!("{}:eq_{}", instance_id, ch);
            let eq = make_eq_element(&eq_id, eq_enabled, &eq_bands, dsp_backend)?;
            elements.push((eq_id.clone(), eq));

            // ----------------------------------------------------------------
            // Pre-fader tee (for PFL tap and pre-fader aux sends)
            // ----------------------------------------------------------------
            let pre_fader_tee_id = format!("{}:pre_fader_tee_{}", instance_id, ch);
            let pre_fader_tee = gst::ElementFactory::make("tee")
                .name(&pre_fader_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("pre_fader_tee ch{}: {}", ch_num, e))
                })?;
            elements.push((pre_fader_tee_id.clone(), pre_fader_tee));

            // audiopanorama (pan control)
            let pan_id = format!("{}:pan_{}", instance_id, ch);
            let panorama = gst::ElementFactory::make("audiopanorama")
                .name(&pan_id)
                .property("panorama", pan as f32)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audiopanorama ch{}: {}", ch_num, e))
                })?;
            elements.push((pan_id.clone(), panorama));

            // volume (channel fader + mute)
            let volume_id = format!("{}:volume_{}", instance_id, ch);
            let effective_volume = if mute { 0.0 } else { fader };
            let volume = gst::ElementFactory::make("volume")
                .name(&volume_id)
                .property("volume", effective_volume)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("volume ch{}: {}", ch_num, e))
                })?;
            elements.push((volume_id.clone(), volume));

            // ----------------------------------------------------------------
            // Post-fader tee (for post-fader aux sends)
            // ----------------------------------------------------------------
            let post_fader_tee_id = format!("{}:post_fader_tee_{}", instance_id, ch);
            let post_fader_tee = gst::ElementFactory::make("tee")
                .name(&post_fader_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("post_fader_tee ch{}: {}", ch_num, e))
                })?;
            elements.push((post_fader_tee_id.clone(), post_fader_tee));

            // level (metering)
            let level_id = format!("{}:level_{}", instance_id, ch);
            let level = gst::ElementFactory::make("level")
                .name(&level_id)
                .property("interval", METER_INTERVAL_NS) // 100ms
                .property("post-messages", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("level ch{}: {}", ch_num, e))
                })?;
            elements.push((level_id.clone(), level));

            // ----------------------------------------------------------------
            // Routing tee (after level, for multi-destination routing)
            // ----------------------------------------------------------------
            let routing_tee_id = format!("{}:routing_tee_{}", instance_id, ch);
            let routing_tee = gst::ElementFactory::make("tee")
                .name(&routing_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("routing_tee ch{}: {}", ch_num, e))
                })?;
            elements.push((routing_tee_id.clone(), routing_tee));

            // ----------------------------------------------------------------
            // PFL path (pre-fader listen)
            // ----------------------------------------------------------------
            let pfl_enabled = get_bool_prop(properties, &format!("ch{}_pfl", ch_num), false);

            let pfl_volume_id = format!("{}:pfl_volume_{}", instance_id, ch);
            let pfl_volume = gst::ElementFactory::make("volume")
                .name(&pfl_volume_id)
                .property("volume", if pfl_enabled { 1.0 } else { 0.0 })
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("pfl_volume ch{}: {}", ch_num, e))
                })?;
            elements.push((pfl_volume_id.clone(), pfl_volume));

            let pfl_queue_id = format!("{}:pfl_queue_{}", instance_id, ch);
            let pfl_queue = gst::ElementFactory::make("queue")
                .name(&pfl_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("pfl_queue ch{}: {}", ch_num, e))
                })?;
            elements.push((pfl_queue_id.clone(), pfl_queue));

            // ----------------------------------------------------------------
            // Aux send paths (pre or post fader)
            // ----------------------------------------------------------------
            for aux in 0..num_aux_buses {
                let aux_send_level = get_float_prop(
                    properties,
                    &format!("ch{}_aux{}_level", ch_num, aux + 1),
                    0.0,
                );
                let aux_pre = get_bool_prop(
                    properties,
                    &format!("ch{}_aux{}_pre", ch_num, aux + 1),
                    aux < 2, // Default: aux 1-2 pre-fader, aux 3-4 post-fader
                );

                let aux_send_id = format!("{}:aux_send_{}_{}", instance_id, ch, aux);
                let aux_send = gst::ElementFactory::make("volume")
                    .name(&aux_send_id)
                    .property("volume", aux_send_level)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!(
                            "aux_send ch{} aux{}: {}",
                            ch_num,
                            aux + 1,
                            e
                        ))
                    })?;
                elements.push((aux_send_id.clone(), aux_send));

                let aux_queue_id = format!("{}:aux_queue_{}_{}", instance_id, ch, aux);
                let aux_queue = gst::ElementFactory::make("queue")
                    .name(&aux_queue_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!(
                            "aux_queue ch{} aux{}: {}",
                            ch_num,
                            aux + 1,
                            e
                        ))
                    })?;
                elements.push((aux_queue_id.clone(), aux_queue));

                // Source tee depends on pre/post setting
                let source_tee_id = if aux_pre {
                    &pre_fader_tee_id
                } else {
                    &post_fader_tee_id
                };

                // Link: (pre|post)_fader_tee → aux_send → aux_queue → aux_mixer
                let aux_mixer_id = format!("{}:aux{}_mixer", instance_id, aux);
                internal_links.push((
                    ElementPadRef::element(source_tee_id), // Request pad from tee
                    ElementPadRef::pad(&aux_send_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&aux_send_id, "src"),
                    ElementPadRef::pad(&aux_queue_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&aux_queue_id, "src"),
                    ElementPadRef::element(&aux_mixer_id), // Request pad from aux_mixer
                ));
            }

            // ----------------------------------------------------------------
            // Main chain links
            // ----------------------------------------------------------------
            // Chain: convert → caps → gain → hpf → gate → comp → eq → pre_fader_tee
            internal_links.push((
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&caps_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&caps_id, "src"),
                ElementPadRef::pad(&gain_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&gain_id, "src"),
                ElementPadRef::pad(&hpf_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&hpf_id, "src"),
                ElementPadRef::pad(&gate_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&gate_id, "src"),
                ElementPadRef::pad(&comp_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&comp_id, "src"),
                ElementPadRef::pad(&eq_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&eq_id, "src"),
                ElementPadRef::pad(&pre_fader_tee_id, "sink"),
            ));

            // pre_fader_tee → pan → volume → post_fader_tee → level → routing_tee
            internal_links.push((
                ElementPadRef::element(&pre_fader_tee_id), // Request pad from tee
                ElementPadRef::pad(&pan_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&pan_id, "src"),
                ElementPadRef::pad(&volume_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&volume_id, "src"),
                ElementPadRef::pad(&post_fader_tee_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::element(&post_fader_tee_id), // Request pad from tee
                ElementPadRef::pad(&level_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&level_id, "src"),
                ElementPadRef::pad(&routing_tee_id, "sink"),
            ));

            // Solo path: PFL (pre-fader) or AFL (post-fader) based on solo_mode
            let solo_source_tee_id = if solo_mode_afl {
                &post_fader_tee_id
            } else {
                &pre_fader_tee_id
            };
            internal_links.push((
                ElementPadRef::element(solo_source_tee_id),
                ElementPadRef::pad(&pfl_volume_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&pfl_volume_id, "src"),
                ElementPadRef::pad(&pfl_queue_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&pfl_queue_id, "src"),
                ElementPadRef::element(&pfl_mixer_id), // Request pad from pfl_mixer
            ));

            // ----------------------------------------------------------------
            // Multi-destination routing (Main + Groups)
            // Each destination has a volume element to enable/disable routing
            // ----------------------------------------------------------------

            // Route to main mixer
            let to_main_enabled = get_bool_prop(properties, &format!("ch{}_to_main", ch_num), true);
            let to_main_vol_id = format!("{}:to_main_vol_{}", instance_id, ch);
            let to_main_vol = gst::ElementFactory::make("volume")
                .name(&to_main_vol_id)
                .property("volume", if to_main_enabled { 1.0 } else { 0.0 })
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("to_main_vol ch{}: {}", ch_num, e))
                })?;
            elements.push((to_main_vol_id.clone(), to_main_vol));

            let to_main_queue_id = format!("{}:to_main_queue_{}", instance_id, ch);
            let to_main_queue = gst::ElementFactory::make("queue")
                .name(&to_main_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("to_main_queue ch{}: {}", ch_num, e))
                })?;
            elements.push((to_main_queue_id.clone(), to_main_queue));

            // Link: routing_tee → to_main_vol → to_main_queue → main_mixer
            internal_links.push((
                ElementPadRef::element(&routing_tee_id),
                ElementPadRef::pad(&to_main_vol_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&to_main_vol_id, "src"),
                ElementPadRef::pad(&to_main_queue_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&to_main_queue_id, "src"),
                ElementPadRef::element(&mixer_id),
            ));

            // Route to groups
            for sg in 0..num_groups {
                let to_grp_enabled =
                    get_bool_prop(properties, &format!("ch{}_to_grp{}", ch_num, sg + 1), false);

                let to_grp_vol_id = format!("{}:to_grp{}_vol_{}", instance_id, sg, ch);
                let to_grp_vol = gst::ElementFactory::make("volume")
                    .name(&to_grp_vol_id)
                    .property("volume", if to_grp_enabled { 1.0 } else { 0.0 })
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!(
                            "to_grp{}_vol ch{}: {}",
                            sg + 1,
                            ch_num,
                            e
                        ))
                    })?;
                elements.push((to_grp_vol_id.clone(), to_grp_vol));

                let to_grp_queue_id = format!("{}:to_grp{}_queue_{}", instance_id, sg, ch);
                let to_grp_queue = gst::ElementFactory::make("queue")
                    .name(&to_grp_queue_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!(
                            "to_grp{}_queue ch{}: {}",
                            sg + 1,
                            ch_num,
                            e
                        ))
                    })?;
                elements.push((to_grp_queue_id.clone(), to_grp_queue));

                // Link: routing_tee → to_grp_vol → to_grp_queue → group_mixer
                let sg_mixer_id = format!("{}:group{}_mixer", instance_id, sg);
                internal_links.push((
                    ElementPadRef::element(&routing_tee_id),
                    ElementPadRef::pad(&to_grp_vol_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&to_grp_vol_id, "src"),
                    ElementPadRef::pad(&to_grp_queue_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&to_grp_queue_id, "src"),
                    ElementPadRef::element(&sg_mixer_id),
                ));

                if to_grp_enabled {
                    debug!("Channel {} routed to group {}", ch_num, sg + 1);
                }
            }

            debug!(
                "Channel {} created: gain={:.1}dB, pan={}, fader={}, mute={}, pfl={}, to_main={}",
                ch_num, gain_db, pan, fader, mute, pfl_enabled, to_main_enabled
            );
        }

        info!("Mixer block created with {} channels", num_channels);

        // Create bus message handler for metering
        let handler_instance_id = instance_id.to_string();
        let bus_message_handler = Some(Box::new(
            move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                connect_mixer_meter_handler(bus, flow_id, events, handler_instance_id.clone())
            },
        ) as crate::blocks::BusMessageConnectFn);

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler,
            pad_properties: HashMap::new(),
        })
    }
}
