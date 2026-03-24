//! Block definition for the Vision Mixer block.

use strom_types::block::*;
use strom_types::vision_mixer::*;
use strom_types::{common_video_resolution_enum_values, MediaType, PropertyValue};

/// Get block definitions for the vision mixer.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![vision_mixer_definition()]
}

fn vision_mixer_definition() -> BlockDefinition {
    let mut exposed_properties = vec![
        // Backend preference
        ExposedProperty {
            name: "compositor_preference".to_string(),
            label: "Backend".to_string(),
            description:
                "Compositor backend: Auto (GPU first, fallback to CPU), GPU Only, or CPU Only"
                    .to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue {
                        value: "auto".to_string(),
                        label: Some("Auto (GPU first)".to_string()),
                    },
                    EnumValue {
                        value: "gpu".to_string(),
                        label: Some("GPU Only (OpenGL)".to_string()),
                    },
                    EnumValue {
                        value: "cpu".to_string(),
                        label: Some("CPU Only (Software)".to_string()),
                    },
                ],
            },
            default_value: Some(PropertyValue::String("auto".to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "compositor_preference".to_string(),
                transform: None,
            },
        },
        // Number of inputs
        ExposedProperty {
            name: "num_inputs".to_string(),
            label: "Number of Inputs".to_string(),
            description: "Number of video inputs (2-10)".to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue {
                        value: "2".to_string(),
                        label: Some("2".to_string()),
                    },
                    EnumValue {
                        value: "4".to_string(),
                        label: Some("4".to_string()),
                    },
                    EnumValue {
                        value: "6".to_string(),
                        label: Some("6".to_string()),
                    },
                    EnumValue {
                        value: "8".to_string(),
                        label: Some("8".to_string()),
                    },
                    EnumValue {
                        value: "10".to_string(),
                        label: Some("10".to_string()),
                    },
                ],
            },
            default_value: Some(PropertyValue::String(DEFAULT_NUM_INPUTS.to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_inputs".to_string(),
                transform: None,
            },
        },
        // PGM output resolution
        ExposedProperty {
            name: "pgm_resolution".to_string(),
            label: "PGM Resolution".to_string(),
            description: "Distribution/PGM output resolution".to_string(),
            property_type: PropertyType::Enum {
                values: common_video_resolution_enum_values(false),
            },
            default_value: Some(PropertyValue::String(DEFAULT_PGM_RESOLUTION.to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "pgm_resolution".to_string(),
                transform: None,
            },
        },
        // Multiview output resolution
        ExposedProperty {
            name: "multiview_resolution".to_string(),
            label: "Multiview Resolution".to_string(),
            description: "Multiview monitor output resolution".to_string(),
            property_type: PropertyType::Enum {
                values: common_video_resolution_enum_values(false),
            },
            default_value: Some(PropertyValue::String(
                DEFAULT_MULTIVIEW_RESOLUTION.to_string(),
            )),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "multiview_resolution".to_string(),
                transform: None,
            },
        },
        // Force live
        ExposedProperty {
            name: "force_live".to_string(),
            label: "Force Live".to_string(),
            description: "Force compositors into live mode".to_string(),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(true)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "force_live".to_string(),
                transform: None,
            },
        },
        // Latency
        ExposedProperty {
            name: "latency".to_string(),
            label: "Latency (ms)".to_string(),
            description: "Compositor latency in milliseconds".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(200)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "latency".to_string(),
                transform: None,
            },
        },
        // Min upstream latency
        ExposedProperty {
            name: "min_upstream_latency".to_string(),
            label: "Min Upstream Latency (ms)".to_string(),
            description: "Minimum upstream latency in milliseconds".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(200)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "min_upstream_latency".to_string(),
                transform: None,
            },
        },
        // Initial PGM input
        ExposedProperty {
            name: "initial_pgm_input".to_string(),
            label: "Initial PGM Input".to_string(),
            description: "Input index initially on program (0-based)".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(DEFAULT_PGM_INPUT as u64)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "initial_pgm_input".to_string(),
                transform: None,
            },
        },
        // Initial PVW input
        ExposedProperty {
            name: "initial_pvw_input".to_string(),
            label: "Initial Preview Input".to_string(),
            description: "Input index initially on preview (0-based)".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(DEFAULT_PVW_INPUT as u64)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "initial_pvw_input".to_string(),
                transform: None,
            },
        },
    ];

    // Per-input labels
    for i in 0..MAX_NUM_INPUTS {
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_label", i),
            label: format!("Input {} Label", i + 1),
            description: format!("Label for input {} shown on multiview", i + 1),
            property_type: PropertyType::String,
            default_value: Some(PropertyValue::String(format!("Input {}", i + 1))),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_label", i),
                transform: None,
            },
        });
    }

    BlockDefinition {
        id: "builtin.vision_mixer".to_string(),
        name: "Vision Mixer".to_string(),
        description:
            "TV broadcast vision mixer with PVW/PGM workflow, transitions, and multiview output"
                .to_string(),
        category: "Production".to_string(),
        exposed_properties,
        external_pads: ExternalPads {
            inputs: (0..MAX_NUM_INPUTS)
                .map(|i| {
                    ExternalPad::with_label(
                        format!("video_in_{}", i),
                        format!("V{}", i),
                        MediaType::Video,
                        format!("queue_{}", i),
                        "sink".to_string(),
                    )
                })
                .collect(),
            outputs: vec![
                ExternalPad::with_label(
                    "pgm_out",
                    "PGM",
                    MediaType::Video,
                    "capsfilter_dist",
                    "src",
                ),
                ExternalPad::with_label(
                    "multiview_out",
                    "MV",
                    MediaType::Video,
                    "capsfilter_mv",
                    "src",
                ),
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: None,
            width: Some(4.0),
            height: Some(5.0),
            ..Default::default()
        }),
    }
}
