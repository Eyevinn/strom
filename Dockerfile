# Dockerfile for Strom - Multi-stage build with cargo-chef for optimal caching

# Stage 1: Chef base - Use lukemathwalker's cargo-chef image as base
FROM lukemathwalker/cargo-chef:latest-rust-bookworm AS chef-base
WORKDIR /app

# Upgrade to trixie for newer GStreamer packages
RUN echo "deb http://deb.debian.org/debian trixie main" > /etc/apt/sources.list.d/trixie.list && \
    apt-get update

FROM chef-base AS chef

# Stage 2: Planner - Analyze dependencies and create recipe
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Builder - Build dependencies using cargo-chef
FROM chef AS builder
WORKDIR /app

# Install GStreamer development dependencies (including WebRTC plugin)
RUN apt-get update && apt-get install -y \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-tools \
    && rm -rf /var/lib/apt/lists/*

# Install trunk for building the WASM frontend from binary release (match CI version)
RUN curl -L https://github.com/trunk-rs/trunk/releases/download/v0.21.5/trunk-x86_64-unknown-linux-gnu.tar.gz | tar -xz -C /usr/local/bin

# Add WASM target for frontend compilation
RUN rustup target add wasm32-unknown-unknown

# Limit parallelism to avoid OOM during gst-plugin-webrtc compilation in CI
# Set before chef cook so build environment matches
ENV CARGO_BUILD_JOBS=2

# Copy recipe and build dependencies (this layer is cached)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Copy entire project source
COPY . .

# Build the frontend
RUN mkdir -p backend/dist && cd frontend && trunk build --release

# Build the backend (headless - no native GUI needed in Docker) and MCP server
RUN cargo build --release --package strom-backend --no-default-features
RUN cargo build --release --package strom-mcp-server

# Stage 4: Runtime - Minimal runtime image with trixie for newer GStreamer
FROM debian:trixie-slim AS runtime
WORKDIR /app

# Install only GStreamer runtime dependencies
RUN apt-get update && apt-get install -y \
    libgstreamer1.0-0 \
    libgstreamer-plugins-base1.0-0 \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-tools \
    graphviz \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binaries from builder
COPY --from=builder /app/target/release/strom-backend /usr/local/bin/strom-backend
COPY --from=builder /app/target/release/strom-mcp-server /usr/local/bin/strom-mcp-server

# Set environment variables
ENV RUST_LOG=info
ENV STROM_PORT=8080
ENV STROM_DATA_DIR=/data

# Create data directory for persistent storage
RUN mkdir -p /data

# Expose the server port
EXPOSE 8080

# Run the server
CMD ["strom-backend"]
