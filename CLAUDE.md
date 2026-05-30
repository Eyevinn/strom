## Project Overview
- **Frontend** (`strom-frontend`): egui-based GUI that compiles to both native and WASM
- **Backend** (`strom-backend`): Axum server that can run the native GUI and serve the embedded WASM version

## Language
- All code, comments, commit messages, PR titles, PR descriptions, and documentation must be in English

## Security
- Always anonymize sensitive data (IP addresses, hostnames, credentials, internal server names) before including in commits, PRs, or documentation
- Use `example.com`, `192.0.2.x`, or placeholder values instead of real infrastructure data

## Code Style
- Do not add emojis to log macros (`info!`, `debug!`, `trace!`, `warn!`, `error!`)
- If you find emojis in existing log rows, remove them. Emojis in UI icons are OK.

## GStreamer Pad Probes
- BUFFER probes fire on **every single buffer** in the pipeline — they are the hottest path in GStreamer. Never add a BUFFER probe without careful consideration.
- Inside a BUFFER probe callback: no Mutex locks, no heap allocations, no string formatting, no system calls. Use atomics and pre-computed values instead.
- Prefer `Instant::now()` over `SystemTime::now()` (Instant uses the vDSO fast path on Linux).
- For rate-limiting inside a BUFFER probe, use `AtomicU64` with `Instant`-based epoch offsets rather than `Mutex<Instant>`.
- EVENT_DOWNSTREAM probes (for caps detection, one-time setup) are fine — they fire infrequently.
- When reviewing or adding a probe, always ask: "Does this fire per-buffer or per-event?" and treat per-buffer probes as performance-critical code.

## GStreamer Object References in Closures
- **NEVER capture a strong reference (`clone()`) to a `gst::Pipeline`, `gst::Element`, or `gst::Bin` inside a signal handler closure** (e.g. `connect_pad_added`, `connect_element_added`, `connect("deep-element-added", ...)`). Elements own their signal handlers — capturing the pipeline or sibling elements creates a circular reference that prevents GStreamer from ever finalizing the pipeline. All OS resources (UDP sockets, threads, file descriptors) will leak on every pipeline restart.
- Use `WeakRef` (`ObjectExt::downgrade()`) instead, and `upgrade()` inside the closure. If upgrade returns `None`, the pipeline is already torn down — just return early.
- This also applies to `HashMap<String, gst::Element>` maps — never clone and capture the map; build a `HashMap<String, WeakRef<gst::Element>>` instead.
- The regression test `pipeline_lifecycle_test.rs` catches these leaks — it must always pass.
- `stop_flow()` in `state.rs` logs `ERROR` at runtime if a pipeline survives after drop — treat these as P0 bugs.

## GStreamer Queues
- Leave `queue`, `queue2`, and `multiqueue` elements with default property values unless there is a documented latency requirement that justifies overriding them.

## Code Organization
- When working in or near a file that exceeds 1500 lines, proactively suggest splitting it into focused sub-modules (following the pattern used for `pipeline.rs` and `app.rs`)
- Each sub-module should have a single clear responsibility (e.g. construction, lifecycle, linking, properties)
- Check for large files with: `find backend/src frontend/src -name "*.rs" | xargs wc -l | sort -rn | head -20`

## Documentation
- **The code is the source of truth.** Do not write or maintain docs that describe how the code works, its design, or its implementation — that documentation always drifts.
- Repo docs (`docs/`) are for navigation only: what Strom is, what it can do, how to set it up, how to contribute, and how things fit together at a high level. Keep them there.
- Do not create code-describing / design / implementation docs at top level. Anything that explains internals belongs in `docs/archive/` with a disclaimer (or should not exist).
- Operator/usage guides (how to *use* a feature) may stay top-level, but carry the disclaimer: "Code is the source of truth — this may have drifted; read the code for the current implementation." Do not cite file paths in disclaimers (paths drift too) — just say "read the code".
- Doc filenames in `docs/` use `UPPER_SNAKE_CASE` (e.g. `DOCKER_GPU_SETUP.md`). Leave `README.md` and `.github/ISSUE_TEMPLATE/*` lowercase (GitHub convention).
- There is no committed roadmap. Ideas go in `docs/FEATURE_SUGGESTIONS.md` (an unordered "not a roadmap" list) or as GitHub Issues/Discussions.
- Strom is authored by Claude Code (AI), not hand-written by humans. We welcome feature requests, ideas, and (ideally AI-written) PRs.

## Shared Types (`strom-types`)
- Before defining a new struct, enum, constant, or default value — always check if it already exists in `strom-types`. All new API-visible or shared types must be placed in `strom-types`, never directly in the backend. If you find a duplicate, move it to `strom-types`.
- `strom-types` must not depend on the backend, GStreamer crates, or other internal crates — only pure utility crates such as `serde` and `uuid`.

## API Contract
- Every new endpoint must have a `#[utoipa::path(...)]` annotation AND be registered in `openapi.rs`. Both are required — an annotation without registration does not appear in the schema.
- After changes to API types or endpoints, run the snapshot test (`cargo test --test openapi_test`). If it fails, update `openapi.json` in the repo root intentionally — do not silently let the schema drift.

## WebSocket Contract
- Any type referenced by a new `StromEvent` variant must have a `ToSchema` annotation (`#[cfg_attr(feature = "openapi", derive(ToSchema))]`). If the variant introduces new inner types, those need `ToSchema` too.
- Never modify an existing `StromEvent` variant (rename, change fields, remove) without treating it as an intentional breaking change.

## Dead Code
- Never use blanket `#![allow(dead_code)]`. Each case must be handled individually. Never use `#[allow(dead_code)]` in `strom-types`.
- For target-specific code (e.g. only used in WASM or only in native), use `#[cfg(target_arch = "wasm32")]` or `#[cfg(not(target_arch = "wasm32"))]` — not `#[allow(dead_code)]`.
- `#[allow(dead_code)]` is acceptable only for serde deserialization fields or event data fields that mirror the backend but are not yet displayed in the UI.

## Build
- Always build with `cargo check`, `cargo build`, or `cargo run` — never use the `-p` flag
- Build from the workspace root
- For focused frontend/GUI work, use `trunk serve` in the `frontend/` directory for a fast iteration loop — avoids full WASM compilation and server restart on every change. Work against `trunk serve` for visual fixes, then verify against the full backend when done.

## Static Files
- Static files (HTML/JS/CSS in `backend/static/`) are embedded at compile time via rust-embed
- Editing static files requires `cargo build` + server restart to take effect
- Bump the version number in the HTML after changes so the browser-loaded version can be verified

## Process Management
- Before starting the server or any test process, check if it is already running (`ps`, `curl`, log grep)

## Troubleshooting

### GUI Issues
1. Add logging to `strom-frontend`
2. Recompile and restart the backend. In native GUI mode (default), the backend log shows the full application log

### Pipeline Errors and Segfaults
- Use `GST_DEBUG` and `GST_DEBUG_FILE` for GStreamer logs
- Use config logging in `.strom.toml`, set level to `debug` or `trace`, then monitor the log file
- See `/docs` for segfault troubleshooting
- Do not suggest blacklisting elements when troubleshooting segfaults
