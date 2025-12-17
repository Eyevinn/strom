# Dockerfile for Strom - Ubuntu 25.04 (Plucky)-based multi-stage build with Zig cross-compilation

# Stage 1: Frontend builder - Build WASM frontend on native platform
# IMPORTANT: Always build on native platform - WASM output is platform-independent!
FROM --platform=$BUILDPLATFORM ubuntu:plucky AS frontend-builder
WORKDIR /app

# Get native build platform architecture
ARG BUILDARCH

# Install Rust and dependencies
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

# Install trunk for native platform and add WASM target
RUN TRUNK_ARCH=$(case ${BUILDARCH} in \
      amd64) echo "x86_64-unknown-linux-gnu" ;; \
      arm64) echo "aarch64-unknown-linux-gnu" ;; \
      *) echo "x86_64-unknown-linux-gnu" ;; \
    esac) && \
    curl -L https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-${TRUNK_ARCH}.tar.gz | \
    tar -xz -C /usr/local/bin && \
    rustup target add wasm32-unknown-unknown

# Copy workspace
COPY . .

# Build frontend (WASM is platform-independent)
RUN cd frontend && trunk build --release

# Stage 2: Backend builder - Build backend with Zig cross-compilation support
# IMPORTANT: Must run on BUILD platform for cross-compilation tools (Zig) to work
FROM --platform=$BUILDPLATFORM ubuntu:plucky AS backend-builder
WORKDIR /app

# Get build and target platform info for cross-compilation detection
ARG BUILDPLATFORM
ARG TARGETPLATFORM
ARG BUILDARCH
ARG TARGETARCH

# Install Rust and base dependencies
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    pkg-config \
    curl \
    xz-utils \
    build-essential \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

# Cross-compilation setup: Install Zig and ARM64 libraries when cross-compiling
# Uses Ubuntu ports.ubuntu.com for ARM64 packages (matching local setup)
RUN if [ "$BUILDPLATFORM" != "$TARGETPLATFORM" ] && [ "$TARGETARCH" = "arm64" ]; then \
    echo "==> Cross-compiling from $BUILDPLATFORM to $TARGETPLATFORM - Setting up Zig"; \
    # Install Zig for cross-compilation
    ZIG_VERSION="0.13.0" && \
    ZIG_ARCH=$(case ${BUILDARCH} in amd64) echo "x86_64" ;; arm64) echo "aarch64" ;; esac) && \
    ZIG_TARBALL="zig-linux-${ZIG_ARCH}-${ZIG_VERSION}.tar.xz" && \
    curl -L "https://ziglang.org/download/${ZIG_VERSION}/${ZIG_TARBALL}" -o "/tmp/${ZIG_TARBALL}" && \
    tar -xf "/tmp/${ZIG_TARBALL}" -C /usr/local && \
    mv /usr/local/zig-linux-${ZIG_ARCH}-${ZIG_VERSION} /usr/local/zig && \
    ln -s /usr/local/zig/zig /usr/local/bin/zig && \
    rm "/tmp/${ZIG_TARBALL}" && \
    # Install cargo-zigbuild
    cargo install --locked cargo-zigbuild && \
    # Add Rust ARM64 target
    rustup target add aarch64-unknown-linux-gnu && \
    # Setup multi-arch for ARM64 (matching setup-arm64-cross.sh)
    dpkg --add-architecture arm64 && \
    # Update ubuntu.sources to specify amd64 architecture (archive.ubuntu.com doesn't have arm64)
    sed -i '/^Types: deb$/a Architectures: amd64' /etc/apt/sources.list.d/ubuntu.sources && \
    # Block ARM64 Python packages (critical - prevents apt from removing amd64 Python!)
    mkdir -p /etc/apt/preferences.d && \
    printf 'Package: python3*:arm64\nPin: release *\nPin-Priority: -1\n' > /etc/apt/preferences.d/block-arm64-python && \
    # Add ARM64 package sources from Ubuntu ports (plucky = 25.04)
    echo "deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports plucky main universe" > /etc/apt/sources.list.d/arm64-cross.list && \
    echo "deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports plucky-updates main universe" >> /etc/apt/sources.list.d/arm64-cross.list && \
    echo "deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports plucky-security main universe" >> /etc/apt/sources.list.d/arm64-cross.list && \
    apt-get update && \
    # Install ARM64 GStreamer development libraries
    apt-get install -y --no-install-recommends \
        libssl-dev:arm64 \
        libglib2.0-dev:arm64 \
        libgstreamer1.0-dev:arm64 \
        libgstreamer-plugins-base1.0-dev:arm64 \
        libgstreamer-plugins-bad1.0-dev:arm64 && \
    rm -rf /var/lib/apt/lists/*; \
else \
    echo "==> Native build for $TARGETPLATFORM - Installing native GStreamer libs"; \
    apt-get update && apt-get install -y \
        libssl-dev \
        libgstreamer1.0-dev \
        libgstreamer-plugins-base1.0-dev \
        libgstreamer-plugins-bad1.0-dev && \
    rm -rf /var/lib/apt/lists/*; \
fi

# Copy entire project source
COPY . .

# Copy the built frontend dist from frontend-builder
# Note: Trunk.toml puts output in ../backend/dist relative to frontend/
COPY --from=frontend-builder /app/backend/dist backend/dist

# Build the backend (headless - no native GUI needed in Docker) and MCP server
ENV RUST_BACKTRACE=1

# Cross-compilation: Use cargo-zigbuild with glibc 2.36 targeting (Raspberry Pi compatible)
# Native compilation: Use regular cargo build
RUN if [ "$BUILDPLATFORM" != "$TARGETPLATFORM" ] && [ "$TARGETARCH" = "arm64" ]; then \
    echo "==> Cross-compiling backend with Zig (targeting glibc 2.36)"; \
    export PKG_CONFIG_ALLOW_CROSS=1 && \
    export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig && \
    export AARCH64_UNKNOWN_LINUX_GNU_OPENSSL_LIB_DIR=/usr/lib/aarch64-linux-gnu && \
    export CFLAGS="-I/usr/include -I/usr/include/aarch64-linux-gnu" && \
    export RUSTFLAGS="-L /usr/lib/aarch64-linux-gnu" && \
    cargo zigbuild --release --package strom --features no-gui --target aarch64-unknown-linux-gnu.2.36 && \
    cargo zigbuild --release --package strom-mcp-server --target aarch64-unknown-linux-gnu.2.36 && \
    # Move binaries to expected location (cargo-zigbuild puts them in target/aarch64-unknown-linux-gnu/release)
    mkdir -p target/release && \
    cp target/aarch64-unknown-linux-gnu/release/strom target/release/strom && \
    cp target/aarch64-unknown-linux-gnu/release/strom-mcp-server target/release/strom-mcp-server; \
else \
    echo "==> Native build for $TARGETPLATFORM"; \
    cargo build --release --package strom --features no-gui && \
    cargo build --release --package strom-mcp-server; \
fi

# Stage 3: Runtime - Minimal runtime image with Ubuntu 25.04 (plucky) for GStreamer 1.26
FROM ubuntu:plucky AS runtime
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
