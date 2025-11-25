# Dockerfile for Strom - Simplified Ubuntu-based multi-stage build

# Stage 1: Builder - Compile strom-backend with embedded frontend
FROM ubuntu:latest AS builder
WORKDIR /app

# Accept build arguments and set as environment variables
ARG CARGO_INCREMENTAL=0
ENV CARGO_INCREMENTAL=${CARGO_INCREMENTAL}

# Install Rust and minimal GStreamer development packages
RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    pkg-config \
    libssl-dev \
    time \
    lld \
    clang \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install trunk for building WASM frontend (match CI version)
RUN curl -L https://github.com/trunk-rs/trunk/releases/download/v0.21.5/trunk-x86_64-unknown-linux-gnu.tar.gz | tar -xz -C /usr/local/bin

# Add WASM target for frontend compilation
RUN rustup target add wasm32-unknown-unknown

# Copy source code
COPY . .

# Debug: Print environment variables to verify they're set
RUN echo "=== Build Environment ===" && \
    echo "CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-not set}" && \
    echo "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-not set}" && \
    printenv | grep CARGO || echo "No CARGO_* env vars found"

# Build the frontend
RUN cd frontend && trunk build --release

# Debug: Verify dist files exist
RUN echo "=== Checking backend/dist contents ===" && \
    ls -lah backend/dist/ && \
    echo "=== Files in backend/dist ===" && \
    find backend/dist -type f

RUN cargo clean

# Build strom-backend with embedded frontend (no native GUI)
# Use lld (LLVM linker) instead of GNU ld to avoid hanging issues
ENV RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=lld -C link-arg=-Wl,--verbose"
RUN echo "=== Starting backend build with lld linker and verbose output ===" && \
    time cargo build -vv --release --package strom-backend --no-default-features

# Build strom-mcp-server
RUN echo "=== Starting MCP server build ===" && \
    time cargo build -vv --release --package strom-mcp-server

# Stage 2: Runtime - Fresh Ubuntu with full GStreamer runtime
FROM ubuntu:latest AS runtime
WORKDIR /app

# Install full GStreamer runtime packages
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

# Copy compiled binaries from builder to /app
COPY --from=builder /app/target/release/strom-backend /app/strom-backend
COPY --from=builder /app/target/release/strom-mcp-server /app/strom-mcp-server

# Set environment variables
ENV RUST_LOG=info
ENV STROM_PORT=8080
ENV STROM_DATA_DIR=/data

# Create data directory for persistent storage
RUN mkdir -p /data

# Expose the server port
EXPOSE 8080

# Run the server
CMD ["/app/strom-backend"]
