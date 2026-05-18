use strom_types::mixer::*;
use strom_types::{block::*, EnumValue, MediaType, PropertyValue};

/// Get metadata for Mixer block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![mixer_definition()]
}

/// Get Mixer block definition (metadata only).
pub(super) fn mixer_definition() -> BlockDefinition {
    // Generate channel properties
    let mut exposed_properties = vec![
        // Global: number of channels (1..=MAX_CHANNELS, clamped on parse)
        ExposedProperty {
            name: "num_channels".to_string(),
            label: "Channels".to_string(),
            description: format!("Number of input channels (1 to {})", MAX_CHANNELS),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(DEFAULT_CHANNELS as u64)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_channels".to_string(),
                transform: None,
            },
            live: false,
        },
        // DSP Backend selection
        ExposedProperty {
            name: "dsp_backend".to_string(),
            label: "DSP Backend".to_string(),
            description: "LV2 uses external C++ LSP plugins, Rust uses built-in lsp-plugins-rs"
                .to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue {
                        value: "rust".to_string(),
                        label: Some("Rust".to_string()),
                    },
                    EnumValue {
                        value: "lv2".to_string(),
                        label: Some("LV2".to_string()),
                    },
                ],
            },
            default_value: Some(PropertyValue::String("rust".to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "dsp_backend".to_string(),
                transform: None,
            },
            live: false,
        },
        // Main fader
        ExposedProperty {
            name: "main_fader".to_string(),
            label: "Main Fader".to_string(),
            description: "Main output level (0.0 to 2.0)".to_string(),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_FADER as f64)),
            mapping: PropertyMapping {
                element_id: "main_volume".to_string(),
                property_name: "volume".to_string(),
                transform: None,
            },
            live: false,
        },
        // Number of aux buses (0..=MAX_AUX_BUSES, clamped on parse)
        ExposedProperty {
            name: "num_aux_buses".to_string(),
            label: "Aux Buses".to_string(),
            description: format!("Number of aux send buses (0 to {})", MAX_AUX_BUSES),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(0)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_aux_buses".to_string(),
                transform: None,
            },
            live: false,
        },
        // Number of groups (0..=MAX_GROUPS, clamped on parse)
        ExposedProperty {
            name: "num_groups".to_string(),
            label: "Groups".to_string(),
            description: format!("Number of group buses (0 to {})", MAX_GROUPS),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(0)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_groups".to_string(),
                transform: None,
            },
            live: false,
        },
        // PFL master level
        ExposedProperty {
            name: "pfl_level".to_string(),
            label: "PFL Level".to_string(),
            description: "PFL/AFL bus master level (0.0 to 2.0)".to_string(),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(1.0)),
            mapping: PropertyMapping {
                element_id: "pfl_master_vol".to_string(),
                property_name: "volume".to_string(),
                transform: None,
            },
            live: false,
        },
        // Solo mode (PFL or AFL)
        ExposedProperty {
            name: "solo_mode".to_string(),
            label: "Solo Mode".to_string(),
            description: "Solo listen mode: PFL (pre-fader) or AFL (after-fader)".to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue {
                        value: "pfl".to_string(),
                        label: Some("PFL".to_string()),
                    },
                    EnumValue {
                        value: "afl".to_string(),
                        label: Some("AFL".to_string()),
                    },
                ],
            },
            default_value: Some(PropertyValue::String("pfl".to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "solo_mode".to_string(),
                transform: None,
            },
            live: false,
        },
    ];

    // ========================================================================
    // Aggregator / live mode properties
    // ========================================================================
    exposed_properties.push(ExposedProperty {
        name: "force_live".to_string(),
        label: "Force Live".to_string(),
        description: "Always operate in live mode. Prevents mixer from hanging when not all inputs are connected. Construction-time only.".to_string(),
        property_type: PropertyType::Bool,
        default_value: Some(PropertyValue::Bool(true)),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "force_live".to_string(),
            transform: None,
        },
        live: false,
    });
    exposed_properties.push(ExposedProperty {
        name: "latency".to_string(),
        label: "Latency".to_string(),
        description: "Mixer aggregator latency in milliseconds. Time to wait for slower inputs before producing output. Construction-time only.".to_string(),
        property_type: PropertyType::UInt,
        default_value: Some(PropertyValue::UInt(DEFAULT_LATENCY_MS)),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "latency".to_string(),
            transform: None,
        },
        live: false,
    });
    exposed_properties.push(ExposedProperty {
        name: "min_upstream_latency".to_string(),
        label: "Min Upstream Latency".to_string(),
        description: "Minimum upstream latency reported to upstream elements in milliseconds. Construction-time only.".to_string(),
        property_type: PropertyType::UInt,
        default_value: Some(PropertyValue::UInt(DEFAULT_MIN_UPSTREAM_LATENCY_MS)),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "min_upstream_latency".to_string(),
            transform: None,
        },
        live: false,
    });

    // ========================================================================
    // Main bus processing properties
    // ========================================================================
    exposed_properties.push(ExposedProperty {
        name: "main_comp_enabled".to_string(),
        label: "Main Comp".to_string(),
        description: "Enable compressor on main bus".to_string(),
        property_type: PropertyType::Bool,
        default_value: Some(PropertyValue::Bool(false)),
        mapping: PropertyMapping {
            element_id: "main_comp".to_string(),
            property_name: "enabled".to_string(),
            transform: None,
        },
        live: false,
    });
    for (prop_suffix, label, gst_prop, default, desc, transform) in [
        (
            "main_comp_threshold",
            "Main Comp Thresh",
            "al",
            DEFAULT_COMP_THRESHOLD as f64,
            "Main bus compressor threshold in dB (-60 to 0)",
            Some("db_to_linear"),
        ),
        (
            "main_comp_ratio",
            "Main Comp Ratio",
            "cr",
            DEFAULT_COMP_RATIO as f64,
            "Main bus compressor ratio (1:1 to 20:1)",
            None,
        ),
        (
            "main_comp_attack",
            "Main Comp Atk",
            "at",
            DEFAULT_COMP_ATTACK as f64,
            "Main bus compressor attack in ms (0-200)",
            None,
        ),
        (
            "main_comp_release",
            "Main Comp Rel",
            "rt",
            DEFAULT_COMP_RELEASE as f64,
            "Main bus compressor release in ms (10-1000)",
            None,
        ),
        (
            "main_comp_makeup",
            "Main Comp Makeup",
            "mk",
            DEFAULT_COMP_MAKEUP as f64,
            "Main bus compressor makeup gain in dB (0 to 24)",
            Some("db_to_linear"),
        ),
    ] {
        exposed_properties.push(ExposedProperty {
            name: prop_suffix.to_string(),
            label: label.to_string(),
            description: desc.to_string(),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(default)),
            mapping: PropertyMapping {
                element_id: "main_comp".to_string(),
                property_name: gst_prop.to_string(),
                transform: transform.map(|s| s.to_string()),
            },
            live: false,
        });
    }

    // Main EQ
    exposed_properties.push(ExposedProperty {
        name: "main_eq_enabled".to_string(),
        label: "Main EQ".to_string(),
        description: "Enable parametric EQ on main bus".to_string(),
        property_type: PropertyType::Bool,
        default_value: Some(PropertyValue::Bool(false)),
        mapping: PropertyMapping {
            element_id: "main_eq".to_string(),
            property_name: "enabled".to_string(),
            transform: None,
        },
        live: false,
    });
    let eq_band_names = ["Low", "Low-Mid", "Hi-Mid", "High"];
    for (band, band_name) in eq_band_names.iter().enumerate() {
        let def_freq = DEFAULT_EQ_BANDS[band].0 as f64;
        let band_num = band + 1;
        exposed_properties.push(ExposedProperty {
            name: format!("main_eq{}_freq", band_num),
            label: format!("Main EQ{} Freq", band_num),
            description: format!(
                "Main bus EQ band {} ({}) frequency in Hz",
                band_num, band_name
            ),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(def_freq)),
            mapping: PropertyMapping {
                element_id: "main_eq".to_string(),
                property_name: format!("f-{}", band),
                transform: None,
            },
            live: false,
        });
        exposed_properties.push(ExposedProperty {
            name: format!("main_eq{}_gain", band_num),
            label: format!("Main EQ{} Gain", band_num),
            description: format!("Main bus EQ band {} gain in dB (-15 to +15)", band_num),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(0.0)),
            mapping: PropertyMapping {
                element_id: "main_eq".to_string(),
                property_name: format!("g-{}", band),
                transform: Some("db_to_linear".to_string()),
            },
            live: false,
        });
        exposed_properties.push(ExposedProperty {
            name: format!("main_eq{}_q", band_num),
            label: format!("Main EQ{} Q", band_num),
            description: format!("Main bus EQ band {} Q factor (0.1 to 10)", band_num),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(1.0)),
            mapping: PropertyMapping {
                element_id: "main_eq".to_string(),
                property_name: format!("q-{}", band),
                transform: None,
            },
            live: false,
        });
    }

    // Main limiter
    exposed_properties.push(ExposedProperty {
        name: "main_limiter_enabled".to_string(),
        label: "Main Limiter".to_string(),
        description: "Enable limiter on main bus".to_string(),
        property_type: PropertyType::Bool,
        default_value: Some(PropertyValue::Bool(false)),
        mapping: PropertyMapping {
            element_id: "main_limiter".to_string(),
            property_name: "enabled".to_string(),
            transform: None,
        },
        live: false,
    });
    exposed_properties.push(ExposedProperty {
        name: "main_limiter_threshold".to_string(),
        label: "Main Lim Thresh".to_string(),
        description: "Main bus limiter threshold in dB (-20 to 0)".to_string(),
        property_type: PropertyType::Float,
        default_value: Some(PropertyValue::Float(DEFAULT_LIMITER_THRESHOLD as f64)),
        mapping: PropertyMapping {
            element_id: "main_limiter".to_string(),
            property_name: "th".to_string(),
            transform: Some("db_to_linear".to_string()),
        },
        live: false,
    });

    // Add aux bus master properties
    for aux in 1..=MAX_AUX_BUSES {
        exposed_properties.push(ExposedProperty {
            name: format!("aux{}_fader", aux),
            label: format!("Aux {} Fader", aux),
            description: format!("Aux bus {} master level (0.0 to 2.0)", aux),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(1.0)),
            mapping: PropertyMapping {
                element_id: format!("aux{}_volume", aux - 1),
                property_name: "volume".to_string(),
                transform: None,
            },
            live: false,
        });
        exposed_properties.push(ExposedProperty {
            name: format!("aux{}_mute", aux),
            label: format!("Aux {} Mute", aux),
            description: format!("Mute aux bus {}", aux),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("aux{}_mute", aux),
                transform: None,
            },
            live: false,
        });
    }

    // Add group properties
    for sg in 1..=MAX_GROUPS {
        exposed_properties.push(ExposedProperty {
            name: format!("group{}_fader", sg),
            label: format!("Group {} Fader", sg),
            description: format!("Group {} level (0.0 to 2.0)", sg),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(1.0)),
            mapping: PropertyMapping {
                element_id: format!("group{}_volume", sg - 1),
                property_name: "volume".to_string(),
                transform: None,
            },
            live: false,
        });
        exposed_properties.push(ExposedProperty {
            name: format!("group{}_mute", sg),
            label: format!("Group {} Mute", sg),
            description: format!("Mute group {}", sg),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("group{}_mute", sg),
                transform: None,
            },
            live: false,
        });
    }

    // Add per-channel properties (we'll generate for max channels, UI will show based on num_channels)
    for ch in 1..=MAX_CHANNELS {
        // Channel label
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_label", ch),
            label: format!("Ch {} Label", ch),
            description: format!("Channel {} display name", ch),
            property_type: PropertyType::String,
            default_value: Some(PropertyValue::String(format!("Ch {}", ch))),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("ch{}_label", ch),
                transform: None,
            },
            live: false,
        });

        // Input gain
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_gain", ch),
            label: format!("Ch {} Gain", ch),
            description: format!("Channel {} input gain in dB (-20 to +20)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(0.0)),
            mapping: PropertyMapping {
                element_id: format!("gain_{}", ch - 1),
                property_name: "volume".to_string(),
                transform: Some("db_to_linear".to_string()),
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_pan", ch),
            label: format!("Ch {} Pan", ch),
            description: format!("Channel {} pan (-1.0=L, 0.0=C, 1.0=R)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(0.0)),
            mapping: PropertyMapping {
                element_id: format!("pan_{}", ch - 1),
                property_name: "panorama".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_fader", ch),
            label: format!("Ch {} Fader", ch),
            description: format!("Channel {} volume (0.0 to 2.0)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(1.0)),
            mapping: PropertyMapping {
                element_id: format!("volume_{}", ch - 1),
                property_name: "volume".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_mute", ch),
            label: format!("Ch {} Mute", ch),
            description: format!("Mute channel {}", ch),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("ch{}_mute", ch),
                transform: None,
            },
            live: false,
        });

        // PFL (Pre-Fader Listen)
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_pfl", ch),
            label: format!("Ch {} PFL", ch),
            description: format!("Enable PFL (Pre-Fader Listen) on channel {}", ch),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: format!("pfl_volume_{}", ch - 1),
                property_name: "volume".to_string(),
                transform: Some("bool_to_volume".to_string()),
            },
            live: false,
        });

        // Routing to main
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_to_main", ch),
            label: format!("Ch {} -> Main", ch),
            description: format!("Route channel {} to main mix", ch),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(true)),
            mapping: PropertyMapping {
                element_id: format!("to_main_vol_{}", ch - 1),
                property_name: "volume".to_string(),
                transform: Some("bool_to_volume".to_string()),
            },
            live: false,
        });

        // Routing to groups
        for sg in 1..=MAX_GROUPS {
            exposed_properties.push(ExposedProperty {
                name: format!("ch{}_to_grp{}", ch, sg),
                label: format!("Ch {} -> SG{}", ch, sg),
                description: format!("Route channel {} to group {}", ch, sg),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: format!("to_grp{}_vol_{}", sg - 1, ch - 1),
                    property_name: "volume".to_string(),
                    transform: Some("bool_to_volume".to_string()),
                },
                live: false,
            });
        }

        // Aux send levels and pre/post toggle (per aux bus)
        for aux in 1..=MAX_AUX_BUSES {
            exposed_properties.push(ExposedProperty {
                name: format!("ch{}_aux{}_level", ch, aux),
                label: format!("Ch {} Aux {} Send", ch, aux),
                description: format!("Channel {} send level to aux bus {} (0.0 to 2.0)", ch, aux),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(0.0)),
                mapping: PropertyMapping {
                    element_id: format!("aux_send_{}_{}", ch - 1, aux - 1),
                    property_name: "volume".to_string(),
                    transform: None,
                },
                live: false,
            });
            exposed_properties.push(ExposedProperty {
                name: format!("ch{}_aux{}_pre", ch, aux),
                label: format!("Ch {} Aux {} Pre", ch, aux),
                description: format!(
                    "Channel {} aux {} pre-fader (true) or post-fader (false)",
                    ch, aux
                ),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)), // all aux sends default post-fader
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: format!("ch{}_aux{}_pre", ch, aux),
                    transform: None,
                },
                live: false,
            });
        }

        // ============================================================
        // HPF properties
        // ============================================================
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_hpf_enabled", ch),
            label: format!("Ch {} HPF", ch),
            description: format!("Enable high-pass filter on channel {}", ch),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("ch{}_hpf_enabled", ch),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_hpf_freq", ch),
            label: format!("Ch {} HPF Freq", ch),
            description: format!(
                "Channel {} high-pass filter cutoff frequency in Hz (20-500)",
                ch
            ),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_HPF_FREQ as f64)),
            mapping: PropertyMapping {
                element_id: format!("hpf_{}", ch - 1),
                property_name: "cutoff".to_string(),
                transform: None,
            },
            live: false,
        });

        // ============================================================
        // Gate properties
        // ============================================================
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_gate_enabled", ch),
            label: format!("Ch {} Gate", ch),
            description: format!("Enable gate on channel {}", ch),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: format!("gate_{}", ch - 1),
                property_name: "enabled".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_gate_threshold", ch),
            label: format!("Ch {} Gate Thresh", ch),
            description: format!("Channel {} gate threshold in dB (-60 to 0)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_GATE_THRESHOLD as f64)),
            mapping: PropertyMapping {
                element_id: format!("gate_{}", ch - 1),
                property_name: "gt".to_string(),
                transform: Some("db_to_linear".to_string()),
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_gate_attack", ch),
            label: format!("Ch {} Gate Atk", ch),
            description: format!("Channel {} gate attack in ms (0-200)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_GATE_ATTACK as f64)),
            mapping: PropertyMapping {
                element_id: format!("gate_{}", ch - 1),
                property_name: "at".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_gate_release", ch),
            label: format!("Ch {} Gate Rel", ch),
            description: format!("Channel {} gate release in ms (10-1000)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_GATE_RELEASE as f64)),
            mapping: PropertyMapping {
                element_id: format!("gate_{}", ch - 1),
                property_name: "rt".to_string(),
                transform: None,
            },
            live: false,
        });

        // Note: LSP gate has no settable range property
        // ("rr" doesn't exist, "gr" is a read-only reduction meter)

        // ============================================================
        // Compressor properties
        // ============================================================
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_comp_enabled", ch),
            label: format!("Ch {} Comp", ch),
            description: format!("Enable compressor on channel {}", ch),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: format!("comp_{}", ch - 1),
                property_name: "enabled".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_comp_threshold", ch),
            label: format!("Ch {} Comp Thresh", ch),
            description: format!("Channel {} compressor threshold in dB (-60 to 0)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_COMP_THRESHOLD as f64)),
            mapping: PropertyMapping {
                element_id: format!("comp_{}", ch - 1),
                property_name: "al".to_string(),
                transform: Some("db_to_linear".to_string()),
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_comp_ratio", ch),
            label: format!("Ch {} Comp Ratio", ch),
            description: format!("Channel {} compressor ratio (1:1 to 20:1)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_COMP_RATIO as f64)),
            mapping: PropertyMapping {
                element_id: format!("comp_{}", ch - 1),
                property_name: "cr".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_comp_attack", ch),
            label: format!("Ch {} Comp Atk", ch),
            description: format!("Channel {} compressor attack in ms (0-200)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_COMP_ATTACK as f64)),
            mapping: PropertyMapping {
                element_id: format!("comp_{}", ch - 1),
                property_name: "at".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_comp_release", ch),
            label: format!("Ch {} Comp Rel", ch),
            description: format!("Channel {} compressor release in ms (10-1000)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_COMP_RELEASE as f64)),
            mapping: PropertyMapping {
                element_id: format!("comp_{}", ch - 1),
                property_name: "rt".to_string(),
                transform: None,
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_comp_makeup", ch),
            label: format!("Ch {} Comp Makeup", ch),
            description: format!("Channel {} compressor makeup gain in dB (0 to 24)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_COMP_MAKEUP as f64)),
            mapping: PropertyMapping {
                element_id: format!("comp_{}", ch - 1),
                property_name: "mk".to_string(),
                transform: Some("db_to_linear".to_string()),
            },
            live: false,
        });

        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_comp_knee", ch),
            label: format!("Ch {} Comp Knee", ch),
            description: format!("Channel {} compressor knee in dB (-24 to 0)", ch),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(DEFAULT_COMP_KNEE as f64)),
            mapping: PropertyMapping {
                element_id: format!("comp_{}", ch - 1),
                property_name: "kn".to_string(),
                transform: Some("db_to_linear".to_string()),
            },
            live: false,
        });

        // ============================================================
        // EQ properties - 4 bands
        // ============================================================
        exposed_properties.push(ExposedProperty {
            name: format!("ch{}_eq_enabled", ch),
            label: format!("Ch {} EQ", ch),
            description: format!("Enable parametric EQ on channel {}", ch),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: format!("eq_{}", ch - 1),
                property_name: "enabled".to_string(),
                transform: None,
            },
            live: false,
        });

        // 4 EQ bands with default frequencies from shared constants
        let ch_eq_band_names = ["Low", "Low-Mid", "Hi-Mid", "High"];
        for (band, band_name) in ch_eq_band_names.iter().enumerate() {
            let def_freq = DEFAULT_EQ_BANDS[band].0 as f64;
            let band_num = band + 1;

            exposed_properties.push(ExposedProperty {
                name: format!("ch{}_eq{}_freq", ch, band_num),
                label: format!("Ch {} EQ{} Freq", ch, band_num),
                description: format!(
                    "Channel {} EQ band {} ({}) frequency in Hz",
                    ch, band_num, band_name
                ),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(def_freq)),
                mapping: PropertyMapping {
                    element_id: format!("eq_{}", ch - 1),
                    property_name: format!("f-{}", band),
                    transform: None,
                },
                live: false,
            });

            exposed_properties.push(ExposedProperty {
                name: format!("ch{}_eq{}_gain", ch, band_num),
                label: format!("Ch {} EQ{} Gain", ch, band_num),
                description: format!(
                    "Channel {} EQ band {} gain in dB (-15 to +15)",
                    ch, band_num
                ),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(0.0)),
                mapping: PropertyMapping {
                    element_id: format!("eq_{}", ch - 1),
                    property_name: format!("g-{}", band),
                    transform: Some("db_to_linear".to_string()),
                },
                live: false,
            });

            exposed_properties.push(ExposedProperty {
                name: format!("ch{}_eq{}_q", ch, band_num),
                label: format!("Ch {} EQ{} Q", ch, band_num),
                description: format!("Channel {} EQ band {} Q factor (0.1 to 10)", ch, band_num),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(1.0)),
                mapping: PropertyMapping {
                    element_id: format!("eq_{}", ch - 1),
                    property_name: format!("q-{}", band),
                    transform: None,
                },
                live: false,
            });
        }
    }

    BlockDefinition {
        id: "builtin.mixer".to_string(),
        name: "Audio Mixer".to_string(),
        description: "Stereo audio mixer with per-channel gain, gate, compressor, EQ, pan, fader, mute and metering. Main bus with compressor, EQ and limiter. Supports aux sends (pre/post) and subgroups.".to_string(),
        category: "Audio".to_string(),
        exposed_properties,
        // External pads are computed dynamically based on num_channels
        // (this is the default, get_external_pads() provides dynamic version)
        external_pads: ExternalPads {
            inputs: (0..DEFAULT_CHANNELS)
                .map(|i| ExternalPad {
                    name: format!("input_{}", i + 1),
                    label: Some(format!("A{}", i)),
                    media_type: MediaType::Audio,
                    internal_element_id: format!("convert_{}", i),
                    internal_pad_name: "sink".to_string(),
                })
                .collect(),
            outputs: vec![
                ExternalPad {
                    name: "main_out".to_string(),
                    label: Some("Main".to_string()),
                    media_type: MediaType::Audio,
                    internal_element_id: "main_out_tee".to_string(),
                    internal_pad_name: "src_%u".to_string(),
                },
                ExternalPad {
                    name: "pfl_out".to_string(),
                    label: Some("PFL".to_string()),
                    media_type: MediaType::Audio,
                    internal_element_id: "pfl_out_tee".to_string(),
                    internal_pad_name: "src_%u".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("\u{1f3a4}".to_string()),
            width: Some(3.0),
            height: Some(4.0),
            ..Default::default()
        }),
    }
}
