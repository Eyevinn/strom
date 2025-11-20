# Strom - GStreamer Flow Engine

**Strom** (Swedish for "stream") is a visual, web-based interface for creating and managing GStreamer media pipelines. Design complex media flows without writing code.

## Features

- **Visual Pipeline Editor** - Node-based graph editor in your browser
- **Real-time Control** - Start, stop, and monitor pipelines via REST API or WebSocket
- **Element Discovery** - Browse and configure any installed GStreamer element
- **Reusable Blocks** - Create custom components from element groups (e.g., AES67 receiver)
- **Auto-restart** - Pipelines survive server restarts
- **Native or Web** - Run as desktop app or web service
- **MCP Integration** - Control pipelines with AI assistants (Claude, etc.)

### Advanced Capabilities

- **Dynamic Pad Linking** - Automatic handling of runtime-created pads (decodebin, demuxers)
- **Automatic Tee Insertion** - Fan-out outputs without manual configuration
- **Pad Properties** - Configure per-pad properties (e.g., volume/mute on audiomixer inputs)
- **Debug Graphs** - Generate SVG visualizations of running pipelines
- **WebSocket/SSE** - Real-time state updates and pipeline events

## Quick Start

### Prerequisites

```bash
# Install GStreamer
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev

# Install Rust and tools
rustup target add wasm32-unknown-unknown
cargo install trunk
```

### Run

```bash
# Production mode (web UI at http://localhost:3000)
cargo run --release

# Development with hot reload
cargo run                    # Backend (Terminal 1)
cd frontend && trunk serve   # Frontend (Terminal 2)

# Headless mode (API only)
cargo run --release -- --headless
```

### Docker

```bash
docker build -t strom .
docker run -p 8080:8080 -v $(pwd)/data:/data strom
```

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
│  - JSON persistence             │
└─────────────────────────────────┘
```

**Workspace Members:**
- `strom-types` - Shared domain models and API types
- `strom-backend` - Server with GStreamer pipeline management
- `strom-frontend` - egui UI (compiles to WASM or native)
- `strom-mcp-server` - Model Context Protocol server for AI integration

## API Overview

**Flows**
- `GET/POST/DELETE /api/flows` - Manage pipeline configurations
- `POST /api/flows/:id/start` - Start pipeline
- `POST /api/flows/:id/stop` - Stop pipeline

**Elements**
- `GET /api/elements` - List available GStreamer elements
- `GET /api/elements/:name` - Get element details and properties

**Blocks**
- `GET/POST/DELETE /api/blocks` - Manage reusable component definitions
- `GET /api/blocks/categories` - List block categories

**Real-time**
- `GET /api/events` - Server-Sent Events stream
- `WS /api/ws` - WebSocket connection

See OpenAPI docs at `/swagger-ui` when server is running.

## Configuration

Environment variables or `config.toml`:

```bash
# Server
STROM_PORT=3000
STROM_HOST=0.0.0.0

# Storage
STROM_FLOWS_PATH=./flows.json
STROM_BLOCKS_PATH=./blocks.json

# Logging
RUST_LOG=info
```

## Blocks System

Create reusable components from element groups:

**Built-in Blocks:**
- **AES67 Receiver** - ST 2110-30 audio receiver with SDP generation
- Custom blocks via JSON or API

Example: Add AES67 block to receive network audio streams, automatically generates proper SDP with multicast addressing.

See `docs/BLOCKS_IMPLEMENTATION.md` for details.

## MCP Integration

Enable AI assistants to manage pipelines:

```bash
# Start MCP server
cd mcp-server
cargo run

# Configure in Claude Desktop
# See mcp-server/README.md
```

Example: "Create a flow that encodes video to H.264 and streams via SRT"

## Development

```bash
# Format and lint
cargo fmt --all
cargo clippy --workspace -- -D warnings

# Run tests
cargo test --workspace

# Install git hooks
./scripts/install-hooks.sh
```

## Project Structure

```
strom/
├── types/          # Shared types (flows, elements, blocks, API)
├── backend/        # Axum server + GStreamer integration
│   └── src/
│       ├── api/    # REST endpoints
│       ├── gst/    # Pipeline management
│       └── blocks/ # Block registry and built-ins
├── frontend/       # egui UI (WASM/native)
├── mcp-server/     # AI assistant integration
└── docs/           # Documentation
    ├── BLOCKS_IMPLEMENTATION.md
    ├── CONTRIBUTING.md
    └── TODO.md
```

## Known Issues

Some GStreamer elements cause segfaults during introspection and are automatically skipped:
- GES elements (gesdemux, gessrc)
- HLS elements (hlssink*, hlsdemux*)
- Certain aggregator elements require special handling

**Windows users:** If you experience panics on startup related to plugin loading (hlssink3, mpegtsmux, etc.), see `docs/WINDOWS_TROUBLESHOOTING.md` for solutions.

See `docs/PAD_TEMPLATE_CRASH_FIX.md` and `docs/MPEGTSMUX_DEADLOCK_FIX.md` for technical details.

## Contributing

See [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md)

## License

MIT OR Apache-2.0
