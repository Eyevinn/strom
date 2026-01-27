# Changelog

All notable changes to the Strom GStreamer Flow Engine project.

## [Unreleased]

---

## [0.3.14] - 2026-01-26

### Added
- WebRTC stats for WHEP Output blocks (#281)
- Double-click WHEP Output block to open player (#280)
- Compositor: Live View mode with scene transitions and thumbnails (#275)
- Generic device discovery using GStreamer DeviceMonitor (#268)
- Double-click on AES67/NDI Input blocks to open stream/source picker (#268)
- Windows development setup documentation in README (#273)

### Changed
- Update CEF to 144.0.11 (Chromium 144 stable) (#279)
- Optimize stats polling to only fetch for selected flow (#277)
- Remove deprecated OpenGL Compositor block (#274)

### Fixed
- Move system stats collection to background thread (#283)
- Filter WebRTC stats by block_id (#278)
- Reduce WHEP output log verbosity (#276)
- Windows dev setup scripts for pkg-config and GStreamer 1.26 (#269, #271, #272)
- Prevent AccessKit crash on Windows when selecting flows (#266)

---

## [0.3.13] - 2026-01-22

### Added
- Compositor: improved layout editor with persistence (#262)
- Zoom-to-fit and reset view in graph editor (#261)
- HTML overlay support via `strom-full` Docker image with CEF/gstcefsrc (#254)
- HTML rendering documentation with example flows (#257, #259)
- gstcefsrc build workflow for CI (#253)

### Changed
- Improved AES67 SDP generation and QoS settings (#252)

### Fixed
- CEF resource symlinks for strom-full Docker image (#256)
- Build gstcefsrc for Ubuntu 25.10 and fix CEF runtime (#255)

---

## [0.3.12] - 2026-01-17

### Added
- QoS DSCP marking for AES67 output (#249)

### Changed
- Update GStreamer to 1.26.10 in installers and CI (#239)
- Remove GStreamer version pinning from Dockerfile (#240)

### Fixed
- Use `use_clock()` to force PTP clock on pipeline (#250)
- VA-API encoder improvements (#242)
- Remove emojis from backend log output (#238)

---

## [0.3.11] - 2026-01-16

### Added
- Windows MSI installer with bundled GStreamer and Graphviz (#230)
- Include GStreamer libexec in Windows installer (#234)
- New Strom icon with platform-specific sizes (#234)
- PWA manifest for iOS standalone mode (#228)
- Mobile debug console with filter controls (#228)
- Panel toggles, zoom controls, and pinch-to-zoom for iOS (#228)
- Compact system monitor widget for top bar (#228)
- Links page redesign with tabs and SRT stream support (#228)

### Fixed
- Respect GST_PLUGIN_FEATURE_RANK in video encoder selection (#237)
- Force dark theme on WASM startup (#228)
- Theme-aware colors and UI defaults (#228)
- Relay Link headers in WHEP proxy for ICE server configuration (#228)
- Improve VLC playlist functionality (#228)

---

## [0.3.10] - 2026-01-15

### Fixed
- Normalize ICE server URLs for GStreamer and browser compatibility (#225)
- Use gst-launch-1.0.exe on Windows for CUDA interop test (#224)

---

## [0.3.9] - 2026-01-15

### Added
- Server-wide ICE server configuration for STUN/TURN support (#220)
- Open Web GUI button in native application (#221)

### Fixed
- Compositor sizing dropdown selection (#221)
- Default is-live=true for videotestsrc and audiotestsrc (#222)

---

## [0.3.8] - 2026-01-14

### Added
- Runtime GPU interop detection for headless Docker support (#215)
- WHEP Output block with video support, proxy system, and built-in player pages (#210)
- Dynamic video codec detection for WHEP output
- H.264 profile matching workarounds for pre-encoded video WebRTC streaming
- Links page in frontend for quick access to WHEP player URLs
- Display host address in WHEP page headers and titles
- Blackmagic DeckLink setup documentation (#217)

### Fixed
- Disable FEC and RTX in WHEP output to prevent bandwidth doubling (#216)
- Use autovideoconvert for GPU-accelerated color conversion (#208)
- Show audio indicator for all streams with audio in WHEP player
- Restore audio transceiver in WHEP player

---

## [0.3.7] - 2026-01-06

### Changed
- Use native ARM64 runners for Docker builds (#205)
- Use cargo-zigbuild on native ARM64 for older glibc targeting (#201-204)

---

## [0.3.6] - 2025-12-30

### Added
- NDI video and audio input/output blocks with mode enum and dynamic pads (#139)
- MCP Streamable HTTP transport for AI assistant integration (#190)
- NDI installation and testing scripts

### Changed
- Reorganize setup scripts into common folder structure

### Fixed
- Remove Windows-incompatible echo hook from Trunk.toml (#189)
- Make NDI SDK license acceptance manual
- Hide NDI blocks from palette when plugins unavailable
- Various ARM64 cross-compilation fixes (#195-200)

---

## [0.3.5] - 2025-12-18

### Added
- mDNS/RAVENNA discovery support for AES67 streams (#182)

### Fixed
- Skip installing GStreamer/Graphviz if already present (#176)

---

## [0.3.4] - 2025-12-15

### Added
- Auto-reload frontend when backend is rebuilt (#174)
- Uptime tracking for process, system, and flows (#172)

### Fixed
- Use 127.0.0.1 instead of localhost for VLC playlists (#173)
- Add glcolorconvert to GPU compositor pipelines (#171)
- Add STROM_MEDIA_PATH env var and fix default media path (#170)

---

## [0.3.2] - 2025-12-10

### Added
- Signal handling for graceful shutdown (#167)
- VLC playlist button for easy stream playback (#167)

### Fixed
- Only apply nvcodec fix for amd64 architecture (#165)
- Miscellaneous fixes and documentation updates (#163-166)

---

## [0.3.0] - 2025-12-06

### Added
- V4L2 encoder support for Raspberry Pi hardware encoding (#115)
- Resolution dropdown with common presets (#116)

### Fixed
- gst-launch import link parsing (#117)

---

## [0.2.9] - 2025-12-04

### Added
- Real-time PTP clock statistics with inline graphs (#109)
- AES67 improvements: PTP clock in SDP and network interface selector (#112)

### Fixed
- Remove OpenSSL dependency, use rustls everywhere (#111)
- Use rustls instead of native-tls in MCP server (#114)
- Add RUSTFLAGS for zigbuild and Strawberry Perl for Windows CI (#108)
- Download GStreamer directly from freedesktop.org for Windows CI (#107)

---

## [0.2.8] - 2025-12-03

### Added
- Blackmagic DeckLink SDI/HDMI block support (#99)
- Visual compositor layout editor (MVP/POC) (#104)

### Fixed
- Pin Windows GStreamer to 1.24.13 in CI workflows (#105)
- Add libssl-dev to CI and release workflows (#103)

---

## [0.2.7] - 2025-12-03

### Added
- OpenGL video compositor block (`glvideomixer`) (#98)
- MPEG-TS/SRT output block with dynamic pads architecture
- Improved video encoder for low-latency streaming (#96)
- MPEG-TS codec validation and documentation
- QoS monitoring for streaming pipelines
- Dynamic block pads architecture for computed external pads

### Changed
- Improved logging with file output and reduced verbosity (#100)

### Fixed
- Disable sync/QoS in MPEG-TS/SRT output for transcoding pipelines (#97)
- SRT crash during auto-restart on server startup
- Proper H.264 stream formatting and MPEG-TS timing for SRT output
- Block pad alignment and link validation
- Codec parser and keyframe generation in video encoder

### Documentation
- WSL2-specific segfault debugging guide (#101)
- Updated documentation to reflect current codebase state (#95)

---

## [0.2.6] - 2025-12-01

### Added
- One-liner install script with GStreamer and Graphviz support (#83)
- Interactive configuration menu for installation
- Static OpenSSL linking for Ubuntu 20.04+ compatibility (#91)

### Changed
- Use Zig for glibc-targeted Linux builds in CI

### Fixed
- Auto-detect piped stdin and enable automated mode
- Set DEBIAN_FRONTEND=noninteractive for apt-get commands
- Redirect all log output to stderr for command substitution
- Use /dev/tty for interactive input to support piped execution
- Root user support in install script

### Legal
- Add MIT and Apache-2.0 license files (#93)

---

## [0.2.5] - 2025-11-30

### Added
- Real-time CPU, memory, and GPU monitoring in topbar (#70)
- Video Encoder block with automatic hardware acceleration detection (#68)
- Audio Format and Video Format blocks with enum label support (#66)
- Hierarchical configuration file support (#67)
- gst-launch-1.0 import/export support (#78)
- ARM64 cross-compilation support (#79)
- Dependabot for automated dependency updates (#72)

---

## [0.2.4] - 2025-11-27

### Added
- Improved keyboard delete behavior and auto-navigate to new flows (#62)

### Fixed
- Proper multi-level ghostpad handling in WHEP input (#64)
- GStreamer 1.26.2 compatibility: Add libnice and update gst-plugins-rs (#63)
- Build only AMD64 Docker images to reduce publish time (#61)

---

## [0.2.3] - 2025-11-26

### Fixed
- Extend Docker publish timeout to 2 hours (#59)

---

## [0.2.2] - 2025-11-26

### Fixed
- Use correct trunk architecture for ARM64 Docker builds (#57)

---

## [0.2.1] - 2025-11-26

### Added
- ARM64 Docker support and architecture labels (#55)
- Trigger Docker publish on tag creation (#53)

---

## [0.2.0] - 2025-11-26

### Added
- PostgreSQL storage support (#47)
- Frontend GUI improvements: UX, theming, and keyboard shortcuts (#50)

### Changed
- **Breaking:** Rename backend crate and binary from `strom-backend` to `strom` (#49)
- **Breaking:** Update default ports (backend 3000->8080, trunk 8080->8095) (#48)

### Fixed
- Clear error message for port binding failures (#51)

---

## [0.1.8] - 2025-11-25

### Fixed
- Hardcode Docker image name to eyevinntechnology/strom (#45)

---

## [0.1.7] - 2025-11-25

### Fixed
- Remove invalid sha tag from Docker publish workflow (#43)

---

## [0.1.6] - 2025-11-25

### Changed
- Split Dockerfile into separate frontend and backend builders for optimization
- Update trunk to v0.21.14 in Docker

### Fixed
- Docker frontend URL detection and build optimizations (#41)

---

## [0.1.5] - 2025-11-24

### Added
- WHIP/WHEP WebRTC blocks with statistics visualization (#30)
- Thread priority configuration for GStreamer streaming threads (#31)
- RFC 7273 clock signaling for AES67 SDP generation (#29)
- RTP jitterbuffer statistics display for AES67
- Human-readable labels for block properties (#28)
- 6 GUI improvements for flow management and monitoring (#28)

### Fixed
- AES67 Input: Disable RTCP, handle SSRC changes (#32)
- Windows thread priority conversion (#35)
- Use `mediaclk:sender` for local clocks per RFC 7273 (#34)
- PTP/NTP clock sync status detection (#33)
- Reduce log verbosity for element state changes (#38)
- WHIP output error handling with multiple bus handlers (#37)
- Various improvements to WHEP input and AES67 output (#36)

---

## [0.1.4] - 2025-11-21

### Added
- Session-based login with HTML form and password manager support (#18)
- Dual authentication support (session login + API keys)
- Native GUI auto-authentication

### Fixed
- CORS configuration for credentials support
- Switch Docker cache from registry to GitHub Actions (#23)
- Build Docker image in headless mode to fix CI hang (#25)
- Pass backend port to native GUI frontend (#22)
- Auto-detect WSL and default to X11 for clipboard compatibility (#20)

---

## [0.1.3] - 2025-11-21

### Fixed
- Add verbose output for Docker build debugging (#19)

---

## [0.1.2] - 2025-11-20

### Fixed
- Switch to Docker registry cache for better build performance (#16)
- Reduce resource usage in Docker builds to prevent compilation hangs
- Disable Docker build attestations to prevent hangs (#15)

---

## [0.1.1] - 2025-11-20

### Added
- Manual dispatch to Docker publish workflow (#11)
- README enhancements: CI/CD info, getting started guide, screenshot (#12)

### Changed
- Disable ARM64 Docker builds temporarily to improve publish time (#14)

### Fixed
- Update Docker Hub organization to eyevinntechnology (#13)

---

## [0.1.0] - 2025-01-13

### Added - Backend
- Complete Cargo workspace structure (types, backend, frontend)
- Axum web server with CORS and static file serving
- Full REST API for flow management (CRUD operations)
- GStreamer pipeline integration:
  - Pipeline creation from flow definitions
  - Element instantiation and property configuration
  - Pad linking with validation
  - Start/stop/pause pipeline control
  - State management and tracking
- Element discovery and introspection API
- JSON file storage backend with async I/O
- OpenAPI documentation with Swagger UI at `/swagger-ui`
- Structured logging with tracing
- Configuration system (environment variables + config file)
- Auto-start flows on server boot
- Health check endpoint

### Added - Frontend
- egui-based WebAssembly UI
- Custom node-based graph editor:
  - Drag nodes to reposition
  - Click-and-drag to create links between pads
  - Pan canvas (drag on background)
  - Zoom canvas (mouse wheel)
  - Grid background for alignment
  - Visual feedback for selected nodes
- Element palette panel:
  - Search functionality
  - Category filtering
  - Pre-loaded with 17 common GStreamer elements
  - Element descriptions and tooltips
- Property inspector:
  - Type-appropriate input widgets (text, number, slider, checkbox)
  - Common properties for well-known elements
  - Custom property support
- Flow management:
  - Create new flow dialog
  - Flow list sidebar
  - Delete flow functionality
- Pipeline controls:
  - Start/Stop buttons
  - State visualization with color-coding
  - Auto-start toggle
- API client with full CRUD support
- LocalStorage integration for async state handling
- Trunk build configuration
- Dark theme UI

### Added - Shared Types
- Domain models: Flow, Element, Link, PipelineState
- API request/response types
- OpenAPI schema support with utoipa
- Serde serialization
- UUID support with WASM compatibility (js feature)

### Technical
- Full Rust implementation (backend + frontend)
- WebAssembly compilation for frontend
- Comprehensive error handling
- Unit and integration tests
- Development and production build configurations

---

## [0.0.1] - Initial Architecture

### Added
- Project architecture design
- Technology stack selection
- Development roadmap
- README with project overview

---

## Version Numbering

This project follows [Semantic Versioning](https://semver.org/):
- MAJOR version for incompatible API changes
- MINOR version for new functionality in a backwards compatible manner
- PATCH version for backwards compatible bug fixes

## Categories

- **Added**: New features
- **Changed**: Changes to existing functionality
- **Deprecated**: Soon-to-be removed features
- **Removed**: Removed features
- **Fixed**: Bug fixes
- **Security**: Security-related changes
