# Frequently Asked Questions

Short answers with pointers into the full docs. See the [documentation index](README.md)
for everything.

## General

### What is Strom?
A GStreamer **media pipeline engine** with a visual, web-based GUI. It builds, runs, and
manages real-time media pipelines; the node-based editor is the interface for designing
and controlling them. See the [root README](../README.md).

### What's the relationship to Open Live?
Strom is standalone, but [Open Live](https://github.com/Eyevinn/open-live) (with
[Open Live Studio](https://github.com/Eyevinn/open-live-studio)) uses Strom as a media
backend over its REST/WebSocket APIs. See [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md).

### Native app, web service, or both?
Both. The same egui frontend compiles to native and to WASM; the backend can run the
native GUI or serve the web UI. See [DEVELOPMENT.md](DEVELOPMENT.md).

## Running & deployment

### What's the easiest way to try it?
The one-liner installer or Docker. See the [root README](../README.md#quick-start) Quick Start.

### How do I deploy with Docker?
See [DOCKER.md](DOCKER.md) for the full reference, or [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md)
for a guided, opinionated setup.

### Do I need a GPU?
No, but it's **highly preferred** for production. Without one, video encoding falls back to
software (x264 etc.), which works for small flows but doesn't scale. See
[DOCKER_GPU_SETUP.md](DOCKER_GPU_SETUP.md).

### Is there help for setting up my host (GPU, capture cards, NDI, clock sync)?
Yes — ready-to-run host setup scripts live in
[`scripts/setup/`](../scripts/setup), each with its own README:
[`nvidia/`](../scripts/setup/nvidia/README.md) (driver + container toolkit),
[`decklink/`](../scripts/setup/decklink/README.md) (Blackmagic SDI/HDMI),
[`ndi/`](../scripts/setup/ndi/README.md) (NewTek NDI SDK + plugin), and
[`ntp/`](../scripts/setup/ntp/README.md) (chrony for clock sync). They are also bundled
inside the Docker images at `/app/scripts/setup/`, so you don't have to clone the repo —
see [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) for how to extract them.

### How do I turn on authentication?
Strom is unauthenticated by default. Set a session login and an API key before exposing it
to any untrusted network. See [AUTHENTICATION.md](AUTHENTICATION.md).

### Can I use a real database instead of JSON files?
Yes — PostgreSQL is supported for production. See [POSTGRESQL.md](POSTGRESQL.md).

### How do I serve over HTTPS?
Built-in TLS (`STROM_TLS_CERT` / `STROM_TLS_KEY`) with hot-reload, or terminate TLS at a
reverse proxy. See the [root README](../README.md) HTTPS/TLS section.

## Features

### How do I switch between sources live (PVW/PGM)?
Use the **Vision Mixer** block — broadcast-style preview/program switching with transitions,
DSK, fade-to-black, PiP, and multiview. See [VISION_MIXER_OPERATOR_GUIDE.md](VISION_MIXER_OPERATOR_GUIDE.md).

### What's the difference between the Compositor and the Vision Mixer?
The Compositor is Strom's first-generation video compositor (WIP, known limitations). The
**Vision Mixer** is the more developed switcher and the recommended path — it's what Open
Live uses. See [VISION_MIXER_OPERATOR_GUIDE.md](VISION_MIXER_OPERATOR_GUIDE.md) (the old
compositor's writeup is in [archive/COMPOSITOR_EDITOR.md](archive/COMPOSITOR_EDITOR.md)).

### Can I render web pages / HTML graphics as a video source?
Yes, via CEF in the `strom-full` image. See [HTML_RENDER.md](HTML_RENDER.md).

### Which video encoders are supported?
H.264/H.265/AV1/VP9 with automatic hardware acceleration (NVENC, QSV, VA-API, AMF, software
fallback). The Video Encoder block picks the best available encoder; its properties in the
in-app inspector are the authoritative list.

### How do I integrate with an AI assistant (Claude, etc.)?
Strom speaks the Model Context Protocol over HTTP (`/api/mcp`) and stdio. See [MCP.md](MCP.md).

## Troubleshooting

### My input streams are out of sync.
Align them with PTP/NTP clocks and `normalize_segment`. See
[STREAM_SYNCHRONIZATION.md](STREAM_SYNCHRONIZATION.md).

### A WHIP/WHEP connection drops after a few seconds.
This was a known issue, now resolved: keep each `whipserversrc` in its own pipeline and set
`drop-on-latency=true` on live RTP inputs. Background in
[archive/WHIP_ICE_DISCONNECT_INVESTIGATION.md](archive/WHIP_ICE_DISCONNECT_INVESTIGATION.md).

### HTML rendering / CEF crashes (SIGILL) in a long-running container.
Handled in our `strom-full` image via a `mallinfo` LD_PRELOAD shim. If you hit it on your own
gstcefsrc build, see [archive/CEF_SIGILL_CRASH.md](archive/CEF_SIGILL_CRASH.md).

### Strom segfaults — how do I debug it?
See [DEBUGGING_SEGFAULTS_WSL2.md](DEBUGGING_SEGFAULTS_WSL2.md) (applies beyond WSL2).

### How do I build for a Raspberry Pi / ARM64?
See [CROSS_COMPILE_ARM64.md](CROSS_COMPILE_ARM64.md).

## Contributing

### How do I set up a dev environment?
See [DEVELOPMENT.md](DEVELOPMENT.md), then [CONTRIBUTING.md](CONTRIBUTING.md).
