# Development Guide

## Quick Start

### Prerequisites

Make sure you have the following installed:

1. **Rust** (1.82 or later, stable toolchain recommended)
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

   # macOS
   brew install gstreamer gst-plugins-base libnice-gstreamer
   ```

3. **WebAssembly target** (for frontend)
   ```bash
   rustup target add wasm32-unknown-unknown
   ```

4. **Trunk** (for building frontend)
   ```bash
   cargo install trunk
   ```

### Optional Cargo Features

| Feature | Description | Extra dependencies |
|---------|-------------|--------------------|
| `nvidia` | NVIDIA GPU monitoring (default) | None |
| `efp` | EFP/SRT input and output blocks | `cmake`, `libclang-dev` (Linux) / `cmake` (macOS, via Homebrew) |

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

**Available options:**
- `--port` / `STROM_PORT` - Port to listen on (default: 8080)
- `--data-dir` / `STROM_DATA_DIR` - Data directory for storage files
- `--flows-path` / `STROM_FLOWS_PATH` - Override flows file path
- `--blocks-path` / `STROM_BLOCKS_PATH` - Override blocks file path
- `--database-url` / `STROM_DATABASE_URL` - Database URL (e.g., postgresql://user:pass@localhost/strom)
- `--headless` - Run without GUI (API only)

**Default storage locations:**
- Linux: `~/.local/share/strom/`
- Windows: `%APPDATA%\strom\`
- macOS: `~/Library/Application Support/strom/`

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

The built files will be in `frontend/dist/`. You can serve these with any static file server, or have the backend serve them.

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
  -d '{"name":"Test Flow","auto_start":false}'
```

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
