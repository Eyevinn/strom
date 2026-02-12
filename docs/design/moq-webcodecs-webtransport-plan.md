# MoQ / WebCodecs / WebTransport as a Complement to WebRTC in Strom

## Background

Strom currently uses WebRTC via WHEP/WHIP for low-latency browser delivery.
WebRTC works well but has limitations when scaling (broadcast/one-to-many)
and gives developers limited control over the codec pipeline in the browser.

Three emerging technologies can complement WebRTC:

| Technology | Role | Maturity |
|------------|------|----------|
| **WebTransport** | Transport layer (QUIC to browser) | Supported in Chrome/Firefox/Edge. **Not Safari.** |
| **WebCodecs** | Encode/decode in the browser | Supported in all major browsers incl. Safari 26.1 |
| **MoQ (Media over QUIC)** | Pub/sub protocol for media on top of QUIC | IETF draft-15, not yet RFC. Experimental. |

---

## Current State of Strom

```
GStreamer pipeline
  ├── WHEP output (whepserversink) ──► WebRTC to browser
  ├── WHIP input/output             ──► WebRTC ingest/egress
  ├── AES67 (RTP multicast)         ──► Professional audio
  ├── SRT/MPEG-TS                   ──► Contribution/distribution
  └── NDI, DeckLink                 ──► Local studio
```

The Axum server proxies WHEP/WHIP signaling and handles SDP/ICE.
The frontend (egui/WASM) communicates via REST + WebSocket.

---

## Technology Overview

### WebTransport

- QUIC-based, provides both reliable streams and unreliable datagrams
- Eliminates head-of-line blocking (each QUIC stream is independent)
- Requires HTTP/3 (ALPN `h3`) and TLS 1.3
- **Safari lacks support** -- no public timeline from Apple
- Rust ecosystem: `quinn` (mature QUIC), `wtransport`, `h3-webtransport`
- GStreamer: `gst-plugin-quinn` in the official `gst-plugins-rs` since 1.26
  - Elements: `quinnwtsrc`, `quinnwtsink`

### WebCodecs

- Low-level API for video/audio encode/decode in the browser
- Gives the developer full control over frames (unlike WebRTC's black box)
- `VideoDecoder` / `VideoEncoder` / `AudioDecoder` / `AudioEncoder`
- Supported in Chrome, Firefox, Edge, Safari 26.1
- Renders via `<canvas>` or `VideoTrackGenerator` -> `<video>`
- Can receive encoded chunks from any transport (WebTransport, WebSocket, fetch)

### MoQ (Media over QUIC)

- IETF standardization in progress (draft-ietf-moq-transport-15, October 2025)
- Pub/sub model: tracks (named streams) and groups (independently decodable chunks, typically GOPs)
- Relay architecture: codec-agnostic nodes that fan out data (CDN-friendly)
- Rust implementations:
  - `moq-dev/moq` (kixelated) -- most active, supports `iroh` (P2P QUIC)
  - `cloudflare/moq-rs` -- production-tested at Cloudflare
- GStreamer plugins:
  - `hang-gst` / `moq-gst` (moq-dev) -- publish/subscribe
  - `gst-moq-pub` (Cloudflare) -- publish to MoQ relay

---

## Architecture Proposal

### Phase 1: WebTransport + WebCodecs (without MoQ)

Simplest starting point. Provides low latency without WebRTC's signaling complexity.

```
┌──────────────── Strom Backend ─────────────────┐
│                                                  │
│  GStreamer pipeline                              │
│    └── videoenc (H.264/H.265)                    │
│        └── [new] quinnwtsink / custom sink       │
│            └── QUIC stream per GOP               │
│                                                  │
│  Axum server                                     │
│    ├── Existing REST/WS API                      │
│    ├── Existing WHEP/WHIP proxy                  │
│    └── [new] WebTransport endpoint (/wt/{id})    │
│         ├── H3 session setup (quinn + h3)        │
│         └── Sends encoded frames                 │
│             via QUIC uni-streams or datagrams    │
│                                                  │
└──────────────────────────────────────────────────┘
              │
              │ QUIC / WebTransport
              ▼
┌──────────── Browser ────────────────┐
│                                      │
│  WebTransport API                    │
│    └── Receive encoded chunks        │
│                                      │
│  WebCodecs                           │
│    ├── VideoDecoder (H.264/H.265)    │
│    └── AudioDecoder (Opus/AAC)       │
│                                      │
│  Rendering                           │
│    └── <canvas> or <video>           │
│                                      │
└──────────────────────────────────────┘
```

#### Concrete Steps

1. **Backend: QUIC/H3 layer in Axum**
   - Add `quinn` and `h3`/`h3-quinn` as dependencies
   - Create a parallel QUIC listener alongside the existing TCP listener
   - Implement WebTransport session handling (HTTP/3 CONNECT)
   - Endpoint: `GET /wt/{endpoint_id}` -> upgrades to a WebTransport session

2. **Backend: New output block `WebTransportOutput`**
   - Similar structure to `WhepOutputBlock` but without SDP/ICE
   - Retrieves encoded data from the GStreamer pipeline (via appsink or inter element)
   - Packages in a simple frame format: `[timestamp][flags][codec_id][data]`
   - Sends each GOP group on a new QUIC uni-directional stream
     (or datagrams for ultra-low latency with accepted loss)
   - Registers itself in a `WebTransportRegistry` (similar to `WhepRegistry`)

3. **Frontend: JavaScript WebTransport player**
   - New HTML page `/player/wt?endpoint=/wt/{id}` (similar to existing WHEP player)
   - Opens a `WebTransport` session to the server
   - Receives streams, demuxes frame headers
   - Feeds `EncodedVideoChunk` / `EncodedAudioChunk` to `VideoDecoder` / `AudioDecoder`
   - Renders to `<canvas>` (VideoFrame -> drawImage)
   - Fallback: displays a message if WebTransport is not supported (Safari)

4. **Configuration**
   - `.strom.toml`: new section `[quic]` with port, cert/key (required for QUIC)
   - Share TLS cert with existing HTTPS configuration if possible
   - Block parameters: latency-mode (stream vs datagram), codec

#### Files to Create/Modify

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `quinn`, `h3`, `h3-quinn`, `h3-webtransport` |
| `backend/src/quic/mod.rs` | New module: QUIC listener, WebTransport session |
| `backend/src/quic/session.rs` | Session handling, stream creation |
| `backend/src/blocks/builtin/wt_output.rs` | New block: WebTransport Output |
| `backend/src/api/wt_player.rs` | Player page and endpoint proxy |
| `backend/src/api/lib.rs` | Register new routes |
| `backend/src/state.rs` | Add `WebTransportRegistry` |
| `strom-types/src/block.rs` | New block type |
| `frontend/assets/wt-player.html` | JavaScript WebTransport + WebCodecs player |

#### Estimated Complexity

- Backend QUIC layer: Medium-high (quinn + h3 integration with Axum)
- Output block: Medium (similar to WHEP but simpler without SDP)
- JS player: Medium (WebTransport + WebCodecs APIs are relatively straightforward)
- Biggest risk: TLS certificates (QUIC requires valid TLS, self-signed requires extra steps)

---

### Phase 2: MoQ Integration

Adds pub/sub semantics and enables relay-based scaling.

```
┌─── Strom Backend (Publisher) ───┐
│                                  │
│  GStreamer pipeline              │
│    └── hang-gst / moq-gst sink  │
│        └── Publishes tracks      │
│                                  │
└──────────┬───────────────────────┘
           │ MoQT (QUIC)
           ▼
┌─── MoQ Relay (optional) ────────┐
│                                  │
│  moq-relay (moq-dev/moq)        │
│  Subscribes upstream,            │
│  fans out to N subscribers       │
│                                  │
└──────────┬───────────────────────┘
           │ MoQT (WebTransport)
           ▼
┌─── Browser ─────────────────────┐
│                                  │
│  moq-js (subscriber)            │
│    └── WebCodecs decode          │
│                                  │
└──────────────────────────────────┘
```

#### Concrete Steps

1. **Integrate the `hang-gst` plugin**
   - Build and load `hang-gst` as a GStreamer plugin
   - Create a `MoqOutputBlock` that configures the hang-gst sink
   - Parameters: relay URL, track name, codec configuration

2. **Optional: embedded MoQ relay**
   - Use `moq-native` as a library in the Strom backend
   - Run a minimal relay instance in-process
   - Or: point to an external relay (moq-dev/moq or Cloudflare)

3. **Web player with moq-js**
   - Integrate `moq-js` (TypeScript/WASM) in a player page
   - Alternative: build a custom subscriber with WebTransport + WebCodecs
     (more control but more work)

4. **MoQ ingest (input)**
   - New `MoqInputBlock` that subscribes to MoQ tracks
   - Feeds decoded media into the GStreamer pipeline
   - Enables: MoQ source -> Strom processing -> any output

#### Risks with MoQ

- **The specification is not stable** -- breaking changes between drafts
- **Few production deployments** outside of Cloudflare
- **moq-dev/moq vs cloudflare/moq-rs** -- two diverging implementations
- **Safari** lacks WebTransport (required for MoQ in the browser)

---

### Phase 3: Hybrid Architecture (long-term)

```
┌────────── Strom Backend ──────────────────────────────┐
│                                                        │
│  GStreamer pipeline                                    │
│    ├── whepserversink  → WebRTC (conferencing, P2P)   │
│    ├── hang-gst sink   → MoQ relay (broadcast)        │
│    ├── quinnwtsink     → Direct WebTransport (simple) │
│    ├── AES67/NDI       → Professional AV              │
│    └── SRT/MPEG-TS     → Contribution                 │
│                                                        │
│  Axum                                                  │
│    ├── HTTP/1.1 + HTTP/2 (TCP) ← existing             │
│    └── HTTP/3 (QUIC)           ← new                  │
│        ├── WebTransport sessions                       │
│        └── MoQ relay (optional)                        │
│                                                        │
│  Adaptive delivery                                     │
│    └── Select protocol based on:                       │
│        - Browser support (Safari → WHEP, others → WT) │
│        - Use case (conferencing → WebRTC, broadcast →  │
│          MoQ)                                          │
│        - Network conditions                            │
│                                                        │
└────────────────────────────────────────────────────────┘
```

#### Adaptive Fallback Strategy in the Browser

```javascript
async function connect(endpointId) {
  // 1. Try WebTransport + WebCodecs (lowest latency, most control)
  if (typeof WebTransport !== 'undefined') {
    return connectWebTransport(endpointId);
  }
  // 2. Fallback to WebRTC/WHEP (Safari, older browsers)
  return connectWhep(endpointId);
}
```

---

## Recommended Order

| Step | What | Why | Depends On |
|------|------|-----|------------|
| **1a** | QUIC layer in Axum (quinn + h3) | Prerequisite for everything else | Nothing |
| **1b** | Simple WebTransport output + JS player | Fastest to validate, immediate value | 1a |
| **1c** | WebTransport input (ingest from browser) | Replaces WHIP for compatible browsers | 1a |
| **2a** | Evaluate hang-gst / moq-gst | Understand API and maturity | Nothing (parallel) |
| **2b** | MoQ output block with external relay | Scalable broadcast | 2a |
| **2c** | Embedded MoQ relay | All-in-one solution | 2b |
| **3** | Adaptive protocol selector | Best possible experience per client | 1b, 2b |

---

## The Safari Problem

Safari has no stable WebTransport support. Experimental support exists behind a
feature flag in iOS 18 but is unreliable. However, WebTransport is included in
the [Interop 2026](https://webkit.org/blog/17818/announcing-interop-2026/)
initiative, which is a strong signal that Safari will gain support during 2026.

**Consequence:** WebRTC/WHEP must be maintained as a full alternative.
WebTransport/MoQ becomes a *complement*, not a replacement.

**In practice:** The player page should feature-detect `WebTransport` in JS and
fall back to WHEP automatically.

---

## Relevant Rust Crates

| Crate | Version | Usage |
|-------|---------|-------|
| `quinn` | 0.11+ | QUIC implementation |
| `h3` | 0.0.6+ | HTTP/3 protocol |
| `h3-quinn` | 0.0.7+ | h3 <-> quinn adapter |
| `h3-webtransport` | 0.1+ | WebTransport sessions |
| `wtransport` | 0.6+ | Alternative: complete WebTransport server |
| `moq-native` | (git) | MoQ pub/sub, QUIC-based |
| `moq-karp` | (git) | MoQ media layer (codecs, catalog) |

## Relevant GStreamer Elements

| Element | Plugin | Function |
|---------|--------|----------|
| `quinnwtsink` | gst-plugin-quinn | Send data via WebTransport |
| `quinnwtsrc` | gst-plugin-quinn | Receive data via WebTransport |
| `quinnquicsrc/sink` | gst-plugin-quinn | Raw QUIC streams |
| `quinnroqmux` | gst-plugin-quinn | RTP-over-QUIC |
| hang-gst elements | hang-gst | MoQ publish/subscribe |

---

## Open Questions

1. **TLS certificates:** QUIC requires valid TLS 1.3. Should we share certs with
   the existing HTTPS config or keep them separate? Self-signed certs require
   the browser to trust them (Chrome: `--origin-to-force-quic-on`).

2. **Framing format (phase 1):** Simple custom format or an existing
   container format (fMP4, CMAF) over WebTransport streams?
   fMP4/CMAF has the advantage of being proven and having good WebCodecs support.

3. **GStreamer integration:** Use `gst-plugin-quinn` elements directly
   in the pipeline, or `appsink` -> Rust code -> QUIC? Appsink gives more control
   but requires more code.

4. **Port sharing:** Can QUIC (UDP) and HTTP (TCP) share the same port number?
   Yes, it is possible but requires listening on both TCP and UDP on the same port.
   Alternative: separate QUIC port (e.g. 4443).

5. **MoQ spec version:** Which draft to follow? moq-dev/moq has its own
   simplifications (moq-lite), cloudflare/moq-rs follows IETF draft-14.
   Suggestion: wait until the RFC is published, or follow moq-dev/moq
   which prioritizes simplicity.
