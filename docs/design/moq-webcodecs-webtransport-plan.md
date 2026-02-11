# MoQ / WebCodecs / WebTransport som komplement till WebRTC i Strom

## Bakgrund

Strom använder idag WebRTC via WHEP/WHIP för webbläsarleverans med låg latens.
WebRTC fungerar bra men har begränsningar vid skalning (broadcast/one-to-many)
och ger utvecklaren begränsad kontroll över codec-pipeline i webbläsaren.

Tre nya teknologier kan komplettera WebRTC:

| Teknologi | Roll | Mognadsgrad |
|-----------|------|-------------|
| **WebTransport** | Transportlager (QUIC till webbläsare) | Stöd i Chrome/Firefox/Edge. **Ej Safari.** |
| **WebCodecs** | Encode/decode i webbläsare | Stöd i alla stora webbläsare inkl. Safari 26.1 |
| **MoQ (Media over QUIC)** | Pub/sub-protokoll för media ovanpå QUIC | IETF draft-15, ej RFC. Experimentellt. |

---

## Nuläge i Strom

```
GStreamer pipeline
  ├── WHEP output (whepserversink) ──► WebRTC till webbläsare
  ├── WHIP input/output             ──► WebRTC ingest/egress
  ├── AES67 (RTP multicast)         ──► Professionell audio
  ├── SRT/MPEG-TS                   ──► Contribution/distribution
  └── NDI, DeckLink                 ──► Lokalt studio
```

Axum-servern proxar WHEP/WHIP-signalering och hanterar SDP/ICE.
Frontend (egui/WASM) kommunicerar via REST + WebSocket.

---

## Teknologiöversikt

### WebTransport

- QUIC-baserat, ger både reliable streams och unreliable datagrams
- Eliminerar head-of-line blocking (varje QUIC-stream är oberoende)
- Kräver HTTP/3 (ALPN `h3`) och TLS 1.3
- **Safari saknar stöd** -- ingen publik tidplan från Apple
- Rust-ekosystem: `quinn` (mogen QUIC), `wtransport`, `h3-webtransport`
- GStreamer: `gst-plugin-quinn` i officiella `gst-plugins-rs` sedan 1.26
  - Element: `quinnwtsink`, `quinnwtclientsrc`

### WebCodecs

- Lågnivå-API för video/audio encode/decode i webbläsaren
- Ger utvecklaren full kontroll över frames (till skillnad från WebRTC:s svarta låda)
- `VideoDecoder` / `VideoEncoder` / `AudioDecoder` / `AudioEncoder`
- Stöd i Chrome, Firefox, Edge, Safari 26.1
- Renderar via `<canvas>` eller `VideoTrackGenerator` → `<video>`
- Kan ta emot encoded chunks från vilken transport som helst (WebTransport, WebSocket, fetch)

### MoQ (Media over QUIC)

- IETF-standardisering pågår (draft-ietf-moq-transport-15, oktober 2025)
- Pub/sub-modell: tracks (namngivna strömmar) och groups (oberoende chunks, typiskt GOP:ar)
- Relay-arkitektur: codec-agnostiska noder som fläktar ut data (CDN-vänligt)
- Rust-implementationer:
  - `moq-dev/moq` (kixelated) -- mest aktiv, stöd för `iroh` (P2P QUIC)
  - `cloudflare/moq-rs` -- productionstestad hos Cloudflare
- GStreamer-plugins:
  - `hang-gst` / `moq-gst` (moq-dev) -- publicera/prenumerera
  - `gst-moq-pub` (Cloudflare) -- publicera mot MoQ-relay

---

## Arkitekturförslag

### Fas 1: WebTransport + WebCodecs (utan MoQ)

Enklast att börja med. Ger låg latens utan WebRTC:s signaleringskomplexitet.

```
┌──────────────── Strom Backend ─────────────────┐
│                                                  │
│  GStreamer pipeline                              │
│    └── videoenc (H.264/H.265)                    │
│        └── [ny] quinnwtsink / custom sink        │
│            └── QUIC stream per GOP               │
│                                                  │
│  Axum server                                     │
│    ├── Befintlig REST/WS API                     │
│    ├── Befintlig WHEP/WHIP proxy                 │
│    └── [ny] WebTransport endpoint (/wt/{id})     │
│         ├── H3 session setup (quinn + h3)        │
│         └── Skickar encoded frames               │
│             via QUIC uni-streams eller datagrams  │
│                                                  │
└──────────────────────────────────────────────────┘
              │
              │ QUIC / WebTransport
              ▼
┌──────────── Webbläsare ─────────────┐
│                                      │
│  WebTransport API                    │
│    └── Ta emot encoded chunks        │
│                                      │
│  WebCodecs                           │
│    ├── VideoDecoder (H.264/H.265)    │
│    └── AudioDecoder (Opus/AAC)       │
│                                      │
│  Rendering                           │
│    └── <canvas> eller <video>        │
│                                      │
└──────────────────────────────────────┘
```

#### Konkreta steg

1. **Backend: QUIC/H3-lager i Axum**
   - Lägg till `quinn` och `h3`/`h3-quinn` som dependencies
   - Skapa en parallell QUIC-listener vid sidan av den befintliga TCP-listenern
   - Implementera WebTransport session-hantering (HTTP/3 CONNECT)
   - Endpoint: `GET /wt/{endpoint_id}` → uppgraderas till WebTransport-session

2. **Backend: Ny output-block `WebTransportOutput`**
   - Liknande struktur som `WhepOutputBlock` men utan SDP/ICE
   - Hämtar encoded data från GStreamer-pipelinen (via appsink eller inter-element)
   - Paketerar i ett enkelt frame-format: `[timestamp][flags][codec_id][data]`
   - Skickar varje GOP-grupp på en ny QUIC uni-directional stream
     (eller datagrams för ultra-låg latens med accepterad förlust)
   - Registrerar sig i en `WebTransportRegistry` (likt `WhepRegistry`)

3. **Frontend: JavaScript WebTransport-spelare**
   - Ny HTML-sida `/player/wt?endpoint=/wt/{id}` (likt befintlig WHEP-player)
   - Öppnar `WebTransport`-session mot servern
   - Tar emot streams, demuxar frame-header
   - Matar `EncodedVideoChunk` / `EncodedAudioChunk` till `VideoDecoder` / `AudioDecoder`
   - Renderar till `<canvas>` (VideoFrame → drawImage)
   - Fallback: visa meddelande om WebTransport ej stöds (Safari)

4. **Konfiguration**
   - `.strom.toml`: ny sektion `[quic]` med port, cert/key (krävs för QUIC)
   - Dela TLS-cert med befintlig HTTPS-konfiguration om möjligt
   - Block-parametrar: latency-mode (stream vs datagram), codec

#### Filer att skapa/ändra

| Fil | Ändring |
|-----|---------|
| `Cargo.toml` (workspace) | Lägg till `quinn`, `h3`, `h3-quinn`, `h3-webtransport` |
| `backend/src/quic/mod.rs` | Ny modul: QUIC-listener, WebTransport session |
| `backend/src/quic/session.rs` | Session-hantering, stream-skapande |
| `backend/src/blocks/builtin/wt_output.rs` | Ny block: WebTransport Output |
| `backend/src/api/wt_player.rs` | Player-sida och endpoint-proxy |
| `backend/src/api/lib.rs` | Registrera nya routes |
| `backend/src/state.rs` | Lägg till `WebTransportRegistry` |
| `strom-types/src/block.rs` | Ny block-typ |
| `frontend/assets/wt-player.html` | JavaScript WebTransport + WebCodecs spelare |

#### Uppskattad komplexitet

- Backend QUIC-lager: Medel-hög (quinn + h3 integration med Axum)
- Output-block: Medel (liknande WHEP men enklare utan SDP)
- JS-spelare: Medel (WebTransport + WebCodecs API:erna är relativt rättframma)
- Största risken: TLS-certifikat (QUIC kräver giltig TLS, self-signed kräver extra steg)

---

### Fas 2: MoQ-integration

Lägger till pub/sub-semantik och möjliggör relay-baserad skalning.

```
┌─── Strom Backend (Publisher) ───┐
│                                  │
│  GStreamer pipeline              │
│    └── hang-gst / moq-gst sink  │
│        └── Publicerar tracks     │
│                                  │
└──────────┬───────────────────────┘
           │ MoQT (QUIC)
           ▼
┌─── MoQ Relay (valfri) ──────────┐
│                                  │
│  moq-relay (moq-dev/moq)        │
│  Prenumererar upstream,          │
│  fläktar ut till N subscribers   │
│                                  │
└──────────┬───────────────────────┘
           │ MoQT (WebTransport)
           ▼
┌─── Webbläsare ──────────────────┐
│                                  │
│  moq-js (subscriber)            │
│    └── WebCodecs decode          │
│                                  │
└──────────────────────────────────┘
```

#### Konkreta steg

1. **Integrera `hang-gst`-pluginet**
   - Bygg och ladda `hang-gst` som GStreamer-plugin
   - Skapa en `MoqOutputBlock` som konfigurerar hang-gst-sink
   - Parametrar: relay-URL, track-namn, codec-konfiguration

2. **Valfritt: inbäddad MoQ-relay**
   - Använd `moq-native` som bibliotek i Strom-backend
   - Kör en minimal relay-instans in-process
   - Eller: peka mot extern relay (moq-dev/moq eller Cloudflare)

3. **Webbspelare med moq-js**
   - Integrera `moq-js` (TypeScript/WASM) i en player-sida
   - Alternativt: bygg custom subscriber med WebTransport + WebCodecs
     (mer kontroll men mer arbete)

4. **MoQ-ingest (input)**
   - Ny `MoqInputBlock` som prenumererar på MoQ-tracks
   - Matar decoded media in i GStreamer-pipelinen
   - Möjliggör: MoQ-källa → Strom-processing → valfri output

#### Risker med MoQ

- **Specifikationen är inte stabil** -- breaking changes mellan drafts
- **Få produktionsinstallationer** utanför Cloudflare
- **moq-dev/moq vs cloudflare/moq-rs** -- två divergerande implementationer
- **Safari** saknar WebTransport (krävs för MoQ i webbläsare)

---

### Fas 3: Hybrid-arkitektur (långsiktigt)

```
┌────────── Strom Backend ──────────────────────────────┐
│                                                        │
│  GStreamer pipeline                                    │
│    ├── whepserversink  → WebRTC (konferens, P2P)      │
│    ├── hang-gst sink   → MoQ relay (broadcast)        │
│    ├── quinnwtsink     → Direkt WebTransport (enkel)  │
│    ├── AES67/NDI       → Professionell AV             │
│    └── SRT/MPEG-TS     → Contribution                 │
│                                                        │
│  Axum                                                  │
│    ├── HTTP/1.1 + HTTP/2 (TCP) ← befintligt           │
│    └── HTTP/3 (QUIC)           ← nytt                  │
│        ├── WebTransport sessions                       │
│        └── MoQ relay (valfritt)                        │
│                                                        │
│  Adaptiv leverans                                      │
│    └── Välj protokoll baserat på:                      │
│        - Webbläsarstöd (Safari → WHEP, övriga → WT)   │
│        - Use case (konferens → WebRTC, broadcast → MoQ)│
│        - Nätverksförhållanden                          │
│                                                        │
└────────────────────────────────────────────────────────┘
```

#### Adaptiv fallback-strategi i webbläsaren

```javascript
async function connect(endpointId) {
  // 1. Försök WebTransport + WebCodecs (lägst latens, mest kontroll)
  if (typeof WebTransport !== 'undefined') {
    return connectWebTransport(endpointId);
  }
  // 2. Fallback till WebRTC/WHEP (Safari, äldre webbläsare)
  return connectWhep(endpointId);
}
```

---

## Rekommenderad ordning

| Steg | Vad | Varför | Beroende av |
|------|-----|--------|-------------|
| **1a** | QUIC-lager i Axum (quinn + h3) | Grundförutsättning för allt annat | Inget |
| **1b** | Enkel WebTransport-output + JS-spelare | Snabbast att validera, ger omedelbart värde | 1a |
| **1c** | WebTransport-input (ingest från webbläsare) | Ersätter WHIP för kompatibla webbläsare | 1a |
| **2a** | Utvärdera hang-gst / moq-gst | Förstå API och mognad | Inget (parallellt) |
| **2b** | MoQ output-block med extern relay | Skalbar broadcast | 2a |
| **2c** | Inbäddad MoQ-relay | Allt-i-ett-lösning | 2b |
| **3** | Adaptiv protokollväljare | Bästa möjliga upplevelse per klient | 1b, 2b |

---

## Safari-problematiken

Safari saknar WebTransport-stöd helt. Ingen publik tidplan från Apple.
Experimentellt stöd bakom feature-flag i iOS 18 men otillförlitligt.

**Konsekvens:** WebRTC/WHEP måste behållas som fullgott alternativ.
WebTransport/MoQ blir ett *komplement*, inte en ersättare.

**Praktiskt:** Player-sidan bör feature-detecta `WebTransport` i JS och
falla tillbaka till WHEP automatiskt.

---

## Relevanta Rust-crates

| Crate | Version | Användning |
|-------|---------|------------|
| `quinn` | 0.11+ | QUIC-implementation |
| `h3` | 0.0.6+ | HTTP/3-protokoll |
| `h3-quinn` | 0.0.7+ | h3 ↔ quinn-adapter |
| `h3-webtransport` | 0.1+ | WebTransport sessions |
| `wtransport` | 0.6+ | Alternativ: komplett WebTransport-server |
| `moq-native` | (git) | MoQ pub/sub, QUIC-baserat |
| `moq-karp` | (git) | MoQ media-lager (codecs, katalog) |

## Relevanta GStreamer-element

| Element | Plugin | Funktion |
|---------|--------|----------|
| `quinnwtsink` | gst-plugin-quinn | Skicka data via WebTransport |
| `quinnwtclientsrc` | gst-plugin-quinn | Ta emot data via WebTransport |
| `quinnquicsrc/sink` | gst-plugin-quinn | Rå QUIC-strömmar |
| `quinnroqmux` | gst-plugin-quinn | RTP-over-QUIC |
| hang-gst elements | hang-gst | MoQ publish/subscribe |

---

## Öppna frågor

1. **TLS-certifikat:** QUIC kräver giltig TLS 1.3. Ska vi dela cert med
   befintlig HTTPS-config eller ha separat? Self-signed-cert kräver att
   webbläsaren litar på det (Chrome: `--origin-to-force-quic-on`).

2. **Framing-format (fas 1):** Enkelt custom-format eller befintligt
   container-format (fMP4, CMAF) över WebTransport-streams?
   fMP4/CMAF har fördelen att vara beprövat och att WebCodecs har bra stöd.

3. **GStreamer-integration:** Använda `gst-plugin-quinn`-element direkt
   i pipelinen, eller `appsink` → Rust-kod → QUIC? Appsink ger mer kontroll
   men mer kod.

4. **Port-delning:** Kan QUIC (UDP) och HTTP (TCP) dela samma portnummer?
   Ja, det är möjligt men kräver att man lyssnar på både TCP och UDP på
   samma port. Alternativ: separat QUIC-port (t.ex. 4443).

5. **MoQ-specversion:** Vilken draft att följa? moq-dev/moq har egna
   förenklingar (moq-lite), cloudflare/moq-rs följer IETF draft-14.
   Föreslår att avvakta tills RFC publiceras, eller följa moq-dev/moq
   som prioriterar enkelhet.
