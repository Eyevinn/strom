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

## GStreamer Queues
- Leave `queue`, `queue2`, and `multiqueue` elements with default property values unless there is a documented latency requirement that justifies overriding them.

## Code Organization
- When working in or near a file that exceeds 1500 lines, proactively suggest splitting it into focused sub-modules (following the pattern used for `pipeline.rs` and `app.rs`)
- Each sub-module should have a single clear responsibility (e.g. construction, lifecycle, linking, properties)
- Check for large files with: `find backend/src frontend/src -name "*.rs" | xargs wc -l | sort -rn | head -20`
- Shared types (structs, enums), default values, and constants used by both frontend and backend must be defined in `strom-types`. Never duplicate these across crates. If you find a duplicate, move it to `strom-types`.

## Dead Code
- Never use blanket `#![allow(dead_code)]`. Each case must be handled individually.
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
