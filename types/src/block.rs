//! Block definitions and instances for reusable element groupings.

use crate::{discovery::DeviceCategory, MediaType, PropertyValue};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};

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

/// Stream content selector for blocks that handle both audio and video.
///
/// Used by blocks like WHEP, WHIP, and DeckLink Input to indicate which
/// kinds of media tracks the block should expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum StreamMode {
    /// Audio only
    Audio,
    /// Video only
    Video,
    /// Both audio and video
    #[default]
    AudioVideo,
}

impl StreamMode {
    pub fn has_audio(&self) -> bool {
        matches!(self, StreamMode::Audio | StreamMode::AudioVideo)
    }

    pub fn has_video(&self) -> bool {
        matches!(self, StreamMode::Video | StreamMode::AudioVideo)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            StreamMode::Audio => "audio",
            StreamMode::Video => "video",
            StreamMode::AudioVideo => "audio_video",
        }
    }

    /// Parse a stream mode from a string. Unknown values fall back to `Video`
    /// (preserves the historical WHEP default).
    pub fn parse(s: &str) -> Self {
        match s {
            "audio" => StreamMode::Audio,
            "audio_video" => StreamMode::AudioVideo,
            _ => StreamMode::Video,
        }
    }
}

/// Property type enumeration for exposed properties
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PropertyType {
    #[default]
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
    /// Local capture/playback device selector. Frontend fetches the live
    /// device list from `/api/discovery/devices?category=<category>` and
    /// renders a dropdown of `DeviceResponse`s.
    Device {
        /// Which `DeviceCategory` to fetch and display (video source,
        /// audio source, audio sink, network source).
        category: DeviceCategory,
    },
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Whether this property updates the pipeline in real-time without requiring a flow save.
    /// Live properties show a LIVE badge in the UI and send updates directly to running elements.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,

    /// Whether live writes to this property should also be persisted to the
    /// block instance's properties map (so they survive pipeline restart).
    ///
    /// `None` (default) means persist. Set explicitly to `Some(false)` for
    /// transient properties — e.g. solo states like `chN_pfl`/`chN_afl` —
    /// that should reset on restart and not dirty the flow on every toggle.
    ///
    /// `Option` is used here (rather than a bare `bool` with a serde default)
    /// to keep existing struct-literal call sites compiling — they get the
    /// default behaviour without an added field. Read via `persist()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
}

impl ExposedProperty {
    /// Effective persist flag (defaults to `true` when unset).
    pub fn persist(&self) -> bool {
        self.persist.unwrap_or(true)
    }
}

/// Maps an exposed property to one or more internal element properties
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Optional display label (shown in graph editor)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Media type (audio, video, generic)
    pub media_type: MediaType,

    /// Which internal element and pad this maps to
    pub internal_element_id: String,
    pub internal_pad_name: String,
}

impl ExternalPad {
    /// Create a new ExternalPad without a label
    pub fn new(
        name: impl Into<String>,
        media_type: MediaType,
        internal_element_id: impl Into<String>,
        internal_pad_name: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            label: None,
            media_type,
            internal_element_id: internal_element_id.into(),
            internal_pad_name: internal_pad_name.into(),
        }
    }

    /// Create a new ExternalPad with a label
    pub fn with_label(
        name: impl Into<String>,
        label: impl Into<String>,
        media_type: MediaType,
        internal_element_id: impl Into<String>,
        internal_pad_name: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            label: Some(label.into()),
            media_type,
            internal_element_id: internal_element_id.into(),
            internal_pad_name: internal_pad_name.into(),
        }
    }
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
    #[serde(serialize_with = "sorted_properties")]
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

/// Default WebRTC jitterbuffer latency in milliseconds.
/// GStreamer's default is 200ms; we use 40ms for lower latency on LAN.
pub const DEFAULT_JITTERBUFFER_LATENCY_MS: i64 = 40;

/// Default SRT URI for output (listener mode).
pub const DEFAULT_SRT_OUTPUT_URI: &str = "srt://:5000?mode=listener";

/// Default SRT URI for input (caller connecting to the output listener).
pub const DEFAULT_SRT_INPUT_URI: &str = "srt://127.0.0.1:5000?mode=caller";

/// Default SRT latency in milliseconds.
pub const DEFAULT_SRT_LATENCY_MS: i32 = 125;

/// Default for the SRT `keep-listening` property on inputs/outputs:
/// stay alive across peer disconnects so reconnects don't need a flow restart.
pub const DEFAULT_SRT_KEEP_LISTENING: bool = true;

/// Default for the SRT `auto-reconnect` property on inputs/outputs:
/// reconnect automatically on connection failure.
pub const DEFAULT_SRT_AUTO_RECONNECT: bool = true;

/// Default for the SRT `wait-for-connection` property on inputs/outputs:
/// do NOT block pipeline state changes waiting for a peer. Upstream srtsrc
/// defaults to `true` but that deadlocks PAUSED→PLAYING when the peer is
/// offline; we override to `false` consistently across all SRT blocks.
pub const DEFAULT_SRT_WAIT_FOR_CONNECTION: bool = false;

/// Default tsdemux latency in milliseconds.
/// GStreamer's default is 700ms (for PCR synchronization). We use 0ms for
/// live pipelines because this property only affects the reported latency in
/// GStreamer's latency query, not internal buffering. SRT already handles
/// jitter, so tsdemux doesn't need to add additional latency margin.
pub const DEFAULT_TSDEMUX_LATENCY_MS: i32 = 0;

/// Default MTU for EFP fragmentation (bytes).
pub const DEFAULT_EFP_MTU: u32 = 1400;

/// Default EFP bucket timeout (units of 10ms).
pub const DEFAULT_EFP_BUCKET_TIMEOUT: u32 = 5;

/// Default EFP head-of-line timeout (units of 10ms).
pub const DEFAULT_EFP_HOL_TIMEOUT: u32 = 5;

/// Default audiobuffersplit output buffer duration in milliseconds.
/// Compacts small AES67 buffers (typically 1ms) into larger chunks to reduce
/// downstream wakeups and context switches.
pub const DEFAULT_AES67_INPUT_BUFFER_DURATION_MS: i64 = 20;

/// Default Opus encoder complexity (0-10). GStreamer defaults to 10 (max CPU).
/// 5 is a good balance between quality and CPU for real-time use cases.
pub const DEFAULT_OPUS_COMPLEXITY: i32 = 5;

/// Default Opus encoder bitrate in bps.
pub const DEFAULT_OPUS_BITRATE: i32 = 64000;

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

/// Common video pixel formats for use in block property dropdowns.
/// Grouped by color model: YUV 4:2:0, YUV 4:2:2, YUV 4:4:4, RGB/BGR, padded RGB, grayscale.
pub const COMMON_VIDEO_PIXEL_FORMATS: &[(&str, &str)] = &[
    // YUV 4:2:0
    ("I420", "I420 (YUV 4:2:0 planar)"),
    ("A420", "A420 (YUV 4:2:0 + alpha)"),
    ("YV12", "YV12 (YUV 4:2:0 planar)"),
    ("NV12", "NV12 (YUV 4:2:0 semi-planar)"),
    ("NV21", "NV21 (YUV 4:2:0 semi-planar)"),
    // YUV 4:2:2
    ("YUY2", "YUY2 (YUV 4:2:2 packed)"),
    ("UYVY", "UYVY (YUV 4:2:2 packed)"),
    ("A422", "A422 (YUV 4:2:2 + alpha)"),
    ("v210", "v210 (10-bit YUV 4:2:2)"),
    // YUV 4:4:4
    ("AYUV", "AYUV (YUV 4:4:4 + alpha packed)"),
    ("A444", "A444 (YUV 4:4:4 + alpha planar)"),
    // RGB / BGR
    ("RGB", "RGB"),
    ("BGR", "BGR"),
    ("RGBA", "RGBA"),
    ("BGRA", "BGRA"),
    ("ARGB", "ARGB"),
    ("ABGR", "ABGR"),
    // Padded RGB (no alpha channel, padded to 32-bit)
    ("RGBx", "RGBx"),
    ("BGRx", "BGRx"),
    ("xRGB", "xRGB"),
    ("xBGR", "xBGR"),
    // Grayscale
    ("GRAY8", "GRAY8 (8-bit grayscale)"),
];

/// Get common video pixel formats as EnumValue list for block properties.
/// Set `include_empty` to true to add an empty "-" option at the start.
pub fn common_video_pixel_format_enum_values(include_empty: bool) -> Vec<EnumValue> {
    let mut values = Vec::new();

    if include_empty {
        values.push(EnumValue {
            value: String::new(),
            label: Some("-".to_string()),
        });
    }

    for (value, label) in COMMON_VIDEO_PIXEL_FORMATS {
        values.push(EnumValue {
            value: (*value).to_string(),
            label: Some((*label).to_string()),
        });
    }

    values
}

/// Common video framerates for use in block property dropdowns.
/// Each entry is (fraction_string, label) where fraction_string is "N/D" format.
pub const COMMON_VIDEO_FRAMERATES: &[(&str, &str)] = &[
    ("10/1", "10 fps"),
    ("15/1", "15 fps"),
    ("24000/1001", "23.976 fps"),
    ("24/1", "24 fps"),
    ("25/1", "25 fps"),
    ("30000/1001", "29.97 fps"),
    ("30/1", "30 fps"),
    ("50/1", "50 fps"),
    ("60000/1001", "59.94 fps"),
    ("60/1", "60 fps"),
    ("120/1", "120 fps"),
];

/// Get common video framerates as EnumValue list for block properties.
/// Set `include_empty` to true to add an empty "-" option at the start.
pub fn common_video_framerate_enum_values(include_empty: bool) -> Vec<EnumValue> {
    let mut values = Vec::new();

    if include_empty {
        values.push(EnumValue {
            value: String::new(),
            label: Some("-".to_string()),
        });
    }

    for (value, label) in COMMON_VIDEO_FRAMERATES {
        values.push(EnumValue {
            value: (*value).to_string(),
            label: Some((*label).to_string()),
        });
    }

    values
}

/// Pixel formats accepted by `decklinkvideosrc` and `decklinkvideosink`'s
/// `video-format` property. These are GstDecklinkVideoFormat enum nicks, not
/// GStreamer caps `format=` strings — see `COMMON_VIDEO_PIXEL_FORMATS` for the
/// caps-format equivalents.
pub const DECKLINK_VIDEO_FORMATS: &[(&str, &str)] = &[
    ("auto", "Auto"),
    ("8bit-yuv", "8-bit YUV (UYVY)"),
    ("10bit-yuv", "10-bit YUV (v210)"),
    ("8bit-argb", "8-bit ARGB"),
    ("8bit-bgra", "8-bit BGRA"),
    ("10bit-rgb", "10-bit RGB (r210)"),
    ("12bit-rgb", "12-bit RGB"),
    ("12bit-rgble", "12-bit RGB LE"),
];

/// Get DeckLink video formats as `EnumValue` list for block properties.
pub fn decklink_video_format_enum_values() -> Vec<EnumValue> {
    DECKLINK_VIDEO_FORMATS
        .iter()
        .map(|(value, label)| EnumValue {
            value: (*value).to_string(),
            label: Some((*label).to_string()),
        })
        .collect()
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

/// Serialize a HashMap with sorted keys for deterministic JSON output.
pub(crate) fn sorted_properties<S>(
    map: &HashMap<String, PropertyValue>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let sorted: BTreeMap<_, _> = map.iter().collect();
    sorted.serialize(serializer)
}
