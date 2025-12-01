# Strom - GStreamer Flow Engine

**Strom** (Swedish for "stream") is a visual, web-based interface for creating and managing GStreamer media pipelines. Design complex media flows without writing code.

![Strom Screenshot](docs/images/strom-demo-flow.png)
*Visual pipeline editor showing a simple test flow*

## Features

- **Visual Pipeline Editor** - Node-based graph editor in your browser
- **Real-time Control** - Start, stop, and monitor pipelines via REST API or WebSocket
- **Element Discovery** - Browse and configure any installed GStreamer element
- **Reusable Blocks** - Create custom components from element groups (e.g., AES67 receiver)
- **gst-launch Import/Export** - Import existing `gst-launch-1.0` commands or export flows to gst-launch syntax
- **System Monitoring** - Real-time CPU, memory, and GPU usage graphs in the topbar
- **Authentication** - Secure with session login or API keys (optional)
- **Auto-restart** - Pipelines survive server restarts
- **Native or Web** - Run as desktop app or web service
- **MCP Integration** - Control pipelines with AI assistants (Claude, etc.)
- **CI/CD** - Automated testing, building, and releases for Linux, Windows, macOS, and ARM64

### Advanced Capabilities

- **Dynamic Pad Linking** - Automatic handling of runtime-created pads (decodebin, demuxers)
- **Automatic Tee Insertion** - Fan-out outputs without manual configuration
- **Pad Properties** - Configure per-pad properties (e.g., volume/mute on audiomixer inputs)
- **Debug Graphs** - Generate SVG visualizations of running pipelines
- **WebSocket/SSE** - Real-time state updates and pipeline events

## Quick Start

### Option 1: One-liner Install (Recommended)

Install Strom with a single command:

```bash
# Interactive install (DEFAULT) - shows configuration menu
curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | bash

# Automated install (skip menu for CI/CD)
curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | AUTO_INSTALL=true bash

# Automated with minimal GStreamer
curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | AUTO_INSTALL=true GSTREAMER_INSTALL_TYPE=minimal bash

# Automated binary only (skip dependencies)
curl -sSL https://raw.githubusercontent.com/Eyevinn/strom/main/install.sh | AUTO_INSTALL=true SKIP_GSTREAMER=true SKIP_GRAPHVIZ=true bash
```

**Interactive mode (default):**
- Use: `curl -sSL ... | bash`
- Shows configuration menu to customize installation
- Reads input from terminal even when piped (uses `/dev/tty`)
- Choose binary (strom or strom-mcp-server)
- Select GStreamer type (minimal or full)
- Configure version and install location
- Human-friendly with clear defaults

**Automated mode (for CI/CD):**
- Use: `curl -sSL ... | AUTO_INSTALL=true bash`
- Skips interactive menu entirely
- Uses defaults or environment variables
- Perfect for scripts and automation

The script will:
- Detect your OS and architecture automatically
- Download the latest release from GitHub
- Install to `/usr/local/bin` (or `~/.local/bin` if no sudo)
- Install GStreamer (full or minimal) - **required for Strom to work**
- Install Graphviz - **required for pipeline debug graphs**

**Environment Variables:**
- `AUTO_INSTALL=true` - Skip interactive menu (default: false)
- `GSTREAMER_INSTALL_TYPE=minimal` - Core GStreamer + base/good plugins only
- `GSTREAMER_INSTALL_TYPE=full` - All plugins including bad/ugly/libav + WebRTC support (libnice) (default)
- `SKIP_GSTREAMER=true` - Skip GStreamer (not recommended)
- `SKIP_GRAPHVIZ=true` - Skip Graphviz (debug graphs won't work)
- `INSTALL_DIR=/custom/path` - Custom installation directory
- `VERSION=v0.2.6` - Install specific version

After installation, run `strom` and open `http://localhost:8080` in your browser.

### Option 2: Using Pre-built Binaries

Download the latest release for your platform from [GitHub Releases](https://github.com/Eyevinn/strom/releases):

```bash
# Linux
wget https://github.com/Eyevinn/strom/releases/latest/download/strom-v*-linux-x86_64
chmod +x strom-v*-linux-x86_64
./strom-v*-linux-x86_64

# macOS
# Download and run the macOS binary

# Windows
# Download and run the .exe file
```

Open your browser to `http://localhost:8080` to access the web UI.

### Option 3: Using Docker (Recommended for Testing)

```bash
# Pull and run the latest version
docker pull eyevinntechnology/strom:latest
docker run -p 8080:8080 -v $(pwd)/data:/data eyevinntechnology/strom:latest

# Or build locally
docker build -t strom .
docker run -p 8080:8080 -v $(pwd)/data:/data strom
```

Access the web UI at `http://localhost:8080`

### Option 4: Building from Source

#### Prerequisites

```bash
# Install GStreamer
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav

# Install Rust and tools
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-unknown-unknown
cargo install trunk
```

#### Run

```bash
# Production mode (web UI at http://localhost:8080)
cargo run --release

# Development with hot reload
cargo run                    # Backend on :8080 (Terminal 1)
cd frontend && trunk serve   # Frontend on :8095 (Terminal 2)

# Headless mode (API only)
cargo run --release -- --headless
```

### First Steps

Once Strom is running:

1. Open `http://localhost:8080` in your browser
2. Browse available GStreamer elements in the palette
3. Drag elements onto the canvas to create your pipeline
4. Connect elements by dragging from output pads to input pads
5. Configure element properties in the inspector panel
6. Click "Start" to launch your pipeline

For API usage, visit `http://localhost:8080/swagger-ui` for interactive documentation.

## CI/CD

Strom includes automated CI/CD pipelines for continuous integration, testing, and releases:

### Continuous Integration

On every push to `main` and pull requests, automated checks run:

- **Format Check** - Ensures code follows Rust formatting standards
- **Clippy Linting** - Static analysis for backend, MCP server, and frontend (WASM)
- **Test Suite** - Runs all tests for backend and MCP server
- **Multi-platform Builds** - Builds binaries for Linux, Windows, and macOS

### Automated Releases

When a version tag is pushed (e.g., `v0.1.0`):

- **Cross-platform Binaries** - Automatically builds for Linux, Windows, and macOS
- **GitHub Releases** - Creates release with binaries and generated release notes
- **Docker Publishing** - Publishes multi-platform images (amd64/arm64) to Docker Hub

### Docker Hub

Pre-built multi-architecture Docker images are available (amd64 and arm64):

```bash
docker pull eyevinntechnology/strom:latest
docker pull eyevinntechnology/strom:0.2.6  # Specific version
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
│  - Storage (JSON or PostgreSQL) │
└─────────────────────────────────┘
```

**Workspace Members:**
- `strom-types` - Shared domain models and API types
- `strom` - Server with GStreamer pipeline management
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

## gst-launch Import/Export

Strom supports importing and exporting pipelines using standard `gst-launch-1.0` syntax, making it easy to convert existing command-line pipelines to visual flows.

**Import:**
- Paste any `gst-launch-1.0` command directly into the import dialog
- Supports multiline pipelines with backslash continuations
- Handles element references for complex pipelines (e.g., `name=mux ... mux.`)
- Automatically strips command prefixes and flags (`-v`, `-e`, etc.)

**Export:**
- Convert visual flows back to `gst-launch-1.0` syntax
- Useful for documentation, sharing, or running pipelines outside Strom

**Example - Import a test pipeline:**
```bash
gst-launch-1.0 -v videotestsrc is-live=true ! x264enc ! mp4mux name=mux ! filesink location=test.mp4 audiotestsrc is-live=true ! lamemp3enc ! mux.
```

**API Endpoints:**
- `POST /api/gst-launch/parse` - Parse gst-launch string to flow
- `POST /api/gst-launch/export` - Export flow to gst-launch string

## Configuration

Configure Strom using config files, command-line arguments, or environment variables.

### Configuration Priority

Settings are applied in this order (highest to lowest priority):
1. Command-line arguments
2. Environment variables
3. Local config file (`.strom.toml` in current directory)
4. User config file (`~/.config/strom/config.toml` on Linux)
5. Default values

### Config File

Create a `.strom.toml` file in your project directory:

```bash
# Copy the example config
cp .strom.toml.example .strom.toml

# Edit with your settings
nano .strom.toml
```

See [.strom.toml.example](.strom.toml.example) for all available options and documentation.

### Command-Line & Environment Variables

```bash
# Server
--port 8080                           # or STROM_SERVER_PORT=8080

# Storage - PostgreSQL (recommended for production)
--database-url postgresql://user:pass@localhost/strom  # or STROM_STORAGE_DATABASE_URL=...

# Storage - JSON files (default)
--data-dir /path/to/data              # or STROM_STORAGE_DATA_DIR=/path/to/data
--flows-path /custom/flows.json       # or STROM_STORAGE_FLOWS_PATH=/custom/flows.json
--blocks-path /custom/blocks.json     # or STROM_STORAGE_BLOCKS_PATH=/custom/blocks.json

# Logging
RUST_LOG=info
```

### Storage Options

**PostgreSQL (Recommended for production):**
- Set `STROM_DATABASE_URL` to use PostgreSQL for flow storage
- Supports multiple isolated instances sharing one PostgreSQL server
- Automatic schema migrations on startup
- See [docs/POSTGRESQL.md](docs/POSTGRESQL.md) for setup guide

**JSON Files (Default):**
- Used when `STROM_DATABASE_URL` is not set
- Simple file-based storage

**Default storage locations:**
- **Docker:** `./data/` (current directory)
- **Linux:** `~/.local/share/strom/`
- **Windows:** `%APPDATA%\strom\`
- **macOS:** `~/Library/Application Support/strom/`

**Note:** Individual file paths (`--flows-path`, `--blocks-path`) override `--data-dir`.

## Authentication

Strom supports two authentication methods to protect your installation:

### 1. Session-Based Authentication (Web Login)

Perfect for web UI access with username/password login.

**Setup:**

```bash
# Generate a password hash
cargo run -- hash-password
# Or with Docker:
docker run eyevinntechnology/strom:latest hash-password

# Enter your desired password when prompted
# Copy the generated hash
```

**Configure environment variables:**

```bash
export STROM_ADMIN_USER="admin"
export STROM_ADMIN_PASSWORD_HASH='$2b$12$...'  # Use single quotes to preserve special characters

# Run Strom
cargo run --release
```

**Usage:**
- Navigate to `http://localhost:8080`
- Login with your configured username and password
- Session persists for 24 hours of inactivity
- Click "Logout" button in the top-right to end session

### 2. API Key Authentication (Bearer Token)

Perfect for programmatic access, scripts, and CI/CD.

**Setup:**

```bash
export STROM_API_KEY="your-secret-api-key-here"

# Run Strom
cargo run --release
```

**Usage:**

```bash
# All API requests must include the Authorization header
curl -H "Authorization: Bearer your-secret-api-key-here" \
  http://localhost:8080/api/flows
```

### Using Both Methods

You can enable both authentication methods simultaneously:

```bash
# Enable both session and API key authentication
export STROM_ADMIN_USER="admin"
export STROM_ADMIN_PASSWORD_HASH='$2b$12$...'
export STROM_API_KEY="your-secret-api-key-here"

cargo run --release
```

Users can then:
- Login via web UI with username/password
- Access API with Bearer token

### Docker Authentication

```bash
docker run -p 8080:8080 \
  -e STROM_ADMIN_USER="admin" \
  -e STROM_ADMIN_PASSWORD_HASH='$2b$12$...' \
  -e STROM_API_KEY="your-api-key" \
  -v $(pwd)/data:/data \
  eyevinntechnology/strom:latest
```

### Disabling Authentication

Authentication is **disabled by default** if no credentials are configured. To run without authentication (development only):

```bash
# Simply run without setting auth environment variables
cargo run --release
```

**⚠️ Security Warning:** Never expose an unauthenticated Strom instance to the internet or untrusted networks.

### Protected Endpoints

When authentication is enabled, all API endpoints except the following require authentication:

- `GET /health` - Health check
- `POST /api/login` - Login endpoint
- `POST /api/logout` - Logout endpoint
- `GET /api/auth/status` - Check auth status
- Static assets (frontend files)

## Blocks System

Create reusable components from element groups:

**Built-in Blocks:**
- **AES67 Receiver** - ST 2110-30 audio receiver with SDP generation
- **Video Encoder** - H.264/H.265/AV1/VP9 encoding with automatic hardware acceleration (NVENC, VA-API, software fallback)
- **Video Format** - Resolution, framerate, and pixel format conversion (8K to QVGA, cinema framerates, professional formats)
- **Audio Format** - Sample rate, channel configuration, and PCM format conversion (supports multi-channel and surround sound with proper channel masks)
- **Audio/Video Meter** - Level monitoring for audio and video streams
- Custom blocks via JSON or API

Example: Add AES67 block to receive network audio streams, automatically generates proper SDP with multicast addressing.

See `docs/BLOCKS_IMPLEMENTATION.md` and `docs/VIDEO_ENCODER_BLOCK.md` for details.

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

### Setup Development Environment

```bash
# Clone the repository
git clone https://github.com/Eyevinn/strom.git
cd strom

# Install GStreamer dependencies
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav

# Install Rust toolchain
rustup target add wasm32-unknown-unknown
cargo install trunk

# Install git hooks for automated checks
./scripts/install-hooks.sh
```

### Testing

Strom includes comprehensive tests to ensure quality and reliability.

#### Run All Tests

```bash
# Run all tests across the workspace
cargo test --workspace

# Run tests with output
cargo test --workspace -- --nocapture

# Run tests for specific package
cargo test --package strom
cargo test --package strom-mcp-server
cargo test --package strom-types
```

#### Run Specific Tests

```bash
# Run a specific test by name
cargo test test_name

# Run tests matching a pattern
cargo test pipeline

# Run integration tests only
cargo test --test '*'
```

#### Code Quality Checks

```bash
# Format code (required before committing)
cargo fmt --all

# Check formatting without modifying files
cargo fmt --all -- --check

# Run linter (Clippy)
cargo clippy --workspace -- -D warnings

# Lint specific packages
cargo clippy --package strom --all-targets --all-features -- -D warnings
cargo clippy --package strom-frontend --target wasm32-unknown-unknown -- -D warnings
```

#### Frontend Testing

```bash
# Build frontend for development
cd frontend
trunk serve

# Build frontend for production
trunk build --release

# Check frontend compiles for WASM
cargo check --package strom-frontend --target wasm32-unknown-unknown
```

#### Manual Testing

To test Strom manually:

1. **Start the backend:**
   ```bash
   cargo run --package strom
   ```

2. **Access the UI:**
   Open `http://localhost:8080` in your browser

3. **Test a simple pipeline:**
   - Add a `videotestsrc` element
   - Add an `autovideosink` element
   - Connect them and click "Start"
   - You should see a test pattern window

4. **Test the API:**
   ```bash
   # List all flows
   curl http://localhost:8080/api/flows

   # Get available elements
   curl http://localhost:8080/api/elements

   # View API documentation
   open http://localhost:8080/swagger-ui
   ```

#### Docker Testing

```bash
# Build Docker image locally
docker build -t strom:test .

# Run and test
docker run -p 8080:8080 strom:test

# Test with custom data directory
docker run -p 8080:8080 -v $(pwd)/test-data:/data strom:test
```

#### Pre-commit Checks

The git hooks run these checks automatically before each commit:

- Code formatting (`cargo fmt`)
- Linting (`cargo clippy`)
- Tests (`cargo test`)

If any check fails, the commit is blocked until issues are resolved.

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

See `docs/PAD_TEMPLATE_CRASH_FIX.md` and `docs/MPEGTSMUX_DEADLOCK_FIX.md` for technical details.

## Contributing

See [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md)

## License

MIT OR Apache-2.0
