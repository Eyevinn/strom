//! SAP (Session Announcement Protocol) implementation per RFC 2974.
//!
//! SAP is used by Dante and other AES67 devices to announce streams via multicast.

use flate2::read::ZlibDecoder;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use thiserror::Error;

/// SAP packet parsing and building errors.
#[derive(Debug, Error)]
pub enum SapError {
    #[error("Packet too short: {0} bytes")]
    PacketTooShort(usize),
    #[error("Invalid SAP version: {0}")]
    InvalidVersion(u8),
    #[error("Encryption not supported")]
    EncryptionNotSupported,
    #[error("Failed to decompress payload: {0}")]
    DecompressionFailed(String),
    #[error("Invalid payload encoding")]
    InvalidPayload,
    #[error("Invalid origin address")]
    InvalidOriginAddress,
}

/// SAP packet header flags.
#[derive(Debug, Clone, Copy)]
pub struct SapFlags {
    /// SAP version (must be 1).
    pub version: u8,
    /// Address type: false = IPv4, true = IPv6.
    pub ipv6: bool,
    /// Reserved bit.
    pub reserved: bool,
    /// Message type: false = announcement, true = deletion.
    pub deletion: bool,
    /// Encryption flag.
    pub encrypted: bool,
    /// Compression flag (zlib).
    pub compressed: bool,
}

impl SapFlags {
    /// Parse flags from first byte.
    fn from_byte(byte: u8) -> Self {
        SapFlags {
            version: (byte >> 5) & 0x07,
            ipv6: (byte & 0x10) != 0,
            reserved: (byte & 0x08) != 0,
            deletion: (byte & 0x04) != 0,
            encrypted: (byte & 0x02) != 0,
            compressed: (byte & 0x01) != 0,
        }
    }

    /// Encode flags to byte.
    fn to_byte(self) -> u8 {
        let mut byte = (self.version & 0x07) << 5;
        if self.ipv6 {
            byte |= 0x10;
        }
        if self.reserved {
            byte |= 0x08;
        }
        if self.deletion {
            byte |= 0x04;
        }
        if self.encrypted {
            byte |= 0x02;
        }
        if self.compressed {
            byte |= 0x01;
        }
        byte
    }
}

/// A parsed SAP packet.
#[derive(Debug, Clone)]
pub struct SapPacket {
    /// Header flags.
    pub flags: SapFlags,
    /// Authentication data length (in 32-bit words).
    pub auth_len: u8,
    /// Message ID hash for deduplication.
    pub msg_id_hash: u16,
    /// Originating source IP address.
    pub origin: IpAddr,
    /// Payload type (e.g., "application/sdp"), if present.
    pub payload_type: Option<String>,
    /// Payload content (SDP).
    pub payload: String,
}

impl SapPacket {
    /// Parse a SAP packet from raw bytes.
    pub fn parse(data: &[u8]) -> Result<Self, SapError> {
        // Minimum header size: 8 bytes for IPv4, 20 bytes for IPv6
        if data.len() < 8 {
            return Err(SapError::PacketTooShort(data.len()));
        }

        let flags = SapFlags::from_byte(data[0]);

        // Verify version
        if flags.version != 1 {
            return Err(SapError::InvalidVersion(flags.version));
        }

        // We don't support encrypted packets
        if flags.encrypted {
            return Err(SapError::EncryptionNotSupported);
        }

        let auth_len = data[1];
        let msg_id_hash = u16::from_be_bytes([data[2], data[3]]);

        // Parse origin address
        let (origin, header_end) = if flags.ipv6 {
            if data.len() < 20 {
                return Err(SapError::PacketTooShort(data.len()));
            }
            let addr_bytes: [u8; 16] = data[4..20]
                .try_into()
                .map_err(|_| SapError::InvalidOriginAddress)?;
            (IpAddr::V6(Ipv6Addr::from(addr_bytes)), 20)
        } else {
            let addr_bytes: [u8; 4] = data[4..8]
                .try_into()
                .map_err(|_| SapError::InvalidOriginAddress)?;
            (IpAddr::V4(Ipv4Addr::from(addr_bytes)), 8)
        };

        // Skip authentication data
        let auth_data_len = (auth_len as usize) * 4;
        let payload_start = header_end + auth_data_len;

        if data.len() <= payload_start {
            return Err(SapError::PacketTooShort(data.len()));
        }

        let payload_data = &data[payload_start..];

        // Extract optional payload type (null-terminated string before SDP)
        let (payload_type, sdp_start) = Self::extract_payload_type(payload_data);

        // Get raw payload
        let raw_payload = &payload_data[sdp_start..];

        // Decompress if needed
        let payload = if flags.compressed {
            Self::decompress(raw_payload)?
        } else {
            String::from_utf8_lossy(raw_payload).to_string()
        };

        Ok(SapPacket {
            flags,
            auth_len,
            msg_id_hash,
            origin,
            payload_type,
            payload,
        })
    }

    /// Build a SAP packet for announcement or deletion.
    pub fn build(origin: IpAddr, msg_id_hash: u16, payload: &str, deletion: bool) -> Vec<u8> {
        let is_ipv6 = origin.is_ipv6();

        let flags = SapFlags {
            version: 1,
            ipv6: is_ipv6,
            reserved: false,
            deletion,
            encrypted: false,
            compressed: false, // We don't compress outgoing packets
        };

        let mut packet = Vec::new();

        // Byte 0: flags
        packet.push(flags.to_byte());

        // Byte 1: auth len (0 = no authentication)
        packet.push(0);

        // Bytes 2-3: message ID hash
        packet.extend_from_slice(&msg_id_hash.to_be_bytes());

        // Bytes 4-7 or 4-19: origin address
        match origin {
            IpAddr::V4(addr) => {
                packet.extend_from_slice(&addr.octets());
            }
            IpAddr::V6(addr) => {
                packet.extend_from_slice(&addr.octets());
            }
        }

        // Payload type (null-terminated)
        packet.extend_from_slice(b"application/sdp\0");

        // Payload (SDP content)
        packet.extend_from_slice(payload.as_bytes());

        packet
    }

    /// Check if this is a deletion message.
    pub fn is_deletion(&self) -> bool {
        self.flags.deletion
    }

    /// Generate a session ID for deduplication.
    /// Combines origin IP and message ID hash.
    pub fn session_id(&self) -> String {
        format!("{}:{}", self.origin, self.msg_id_hash)
    }

    /// Extract payload type string from payload data.
    /// Returns (payload_type, offset_to_sdp).
    fn extract_payload_type(data: &[u8]) -> (Option<String>, usize) {
        // Look for null terminator within first 64 bytes
        for (i, &byte) in data.iter().take(64).enumerate() {
            if byte == 0 {
                let type_str = String::from_utf8_lossy(&data[..i]).to_string();
                // Skip the null terminator
                return (Some(type_str), i + 1);
            }
        }

        // No payload type found, assume SDP starts immediately
        // Check if it looks like SDP (starts with v=)
        if data.starts_with(b"v=") {
            (None, 0)
        } else {
            // Try to find v= within first 32 bytes
            for i in 0..data.len().min(32) {
                if data[i..].starts_with(b"v=") {
                    return (None, i);
                }
            }
            (None, 0)
        }
    }

    /// Decompress zlib-compressed payload.
    fn decompress(data: &[u8]) -> Result<String, SapError> {
        let mut decoder = ZlibDecoder::new(data);
        let mut decompressed = String::new();
        decoder
            .read_to_string(&mut decompressed)
            .map_err(|e| SapError::DecompressionFailed(e.to_string()))?;
        Ok(decompressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flags_roundtrip() {
        let flags = SapFlags {
            version: 1,
            ipv6: false,
            reserved: false,
            deletion: false,
            encrypted: false,
            compressed: false,
        };

        let byte = flags.to_byte();
        let parsed = SapFlags::from_byte(byte);

        assert_eq!(parsed.version, 1);
        assert!(!parsed.ipv6);
        assert!(!parsed.deletion);
        assert!(!parsed.encrypted);
        assert!(!parsed.compressed);
    }

    #[test]
    fn test_flags_deletion() {
        let flags = SapFlags {
            version: 1,
            ipv6: false,
            reserved: false,
            deletion: true,
            encrypted: false,
            compressed: false,
        };

        let byte = flags.to_byte();
        let parsed = SapFlags::from_byte(byte);

        assert!(parsed.deletion);
    }

    #[test]
    fn test_build_and_parse() {
        let origin = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));
        let msg_id_hash = 0x1234;
        let sdp = "v=0\r\no=- 1 1 IN IP4 192.168.1.100\r\ns=Test\r\n";

        let packet = SapPacket::build(origin, msg_id_hash, sdp, false);
        let parsed = SapPacket::parse(&packet).unwrap();

        assert_eq!(parsed.flags.version, 1);
        assert!(!parsed.flags.deletion);
        assert_eq!(parsed.msg_id_hash, msg_id_hash);
        assert_eq!(parsed.origin, origin);
        assert_eq!(parsed.payload_type, Some("application/sdp".to_string()));
        assert_eq!(parsed.payload, sdp);
    }

    #[test]
    fn test_build_deletion() {
        let origin = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let msg_id_hash = 0xABCD;
        let sdp = "v=0\r\ns=Test\r\n";

        let packet = SapPacket::build(origin, msg_id_hash, sdp, true);
        let parsed = SapPacket::parse(&packet).unwrap();

        assert!(parsed.is_deletion());
    }

    #[test]
    fn test_session_id() {
        let origin = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let msg_id_hash = 0x5678;
        let sdp = "v=0\r\ns=Test\r\n";

        let packet = SapPacket::build(origin, msg_id_hash, sdp, false);
        let parsed = SapPacket::parse(&packet).unwrap();

        assert_eq!(parsed.session_id(), "192.168.1.1:22136");
    }

    #[test]
    fn test_ipv6_packet() {
        let origin = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
        let msg_id_hash = 0x9999;
        let sdp = "v=0\r\ns=IPv6 Test\r\n";

        let packet = SapPacket::build(origin, msg_id_hash, sdp, false);
        let parsed = SapPacket::parse(&packet).unwrap();

        assert!(parsed.flags.ipv6);
        assert_eq!(parsed.origin, origin);
    }

    #[test]
    fn test_packet_too_short() {
        let data = [0x20, 0x00, 0x12, 0x34]; // Only 4 bytes
        let result = SapPacket::parse(&data);
        assert!(matches!(result, Err(SapError::PacketTooShort(_))));
    }

    #[test]
    fn test_invalid_version() {
        // Version 0 in flags byte
        let data = [
            0x00, 0x00, 0x12, 0x34, 0x01, 0x02, 0x03, 0x04, b'v', b'=', b'0',
        ];
        let result = SapPacket::parse(&data);
        assert!(matches!(result, Err(SapError::InvalidVersion(0))));
    }
}
