# AES67 Discovery - Design & Implementation

## Overview

AES67 deliberately **does not mandate** a discovery mechanism. Different vendors use different approaches, which creates interoperability challenges. This document outlines the discovery protocols used in the AES67 ecosystem and documents Strom's implementation.

## Protocol Landscape

| Protocol | Used By | Mechanism | Strom Support |
|----------|---------|-----------|---------------|
| **SAP** | Dante (AES67 mode) | Multicast SDP announcements on 224.2.127.254:9875 | ✅ Implemented |
| **mDNS/DNS-SD + RTSP** | RAVENNA, Livewire+ | Bonjour service discovery → RTSP query for SDP | ❌ Not yet |
| **NMOS IS-04/IS-05** | Broadcast/ST2110 | HTTP REST APIs + DNS-SD for registry discovery | ❌ Not yet |

---

## Current Implementation

Strom implements **SAP (Session Announcement Protocol)** for AES67 stream discovery, providing interoperability with Dante devices in AES67 mode.

### Architecture

```
backend/src/discovery/
├── mod.rs          # DiscoveryService - manages listener, announcer, cleanup
├── sap.rs          # SAP packet parsing/generation (RFC 2974)
└── types.rs        # DiscoveredStream, AnnouncedStream, SdpStreamInfo
```

### Features

**SAP Listener:**
- Joins multicast group 224.2.127.254:9875
- Parses SAP announcement and deletion packets
- Extracts stream metadata from SDP (name, channels, sample rate, encoding)
- Tracks stream TTL and removes expired streams
- Broadcasts discovery events via WebSocket

**SAP Announcer:**
- Announces AES67 Output blocks from running flows
- Sends periodic announcements (30-second interval)
- Sends deletion packets when flows stop
- Uses consistent message ID hash for stream identity

### API Endpoints

```
GET  /api/discovery/streams           # List discovered streams
GET  /api/discovery/streams/{id}      # Get stream details
GET  /api/discovery/streams/{id}/sdp  # Get raw SDP content
GET  /api/discovery/announced         # List streams we're announcing
```

### Frontend Integration

**Discovery Page:**
- Split view with stream list and details panel
- Tabs for Discovered (RX) and Announced (TX) streams
- Search/filter functionality
- SDP display with copy button
- "Create Flow" button to create AES67 Input from discovered stream

**AES67 Input Block:**
- "Browse Streams" button in properties panel
- Stream picker modal to select from discovered streams
- Auto-fills SDP field when stream is selected

---

## SAP Protocol Details

**RFC:** [RFC 2974](https://datatracker.ietf.org/doc/html/rfc2974)

### Key Specifications

- **Multicast address:** 224.2.127.254 (global scope)
- **Port:** 9875
- **Default TTL:** 300 seconds (5 minutes)
- **Announcement interval:** 30 seconds (Strom default)
- **Payload:** SDP (Session Description Protocol)

### SAP Packet Format

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| V=1 |A|R|T|E|C|   auth len    |         msg id hash           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                  originating source (32 bits for IPv4)        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         SDP payload                           |
:                              ....                             :
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

**Header fields:**
- V (3 bits): Version, must be 1
- A (1 bit): Address type (0 = IPv4, 1 = IPv6)
- R (1 bit): Reserved
- T (1 bit): Message type (0 = announcement, 1 = deletion)
- E (1 bit): Encryption flag (not supported)
- C (1 bit): Compressed flag (zlib, not supported)
- msg id hash (16 bits): Consistent hash for stream identity

---

## Core Types

```rust
/// A discovered AES67 stream from SAP
pub struct DiscoveredStream {
    pub id: String,                    // Hash of origin + session ID
    pub name: String,                  // From SDP s= line
    pub source: DiscoverySource,       // SAP origin info
    pub sdp: String,                   // Raw SDP content
    pub multicast_address: IpAddr,     // From SDP c= line
    pub port: u16,                     // From SDP m= line
    pub channels: u8,                  // From SDP a=rtpmap
    pub sample_rate: u32,              // From SDP a=rtpmap
    pub encoding: AudioEncoding,       // L16, L24, or AM824
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub ttl: Duration,
}

/// A stream being announced by Strom
pub struct AnnouncedStream {
    pub flow_id: FlowId,
    pub block_id: String,
    pub msg_id_hash: u16,              // Consistent hash for SAP
    pub sdp: String,
    pub origin_ip: IpAddr,
    pub last_announced: Instant,
}
```

---

## Future Work

### mDNS/DNS-SD + RTSP (for RAVENNA)

Would add interoperability with RAVENNA and Livewire+ devices:

1. Browse for `_rtsp._tcp.local` services via mDNS
2. Parse TXT records for metadata
3. Connect to RTSP URL and send DESCRIBE request
4. Parse returned SDP

**Rust crates:** `mdns-sd`, `rtsp-types`

### NMOS IS-04/IS-05

Full broadcast-grade solution for ST2110 environments. Significant undertaking - essentially a broadcast control layer. Consider if targeting enterprise broadcast.

---

## References

- [RFC 2974 - Session Announcement Protocol](https://datatracker.ietf.org/doc/html/rfc2974)
- [RFC 4566 - Session Description Protocol](https://datatracker.ietf.org/doc/html/rfc4566)
- [AES67 Practical Guide - RAVENNA](https://ravenna-network.com/wp-content/uploads/2020/02/AES67-Practical-Guide-1.pdf)
