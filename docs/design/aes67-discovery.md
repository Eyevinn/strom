# AES67 Discovery Protocols - Design Document

## Overview

AES67 deliberately **does not mandate** a discovery mechanism. Different vendors use different approaches, which creates interoperability challenges. This document outlines the discovery protocols used in the AES67 ecosystem and proposes an implementation strategy for Strom.

## Protocol Landscape

| Protocol | Used By | Mechanism |
|----------|---------|-----------|
| **SAP** | Dante (AES67 mode) | Multicast SDP announcements on 224.2.127.254:9875 |
| **mDNS/DNS-SD + RTSP** | RAVENNA, Livewire+ | Bonjour service discovery → RTSP query for SDP |
| **NMOS IS-04/IS-05** | Broadcast/ST2110 | HTTP REST APIs + DNS-SD for registry discovery |
| **Proprietary** | Dante (native), Livewire (native) | Vendor-specific protocols |

---

## Protocol Details

### 1. SAP (Session Announcement Protocol)

**RFC:** [RFC 2974](https://datatracker.ietf.org/doc/html/rfc2974)

SAP is a simple multicast protocol for announcing multimedia sessions. Dante devices use SAP when operating in AES67 mode.

#### Key Specifications

- **Multicast address:** 224.2.127.254 (global scope)
- **Port:** 9875
- **Max announcement interval:** 300 seconds (5 minutes)
- **Bandwidth budget:** 4000 bps shared across all announcers in scope
- **Payload:** SDP (Session Description Protocol)
- **Compression:** Optional zlib compression

#### SAP Packet Format

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| V=1 |A|R|T|E|C|   auth len    |         msg id hash           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
:                  originating source (32 or 128 bits)          :
:                                                               :
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                    optional authentication data               |
:                              ....                             :
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      optional payload type                    |
+                location 0-3                     +-+-+-+-+-+-+-+
|                                                               |
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
- E (1 bit): Encryption flag
- C (1 bit): Compressed flag (zlib)
- auth len (8 bits): Authentication data length (in 32-bit words)
- msg id hash (16 bits): Hash for deduplication

#### Use Cases

- **Listener:** Discover Dante devices operating in AES67 mode
- **Announcer:** Make Strom AES67 outputs visible to Dante controllers

---

### 2. mDNS/DNS-SD (Bonjour)

**RFCs:** [RFC 6762 (mDNS)](https://datatracker.ietf.org/doc/html/rfc6762), [RFC 6763 (DNS-SD)](https://datatracker.ietf.org/doc/html/rfc6763)

RAVENNA and Livewire+ use mDNS for device/service discovery, combined with RTSP for SDP retrieval.

#### Service Types

```
_ravenna._sub._rtsp._tcp.local    # RAVENNA-specific streams
_rtsp._tcp.local                  # Generic RTSP services
```

#### Discovery Flow

1. Browse for `_rtsp._tcp.local` services via mDNS
2. Receive TXT records with service metadata
3. Connect to advertised RTSP URL
4. Send `DESCRIBE` request to get SDP
5. Parse SDP and present stream to user

#### RTSP DESCRIBE Example

```
DESCRIBE rtsp://192.168.1.100:554/stream1 RTSP/1.0
CSeq: 1
Accept: application/sdp

---

RTSP/1.0 200 OK
CSeq: 1
Content-Type: application/sdp
Content-Length: 420

v=0
o=- 1234567890 1 IN IP4 192.168.1.100
s=RAVENNA Stream
c=IN IP4 239.69.1.1/32
t=0 0
m=audio 5004 RTP/AVP 96
a=rtpmap:96 L24/48000/2
...
```

#### Rust Crates

- [`mdns-sd`](https://crates.io/crates/mdns-sd) - Pure Rust, 1.1M+ downloads, no async runtime dependency
- [`simple-mdns`](https://crates.io/crates/simple-mdns) - Alternative with sync/async options
- [`zeroconf`](https://crates.io/crates/zeroconf) - Wrapper around system Bonjour/Avahi

---

### 3. NMOS IS-04/IS-05

**Specifications:** [AMWA NMOS](https://specs.amwa.tv/)

NMOS is a suite of specifications from the Advanced Media Workflow Association for broadcast-grade discovery and connection management.

#### Components

| Spec | Name | Purpose |
|------|------|---------|
| IS-04 | Discovery & Registration | Device/resource discovery |
| IS-05 | Connection Management | Sender/receiver connections |
| IS-08 | Audio Channel Mapping | Audio routing within devices |

#### IS-04 Architecture

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Node A    │     │  Registry   │     │   Node B    │
│             │───▶│             │◀────│             │
│ Registration│     │ Query API   │     │ Registration│
│    API      │     │             │     │    API      │
└─────────────┘     └─────────────┘     └─────────────┘
       │                   │                   │
       ▼                   ▼                   ▼
   Senders            Query for            Receivers
   Receivers          resources
```

#### Peer-to-Peer Mode

For smaller networks without a registry:
1. Nodes announce via DNS-SD: `_nmos-node._tcp.local`
2. Peers browse and query Node APIs directly
3. No central registry required

#### Complexity Note

Full NMOS implementation is a significant undertaking - essentially building a broadcast control layer. Consider for future roadmap if targeting enterprise broadcast environments.

---

## Implementation Options

### Option A: SAP Only

**Scope:** Minimal viable discovery for Dante interop

**Components:**
- SAP listener service (background thread)
- SAP announcer for AES67 outputs
- API endpoints for discovered streams
- Frontend stream browser panel

**Pros:**
- Simple protocol, easy to implement
- Covers Dante AES67 interop (large install base)

**Cons:**
- No RAVENNA support
- SAP is somewhat legacy

---

### Option B: SAP + mDNS/DNS-SD (Recommended)

**Scope:** RAVENNA + Dante compatibility

Everything in Option A, plus:
- mDNS service browser using `mdns-sd` crate
- RTSP client for SDP retrieval
- Service registration for Strom outputs
- Unified stream list combining both discovery methods

**Pros:**
- Covers ~90% of real-world AES67 deployments
- Both protocols are well-documented
- Good Rust crate support for mDNS

**Cons:**
- More complex than SAP-only
- RTSP client adds some work

---

### Option C: Full NMOS

**Scope:** Broadcast-grade solution

Everything in Option B, plus:
- NMOS IS-04 Node API implementation
- NMOS IS-05 Connection Management
- Registry client for enterprise deployments

**Pros:**
- Full broadcast interoperability
- Future-proof for ST2110 environments

**Cons:**
- Significant implementation effort
- Overkill for most pro-audio use cases

---

## Proposed Architecture

```
backend/src/discovery/
├── mod.rs          # DiscoveryService, unified stream management
├── sap.rs          # SAP listener/announcer (RFC 2974)
├── mdns.rs         # mDNS browser/advertiser (RFC 6762/6763)
├── rtsp.rs         # Minimal RTSP DESCRIBE client
└── types.rs        # DiscoveredStream, DiscoverySource, etc.
```

### Core Types

```rust
/// A discovered AES67 stream
pub struct DiscoveredStream {
    pub id: String,
    pub name: String,
    pub source: DiscoverySource,
    pub sdp: String,
    pub multicast_address: IpAddr,
    pub port: u16,
    pub channels: u8,
    pub sample_rate: u32,
    pub encoding: AudioEncoding,
    pub last_seen: Instant,
    pub ttl: Duration,
}

pub enum DiscoverySource {
    Sap { origin_ip: IpAddr },
    Mdns { hostname: String, rtsp_url: String },
    Manual,
}

pub enum AudioEncoding {
    L16,
    L24,
    AM824,  // AES3 in RTP
}
```

### API Endpoints

```
GET  /api/discovery/streams          # List all discovered streams
GET  /api/discovery/streams/{id}     # Get specific stream details
POST /api/discovery/streams/{id}/use # Use stream SDP in a flow

GET  /api/discovery/config           # Discovery settings
PUT  /api/discovery/config           # Update settings (enable/disable protocols)

POST /api/discovery/announce         # Manually trigger announcement
```

### Frontend Integration

1. **Discovery Panel:** Sidebar or modal showing discovered streams
2. **Stream Browser:** Grouped by source (SAP/mDNS), filterable
3. **Quick Import:** Click stream → auto-populate AES67 Input block SDP
4. **Status Indicators:** Show stream health, last-seen time

---

## Implementation Phases

### Phase 1: SAP Listener
- Implement SAP packet parsing
- Background listener thread
- Store discovered streams with TTL
- Basic API endpoint

### Phase 2: SAP Announcer
- Generate SAP packets from AES67 Output blocks
- Configurable announcement interval
- Support announcement deletion on flow stop

### Phase 3: mDNS Browser
- Integrate `mdns-sd` crate
- Browse for `_rtsp._tcp.local` services
- Parse TXT records for metadata

### Phase 4: RTSP Client
- Implement minimal RTSP DESCRIBE
- Fetch SDP from discovered services
- Handle connection timeouts/retries

### Phase 5: mDNS Advertiser
- Register Strom AES67 outputs as RTSP services
- Implement RTSP server for DESCRIBE responses
- Or: Generate static SDP files accessible via HTTP

### Phase 6: Frontend Integration
- Stream discovery panel
- One-click import to AES67 Input
- Real-time stream status updates

---

## References

- [RFC 2974 - Session Announcement Protocol](https://datatracker.ietf.org/doc/html/rfc2974)
- [RFC 4566 - Session Description Protocol](https://datatracker.ietf.org/doc/html/rfc4566)
- [RFC 6762 - Multicast DNS](https://datatracker.ietf.org/doc/html/rfc6762)
- [RFC 6763 - DNS-Based Service Discovery](https://datatracker.ietf.org/doc/html/rfc6763)
- [AES67 Practical Guide - RAVENNA](https://ravenna-network.com/wp-content/uploads/2020/02/AES67-Practical-Guide-1.pdf)
- [RAVENNA Discovery](https://www.ravenna-network.com/demystifying-discovery/)
- [AMWA IS-04 Specification](https://specs.amwa.tv/is-04/)
- [AMWA IS-05 Specification](https://specs.amwa.tv/is-05/)
- [mdns-sd crate](https://crates.io/crates/mdns-sd)
- [tschiemer/aes67 framework](https://github.com/tschiemer/aes67)
- [RAV2SAP Bridge Tool](https://www.ravenna-network.com/)
