# Development Guide

> Code is the source of truth — this may have drifted; read the code for the current implementation.

## Quick Start

### Prerequisites

Make sure you have the following installed:

1. **Rust** via rustup (the toolchain version is pinned in `rust-toolchain.toml`; rustup
   installs it on the first build)
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **GStreamer development libraries and runtime plugins**
   ```bash
   # Ubuntu/Debian (full set, matches what the binaries are built against)
   sudo apt-get install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
     libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-base \
     gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
     gstreamer1.0-plugins-ugly gstreamer1.0-libav \
     gstreamer1.0-tools libnice-dev gstreamer1.0-nice \
     libcairo2-dev graphviz

   # Fedora
   sudo dnf install gstreamer1-devel gstreamer1-plugins-base-devel

   # macOS (libnice-gstreamer, not libnice — it provides the nicesrc/nicesink
   # elements webrtcbin needs for ICE; without them WHIP/WHEP aborts the process)
   brew install gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
     gst-plugins-ugly gst-libav libnice-gstreamer cairo graphviz
   ```

3. **WebAssembly target** (for frontend)
   ```bash
   rustup target add wasm32-unknown-unknown
   ```

4. **Trunk** (for building frontend)
   ```bash
   cargo install trunk --locked
   ```

### Optional Cargo Features

| Feature | Description | Extra dependencies |
|---------|-------------|--------------------|
| `nvidia` | NVIDIA GPU monitoring (default) | None |
| `efp` | EFP/SRT input and output blocks | `cmake`, `libclang-dev` (Linux) / `cmake` (macOS, via Homebrew) |
| `no-gui` | Build without the native GUI (server only; used by the Docker image) | None |

```bash
# Build with EFP support (Linux)
sudo apt install cmake libclang-dev
cargo run --release --features efp

# Build with EFP support (macOS)
brew install cmake
cargo run --release --features efp
```

Pre-built Linux and macOS binaries, plus the Docker images, ship with the `efp`
feature enabled. Windows binaries currently ship without EFP because
`gst-plugin-efp` has not been validated on that platform yet.

## Project Structure

```
strom/
├── types/          # Shared types library (strom-types)
├── backend/        # Backend server (strom)
└── frontend/       # Frontend WASM app (strom-frontend)
```

## Building

### Build everything
```bash
cargo build
```

### Build specific crates
```bash
# All crates are built from the workspace root (never use -p flag)
cargo build
# Frontend builds with trunk (see below)
```

### Check for errors (faster than build)
```bash
cargo check --workspace
```

## Running

### Backend Server

Start the backend server:
```bash
cargo run
```

The server will start on `http://localhost:8080` by default.

**Configuration options:**
```bash
# Via CLI arguments
cargo run -- --port 8080 --data-dir ./my-data

# Via environment variables
STROM_PORT=8080 STROM_DATA_DIR=./my-data cargo run
```

**Available options** (`cargo run -- --help` prints the current list):
- `--port` / `STROM_PORT` - Port to listen on (default: 8080)
- `--data-dir` / `STROM_DATA_DIR` - Data directory for storage files
- `--flows-path` / `STROM_FLOWS_PATH` - Override flows file path
- `--blocks-path` / `STROM_BLOCKS_PATH` - Override blocks file path
- `--media-path` / `STROM_MEDIA_PATH` - Override media files directory
- `--cef-cache-path` / `STROM_CEF_CACHE_PATH` - Directory for the CEF/Chromium profile used by HTML sources
- `--database-url` / `STROM_DATABASE_URL` - Database URL (e.g., postgresql://user:pass@localhost/strom)
- `--tls-cert` / `STROM_TLS_CERT` and `--tls-key` / `STROM_TLS_KEY` - Serve HTTPS (PEM files, reloaded on change)
- `--headless` - Run without GUI (API only)
- `--x11` / `--wayland` - Force the display backend on Linux
- `--no-auto-restart` - Do not restart flows on startup
- `--version-info` - Print version and build information and exit
- `hash-password` - Subcommand that generates a password hash (see [AUTHENTICATION.md](AUTHENTICATION.md))

Other settings (logging, ICE servers, port pool, discovery) are set in `.strom.toml`; see
`.strom.toml.example`. CEF settings are covered in [HTML_RENDER.md](HTML_RENDER.md).

**Default storage locations:**
- Linux: `~/.local/share/strom/`
- Windows: `%APPDATA%\eyevinn\strom\data\`
- macOS: `~/Library/Application Support/com.eyevinn.strom/`
- Docker: `./data` (the image sets `STROM_DATA_DIR=/data`)

### Frontend (Development)

The frontend is designed to run as WebAssembly in a browser.

**Option 1: Using trunk (recommended for development)**
```bash
cd frontend
trunk serve
```

This will:
- Build the frontend for WASM
- Start a dev server on `http://localhost:8095`
- Auto-reload on file changes

**Option 2: Build for production**
```bash
cd frontend
trunk build --release
```

The built files go to `backend/dist/`, where the backend embeds them at compile time.

### Full Stack Development

Run both backend and frontend simultaneously:

**Terminal 1: Backend**
```bash
cargo run
```

**Terminal 2: Frontend**
```bash
cd frontend
trunk serve
```

Then open `http://localhost:8095` in your browser. The frontend will connect to the backend API at `http://localhost:8080`.

## Testing the API

### Health check
```bash
curl http://localhost:8080/health
# Expected: OK
```

### List flows
```bash
curl http://localhost:8080/api/flows
# Expected: {"flows":[]}
```

### Create a flow
```bash
curl -X POST http://localhost:8080/api/flows \
  -H "Content-Type: application/json" \
  -d '{"id":"00000000-0000-0000-0000-000000000000","name":"Test Flow"}'
```

`id` is required. Send the nil uuid to have the server assign one, and read it from
`flow.id` in the response.

### Get a specific flow
```bash
curl http://localhost:8080/api/flows/<flow-id>
```

## Common Tasks

### Format code
```bash
cargo fmt --all
```

### Run linter
```bash
cargo clippy --workspace
```

### Clean build artifacts
```bash
cargo clean
```

### Update dependencies
```bash
cargo update
```

## Project Status

See [README.md](../README.md) for a full list of features and capabilities.

## Troubleshooting

### GStreamer not found
Make sure GStreamer development libraries are installed and `pkg-config` can find them:
```bash
pkg-config --modversion gstreamer-1.0
```

### Frontend won't compile for WASM
Make sure the WASM target is installed:
```bash
rustup target add wasm32-unknown-unknown
```

### Port already in use
Change the backend port:
```bash
cargo run -- --port 8081
# or
STROM_PORT=8081 cargo run
```

Then update the frontend API URL if needed.

### Storage files in unexpected location
By default, storage files go to platform-specific directories. To use current directory:
```bash
cargo run -- --data-dir ./data
```
