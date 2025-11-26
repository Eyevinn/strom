# Development Guide

## Quick Start

### Prerequisites

Make sure you have the following installed:

1. **Rust** (1.82+)
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **GStreamer development libraries**
   ```bash
   # Ubuntu/Debian
   sudo apt-get install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev

   # Fedora
   sudo dnf install gstreamer1-devel gstreamer1-plugins-base-devel

   # macOS
   brew install gstreamer gst-plugins-base
   ```

3. **WebAssembly target** (for frontend)
   ```bash
   rustup target add wasm32-unknown-unknown
   ```

4. **Trunk** (for building frontend)
   ```bash
   cargo install trunk
   ```

## Project Structure

```
strom/
├── types/          # Shared types library (strom-types)
├── backend/        # Backend server (strom-backend)
└── frontend/       # Frontend WASM app (strom-frontend)
```

## Building

### Build everything
```bash
cargo build
```

### Build specific crates
```bash
cargo build -p strom-types
cargo build -p strom-backend
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
cargo run -p strom-backend
```

The server will start on `http://localhost:8080` by default.

**Configuration options:**
```bash
# Via CLI arguments
cargo run -p strom-backend -- --port 8080 --data-dir ./my-data

# Via environment variables
STROM_PORT=8080 STROM_DATA_DIR=./my-data cargo run -p strom-backend
```

**Available options:**
- `--port` / `STROM_PORT` - Port to listen on (default: 8080)
- `--data-dir` / `STROM_DATA_DIR` - Data directory for storage files
- `--flows-path` / `STROM_FLOWS_PATH` - Override flows file path
- `--blocks-path` / `STROM_BLOCKS_PATH` - Override blocks file path
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
cargo run -p strom-backend
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

Currently implemented:
- ✅ Workspace structure with 3 crates
- ✅ Shared types library (Flow, Element, Link, API types, SSE events)
- ✅ Complete backend server with REST API and SSE
- ✅ Full Flow CRUD endpoints
- ✅ GStreamer pipeline execution with dynamic pad linking
- ✅ Element discovery/introspection with media type classification
- ✅ Server-Sent Events for real-time updates
- ✅ Visual flow editor with multi-port support
- ✅ JSON file persistence with auto-save
- ✅ Auto-start flows on server boot
- ✅ Automatic tee insertion for multiple outputs
- ✅ Pipeline debug graph visualization
- ✅ egui frontend with color-coded ports
- ✅ CORS enabled for local development

See TODO.md for planned future enhancements.

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
cargo run -p strom-backend -- --port 8081
# or
STROM_PORT=8081 cargo run -p strom-backend
```

Then update the frontend API URL if needed.

### Storage files in unexpected location
By default, storage files go to platform-specific directories. To use current directory:
```bash
cargo run -p strom-backend -- --data-dir ./data
```
