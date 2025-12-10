//! Block definitions and instances for reusable element groupings.

use crate::{MediaType, PropertyValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Enum value with optional label for display
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EnumValue {
    /// The actual value stored/used
    pub value: String,

    /// Optional human-readable label for UI display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Property type enumeration for exposed properties
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PropertyType {
    String,
    Multiline,
    Int,
    UInt,
    Float,
    Bool,
    Enum {
        values: Vec<EnumValue>,
    },
    /// Network interface selector - frontend fetches available interfaces from API
    NetworkInterface,
}

/// Block definition - metadata for creating block instances.
///
/// Note: Built-in blocks use the BlockBuilder trait to create GStreamer elements directly.
/// User-defined blocks are not yet supported.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BlockDefinition {
    /// Unique identifier for this block definition
    pub id: String,

    /// Human-readable name (e.g., "AES67 Input")
    pub name: String,

    /// Description of what this block does
    pub description: String,

    /// Category for organization (e.g., "Inputs", "Outputs", "Codecs")
    pub category: String,

    /// Exposed properties that users can configure
    pub exposed_properties: Vec<ExposedProperty>,

    /// External pads exposed by this block
    pub external_pads: ExternalPads,

    /// Whether this is a built-in block (read-only) or user-defined
    pub built_in: bool,

    /// Visual representation settings
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_metadata: Option<BlockUIMetadata>,
}

/// Property exposed by a block to the outside
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ExposedProperty {
    /// Name of the exposed property (used as key)
    pub name: String,

    /// Human-readable label for display in UI (e.g., "Auth Token" instead of "auth_token")
    pub label: String,

    /// Description for users
    pub description: String,

    /// Type of property
    pub property_type: PropertyType,

    /// Default value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<PropertyValue>,

    /// Mapping to internal element property
    pub mapping: PropertyMapping,
}

/// Maps an exposed property to one or more internal element properties
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PropertyMapping {
    /// Which internal element's property to set
    pub element_id: String,

    /// Property name on that element
    pub property_name: String,

    /// Optional transformation (for future use)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
}

/// External pads that the block exposes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ExternalPads {
    /// Input pads (mapped to internal element pads)
    pub inputs: Vec<ExternalPad>,

    /// Output pads (mapped to internal element pads)
    pub outputs: Vec<ExternalPad>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ExternalPad {
    /// External name for this pad
    pub name: String,

    /// Media type (audio, video, generic)
    pub media_type: MediaType,

    /// Which internal element and pad this maps to
    pub internal_element_id: String,
    pub internal_pad_name: String,
}

/// Block instance in a flow
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BlockInstance {
    /// Unique ID for this instance
    pub id: String,

    /// Reference to the block definition
    pub block_definition_id: String,

    /// User-assigned name for this instance
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Property values for this instance
    pub properties: HashMap<String, PropertyValue>,

    /// Position in the visual editor
    pub position: Position,

    /// Runtime data (not persisted to storage, only available when flow is running)
    /// Used for things like generated SDP for AES67 blocks
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub runtime_data: Option<HashMap<String, String>>,

    /// Computed external pads for this instance based on properties
    /// If None, falls back to the pads from the block definition
    /// This allows blocks to have dynamic pads based on their configuration
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub computed_external_pads: Option<ExternalPads>,
}

/// Position in the visual editor
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

/// UI metadata for block rendering
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BlockUIMetadata {
    /// Icon or visual identifier (emoji or name)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,

    /// Width in the editor (in grid units)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,

    /// Height in the editor (in grid units)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,

    // Light mode colors (all optional, use defaults if unset)
    /// Fill/background color in light mode (hex color)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_fill_color: Option<String>,

    /// Stroke/border color in light mode (hex color)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_stroke_color: Option<String>,

    /// Text color in light mode (hex color)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_text_color: Option<String>,

    // Dark mode colors (all optional, use defaults if unset)
    /// Fill/background color in dark mode (hex color)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dark_fill_color: Option<String>,

    /// Stroke/border color in dark mode (hex color)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dark_stroke_color: Option<String>,

    /// Text color in dark mode (hex color)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dark_text_color: Option<String>,
}

/// Request to create a new block definition (currently not supported for user-defined blocks)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateBlockRequest {
    pub name: String,
    pub description: String,
    pub category: String,
    pub exposed_properties: Vec<ExposedProperty>,
    pub external_pads: ExternalPads,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_metadata: Option<BlockUIMetadata>,
}

/// Response containing a block definition
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BlockResponse {
    pub block: BlockDefinition,
}

/// Response containing a list of blocks
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BlockListResponse {
    pub blocks: Vec<BlockDefinition>,
}

/// Response containing block categories
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BlockCategoriesResponse {
    pub categories: Vec<String>,
}

/// Common video resolutions for use in block property dropdowns.
/// Ordered from largest to smallest.
pub const COMMON_VIDEO_RESOLUTIONS: &[(&str, &str)] = &[
    ("7680x4320", "8K UHD (7680x4320)"),
    ("4096x2160", "4K DCI (4096x2160)"),
    ("3840x2160", "4K UHD (3840x2160)"),
    ("2560x1440", "QHD / 1440p (2560x1440)"),
    ("1920x1080", "Full HD (1920x1080)"),
    ("1600x900", "HD+ (1600x900)"),
    ("1280x720", "HD (1280x720)"),
    ("720x576", "PAL SD (720x576)"),
    ("720x480", "NTSC SD (720x480)"),
    ("640x480", "VGA (640x480)"),
    ("640x360", "nHD (640x360)"),
    ("320x240", "QVGA (320x240)"),
];

/// Get common video resolutions as EnumValue list for block properties.
/// Set `include_empty` to true to add an empty "-" option at the start.
pub fn common_video_resolution_enum_values(include_empty: bool) -> Vec<EnumValue> {
    let mut values = Vec::new();

    if include_empty {
        values.push(EnumValue {
            value: String::new(),
            label: Some("-".to_string()),
        });
    }

    for (value, label) in COMMON_VIDEO_RESOLUTIONS {
        values.push(EnumValue {
            value: (*value).to_string(),
            label: Some((*label).to_string()),
        });
    }

    values
}

/// Parse a resolution string like "1920x1080" into (width, height).
/// Returns None if parsing fails.
pub fn parse_resolution_string(s: &str) -> Option<(u32, u32)> {
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() == 2 {
        if let (Ok(w), Ok(h)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
            return Some((w, h));
        }
    }
    None
}
