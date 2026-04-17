//! Block definition for the Vision Mixer block.

use strom_types::block::*;
use strom_types::vision_mixer::*;
use strom_types::{
    common_video_framerate_enum_values, common_video_pixel_format_enum_values,
    common_video_resolution_enum_values, MediaType, PropertyValue,
};

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
            live: false,
        },
        // Number of inputs
        ExposedProperty {
            name: "num_inputs".to_string(),
            label: "Number of Inputs".to_string(),
            description: "Number of video inputs (2-10)".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(DEFAULT_NUM_INPUTS as u64)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_inputs".to_string(),
                transform: None,
            },
            live: false,
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
            live: false,
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
            live: false,
        },
        // PGM output framerate
        ExposedProperty {
            name: "pgm_framerate".to_string(),
            label: "PGM Framerate".to_string(),
            description: "Distribution/PGM output framerate".to_string(),
            property_type: PropertyType::Enum {
                values: common_video_framerate_enum_values(false),
            },
            default_value: Some(PropertyValue::String(
                DEFAULT_PGM_FRAMERATE.to_string(),
            )),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "pgm_framerate".to_string(),
                transform: None,
            },
            live: false,
        },
        // Multiview output framerate
        ExposedProperty {
            name: "multiview_framerate".to_string(),
            label: "Multiview Framerate".to_string(),
            description: "Multiview monitor output framerate".to_string(),
            property_type: PropertyType::Enum {
                values: common_video_framerate_enum_values(false),
            },
            default_value: Some(PropertyValue::String(
                DEFAULT_MULTIVIEW_FRAMERATE.to_string(),
            )),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "multiview_framerate".to_string(),
                transform: None,
            },
            live: false,
        },
        // Latency
        ExposedProperty {
            name: "latency".to_string(),
            label: "Latency (ms)".to_string(),
            description: "Compositor latency in milliseconds".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(
                strom_types::vision_mixer::DEFAULT_LATENCY_MS,
            )),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "latency".to_string(),
                transform: None,
            },
            live: false,
        },
        // Min upstream latency
        ExposedProperty {
            name: "min_upstream_latency".to_string(),
            label: "Min Upstream Latency (ms)".to_string(),
            description: "Minimum upstream latency in milliseconds".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(
                strom_types::vision_mixer::DEFAULT_MIN_UPSTREAM_LATENCY_MS,
            )),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "min_upstream_latency".to_string(),
                transform: None,
            },
            live: false,
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
            live: false,
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
            live: false,
        },
        // Output pixel format
        ExposedProperty {
            name: "output_format".to_string(),
            label: "Output Pixel Format".to_string(),
            description: "Pixel format for compositor outputs. Auto lets GStreamer negotiate."
                .to_string(),
            property_type: PropertyType::Enum {
                values: {
                    let mut v = vec![EnumValue {
                        value: String::new(),
                        label: Some("Auto".to_string()),
                    }];
                    v.extend(common_video_pixel_format_enum_values(false));
                    v
                },
            },
            default_value: Some(PropertyValue::String("".to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "output_format".to_string(),
                transform: None,
            },
            live: false,
        },
        // GL download (GPU path only)
        ExposedProperty {
            name: "gl_download".to_string(),
            label: "GL Download".to_string(),
            description: "Download GPU memory to system memory on output. Disable to pass GL memory downstream (GPU path only).".to_string(),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(DEFAULT_GL_DOWNLOAD)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "gl_download".to_string(),
                transform: None,
            },
            live: false,
        },
        // Show VU meters in multiview overlay
        ExposedProperty {
            name: "show_vu_meters".to_string(),
            label: "Show VU Meters".to_string(),
            description:
                "Render per-input and PGM audio VU meters on the multiview overlay".to_string(),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(DEFAULT_SHOW_VU_METERS)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "show_vu_meters".to_string(),
                transform: None,
            },
            live: false,
        },
        // Number of DSK inputs
        ExposedProperty {
            name: "num_dsk_inputs".to_string(),
            label: "DSK Inputs".to_string(),
            description: "Number of Downstream Keyer inputs for graphics overlay (0-4)".to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue {
                        value: "0".to_string(),
                        label: Some("None".to_string()),
                    },
                    EnumValue {
                        value: "1".to_string(),
                        label: Some("1 DSK".to_string()),
                    },
                    EnumValue {
                        value: "2".to_string(),
                        label: Some("2 DSK".to_string()),
                    },
                    EnumValue {
                        value: "3".to_string(),
                        label: Some("3 DSK".to_string()),
                    },
                    EnumValue {
                        value: "4".to_string(),
                        label: Some("4 DSK".to_string()),
                    },
                ],
            },
            default_value: Some(PropertyValue::String(DEFAULT_DSK_INPUTS.to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_dsk_inputs".to_string(),
                transform: None,
            },
            live: false,
        },
    ];

    // Per-input labels
    for i in 0..MAX_NUM_INPUTS {
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_label", i),
            label: format!("Input {} Label", i + 1),
            description: format!("Label for input {} shown on multiview", i + 1),
            property_type: PropertyType::String,
            default_value: Some(PropertyValue::String(format!("In {}", i + 1))),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_label", i),
                transform: None,
            },
            live: false,
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
            inputs: {
                let mut pads: Vec<ExternalPad> = (0..DEFAULT_NUM_INPUTS)
                    .map(|i| {
                        ExternalPad::with_label(
                            format!("video_in_{}", i),
                            format!("V{}", i),
                            MediaType::Video,
                            format!("queue_{}", i),
                            "sink".to_string(),
                        )
                    })
                    .collect();
                for i in 0..DEFAULT_NUM_INPUTS {
                    pads.push(ExternalPad::with_label(
                        format!("audio_in_{}", i),
                        format!("A{}", i),
                        MediaType::Audio,
                        format!("queue_audio_{}", i),
                        "sink".to_string(),
                    ));
                }
                pads.push(ExternalPad::with_label(
                    "pgm_audio_in",
                    "PGM Audio",
                    MediaType::Audio,
                    "queue_audio_pgm",
                    "sink".to_string(),
                ));
                pads
            },
            outputs: vec![
                ExternalPad::with_label(
                    "pgm_out",
                    "PGM",
                    MediaType::Video,
                    "queue_dist_out",
                    "src",
                ),
                ExternalPad::with_label(
                    "multiview_out",
                    "MV",
                    MediaType::Video,
                    "queue_mv_out",
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
