# Docker Deployment Guide

This guide covers deploying Strom using Docker and Docker Compose, including both the backend server and MCP server.

## Quick Start

### Using Docker Compose (Recommended)

The easiest way to run Strom with both backend and MCP server:

```bash
# Build and start all services
docker-compose up -d

# View logs
docker-compose logs -f

# Stop services
docker-compose down
```

This starts:
- **strom**: Web server on port 8080
- **strom-mcp**: MCP server for AI integration

### Using Docker Only

If you prefer to run services individually:

```bash
# Build the image
docker build -t strom:latest .

# Run backend only
docker run -d \
  -p 8080:8080 \
  -v $(pwd)/data:/data \
  -e RUST_LOG=info \
  --name strom \
  strom:latest

# Run MCP server (requires backend running)
docker run -d \
  --link strom \
  -e STROM_API_URL=http://strom:8080 \
  -e RUST_LOG=info \
  --name strom-mcp \
  strom:latest \
  strom-mcp-server
```

## Architecture

```
┌─────────────────────────────────────────┐
│        Docker Compose Network           │
│                                          │
│  ┌────────────────┐  ┌───────────────┐ │
│  │ strom  │  │  strom-mcp    │ │
│  │   Port: 8080   │◄─┤  (stdio MCP)  │ │
│  │                │  │               │ │
│  │  - Web UI      │  │  - AI Tools   │ │
│  │  - REST API    │  │  - JSON-RPC   │ │
│  │  - GStreamer   │  └───────────────┘ │
│  └────────────────┘                     │
│         │                                │
│         │ Volume Mount                   │
│         ▼                                │
│    ./data/ (flows.json, blocks.json)     │
└─────────────────────────────────────────┘
```

## Configuration

### Environment Variables

#### Backend (strom)
- `RUST_LOG` - Logging level (default: `info`)
- `STROM_PORT` - HTTP server port (default: `8080`)
- `STROM_DATA_DIR` - Data directory for storage files (default: `/data`)
- `STROM_FLOWS_PATH` - Override flows file path (optional)
- `STROM_BLOCKS_PATH` - Override blocks file path (optional)
- `STROM_DATABASE_URL` - Database URL for PostgreSQL storage (optional, e.g., `postgresql://user:pass@localhost/strom`)
- `STROM_ADMIN_USER` - Admin username for authentication (optional)
- `STROM_ADMIN_PASSWORD_HASH` - Admin password hash for authentication (optional)
- `STROM_API_KEY` - API key for bearer token authentication (optional)

#### MCP Server (strom-mcp)
- `RUST_LOG` - Logging level (default: `info`)
- `STROM_API_URL` - Backend URL (default: `http://strom:8080`)

### Volumes

- `./data:/data` - Persistent storage for flow configurations

### Ports

- `8080` - Backend HTTP server (web UI and REST API)

## Using the MCP Server with Claude Desktop

To connect Claude Desktop to the dockerized MCP server:

1. **Expose the MCP server via named pipe or socket** (advanced setup required)
2. **Or use the standalone MCP server** outside Docker:

```bash
# Run MCP server locally, pointing to dockerized backend
STROM_API_URL=http://localhost:8080 ./target/release/strom-mcp-server
```

Configure Claude Desktop:
```json
{
  "mcpServers": {
    "strom": {
      "command": "/path/to/strom-mcp-server",
      "env": {
        "STROM_API_URL": "http://localhost:8080"
      }
    }
  }
}
```

## Multi-Stage Build

The Dockerfile uses a multi-stage build with cargo-chef for optimal caching:

1. **Planner**: Analyzes dependencies
2. **Builder**: Builds dependencies and binaries
3. **Runtime**: Minimal Debian image with only runtime dependencies

### Build Stages

```dockerfile
# Stage 1: Dependency analysis
FROM rust:1.82-bookworm as planner
RUN cargo chef prepare

# Stage 2: Build everything
FROM rust:1.82-bookworm as builder
RUN cargo chef cook    # Build dependencies (cached)
RUN cargo build        # Build binaries

# Stage 3: Minimal runtime
FROM debian:trixie-slim
COPY --from=builder binaries
```

This approach:
- Caches dependencies for faster rebuilds
- Produces small final images (~200MB vs ~2GB)
- Includes only runtime dependencies

## Health Checks

The backend includes a health check:

```yaml
healthcheck:
  test: ["CMD", "curl", "-f", "http://localhost:8080/health"]
  interval: 30s
  timeout: 10s
  retries: 3
```

Check service health:
```bash
docker-compose ps
```

## Logs

View logs for all services:
```bash
docker-compose logs -f
```

View logs for specific service:
```bash
docker-compose logs -f strom
docker-compose logs -f strom-mcp
```

## Troubleshooting

### Backend won't start

Check GStreamer dependencies:
```bash
docker-compose exec strom gst-inspect-1.0 --version
```

### MCP server can't connect to backend

Verify networking:
```bash
docker-compose exec strom-mcp curl http://strom:8080/health
```

### Rebuilding after code changes

```bash
docker-compose down
docker-compose build --no-cache
docker-compose up -d
```

## Production Deployment

### Reverse Proxy

Use nginx or Caddy in front of the backend:

```nginx
server {
    listen 80;
    server_name strom.example.com;

    location / {
        proxy_pass http://localhost:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### Docker Compose for Production

```yaml
services:
  strom:
    image: strom:latest
    restart: always
    environment:
      - RUST_LOG=warn  # Less verbose
    volumes:
      - /var/strom/data:/data  # Persistent location
    deploy:
      resources:
        limits:
          cpus: '2'
          memory: 2G
```

### Backup

Backup data files:
```bash
docker cp strom:/data/flows.json ./backup/
docker cp strom:/data/blocks.json ./backup/
# Or backup entire data directory
docker cp strom:/data ./backup/
```

## Development

Mount source code for development:
```yaml
volumes:
  - .:/app
  - /app/target  # Separate target directory
```

Hot reload not supported - rebuild after changes:
```bash
docker-compose build strom
docker-compose restart strom
```

## Security

### Best Practices

1. **Run as non-root user** (add to Dockerfile):
   ```dockerfile
   RUN useradd -m strom
   USER strom
   ```

2. **Read-only filesystem**:
   ```yaml
   read_only: true
   tmpfs:
     - /tmp
   ```

3. **Resource limits**:
   ```yaml
   deploy:
     resources:
       limits:
         cpus: '2'
         memory: 2G
   ```

4. **Network isolation**: Use custom networks, not default bridge

## Monitoring

Add Prometheus metrics:
```yaml
services:
  prometheus:
    image: prom/prometheus
    ports:
      - "9090:9090"
```

Add Grafana dashboards:
```yaml
services:
  grafana:
    image: grafana/grafana
    ports:
      - "3000:3000"
```

## Additional Resources

- [Official Docker Documentation](https://docs.docker.com/)
- [Docker Compose Reference](https://docs.docker.com/compose/)
- [Multi-stage Builds](https://docs.docker.com/build/building/multi-stage/)
- [cargo-chef](https://github.com/LukeMathWalker/cargo-chef)
