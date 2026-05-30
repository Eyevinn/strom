# Docker Deployment Guide

This is the generic Docker reference for Strom. For a guided, opinionated deployment
(Docker run, GPU, ICE servers, authentication, verification) see
[OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) — it is the recommended starting point and ships a
working `docker-compose.yml` example.

## Images

Pre-built multi-architecture images (amd64/arm64) are published on Docker Hub:

```bash
docker pull eyevinntechnology/strom:latest        # base image
docker pull eyevinntechnology/strom-full:latest   # + CEF/Chromium for HTML rendering
```

Pin a specific version with a tag, e.g. `eyevinntechnology/strom:0.6.0`.

## Quick start

```bash
docker run -d \
  --name strom \
  --restart unless-stopped \
  -p 8080:8080 \
  -v "$(pwd)/data:/data" \
  eyevinntechnology/strom:latest
```

Open `http://localhost:8080`. The `/data` volume persists flows, blocks, and other
configuration across restarts.

For GPU acceleration add `--gpus all` and `-e NVIDIA_DRIVER_CAPABILITIES=all` — see
[DOCKER_GPU_SETUP.md](DOCKER_GPU_SETUP.md). For WHEP/WHIP, AES67, NDI, or SRT, prefer
`--network host` so these protocols don't have to fight Docker NAT.

## Configuration

Strom is configured via environment variables (see [DEVELOPMENT.md](DEVELOPMENT.md) for the
full list and the CLI equivalents):

| Variable | Purpose |
|----------|---------|
| `STROM_PORT` | HTTP server port (default `8080`) |
| `STROM_DATA_DIR` | Data directory (default `/data` in the image) |
| `STROM_DATABASE_URL` | PostgreSQL connection string (optional) — see [POSTGRESQL.md](POSTGRESQL.md) |
| `STROM_ADMIN_USER` / `STROM_ADMIN_PASSWORD_HASH` / `STROM_API_KEY` | Authentication — see [AUTHENTICATION.md](AUTHENTICATION.md) |
| `STROM_SERVER_ICE_SERVERS` | STUN/TURN servers for WebRTC |
| `STROM_TLS_CERT` / `STROM_TLS_KEY` | Built-in TLS (PEM) |
| `RUST_LOG` | Logging level (default `info`) |

Volumes: mount `./data:/data` for persistent storage. The `/data` volume is the only state
Strom keeps by default.

## Docker Compose

No `docker-compose.yml` is committed to the repository — compose files are deployment-specific
and gitignored. Use the worked example in [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) §5 as a
starting point and adapt it (GPU, auth, TLS, network mode, DeckLink mounts) to your host.

## MCP server in Docker

The image bundles the standalone MCP server binary at `/app/strom-mcp-server` (stdio
transport). The backend also serves MCP over HTTP at `/api/mcp` directly, so for most setups
you do **not** need to run the separate binary — point your MCP client at
`http://<host>:8080/api/mcp`. See [MCP.md](MCP.md).

To run the stdio MCP server against a running backend (e.g. for Claude Desktop), it's usually
simplest to run it on the host, pointing at the container's HTTP port:

```bash
STROM_API_URL=http://localhost:8080 ./target/release/strom-mcp-server
```

```json
{
  "mcpServers": {
    "strom": {
      "command": "/path/to/strom-mcp-server",
      "env": { "STROM_API_URL": "http://localhost:8080" }
    }
  }
}
```

## Building the image yourself

The [`Dockerfile`](../Dockerfile) uses a multi-stage build on Ubuntu 25.10 (Questing), which
provides GStreamer 1.26 with the nvcodec fix:

1. **Frontend builder** — builds the WASM frontend (platform-independent output).
2. **Backend builder** — builds the backend and the MCP server, optionally cross-compiling
   for ARM64 via Zig (targets an older glibc for broad compatibility — see
   [CROSS_COMPILE_ARM64.md](CROSS_COMPILE_ARM64.md)).
3. **Runtime** — minimal Ubuntu with the GStreamer runtime plugins and the GL/EGL libraries
   required for CUDA-GL interop.

```bash
docker build -t strom:local .
docker run -p 8080:8080 -v "$(pwd)/data:/data" strom:local
```

CI builds and publishes the multi-arch images on release; building locally is only needed for
development or custom images.

## Production notes

### Reverse proxy

Terminate TLS and route to the backend with nginx, Caddy, or Traefik:

```nginx
server {
    listen 443 ssl;
    server_name strom.example.com;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        # WebSocket upgrade for /api/ws
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }
}
```

Alternatively, terminate TLS in Strom itself with `STROM_TLS_CERT` / `STROM_TLS_KEY`.

### Logging and restart

```yaml
restart: unless-stopped
logging:
  driver: json-file
  options:
    max-size: "50m"
    max-file: "5"
```

### Backup

The entire state lives under `/data`:

```bash
docker cp strom:/data ./backup/
```

## Troubleshooting

```bash
# Health endpoint (no auth required)
curl http://localhost:8080/health

# Confirm GStreamer is present in the container
docker exec strom gst-inspect-1.0 --version
```

For GPU issues see [DOCKER_GPU_SETUP.md](DOCKER_GPU_SETUP.md); for segfaults see
[DEBUGGING_SEGFAULTS_WSL2.md](DEBUGGING_SEGFAULTS_WSL2.md).

## See also

- [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) — guided deployment with a compose example
- [DOCKER_GPU_SETUP.md](DOCKER_GPU_SETUP.md) — NVIDIA GPU acceleration
- [AUTHENTICATION.md](AUTHENTICATION.md) · [POSTGRESQL.md](POSTGRESQL.md) · [MCP.md](MCP.md)
