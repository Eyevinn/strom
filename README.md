<img src="assets/icon-128.png" alt="Strom" width="64" align="left" style="margin-right: 16px;">

# Strom - GStreamer Flow Engine

**Strom** is a GStreamer media pipeline engine with a visual, web-based GUI. At its core it builds, runs, and manages real-time media pipelines; the node-based editor lets you design and control complex media flows without writing code.

> **Used by [Open Live](https://github.com/Eyevinn/open-live):** Strom is a standalone GStreamer flow engine, but [Open Live](https://github.com/Eyevinn/open-live) — an open-source live production platform with [Open Live Studio](https://github.com/Eyevinn/open-live-studio) as its web-based production interface — uses Strom as a media backend, driving it over its REST and WebSocket APIs. Try the hosted platform at [openlive.apps.osaas.io](https://openlive.apps.osaas.io/), or see [docs/OPEN_LIVE_SETUP.md](docs/OPEN_LIVE_SETUP.md) for running your own Strom instance for Open Live.

---
<div align="center">

## Quick Demo: Open Source Cloud

Run this service in the cloud with a single click.

[![Badge OSC](https://img.shields.io/badge/Try%20it%20out!-1E3A8A?style=for-the-badge&logo=data:image/svg+xml;base64,PHN2ZyB3aWR0aD0iMjQiIGhlaWdodD0iMjQiIHZpZXdCb3g9IjAgMCAyNCAyNCIgZmlsbD0ibm9uZSIgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIj4KPGNpcmNsZSBjeD0iMTIiIGN5PSIxMiIgcj0iMTIiIGZpbGw9InVybCgjcGFpbnQwX2xpbmVhcl8yODIxXzMxNjcyKSIvPgo8Y2lyY2xlIGN4PSIxMiIgY3k9IjEyIiByPSI3IiBzdHJva2U9ImJsYWNrIiBzdHJva2Utd2lkdGg9IjIiLz4KPGRlZnM+CjxsaW5lYXJHcmFkaWVudCBpZD0icGFpbnQwX2xpbmVhcl8yODIxXzMxNjcyIiB4MT0iMTIiIHkxPSIwIiB4Mj0iMTIiIHkyPSIyNCIgZ3JhZGllbnRVbml0cz0idXNlclNwYWNlT25Vc2UiPgo8c3RvcCBzdG9wLWNvbG9yPSIjQzE4M0ZGIi8+CjxzdG9wIG9mZnNldD0iMSIgc3RvcC1jb2xvcj0iIzREQzlGRiIvPgo8L2xpbmVhckdyYWRpZW50Pgo8L2RlZnM+Cjwvc3ZnPgo=)](https://app.osaas.io/browse/eyevinn-strom)

</div>

---

![Strom Screenshot](docs/images/strom-demo-flow.png)
*Visual pipeline editor showing a simple test flow*

## Features

- **Visual Pipeline Editor** - Node-based graph editor in your browser
- **Real-time Control** - Start, stop, and monitor pipelines via REST API or WebSocket
- **Element Discovery** - Browse and configure any installed GStreamer element
- **Reusable Blocks** - Pre-built inputs, outputs, and processing blocks (mixers, encoders, WebRTC, AES67, SRT, NDI, DeckLink, …)
- **Vision Mixer** - Broadcast-style PVW/PGM video switcher with web control UI and a GPU shader FX engine (GLSL looks, wipes, and master FX takes)
- **Audio Mixer** - Digital mixing console with channel processing, aux sends, groups, and metering
- **WebRTC / AES67 / SRT / NDI / DeckLink** - Wide protocol and hardware I/O coverage
- **HTML Rendering** - Render web pages as video sources using CEF (via `strom-full` Docker image)
- **gst-launch Import/Export** - Import existing `gst-launch-1.0` commands or export flows
- **System Monitoring** - Real-time CPU, memory, and GPU usage graphs
- **Authentication** - Optional session login or API keys
- **MCP Integration** - Control pipelines with AI assistants (Claude, etc.)
- **Native or Web** - Run as a desktop app or a web service

Browse the full set of built-in blocks and their properties in the app's element palette and inspector.

## Quick Start

### One-liner install (recommended)

```bash
curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | bash
```

The interactive installer detects your OS, downloads the latest release, and installs GStreamer dependencies. Then run `strom` and open `http://localhost:8080`.

### Docker

```bash
docker run -p 8080:8080 -v $(pwd)/data:/data eyevinntechnology/strom:latest
```

Use `eyevinntechnology/strom-full:latest` for the image with HTML rendering (CEF). Access the web UI at `http://localhost:8080`.

### Other ways to install

- **Pre-built binaries** (Linux, macOS, Windows MSI) — see [GitHub Releases](https://github.com/Eyevinn/strom/releases).
- **Docker / Docker Compose** (production, GPU, reverse proxy) — see [docs/DOCKER.md](docs/DOCKER.md) and [docs/DOCKER_GPU_SETUP.md](docs/DOCKER_GPU_SETUP.md).
- **Build from source** — see [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).
- **Deploy for Open Live** — try the hosted platform at [openlive.apps.osaas.io](https://openlive.apps.osaas.io/), or see [docs/OPEN_LIVE_SETUP.md](docs/OPEN_LIVE_SETUP.md).

### First steps

1. Open `http://localhost:8080`.
2. Drag elements from the palette onto the canvas and connect their pads.
3. Configure properties in the inspector, then click **Start**.

For interactive API docs, visit `http://localhost:8080/swagger-ui`.

## Documentation

- **[Documentation index](docs/README.md)** — everything, organized by topic.
- **[FAQ](docs/FAQ.md)** — short answers to common questions.

Quick links: [Open Live setup](docs/OPEN_LIVE_SETUP.md) · [Docker](docs/DOCKER.md) · [GPU setup](docs/DOCKER_GPU_SETUP.md) · [Authentication](docs/AUTHENTICATION.md) · [Vision Mixer](docs/VISION_MIXER_OPERATOR_GUIDE.md) · [Audio Mixer](docs/AUDIO_MIXER_OPERATOR_GUIDE.md) · [MCP](docs/MCP.md) · [Development](docs/DEVELOPMENT.md) · [Changelog](docs/CHANGELOG.md)

## Architecture

```
┌─────────────────────────────────┐
│  Frontend (egui → WebAssembly)  │
│  - Visual flow editor           │
│  - Element palette              │
│  - Property inspector           │
└────────────┬────────────────────┘
             │ REST + WebSocket/SSE
┌────────────▼────────────────────┐
│  Backend (Rust + Axum)          │
│  - Flow manager                 │
│  - GStreamer integration        │
│  - Block registry (AES67, ...)  │
│  - Storage (JSON or PostgreSQL) │
└─────────────────────────────────┘
```

**Workspace members:**
- `strom-types` - Shared domain models and API types
- `strom` - Server with GStreamer pipeline management
- `strom-frontend` - egui UI (compiles to WASM or native)
- `strom-mcp-server` - Model Context Protocol server for AI integration

## Configuration

Configure via config file, CLI arguments, or environment variables (in priority order):

```bash
--port 8080                      # or STROM_PORT=8080
--data-dir /path/to/data         # or STROM_DATA_DIR=...
--database-url postgresql://...   # or STROM_DATABASE_URL=... (production)
```

Copy `.strom.toml.example` to `.strom.toml` for all options. Key topics have dedicated guides: [storage](docs/POSTGRESQL.md), [authentication](docs/AUTHENTICATION.md), and HTTPS/TLS (built-in `--tls-cert`/`--tls-key` with hot-reload, or a reverse proxy). See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for the full option list.

## API & MCP

- REST + WebSocket/SSE API, with interactive OpenAPI docs at `/swagger-ui`.
- `WS /api/ws` and `GET /api/events` for real-time state and pipeline events.
- gst-launch import/export via `POST /api/gst-launch/parse` and `/export`.
- Model Context Protocol at `/api/mcp` (HTTP) or the `strom-mcp-server` binary (stdio) — see [docs/MCP.md](docs/MCP.md).

## Built by AI

Strom is written by **Claude Code**. The codebase is authored by AI, not hand-written by humans — that's intentional, and it's also why the code (not the docs) is the source of truth for how things work.

We welcome feature requests, ideas, and pull requests — ideally AI-written, in the same spirit. Open a [GitHub Discussion](https://github.com/Eyevinn/strom/discussions), file a [feature request](https://github.com/Eyevinn/strom/issues), or send a PR.

## Contributing

See [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) and [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md). CI runs tests and builds binaries for Linux, Windows, macOS, and ARM64, and publishes Docker images on release.

## License

MIT OR Apache-2.0

---

> The name **Strom** comes from "Ström" — Swedish for "stream".
