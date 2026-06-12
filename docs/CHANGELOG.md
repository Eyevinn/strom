# Changelog

All notable changes to the Strom GStreamer Flow Engine project.

## [0.6.5] - 2026-06-12

### Added
- Vision Mixer: ColorCorrect look — primary correction (brightness, contrast, gamma, saturation) plus white balance (temperature, tint) and hue rotation, computed in YCbCr; a strict superset of `glcolorbalance` for camera matching. Neutral at every default, so an untouched correction is an identity pass. (#639)
- Vision Mixer: swappable multiview PVW/PGM positions via the `swap_pvw_pgm` block property (#637)

### Changed
- Vision Mixer: dithered looks — ~1 LSB TPDF dither on color-correct, blur, duotone and thermal to kill 8-bit banding; 24-tap Vogel-spiral blur with aspect-corrected circular bokeh; fixed-raster CRT scanlines and aperture grille; blur radius cap raised to 40 px (#639)
- Vision Mixer: FX look parameters laid out in a 4-column grid (#639)

### Fixed
- Vision Mixer: glitch, roll and punch transitions left driver-dependent residual artifacts after completion — transition envelopes now settle to an exact identity pass (#639)
- Frontend: keep `wgpu` out of the WASM build after the eframe 0.34.2 default-feature regression (#639)

---

## [0.6.4] - 2026-06-08

### Fixed
- Vision Mixer: shader FX time-precision freeze over long uptime, heart wipe pop, real TV roll (#636)
- Video encoder: tame VideoToolbox burst behavior (#635)
- Vision Mixer: FX shader crash on macOS core-profile GL (#634)
- Pipeline: skip missing pad properties instead of panicking (#633)

---

## [0.6.3] - 2026-06-05

### Fixed
- Video encoder: cap VBR excursions and bound frame sizes via VBV (#631)
- Server: harden the HTTP(S) accept path against fd exhaustion (#630)
- WHEP: re-enable RTX retransmission, keep FEC disabled (#629)

### Documentation
- Add 0.6.1 and 0.6.2 release notes (#628)

---

## [0.6.2] - 2026-06-05

### Added
- Vision Mixer: shader FX engine (GPU) — custom-GLSL looks, wipe transitions, and master FX takes (#626)
  - 14 parameterized looks (chroma key, blur, pixelate, vignette, VHS, CRT, thermal, …) per input or on the PGM master
  - 19 shader wipes and 11 master-FX takes; operator UI tiers production staples up front with novelty effects behind a MORE toggle
  - Master look and FX takes run on independent PGM slots — a take never evicts the look
  - Self-animating shaders (time uniform from buffer PTS): frame-accurate with zero per-frame CPU
- Vision Mixer: zone borders as compositor underlay pads, per-PiP layout presets (#625)
- Vision Mixer: per-source crop/zoom (punch-in) for PiP compositions (#622)
- Devices: show exposing API prefix in device picker and sort the list (#614)

### Changed
- Pipeline: split `effects.rs` into focused sub-modules (#624)

### Fixed
- Vision mixer: aspect-aware wipe orientation, per-branch PTS latching for SRT sources, destination-flash kill at wipe start (#626)
- Vision mixer: slide/push geometry for mixed aspect ratios (#625)
- Video encoder: decouple VideoToolbox realtime from quality, wire rate-control + keyframe duration (#612)
- Tests: vision mixer FX tests skip without a working GL context, event-driven sampling (#627)

### CI
- Route sccache through self-hosted MinIO (S3) cache, build without sccache when credentials are unavailable (#611, #623)
- Trim GHA cache footprint and split rust-cache keys; bump actions to Node 24 runtimes (#615, #613, #616)

### Dependencies
- Bump uuid (#621), serial_test (#620), egui (#619), sysinfo (#618), socket2 (#617)

---

## [0.6.1] - 2026-06-02

### Added
- Setup: non-interactive NVIDIA driver install via REBOOT env var (#608)

### Fixed
- Sources: deinterlace interlaced SRT/EFP inputs for the GL vision mixer (#606)
- Vision mixer UX: pixel zone editor, snap feedback, honest bus tally (#603)
- Clocks: restore PTP statistics panel hidden behind domain list (#610)
- QoS: drop upstream QoS events at sink source to stop a GstEvent leak (#609)

### Documentation
- Open Live setup guide for a local Strom instance (#604)
- Documentation overhaul — Open Live, archive, slimmer README, AI authorship (#605)
- Correct changelog release dates to match git tags (#607)

---

## [0.6.0] - 2026-05-28

### Added
- Vision Mixer: Picture-in-Picture sources with zones, FIFO + morph transitions (#578)
- Audio Mixer: Monitor bus with parallel PFL/AFL, collapsing property panel, and scaled max channel counts (#570)
- Audio Mixer: AFL on every aux master and group bus (#594)
- Audio Mixer: pre-fader channel level meter taps (#592)
- Audio Mixer: derive monitor source gates from PFL/AFL writes (#588)
- Block-properties API endpoint with persist semantics for transient state (#579)
- Per-property `ramp_ms` overrides on block PATCH (#600)

### Changed
- Migrate legacy WHEP `mode` to explicit track counts on flow load (#587)
- Drop unused agua-gst watermark plugin (#595)

### Fixed
- Vision mixer: render overlay VU meters with 4-zone sectors (#590)
- Vision mixer: lift morphing pad when a higher-z incoming source is inside `morph_start` (#583)
- SRT: don't synthesize a phantom caller on an idle listener (#585)
- Properties: coerce int/uint values for `gdouble`/`gfloat` properties (#586)
- Build: isolate the WASM target dir to avoid a cargo lock deadlock (#591)

### Performance
- Skip absent elements in block property read-back (#601)

### Documentation
- Add an operator-facing Vision Mixer user guide (#593)

### Dependencies
- Bump reqwest (#597), garde (#596), serde_json (#598), mdns-sd (#599), gstreamer-controller (#574), tower-http (#575), egui_extras (#576), gst-plugin-webrtc (#573), rand (#572)

---

## [0.5.1] - 2026-05-13

### Fixed
- Video encoder: replace `auto` profile with a `none` default and expand the codec profile enum (#569)
- Audio: bump default mute anti-click ramp from 10ms to 30ms (#568)

---

## [0.5.0] - 2026-05-12

### Added
- Local Input block — cross-platform USB/built-in capture with device picker (#563)
- Time Offset block for live PTS shifting; surface `offset_ms` on the property read path (#555, #565)
- SRT statistics for inputs and outputs; expose resilience knobs on inputs (#564, #553)
- Multi-audio + multi-video WHEP Output, replacing `mode` with explicit track counts (#556)
- Audio Mixer: smooth volume/mute via GstController (anti-zipper, anti-click) and honor `ramp_ms` on mute toggles with a cancel-guard (#539, #540)
- Patched DeckLink plugin with synchronized capture group support (#554)
- chrony NTP install script and runbook (#547)

### Changed
- Merge per-media DeckLink blocks into a single Input/Output block (#546)
- Unify SRT property order and share defaults across blocks (#553)
- Vision mixer: default `gl_download` to false (#548)
- Move the Debug Graph button next to Save in the toolbar (#538)
- Audio Mixer: use UInt type for channel/aux/group count properties (#537)

### Fixed
- WHEP: align encoder profile and SDP fmtp with WebRTC decoder expectations (#566)
- Video encoder: vp9enc bitrate unit and realtime knobs (#562)
- Vision mixer: enforce framerate on PGM/MV outputs in GL passthrough (#549)
- Vision mixer: use GPU-aware videoconvert in the CPU pipeline (#534)
- NVIDIA setup: apply cgroupfs + dev-char workarounds for the NVML cgroup-reload bug (#536)
- Buffer age: show external pad label instead of internal "sink" (#535)

### Dependencies
- Bump sysinfo (#559), tower-http (#561), gst-plugin-inter (#560), gst-plugin-audiofx (#558), gstreamer-app (#557), utoipa (#542), egui (#545), tokio (#544), rustls (#543)

---

## [0.4.12] - 2026-04-30

### Added
- TAI and real NTP clocks, direct media timing opt-in, and a System Clock page (#520)
- System clock health badge with per-row tooltips (#532)
- Opt-in CEF GPU rendering via `STROM_CEF_GPU=1` (#522)
- Enable the EFP feature for macOS release builds (#531)

### Fixed
- Leak fixes: stop the pipeline on delete and clean up the overlay renderer (#530)
- Frontend: avoid an `Instant` underflow panic on WASM startup (#521)

### Dependencies
- Bump gst-plugin-efp to 0.3.0 (#523), rustls (#526), clap (#527), libc (#528), reqwest (#529), rustls-webpki (#524), mdns-sd (#519), tokio (#518), axum (#517), tracing-appender (#516), uuid (#515)

---

## [0.4.11] - 2026-04-20

### Added
- Vision mixer: per-input VU meters on the multiview overlay (#511)
- EFP: cross-source sync via `normalize_segment` plus a preroll fix (#510)

### Changed
- Bump CEF to 144.0.21 and align the strom-full `GSTCEFSRC_VERSION` (#509)
- LD_PRELOAD mallinfo shim to fix the MemoryInfra SIGILL crash; restore CEF 144 (#508)
- Lower the client ICE gathering timeout from 2s to 1s (#512)

### Fixed
- Unblock preroll on mpegtssrt output (`async=false`) (#504)

---

## [0.4.10] - 2026-04-17

### Added
- Runtime log level control via REST API and the info page (#494)
- Debug logging for flow API request bodies (#493)

### Fixed
- Vision mixer: A/V sync and multiview latency; eliminate overlay lag (#502, #499)
- Add buffer limits to appsrc elements to prevent unbounded memory growth (#500)
- Align static `external_pads` with computed defaults (#492)
- Filter vision mixer control UI events by both `flow_id` and `block_id` (#501)

### Dependencies
- Bump rustls (#496), libc (#495), tokio (#497)

---

## [0.4.9] - 2026-04-13

### Fixed
- Use mixer stream-time for transition and FTB keyframes (#490)
- Change vision mixer `num_inputs` from enum to uint (#488)
- Use a unique session cookie name per port to prevent collisions (#489)

### CI
- Add a manual workflow to trigger the OSC fork sync (#487)

---

## [0.4.8] - 2026-04-13

### Added
- Isolate the media player in an internal pipeline with clocksync; add seek throttle and jump buttons (#484)

### Fixed
- Resolve passthrough audio not flowing in the media player bridge (#486)
- Add peer address to connection logs and fix the audiogain f32 type (#485)
- Skip the GL compositor when only Mesa software rendering is available (#482)
- Frontend: surface probe activation errors in the UI, show the login form on 401, make the flow properties window movable, and refresh session expiry on activity (#483)

---

## [0.4.7] - 2026-04-08

### Added
- Framerate and GL passthrough properties on the vision mixer (#481)

### Fixed
- Use `start-time-selection=zero` for vision mixer compositors (#480)

---

## [0.4.6] - 2026-04-07

### Added
- Ephemeral flows and accept a full Flow object on the create API (#479)
- `GET` multiview-endpoint API, used by the vision mixer page (#476)

### Changed
- Move the OpenAPI spec to the repo root and rename the snapshot test
- Bump CI actions to Node.js 24 (checkout v5, action-gh-release v2)

### Fixed
- Name validation on the create_flow endpoint (#479)
- Restore the glow renderer for WASM and fix renderer detection (#478)
- Render the status bar before page content for correct panel ordering (#478)
- Reject duplicate WHIP/WHEP endpoint IDs and trim endpoint strings (#476)

---

## [0.4.5] - 2026-04-07

### Added
- Vision mixer output pixel format property (#471)
- Diagnostic pad probes for WHEP input blocks
- OpenAPI discriminator hints for tagged enum schemas (#474)

### Changed
- Upgrade the egui ecosystem from 0.33 to 0.34 (#473)
- Extract the shared pixel format list to strom-types
- Stop tracking docker-compose.yml and gitignore it

### Fixed
- Work around a GStreamer jitterbuffer stall after a mute gap; set `drop-on-latency=true` on WHIP and AES67 inputs (#472)
- GPU vision mixer output chain and OpenAPI registration

### Dependencies
- cargo update (tokio 1.51, hyper 1.9, wasm-bindgen); bump gloo-net (#469), gloo-timers (#468), uuid (#466)

---

## [0.4.3] - 2026-03-30

### Added
- Vision Mixer block with a PVW/PGM workflow and web control UI: CUT/AUTO transitions, DSK overlays, fade-to-black (FTB), multiview output, background source, and multi-source groups (#463)
- Audio Gain block with live property updates (#464)
- Generic live property flag on `ExposedProperty`

### Fixed
- Break circular GObject references that leaked pipelines and sockets (#465)
- Vision mixer: resolution scaling, DSK/FTB state sync, transition cleanup, GPU BGRA conversion, and latency fixes
- Correct the Fedora libnice package name and remove `-dev`/`-devel` packages from install.sh

### Documentation
- Add a CLAUDE.md rule for GStreamer object references in closures
- Add libcairo2-dev to the build-from-source prerequisites

---

## [0.4.2] - 2026-03-26

### Fixed
- CEF: replace the non-existent `disable-background-tracing` flag with working flags (#461)
- Preserve legend label and edge caps in debug graph DOT output (#459)
- Frontend: graph editor and flow list improvements, prevent double-click flicker, and preserve the selected link index on detach
- Use `rsplit_once` for namespaced block element IDs in detach

### Dependencies
- Bump tokio-tungstenite (#457)

---

## [0.4.1] - 2026-03-23

### Fixed
- Harden DOT graph generation for complex pipelines (#453)
- Set `min-upstream-latency` on liveadder and revert dropout overrides (#456)
- Set `max-dropout-time` on rtpbin instead of rtpjitterbuffer (#455)
- Disable jitterbuffer dropout detection for WHEP input (#454)

---

## [0.4.0] - 2026-03-23

### Added
- Complete OpenAPI contract: full coverage with a snapshot test, runtime validation and structured JSON errors on all endpoints, and oasdiff CI (#432)
- Automatic buffer age monitoring with probe UI improvements (#439)
- GL renderer probe from GStreamer's GL context (#440)
- Reusable thumbnail tap module and standalone thumbnail block; default thumbnails bumped to 320x180 (#445)
- Configurable max video bitrate per WHIP Input block (#448)
- Per-slot WHIP input model with auto-cleanup and A/V sync; isolated pipeline per WHIP session (#450)
- Truncate long property values in debug graph DOT labels (#449)
- Smart CPU affinity with an AffinityManager and cgroup-aware core detection (#431)

### Changed
- Move 21 API-visible types from the backend to strom-types
- Replace `flow.state` with a `running` bool and `gst_state`
- Remove unnecessary queue property overrides, using GStreamer defaults (#438)
- Correct the EFP acronym to Elastic Frame Protocol (#430)
- Default CPU affinity to Off, with an icon shown when overridden

### Fixed
- Use weak pipeline refs in probe closures and clean up before `set_state(Null)`
- Move buffer age broadcasting and webrtc stats off the GStreamer hot path
- Detach the webrtc stats timeout thread to avoid 500ms×N blocking
- Pin flows to physical cores instead of single hyperthreads
- Treat Paused pipelines as active

### Security
- Update aws-lc-sys to 0.39.0 to resolve 2 high-severity vulnerabilities

### Dependencies
- Update all dependencies to latest compatible versions; bump rustls-webpki (#451), tempfile (#437), clap (#436), tokio (#435), mdns-sd (#434), sysinfo (#433)

---

## [0.3.26] - 2026-03-12

### Added
- EFP/SRT input and output blocks (Elastic Frame Protocol over SRT), gated behind a cargo feature flag (#421)
- EFP mux/demux GStreamer plugin integration

### Changed
- Add dbus and avahi-daemon to the Docker images for NDI discovery (#427)
- Bump efp to v0.2.5 to fix a GCC 15 build failure; bump gst-plugin-efp to v0.2.3
- Document the EFP feature flag and build dependencies in the README

### Fixed
- Disable Chromium background tracing to prevent the MemoryInfra SIGILL crash
- Fix CEF GPU isolation in strom-full to prevent SharedImageManager crashes (#414)
- Sort the WHEP streams page endpoints in natural numeric order (#426)
- Move the SDP copy button above the SDP text in AES67 Output (#425)
- Order video pads before audio pads in WHEP Output and WHIP Input (#424)
- Use eframe's glow re-export instead of a direct dependency (#420)

### Dependencies
- Bump quinn-proto (#423), uuid (#419), bcrypt (#417), socket2 (#416), libc (#415)

---

## [0.3.25] - 2026-03-09

### Added
- Recorder block for writing audio/video streams to file with splitmuxsink (#411)
  - Configurable output path, segment duration, and max file size
  - Recording duration counter and auto-stop after configurable duration
  - Download button for current recording file in properties panel
- Audio Analyzer block with real-time waveform and vectorscope visualization (#410)
  - Full analyzer view with zoom sliders and legends via egui_plot
  - Base64-encoded WebSocket transport to reduce throughput
- EBU R128 Loudness Meter block with reset button (#406)
- Spectrum Analyzer block (`builtin.spectrum`) (#405)

### Changed
- Replace all emoji/unicode buttons with Phosphor icons throughout the UI (#404)
- Limit macOS and Windows CI builds to manual trigger only (#408)

### Fixed
- Media player file path resolution to use configured `data_dir` media path (#412)
  - Backend injects `_media_path` as block property (canonicalized at expansion time)
  - Frontend stores playlist paths relative to media root instead of hardcoded `./media/`
  - Legacy `./media/` prefix stripped for backward compatibility
- Fix negative `num_audio_tracks` in `get_external_pads` (#411)

### Security
- Update aws-lc-rs/aws-lc-sys to fix 3 high-severity vulnerabilities (#407)

### Dependencies
- Bump gstreamer from 0.25.0 to 0.25.1 (#402)
- Bump gst-plugin-rtp from 0.15.0 to 0.15.1 (#399)
- Bump rustls from 0.23.36 to 0.23.37 (#400)
- Bump mdns-sd from 0.18.0 to 0.18.1 (#403)
- Bump wasm-bindgen-futures from 0.4.62 to 0.4.64 (#401)

---

## [0.3.24] - 2026-03-02

### Added
- MPEG-TS/SRT Input block for receiving and demuxing SRT streams (#391)
- Display custom names on element and block nodes (#393)

### Changed
- Standardize port labels to short form V0/A0 across all blocks (#395)
- Limit DeviceMonitor to Source/Network only (#392)
- Increase Windows CI/release build timeout to 45 minutes (#397)
- Simplify release notes to use auto-generated changelog (#390)

### Fixed
- Prevent audio meter block height jump for 1-2 channels (#394)

---

## [0.3.23] - 2026-02-24

### Added
- Agua watermark plugin support (#387)
- QR codes for WHIP/WHEP mobile access (#386)
- Interactive overlay for UI testing (#383)

### Changed
- Upgrade GStreamer bindings to stable releases (#385, #387)
- Bump agua-gst to v0.2.4 (#389)
- Clean dead code and consolidate shared types in strom-types (#380)

### Fixed
- Replace deprecated screen_rect() with content_rect() (#384)
- Various small fixes (#382)

---

## [0.3.22] - 2026-02-19

### Added
- Audio Mixer block with full channel strip processing (#365)
  - Per-channel gate, compressor, parametric EQ, and high-pass filter
  - Multi-row layout with pan knobs and dB-aligned faders
  - LCD-style value displays and editable channel labels
  - PFL/AFL solo mode, aux sends, and subgroup routing
  - Input gain and main bus processing chain
  - Rust DSP backend (lsp-plugins-rs) as alternative to LV2
  - Force-live mode, configurable latency, and output tees
  - Bus selection, save/reset, and per-section reset buttons
  - 38 unit and integration tests
- WHEP Player: Real audio level meter replacing CSS animation (#376)
- Frontend: Rename WHEP Players tab to WHIP/WHEP and add ingest link (#376)
- WHEP: Configurable jitterbuffer latency (#366)

### Changed
- Update GStreamer bindings to 0.25.0-alpha.2 (#362)
- Refactor: Split app.rs (7172 lines) into focused modules (#372)
- Refactor: Split pipeline.rs into focused sub-modules (#373)
- Refactor: Split graph.rs, compositor_editor.rs, api.rs into focused modules (#375)
- Refactor: Split mixer.rs into directory module (#365)
- Centralize mixer defaults in strom-types (#365)

### Fixed
- WHEP: Encode server-provided endpoint path in player URL (#376)
- WHEP: URL-encode endpoint IDs in streams page URLs (#376)
- WHEP: Route audio through video element for A+V streams (#376)
- WHEP: Start audio muted to comply with browser autoplay policy (#376)
- WHEP: Fix ice-transport-policy crash (#366)
- Mixer: Various fixes for HPF, faders, knobs, and plugin properties (#365)
- GUI: Use wgpu renderer on macOS to avoid OpenGL conflict (#361)
- Protect Swagger UI with auth (#360)

---

## [0.3.21] - 2026-02-11

### Fixed
- Resolution live-change and preserve pre-built WASM in Docker (#358)

---

## [0.3.20] - 2026-02-11

### Added
- WHIP Input block for browser/encoder ingest (#350)
- Backend: Placeholder page when WASM frontend is not built (#341)

### Changed
- Remove VP8 from offered video codecs in WHEP (#346)

### Fixed
- Auth: Disable Secure flag on session cookie for HTTP access (#356)
- Video encoder: Correct vtenc bitrate unit and remove conflicting quality (#348)
- WHEP: Strip H.264 profile-level-id from webrtcsink capsfilters (#347)
- WHEP: Strip opus stereo fmtp params to fix audio-only timeout (#345)
- WHEP: Correct default stream mode to video-only and fix audio+video port creation (#343)
- Installer: Create install directory if it doesn't exist (#342)

---

## [0.3.19] - 2026-02-06

### Added
- WHEP Player: Debug mode toggle for ICE/TURN logging (#331)
- WHEP Player: Persist debug mode setting across reloads (#338)
- Backend: Configurable ICE transport policy (#332)
- Backend: Configurable CORS allowed origins (#330)

### Fixed
- Frontend: Clear stale WebRTC stats when peers disconnect or flows stop (#339)
- Docker: Clear stale CEF cache on container start (#337)

---

## [0.3.18] - 2026-02-02

### Added
- Frontend: Debug console button in footer (#320)
- OpenAPI documentation for API endpoints (#319)
- Open source community standards documentation (#318)

### Fixed
- gstcefsrc: Only use x86-64-v3 on amd64 architecture (#323)
- gstcefsrc: Target x86-64-v3 and bump CEF to 144.0.12 (#321)
- AES67: Make rtcp-mode property optional for older GStreamer (#317)

---

## [0.3.17] - 2026-01-30

### Changed
- Consolidate CI jobs and add cross-platform tests (#311)

### Fixed
- WHIP: Handle incoming RTP from SMB to prevent not-linked errors (#315)
- Frontend: Add clipboard fallback for insecure HTTP contexts (#313)
- Remove emojis from log statements (#312)

---

## [0.3.16] - 2026-01-29

### Fixed
- Resolve build warnings and increase macOS CI timeout (#308)

---

## [0.3.15] - 2026-01-29

### Added
- Audio Latency measurement block using GStreamer audiolatency element (#299)
- Audio Router block for flexible channel routing and remapping (#298)
- Thread monitoring with per-thread CPU usage tracking (#300)
- Claude theme for UI (#294)
- Reconnect Now button on connection splash screen (#293)

### Changed
- Reduce compositor auto-select log level to debug (#301)

### Fixed
- AES67: Dynamic channel-mask override for multi-channel audio (#297)
- AES67 channel-mask handling and improved RTP stats display (#295)
- Theme contrast issues (#294)

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

## [0.3.12] - 2026-01-20

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

## [0.3.11] - 2026-01-19

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

## [0.3.7] - 2026-01-02

### Changed
- Use native ARM64 runners for Docker builds (#205)
- Use cargo-zigbuild on native ARM64 for older glibc targeting (#201-204)

---

## [0.3.6] - 2026-01-02

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

## [0.3.5] - 2025-12-29

### Added
- mDNS/RAVENNA discovery support for AES67 streams (#182)

### Fixed
- Skip installing GStreamer/Graphviz if already present (#176)

---

## [0.3.4] - 2025-12-19

### Added
- Auto-reload frontend when backend is rebuilt (#174)
- Uptime tracking for process, system, and flows (#172)

### Fixed
- Use 127.0.0.1 instead of localhost for VLC playlists (#173)
- Add glcolorconvert to GPU compositor pipelines (#171)
- Add STROM_MEDIA_PATH env var and fix default media path (#170)

---

## [0.3.2] - 2025-12-18

### Added
- Signal handling for graceful shutdown (#167)
- VLC playlist button for easy stream playback (#167)

### Fixed
- Only apply nvcodec fix for amd64 architecture (#165)
- Miscellaneous fixes and documentation updates (#163-166)

---

## [0.3.0] - 2025-12-17

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

## [0.1.0] - 2025-11-14

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
