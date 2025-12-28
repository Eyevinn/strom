//! Types for AES67 stream discovery (SAP/mDNS).

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::{Duration, Instant};
use strom_types::FlowId;
use utoipa::ToSchema;

/// Default TTL for discovered streams (5 minutes per RFC 2974).
pub const DEFAULT_STREAM_TTL: Duration = Duration::from_secs(300);

/// SAP announcement interval (30 seconds - good balance per RFC 2974).
pub const SAP_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

/// SAP multicast address.
pub const SAP_MULTICAST_ADDR: &str = "224.2.127.254";

/// SAP port.
pub const SAP_PORT: u16 = 9875;

/// RTSP server port for mDNS/RAVENNA announcements.
pub const RTSP_PORT: u16 = 8554;

/// A discovered AES67 stream from SAP or mDNS.
#[derive(Debug, Clone)]
pub struct DiscoveredStream {
    /// Unique ID for this stream (hash of origin + session ID).
    pub id: String,
    /// Stream name from SDP s= line.
    pub name: String,
    /// How this stream was discovered.
    pub source: DiscoverySource,
    /// Raw SDP content.
    pub sdp: String,
    /// Multicast address from SDP c= line.
    pub multicast_address: IpAddr,
    /// Port from SDP m= line.
    pub port: u16,
    /// Number of audio channels.
    pub channels: u8,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Audio encoding format.
    pub encoding: AudioEncoding,
    /// Origin host/IP from SDP o= line.
    pub origin_host: String,
    /// When this stream was first seen.
    pub first_seen: Instant,
    /// When this stream was last seen (updated on each announcement).
    pub last_seen: Instant,
    /// Time-to-live for this stream.
    pub ttl: Duration,
}

impl DiscoveredStream {
    /// Check if this stream has expired.
    pub fn is_expired(&self) -> bool {
        self.last_seen.elapsed() > self.ttl
    }

    /// Convert to API response format.
    pub fn to_api_response(&self) -> DiscoveredStreamResponse {
        DiscoveredStreamResponse {
            id: self.id.clone(),
            name: self.name.clone(),
            source: self.source.to_string(),
            multicast_address: self.multicast_address.to_string(),
            port: self.port,
            channels: self.channels,
            sample_rate: self.sample_rate,
            encoding: self.encoding.to_string(),
            origin_host: self.origin_host.clone(),
            first_seen_secs_ago: self.first_seen.elapsed().as_secs(),
            last_seen_secs_ago: self.last_seen.elapsed().as_secs(),
            ttl_secs: self.ttl.as_secs(),
        }
    }
}

/// API response for a discovered stream.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
}

/// How a stream was discovered.
#[derive(Debug, Clone)]
pub enum DiscoverySource {
    /// Discovered via SAP multicast.
    Sap {
        /// IP address of the announcing host.
        origin_ip: IpAddr,
        /// Message ID hash from SAP header.
        msg_id_hash: u16,
    },
    /// Discovered via mDNS (Bonjour/Zeroconf).
    Mdns {
        /// Service type (e.g., "_rtsp._tcp.local", "_ndi._tcp.local").
        service_type: String,
        /// Service instance name.
        instance_name: String,
        /// Hostname from mDNS.
        hostname: String,
        /// Port number.
        port: u16,
    },
    /// Manually added stream.
    Manual,
}

impl std::fmt::Display for DiscoverySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoverySource::Sap { .. } => write!(f, "SAP"),
            DiscoverySource::Mdns { service_type, .. } => {
                // Extract protocol name from service type (e.g., "_rtsp._tcp.local" -> "RTSP")
                if service_type.starts_with("_rtsp.") {
                    write!(f, "mDNS (RAVENNA)")
                } else if service_type.starts_with("_ndi.") {
                    write!(f, "mDNS (NDI)")
                } else {
                    write!(f, "mDNS")
                }
            }
            DiscoverySource::Manual => write!(f, "Manual"),
        }
    }
}

/// Audio encoding format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEncoding {
    /// 16-bit linear PCM.
    L16,
    /// 24-bit linear PCM.
    L24,
    /// AES3 (professional digital audio).
    AM824,
    /// Unknown encoding.
    Unknown,
}

impl std::fmt::Display for AudioEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioEncoding::L16 => write!(f, "L16"),
            AudioEncoding::L24 => write!(f, "L24"),
            AudioEncoding::AM824 => write!(f, "AM824"),
            AudioEncoding::Unknown => write!(f, "unknown"),
        }
    }
}

impl AudioEncoding {
    /// Parse encoding from SDP rtpmap attribute.
    pub fn from_rtpmap(encoding_name: &str) -> Self {
        match encoding_name.to_uppercase().as_str() {
            "L16" => AudioEncoding::L16,
            "L24" => AudioEncoding::L24,
            "AM824" => AudioEncoding::AM824,
            _ => AudioEncoding::Unknown,
        }
    }
}

/// A stream being announced by Strom via SAP.
#[derive(Debug, Clone)]
pub struct AnnouncedStream {
    /// Flow ID that owns this output.
    pub flow_id: FlowId,
    /// Block ID within the flow.
    pub block_id: String,
    /// Consistent hash for SAP message ID.
    pub msg_id_hash: u16,
    /// SDP content to announce.
    pub sdp: String,
    /// Local IP address for origin field.
    pub origin_ip: IpAddr,
    /// When this was last announced.
    pub last_announced: Instant,
    /// mDNS service fullname (if announced via mDNS).
    pub mdns_fullname: Option<String>,
}

impl AnnouncedStream {
    /// Generate a unique key for this stream.
    pub fn key(flow_id: &FlowId, block_id: &str) -> String {
        format!("{}:{}", flow_id, block_id)
    }
}

/// Generate a consistent 16-bit hash for SAP message ID.
pub fn generate_msg_id_hash(flow_id: &FlowId, block_id: &str) -> u16 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    flow_id.hash(&mut hasher);
    block_id.hash(&mut hasher);
    (hasher.finish() & 0xFFFF) as u16
}

/// Parsed stream info from SDP.
#[derive(Debug, Clone)]
pub struct SdpStreamInfo {
    /// Session name (s= line).
    pub name: String,
    /// Origin username (o= line).
    pub origin_username: String,
    /// Origin session ID (o= line).
    pub origin_session_id: String,
    /// Origin address (o= line).
    pub origin_address: String,
    /// Connection address (c= line).
    pub connection_address: Option<IpAddr>,
    /// Media port (m= line).
    pub port: Option<u16>,
    /// Audio encoding from rtpmap.
    pub encoding: AudioEncoding,
    /// Sample rate from rtpmap.
    pub sample_rate: Option<u32>,
    /// Channel count from rtpmap.
    pub channels: Option<u8>,
}

impl SdpStreamInfo {
    /// Parse SDP content to extract stream information.
    pub fn parse(sdp: &str) -> Option<Self> {
        let mut name = String::new();
        let mut origin_username = String::new();
        let mut origin_session_id = String::new();
        let mut origin_address = String::new();
        let mut connection_address = None;
        let mut port = None;
        let mut encoding = AudioEncoding::Unknown;
        let mut sample_rate = None;
        let mut channels = None;

        for line in sdp.lines() {
            let line = line.trim();

            if let Some(rest) = line.strip_prefix("s=") {
                name = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("o=") {
                // o=<username> <sess-id> <sess-version> <nettype> <addrtype> <unicast-address>
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 6 {
                    origin_username = parts[0].to_string();
                    origin_session_id = parts[1].to_string();
                    origin_address = parts[5].to_string();
                }
            } else if let Some(rest) = line.strip_prefix("c=") {
                // c=IN IP4 239.69.1.1/32
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 3 {
                    // Remove TTL suffix if present (e.g., "239.69.1.1/32" -> "239.69.1.1")
                    let addr_str = parts[2].split('/').next().unwrap_or(parts[2]);
                    if let Ok(addr) = addr_str.parse::<IpAddr>() {
                        connection_address = Some(addr);
                    }
                }
            } else if let Some(rest) = line.strip_prefix("m=audio ") {
                // m=audio 5004 RTP/AVP 96
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if !parts.is_empty() {
                    port = parts[0].parse().ok();
                }
            } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                // a=rtpmap:96 L24/48000/2
                // Skip payload type number
                if let Some(format_part) = rest.split_whitespace().nth(1) {
                    let format_parts: Vec<&str> = format_part.split('/').collect();
                    if !format_parts.is_empty() {
                        encoding = AudioEncoding::from_rtpmap(format_parts[0]);
                    }
                    if format_parts.len() >= 2 {
                        sample_rate = format_parts[1].parse().ok();
                    }
                    if format_parts.len() >= 3 {
                        channels = format_parts[2].parse().ok();
                    }
                }
            }
        }

        // Name is required
        if name.is_empty() {
            return None;
        }

        Some(SdpStreamInfo {
            name,
            origin_username,
            origin_session_id,
            origin_address,
            connection_address,
            port,
            encoding,
            sample_rate,
            channels,
        })
    }

    /// Generate a unique stream ID.
    pub fn generate_id(&self, origin_ip: &IpAddr) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        origin_ip.hash(&mut hasher);
        self.origin_session_id.hash(&mut hasher);
        self.origin_address.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sdp() {
        let sdp = r#"v=0
o=- 1234567890 1 IN IP4 192.168.1.100
s=Test Stream
c=IN IP4 239.69.1.1/32
t=0 0
m=audio 5004 RTP/AVP 96
a=rtpmap:96 L24/48000/2
"#;

        let info = SdpStreamInfo::parse(sdp).unwrap();
        assert_eq!(info.name, "Test Stream");
        assert_eq!(info.origin_session_id, "1234567890");
        assert_eq!(info.origin_address, "192.168.1.100");
        assert_eq!(info.connection_address, Some("239.69.1.1".parse().unwrap()));
        assert_eq!(info.port, Some(5004));
        assert_eq!(info.encoding, AudioEncoding::L24);
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.channels, Some(2));
    }

    #[test]
    fn test_parse_sdp_l16() {
        let sdp = r#"v=0
o=- 1 1 IN IP4 10.0.0.1
s=Dante AES67
c=IN IP4 239.1.2.3/32
m=audio 5004 RTP/AVP 97
a=rtpmap:97 L16/48000/8
"#;

        let info = SdpStreamInfo::parse(sdp).unwrap();
        assert_eq!(info.encoding, AudioEncoding::L16);
        assert_eq!(info.channels, Some(8));
    }

    #[test]
    fn test_audio_encoding_from_rtpmap() {
        assert_eq!(AudioEncoding::from_rtpmap("L16"), AudioEncoding::L16);
        assert_eq!(AudioEncoding::from_rtpmap("l24"), AudioEncoding::L24);
        assert_eq!(AudioEncoding::from_rtpmap("AM824"), AudioEncoding::AM824);
        assert_eq!(
            AudioEncoding::from_rtpmap("unknown"),
            AudioEncoding::Unknown
        );
    }

    #[test]
    fn test_msg_id_hash() {
        let flow_id = FlowId::from(uuid::Uuid::new_v4());
        let block_id = "block_0";

        let hash1 = generate_msg_id_hash(&flow_id, block_id);
        let hash2 = generate_msg_id_hash(&flow_id, block_id);

        // Should be consistent
        assert_eq!(hash1, hash2);

        // Different block should produce different hash
        let hash3 = generate_msg_id_hash(&flow_id, "block_1");
        assert_ne!(hash1, hash3);
    }
}
