# TAMS Storage Integration Plan

> **Code is the source of truth — this plan may have drifted; read the code for the
> current implementation.** This is a planning/design document, kept in `docs/archive/`
> per the documentation rules. It captures intent at the time of writing, not a
> guaranteed description of the shipped code.

## Goal

Wire [TAMS](https://github.com/bbc/tams) (Time-Addressable Media Store) storage into
Strom for **recording**, **playback** and **replay**, using the
[Eyevinn TAMS Gateway](https://github.com/Eyevinn/tams-gateway) as the store
(CouchDB index + S3/MinIO essence, presigned URLs, no transcoding/HLS).

## TAMS model (the parts we use)

- **Source** — abstract content (UUID). One per logical recording.
- **Flow** — an *immutable* single-essence timeline (UUID). One video flow + one
  audio flow per recording, grouped under a Source.
- **Flow Segment** — one independently decodable media object (a self-contained mp4
  here) mapped to a `timerange` on the flow timeline.
- **Timerange** string: `[<sec>:<ns>_<sec>:<ns>)` in **TAI**.

### Gateway contract (verified against tams-gateway source)

- `PUT /flows/{id}` — create/replace flow. Body requires `id`, `source_id`, `codec`
  (string, e.g. `video/h264`), `format` (`urn:x-nmos:format:video|audio`),
  `essence_parameters` (object; all sub-fields optional). Optional `container`
  (`video/mp4`), `label`, etc. The gateway auto-creates the Source from `source_id`.
- `POST /flows/{id}/storage` — body `{ limit?, content_type? }` → `201
  { media_objects: [{ object_id, put_url: { url, "content-type" } }] }`.
- `PUT <put_url.url>` — S3 presigned PUT, header `Content-Type` = the returned
  `content-type`, body = the segment bytes.
- `POST /flows/{id}/segments` — body single or array of
  `{ object_id, timerange, sample_count?, sample_offset? }` → `201` (empty) on
  success, `200 { failed_segments: [...] }` on partial failure.
- `GET /flows/{id}/segments?timerange=[..)` → `[{ object_id, timerange,
  sample_count, sample_offset, get_urls: [{ url }] }]` (presigned GET URLs).
- Auth: `Authorization: Bearer <API_TOKEN>` when the gateway enforces its own token
  (standalone). Behind an OSC access gate, leave the gateway token unset and pass the
  SAT through instead.

**Key simplification:** with presigned URLs, Strom only needs the **gateway base URL
+ bearer token**. No S3 credentials live in Strom.

## Naming collision

Strom "Flow" = the pipeline graph. TAMS "Flow" = an immutable essence timeline. They
are different. All TAMS identifiers are prefixed `tams_` in code/UI to avoid
confusion.

## Mapping onto Strom

| TAMS concept | Strom mechanism |
|---|---|
| Write segments | a recorder-style block: `splitmuxsink` (mp4mux) → temp dir, GOP-aligned splits; each completed file = one segment |
| Per-segment timerange | `splitmuxsink::format-location-full` gives the first sample of each new fragment → fragment *N*'s range is `[pts_N, pts_{N+1})` |
| Upload + register | async tokio task fed by a bounded channel; HTTP stays off GStreamer threads |
| Read / replay | a source block: `GET segments?timerange` → ordered presigned URLs → `souphttpsrc`→`concat`→`parsebin`/`decodebin` |
| Absolute timeline | pipeline clock in TAI/PTP mode (already supported); map buffer PTS → TAI |
| Config | per-block `gateway_url` + `api_token` properties (env-var fallback), not the flows `Storage` backend |

Because `splitmuxsink` already writes a **complete, independently decodable mp4 per
split**, no CMAF fragmentation is needed for v1 — one split file = one TAMS media
object = one segment.

## Phases

### Phase 0 — Spike (validate the risky bits)
- Stand up the gateway locally (Docker: CouchDB + MinIO + gateway).
- Confirm exact per-fragment timerange from `splitmuxsink::format-location-full`.
- Round-trip: write a few mp4 segments → register → `GET segments` → play back.

### Phase 1 — Write path (this change)
- `strom-types`: `StromEvent::TamsSegmentRegistered` + `StromEvent::TamsError`.
- `backend/src/tams/`: gateway HTTP client (`client.rs`) + segment uploader task
  (`uploader.rs`).
- `backend/src/blocks/builtin/tams_output.rs`: `builtin.tams_output` block. One video
  + one audio track, each muxed to its own TAMS flow (single-essence). Deterministic
  (UUIDv5) source/flow IDs derived from the block instance id so flow restarts append
  to the same timeline.
- Register block in `builtin/mod.rs` (`get_all_builtin_blocks`, `get_builder`).

**Known v1 limitations (documented, not silently dropped):**
- The final in-progress segment is not uploaded until the next split/rotation
  (no EOS flush yet). Short recordings may lose the tail; mitigate with a small
  `segment_duration`.
- TAI mapping uses `Unix + leap-second offset` captured at first fragment; good
  enough for an isolated flow, refine with the pipeline clock for broadcast sync.
- Upload channel is bounded; on sustained backpressure segments are dropped with a
  `warn!` + `TamsError` event rather than blocking the pipeline.

### Phase 2 — Read / replay
- `backend/src/tams/source.rs` + `builtin.tams_input` block. Resolve `tams_flow_id` +
  `timerange` → ordered segment URLs → `concat` → decode/parse → downstream blocks.
- Two modes: static replay (bounded range, seek/slow-mo) and live-edge following
  (poll for new segments).
- Replay output reuses existing WHEP/SRT/recorder blocks.

### Phase 3 — UX & polish
- `tams://{flow_id}?timerange=...` URIs in the Media Player block.
- Frontend: gateway config, flow browser, timerange picker, upload-health events.
- EOS flush of the tail segment; optional CMAF/fMP4 for lower-latency replay.

## Files (Phase 1)

- `types/src/events.rs` — new event variants (+ `ToSchema`, + description match arm).
- `backend/src/tams/mod.rs`, `client.rs`, `uploader.rs` — new module.
- `backend/src/blocks/builtin/tams_output.rs` — new block.
- `backend/src/blocks/builtin/mod.rs` — registration.
- `backend/src/lib.rs` / `main.rs` — declare `mod tams;` if needed.

## Testing

- Unit: timerange formatting (`format_timerange`), deterministic ID derivation.
- Integration (manual / Phase 0 harness): full round-trip against a local gateway.
- `cargo test --test openapi_test` if any API-visible type changes (events are
  WebSocket, so update the WS/openapi snapshot intentionally).
