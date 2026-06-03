//! Discovery API types shared between backend and frontend.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

// ============================================================================
// Stream Discovery Types
// ============================================================================

/// API response for a discovered AES67 stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DiscoveredStreamResponse {
    pub id: String,
    pub name: String,
    pub source: String,
    pub multicast_address: String,
    pub port: u16,
    pub channels: u8,
    pub sample_rate: u32,
    pub encoding: String,
    pub origin_host: String,
    pub first_seen_secs_ago: u64,
    pub last_seen_secs_ago: u64,
    pub ttl_secs: u64,
    /// Network interface the stream was discovered on (for SAP).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received_on_interface: Option<String>,
}

/// Response for announced streams list.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AnnouncedStreamResponse {
    pub flow_id: String,
    pub block_id: String,
    pub origin_ip: String,
    pub sdp: String,
    /// Network interface the stream is announced on (None = all interfaces).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce_interface: Option<String>,
}

// ============================================================================
// Device Discovery Types
// ============================================================================

/// Device category for filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum DeviceCategory {
    /// Audio input devices (microphones, line-in).
    AudioSource,
    /// Audio output devices (speakers, headphones).
    AudioSink,
    /// Video input devices (cameras, capture cards).
    VideoSource,
    /// Network sources (NDI, etc.).
    NetworkSource,
    /// Other/unknown device types.
    Other,
}

impl DeviceCategory {
    /// Parse device category from GStreamer device class string.
    pub fn from_device_class(class: &str) -> Self {
        match class {
            "Audio/Source" => Self::AudioSource,
            "Audio/Sink" => Self::AudioSink,
            "Video/Source" => Self::VideoSource,
            "Source/Network" => Self::NetworkSource,
            _ => Self::Other,
        }
    }

    /// Get GStreamer device class filter string.
    pub fn to_filter_string(&self) -> Option<&'static str> {
        match self {
            Self::AudioSource => Some("Audio/Source"),
            Self::AudioSink => Some("Audio/Sink"),
            Self::VideoSource => Some("Video/Source"),
            Self::NetworkSource => Some("Source/Network"),
            Self::Other => None,
        }
    }
}

/// API response for a discovered device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DeviceResponse {
    /// Unique ID for this device.
    pub id: String,
    /// Display name of the device.
    pub name: String,
    /// Device class (e.g., "Audio/Source", "Video/Source", "Source/Network").
    pub device_class: String,
    /// Device category.
    pub category: DeviceCategory,
    /// Provider that discovered this device.
    pub provider: String,
    /// Additional properties from the device.
    pub properties: HashMap<String, String>,
    /// Seconds since first discovery.
    pub first_seen_secs_ago: u64,
    /// Seconds since last seen.
    pub last_seen_secs_ago: u64,
}

impl DeviceResponse {
    /// Human-readable name of the OS media API exposing this device
    /// (e.g. "WASAPI", "PulseAudio", "V4L2").
    ///
    /// On platforms with several competing media APIs (Windows: WASAPI /
    /// DirectSound / Media Foundation / ASIO; Linux: PulseAudio / PipeWire /
    /// ALSA / V4L2) the same physical device can show up once per API, and
    /// which one you pick matters — surfacing the API lets users tell the
    /// entries apart.
    ///
    /// Reads the GStreamer `device.api` property when present, otherwise
    /// falls back to the provider name with its `deviceprovider`/`provider`
    /// suffix stripped. Known API tokens are mapped to their conventional
    /// spelling; unknown tokens are returned as-is.
    pub fn api_label(&self) -> String {
        let raw = self
            .properties
            .get("device.api")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let p = self.provider.as_str();
                p.strip_suffix("deviceprovider")
                    .or_else(|| p.strip_suffix("provider"))
                    .unwrap_or(p)
            });
        match raw.to_ascii_lowercase().as_str() {
            "alsa" => "ALSA".to_string(),
            "asio" => "ASIO".to_string(),
            "avf" | "avfoundation" => "AVFoundation".to_string(),
            "decklink" => "DeckLink".to_string(),
            "dshow" | "directshow" => "DirectShow".to_string(),
            "directsound" | "dsound" => "DirectSound".to_string(),
            "jack" => "JACK".to_string(),
            "mediafoundation" | "mf" => "Media Foundation".to_string(),
            "ndi" => "NDI".to_string(),
            "osxaudio" | "coreaudio" => "CoreAudio".to_string(),
            "pipewire" => "PipeWire".to_string(),
            "pulse" | "pulseaudio" => "PulseAudio".to_string(),
            "v4l2" => "V4L2".to_string(),
            "wasapi" => "WASAPI".to_string(),
            "wasapi2" => "WASAPI2".to_string(),
            _ => raw.to_string(),
        }
    }
}

/// Device discovery status response.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DeviceDiscoveryStatus {
    /// Whether device discovery is running.
    pub running: bool,
    /// Whether NDI device provider is available.
    pub ndi_available: bool,
    /// Total number of discovered devices.
    pub device_count: usize,
    /// Number of devices by category.
    pub by_category: DeviceCountByCategory,
}

/// Device counts by category.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DeviceCountByCategory {
    pub audio_source: usize,
    pub audio_sink: usize,
    pub video_source: usize,
    pub network_source: usize,
    pub other: usize,
}

/// NDI discovery status response.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct NdiDiscoveryStatus {
    /// Whether NDI discovery is available (plugin installed).
    pub available: bool,
    /// Number of discovered NDI sources.
    pub source_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(provider: &str, api: Option<&str>) -> DeviceResponse {
        let mut properties = HashMap::new();
        if let Some(api) = api {
            properties.insert("device.api".to_string(), api.to_string());
        }
        DeviceResponse {
            id: "dev-test".to_string(),
            name: "Test Device".to_string(),
            device_class: "Audio/Source".to_string(),
            category: DeviceCategory::AudioSource,
            provider: provider.to_string(),
            properties,
            first_seen_secs_ago: 0,
            last_seen_secs_ago: 0,
        }
    }

    #[test]
    fn api_label_from_device_api_property() {
        assert_eq!(
            device("wasapiprovider", Some("wasapi")).api_label(),
            "WASAPI"
        );
        assert_eq!(
            device("pulseprovider", Some("pulse")).api_label(),
            "PulseAudio"
        );
        assert_eq!(device("v4l2provider", Some("v4l2")).api_label(), "V4L2");
        assert_eq!(
            device("mfprovider", Some("mediafoundation")).api_label(),
            "Media Foundation"
        );
    }

    #[test]
    fn api_label_falls_back_to_provider_suffix_stripping() {
        assert_eq!(
            device("pulsedeviceprovider", None).api_label(),
            "PulseAudio"
        );
        assert_eq!(device("v4l2deviceprovider", None).api_label(), "V4L2");
        assert_eq!(device("asioprovider", None).api_label(), "ASIO");
    }

    #[test]
    fn api_label_passes_unknown_tokens_through() {
        assert_eq!(device("unknown", None).api_label(), "unknown");
        assert_eq!(
            device("someprovider", Some("someapi")).api_label(),
            "someapi"
        );
    }

    #[test]
    fn api_label_ignores_empty_device_api() {
        assert_eq!(device("alsadeviceprovider", Some("")).api_label(), "ALSA");
    }
}
