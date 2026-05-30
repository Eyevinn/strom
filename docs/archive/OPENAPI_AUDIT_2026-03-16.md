# OpenAPI/utoipa Audit – Strom

> **Archived — completed.** A point-in-time audit; all findings shipped and the contract
> is now snapshot-tested in CI. Kept as a record of the initiative.

**Date:** 2026-03-16
**Purpose:** Map how strict the API contract is today and what is required for external consumers (e.g. Open Live) to build on it.

## 1. Coverage

**Before (audit):**

| Category | Count | Share |
|----------|-------|-------|
| Endpoints with `#[utoipa::path]` | 68 | 72 % of 94 routes |
| Endpoints registered in `openapi.rs` | 47 | 50 % of 94 routes |
| Annotated but **missing from openapi.rs** | 24 | 26 % |
| No annotation at all | 26 | 28 % |

**After (fixed):**

| Category | Count | Share |
|----------|-------|-------|
| Endpoints with `#[utoipa::path]` | 83 | 88 % of 94 routes |
| Endpoints registered in `openapi.rs` | 83 | 88 % of 94 routes |
| Annotated but missing from `openapi.rs` | 0 | 0 % |
| No annotation at all | 11 | 12 % |

### Fixed
- 24 previously annotated but unregistered endpoints now registered in `openapi.rs` (discovery, media player, flow endpoints)
- 12 WHIP/WHEP proxy endpoints annotated and registered
- 30 schema types added to `components(schemas(...))`

### Remaining gaps
- `health` endpoint – intentionally undocumented
- 3 HTML page routes (`whep_player`, `whep_streams_page`, `whip_ingest_page`) – serve HTML, not JSON
- 6 static asset routes – CSS/JS files, not API endpoints
- ~10 discovery endpoints still use `impl IntoResponse` (annotations specify types explicitly, but signatures are not compile-time checked)

## 2. Validation status

| Aspect | Before | After |
|--------|--------|-------|
| Validation crate | None | **`garde` added** – derives on 10 request types with constraints (non-empty, length limits, duration ranges) |
| Custom Axum extractors | None | **`JsonBody<T>` + `ValidatedJson<T>` added** |
| Handler migration | All used `Json<T>` | **All migrated** – no bare `Json<T>` input extractors remain |
| Runtime validation | None | **Enforced** – `ValidatedJson<T>` calls `garde::Validate` on deserialization, returns 422 on failure |
| Structured error responses | Only in handler logic | **All endpoints** – deserialization failures now return `ErrorResponse` JSON |
| Path traversal protection | Yes | Yes |
| Query param validation | Partial | Partial |
| Body size limit | Upload only | Upload only |
| Auth validation | Yes | Yes |

### How it works now
- `ValidatedJson<T>` – deserializes JSON, runs `garde::Validate`, returns structured `ErrorResponse` on either failure (400/415/422)
- `JsonBody<T>` – deserializes JSON, returns structured `ErrorResponse` on failure (for types without garde validation)
- All handlers migrated: validated types use `ValidatedJson<T>`, others use `JsonBody<T>`

## 3. WebSocket contract status

| Aspect | Before | After |
|--------|--------|-------|
| Endpoint | Registered | Registered |
| Event type | `StromEvent`, 33 variants | No change |
| Serialization | JSON tagged enum | No change |
| ToSchema annotation | **Missing** | **Added** on `StromEvent`, `ServerMessage`, `ClientMessage` |
| Formal schema definition | None | **Event types now in OpenAPI components** |
| Versioning | None | None |
| Direction | Server → client | No change |

### Remaining work
- No versioning mechanism for WebSocket events
- Breaking change rules documented in `CLAUDE.md` but not enforced at CI level

## 4. ToSchema gaps

### Previously missing – now fixed

| Type | Status |
|------|--------|
| `StromEvent` | **Fixed** – `ToSchema` added |
| `ServerMessage` | **Fixed** – `ToSchema` added |
| `ClientMessage` | **Fixed** – `ToSchema` added |

### Serde attributes with schema risk (unchanged)

| Attribute | Type | Risk |
|-----------|------|------|
| `#[serde(untagged)]` | `PropertyValue` (`element.rs:136`) | Schema generates `oneOf` without discriminator |
| Custom `Deserialize` impl | `CpuAffinity` (`flow.rs:31`) | Schema does not reflect custom parsing logic |

### Backend types outside `strom-types` (fixed)

21 API-visible types moved to `strom-types` in dedicated modules:
- `types/src/discovery.rs` – `DiscoveredStreamResponse`, `DeviceResponse`, `DeviceCategory`, `AnnouncedStreamResponse`, `DeviceDiscoveryStatus`, `DeviceCountByCategory`, `NdiDiscoveryStatus`
- `types/src/mediaplayer.rs` – `PlayerAction`, `PlayerControlRequest`, `SetPlaylistRequest`, `SeekRequest`, `GotoRequest`, `PlayerStateResponse`
- `types/src/auth.rs` – `LoginRequest`, `LoginResponse`
- `types/src/whep.rs` – `WhepStreamInfo`, `WhepStreamsResponse`, `IceServersResponse`, `IceServer`
- `types/src/whip.rs` – `ClientLogEntry`
- `DynamicPadsResponse` added to `types/src/api.rs`

Backend modules re-export via `pub use` for internal backward compatibility.

## 5. CI protection

| Protection mechanism | Before | After |
|----------------------|--------|-------|
| OpenAPI snapshot test | Missing | **Added** – `openapi_snapshot_test.rs` |
| OpenAPI spec in repo | Missing | **Added** – `openapi_snapshot.json` |
| Schema diff in CI | Missing | **Added** – `oasdiff` job in CI (PRs only) |
| Breaking change detection | Missing | **Added** – `oasdiff breaking` fails CI on breaking changes |
| Integration tests against schema | Missing | Missing |
| Pre-commit hook | No schema check | No schema check |

### How it works now
- `cargo test --test openapi_snapshot_test` catches any spec drift
- CI runs `oasdiff` on PRs to detect breaking changes and generate a changelog
- Contract rules documented in `CLAUDE.md`

## 6. Prioritized action list

### Phase 1 – Minimal protection (DONE)

| # | Action | Status |
|---|--------|--------|
| 1 | Register 24 missing endpoints in `openapi.rs` | Done |
| 2 | Global rejection handler (`JsonBody<T>`) | Done |
| 3 | Snapshot test + committed `openapi_snapshot.json` | Done |

### Phase 2 – Externally consumable contract (DONE)

| # | Action | Status |
|---|--------|--------|
| 4 | `ToSchema` on `StromEvent`, `ServerMessage`, `ClientMessage` | Done |
| 5 | Annotate WHIP/WHEP proxy endpoints | Done |
| 6 | `garde` validation derives on request types | Done |
| 7 | `oasdiff` breaking change detection in CI | Done |
| 8 | OpenAPI version from `CARGO_PKG_VERSION` | Done |
| 9 | Contract rules in `CLAUDE.md` | Done |

### Phase 3 – Robust contract (DONE)

| # | Action | Status |
|---|--------|--------|
| 10 | `ValidatedJson<T>` extractor with garde validation | Done |
| 11 | Migrate all handlers from `Json<T>` to `JsonBody<T>`/`ValidatedJson<T>` | Done |
| 12 | Move 21 backend API types to `strom-types` | Done |

### Remaining (future work)

| # | Action | Impact | Effort |
|---|--------|--------|--------|
| 13 | WebSocket event versioning | Backward compatibility on event changes | Large |
| 14 | Integration tests validating responses against schema | Proves schema is correct, not just declarative | Large |
