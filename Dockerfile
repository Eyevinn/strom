# Dockerfile for Strom - Simplified Ubuntu-based multi-stage build

# Stage 1: Builder - Compile strom-backend with minimal dependencies
FROM ubuntu:latest AS builder
WORKDIR /app

# Install Rust and minimal GStreamer development packages
RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    pkg-config \
    libssl-dev \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Copy source code
COPY . .

# Build strom-backend only (headless - no frontend)
RUN cargo build --release --package strom-backend --no-default-features

# Build strom-mcp-server
RUN cargo build --release --package strom-mcp-server

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

# Copy compiled binaries from builder
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
