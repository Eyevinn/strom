# Dockerfile for Strom - Multi-stage build with separate frontend and backend builders

# Stage 1: Chef base - Use lukemathwalker's cargo-chef image as base
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

# Stage 2: Planner - Analyze dependencies and create recipe
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Frontend builder - Build WASM frontend separately
FROM chef AS frontend-builder
WORKDIR /app

# Get the target architecture from BuildKit
ARG TARGETARCH

# Install trunk for building the WASM frontend from binary release (match CI version)
# Map Docker arch to Rust target triple: amd64->x86_64, arm64->aarch64
RUN TRUNK_ARCH=$(case ${TARGETARCH} in \
      amd64) echo "x86_64-unknown-linux-gnu" ;; \
      arm64) echo "aarch64-unknown-linux-gnu" ;; \
      *) echo "x86_64-unknown-linux-gnu" ;; \
    esac) && \
    curl -L https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-${TRUNK_ARCH}.tar.gz | tar -xz -C /usr/local/bin

# Add WASM target for frontend compilation
RUN rustup target add wasm32-unknown-unknown

# Copy workspace (needed for cargo metadata, but we only build frontend)
COPY . .

# Build the frontend only
RUN cd frontend && trunk build --release

# Stage 4: Backend builder - Build backend with embedded frontend
FROM chef AS backend-builder
WORKDIR /app

# Install GStreamer development dependencies (NOT trunk/wasm - backend only)
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    pkg-config \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy recipe and build dependencies (this layer is cached)
COPY --from=planner /app/recipe.json recipe.json
# Cook with the same feature flags we'll use for the actual build
RUN cargo chef cook --release --recipe-path recipe.json \
    --package strom --no-default-features \
    --package strom-mcp-server

# Copy entire project source
COPY . .

# Copy the built frontend dist from frontend-builder
# Note: Trunk.toml puts output in ../backend/dist relative to frontend/
COPY --from=frontend-builder /app/backend/dist backend/dist

# Build the backend (headless - no native GUI needed in Docker) and MCP server
ENV RUST_BACKTRACE=1
RUN cargo build --release --package strom --no-default-features
RUN cargo build --release --package strom-mcp-server

# Stage 5: Runtime - Minimal runtime image with trixie for newer GStreamer
FROM debian:trixie-slim AS runtime
WORKDIR /app

# Install only GStreamer runtime dependencies
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    libgstreamer1.0-0 \
    libgstreamer-plugins-base1.0-0 \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-nice \
    gstreamer1.0-tools \
    graphviz \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binaries from backend-builder to /app
COPY --from=backend-builder /app/target/release/strom /app/strom
COPY --from=backend-builder /app/target/release/strom-mcp-server /app/strom-mcp-server

# Set environment variables
ENV RUST_LOG=info
ENV STROM_PORT=8080
ENV STROM_DATA_DIR=/data

# Create data directory for persistent storage
RUN mkdir -p /data

# Expose the server port
EXPOSE 8080

# Run the server from /app
CMD ["/app/strom"]
