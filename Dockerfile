# Dockerfile for Strom - Ubuntu 25.10 (Questing)-based multi-stage build with Zig cross-compilation

# Stage 1: Frontend builder - Build WASM frontend on native platform
# IMPORTANT: Always build on native platform - WASM output is platform-independent!
FROM --platform=$BUILDPLATFORM ubuntu:questing AS frontend-builder
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

# Install sccache (compiler cache backed by the self-hosted MinIO S3 bucket).
# It only activates when AWS credentials are mounted as build secrets (see the
# build step below); a plain `docker build` without secrets compiles normally.
ARG SCCACHE_VERSION=0.15.0
RUN ARCH="$(uname -m)" && \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /tmp && \
    install -m0755 "/tmp/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl/sccache" /usr/local/bin/sccache && \
    rm -rf /tmp/sccache-*
ENV SCCACHE_BUCKET=sccache \
    SCCACHE_ENDPOINT=https://olivedev-rustcache.minio-minio.auto.prod-se.osaas.io \
    SCCACHE_REGION=us-east-1 \
    SCCACHE_S3_USE_SSL=true

# Copy workspace
COPY . .

# Build frontend (WASM is platform-independent).
# sccache is enabled only when the credential secrets are present AND the cache
# backend is reachable. sccache validates its bucket on server start (fast
# non-zero exit if the bucket is missing or the endpoint is unreachable), so we
# probe with `sccache --start-server` and wrap rustc only on success. A broken
# cache degrades to an uncached build instead of failing the image build.
RUN --mount=type=secret,id=aws_access_key_id \
    --mount=type=secret,id=aws_secret_access_key \
    if [ -s /run/secrets/aws_access_key_id ]; then \
        export AWS_ACCESS_KEY_ID="$(cat /run/secrets/aws_access_key_id)" && \
        export AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/aws_secret_access_key)" && \
        if timeout 30 sccache --start-server >/dev/null 2>&1; then \
            export RUSTC_WRAPPER=sccache; \
        else \
            echo "==> sccache backend unreachable - building without compile cache"; \
        fi; \
    fi && \
    cd frontend && trunk build --release && \
    { command -v sccache >/dev/null && [ -n "$RUSTC_WRAPPER" ] && sccache --show-stats || true; }

# Stage 2: Backend builder - Build backend with Zig cross-compilation support
# IMPORTANT: Must run on BUILD platform for cross-compilation tools (Zig) to work
FROM --platform=$BUILDPLATFORM ubuntu:questing AS backend-builder
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
    cmake \
    libclang-dev \
    curl \
    xz-utils \
    build-essential \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

# Install sccache (compiler cache backed by the self-hosted MinIO S3 bucket).
# Activates only when AWS credentials are mounted as build secrets in the build
# step below; a plain `docker build` without secrets compiles normally.
# When cross-compiling, this stage runs on the native BUILDPLATFORM, so the
# host-arch sccache binary is the correct one for the rustc that actually runs.
ARG SCCACHE_VERSION=0.15.0
RUN ARCH="$(uname -m)" && \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /tmp && \
    install -m0755 "/tmp/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl/sccache" /usr/local/bin/sccache && \
    rm -rf /tmp/sccache-*
ENV SCCACHE_BUCKET=sccache \
    SCCACHE_ENDPOINT=https://olivedev-rustcache.minio-minio.auto.prod-se.osaas.io \
    SCCACHE_REGION=us-east-1 \
    SCCACHE_S3_USE_SSL=true

# Cross-compilation setup: Install Zig and ARM64 libraries when cross-compiling
# Uses Ubuntu ports.ubuntu.com for ARM64 packages (matching local setup)
RUN if [ "$BUILDPLATFORM" != "$TARGETPLATFORM" ] && [ "$TARGETARCH" = "arm64" ]; then \
    echo "==> Cross-compiling from $BUILDPLATFORM to $TARGETPLATFORM - Setting up Zig"; \
    # Install Zig for cross-compilation
    ZIG_VERSION="0.13.0" && \
    ZIG_ARCH=$(case ${BUILDARCH} in amd64) echo "x86_64" ;; arm64) echo "aarch64" ;; esac) && \
    ZIG_SHA256=$(case ${ZIG_ARCH} in \
        x86_64) echo "d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea" ;; \
        aarch64) echo "041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556" ;; \
    esac) && \
    CZB_VERSION="0.22.3" && \
    CZB_SHA256=$(case ${ZIG_ARCH} in \
        x86_64) echo "6a014d41ba41ca4b69ca4c4819b9f78a41b0197b5d486904e31c1244e3686190" ;; \
        aarch64) echo "6f86a78cf8be222ac08a68a944ffd8a1ef9d455c504097f0ffbd8bcfbe434a55" ;; \
    esac) && \
    ZIG_TARBALL="zig-linux-${ZIG_ARCH}-${ZIG_VERSION}.tar.xz" && \
    # Prefer community mirror (faster, not throttled); fall back to upstream
    (curl -L --fail --retry 3 --retry-all-errors --retry-delay 2 \
        "https://zigmirror.hryx.net/zig/${ZIG_TARBALL}" \
        -o "/tmp/${ZIG_TARBALL}" || \
     curl -L --fail --retry 5 --retry-all-errors --retry-delay 3 \
        "https://ziglang.org/download/${ZIG_VERSION}/${ZIG_TARBALL}" \
        -o "/tmp/${ZIG_TARBALL}") && \
    echo "${ZIG_SHA256}  /tmp/${ZIG_TARBALL}" | sha256sum -c && \
    tar -xf "/tmp/${ZIG_TARBALL}" -C /usr/local && \
    mv /usr/local/zig-linux-${ZIG_ARCH}-${ZIG_VERSION} /usr/local/zig && \
    ln -s /usr/local/zig/zig /usr/local/bin/zig && \
    rm "/tmp/${ZIG_TARBALL}" && \
    # Install cargo-zigbuild from prebuilt binary (avoids ~10 min compile)
    CZB_TARGET="${ZIG_ARCH}-unknown-linux-gnu" && \
    curl -L --fail --retry 5 --retry-all-errors --retry-delay 3 \
        "https://github.com/rust-cross/cargo-zigbuild/releases/download/v${CZB_VERSION}/cargo-zigbuild-${CZB_TARGET}.tar.xz" \
        -o "/tmp/cargo-zigbuild.tar.xz" && \
    echo "${CZB_SHA256}  /tmp/cargo-zigbuild.tar.xz" | sha256sum -c && \
    tar -xf /tmp/cargo-zigbuild.tar.xz -C /tmp && \
    install -m 0755 "/tmp/cargo-zigbuild-${CZB_TARGET}/cargo-zigbuild" /root/.cargo/bin/cargo-zigbuild && \
    rm -rf /tmp/cargo-zigbuild.tar.xz "/tmp/cargo-zigbuild-${CZB_TARGET}" && \
    # Add Rust ARM64 target
    rustup target add aarch64-unknown-linux-gnu && \
    # Setup multi-arch for ARM64 (matching setup-arm64-cross.sh)
    dpkg --add-architecture arm64 && \
    # Update ubuntu.sources to specify amd64 architecture (archive.ubuntu.com doesn't have arm64)
    sed -i '/^Types: deb$/a Architectures: amd64' /etc/apt/sources.list.d/ubuntu.sources && \
    # Block ARM64 Python packages (critical - prevents apt from removing amd64 Python!)
    mkdir -p /etc/apt/preferences.d && \
    printf 'Package: python3*:arm64\nPin: release *\nPin-Priority: -1\n' > /etc/apt/preferences.d/block-arm64-python && \
    # Add ARM64 package sources from Ubuntu ports (questing = 25.10)
    echo "deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports questing main universe" > /etc/apt/sources.list.d/arm64-cross.list && \
    echo "deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports questing-updates main universe" >> /etc/apt/sources.list.d/arm64-cross.list && \
    echo "deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports questing-security main universe" >> /etc/apt/sources.list.d/arm64-cross.list && \
    apt-get update && \
    # Install ARM64 GStreamer development libraries
    apt-get install -y --no-install-recommends \
        libssl-dev:arm64 \
        libcairo2-dev:arm64 \
        libglib2.0-dev:arm64 \
        libgstreamer1.0-dev:arm64 \
        libgstreamer-plugins-base1.0-dev:arm64 \
        libgstreamer-plugins-bad1.0-dev:arm64 \
        cmake \
        libclang-dev && \
    rm -rf /var/lib/apt/lists/*; \
else \
    echo "==> Native build for $TARGETPLATFORM - Installing native GStreamer libs"; \
    apt-get update && apt-get install -y \
        libssl-dev \
        libcairo2-dev \
        libgstreamer1.0-dev \
        libgstreamer-plugins-base1.0-dev \
        libgstreamer-plugins-bad1.0-dev \
        cmake \
        libclang-dev && \
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
# sccache: see the frontend stage — probe the backend and only wrap rustc when
# the cache is healthy, so a broken cache never fails the build.
RUN --mount=type=secret,id=aws_access_key_id \
    --mount=type=secret,id=aws_secret_access_key \
    if [ -s /run/secrets/aws_access_key_id ]; then \
        export AWS_ACCESS_KEY_ID="$(cat /run/secrets/aws_access_key_id)" && \
        export AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/aws_secret_access_key)" && \
        if timeout 30 sccache --start-server >/dev/null 2>&1; then \
            export RUSTC_WRAPPER=sccache; \
        else \
            echo "==> sccache backend unreachable - building without compile cache"; \
        fi; \
    fi && \
    if [ "$BUILDPLATFORM" != "$TARGETPLATFORM" ] && [ "$TARGETARCH" = "arm64" ]; then \
    echo "==> Cross-compiling backend with Zig (targeting glibc 2.36)"; \
    export PKG_CONFIG_ALLOW_CROSS=1 && \
    export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig && \
    export AARCH64_UNKNOWN_LINUX_GNU_OPENSSL_LIB_DIR=/usr/lib/aarch64-linux-gnu && \
    # Use C17 standard to avoid glibc 2.38+ C23 symbols (__isoc23_sscanf, __isoc23_strtol) \
    # that aws-lc-sys would otherwise use when built on Ubuntu 25.10 \
    # CFLAGS/CXXFLAGS for regular builds, CMAKE_C_FLAGS/CMAKE_CXX_FLAGS for CMake (aws-lc-sys) \
    export CFLAGS="-std=gnu17 -I/usr/include -I/usr/include/aarch64-linux-gnu" && \
    export CXXFLAGS="-std=gnu++17" && \
    export CMAKE_C_FLAGS="-std=gnu17" && \
    export CMAKE_CXX_FLAGS="-std=gnu++17" && \
    export RUSTFLAGS="-L /usr/lib/aarch64-linux-gnu" && \
    cargo zigbuild --release --package strom --no-default-features --features no-gui,efp --target aarch64-unknown-linux-gnu.2.36 && \
    cargo zigbuild --release --package strom-mcp-server --target aarch64-unknown-linux-gnu.2.36 && \
    # Move binaries to expected location (cargo-zigbuild puts them in target/aarch64-unknown-linux-gnu/release)
    mkdir -p target/release && \
    cp target/aarch64-unknown-linux-gnu/release/strom target/release/strom && \
    cp target/aarch64-unknown-linux-gnu/release/strom-mcp-server target/release/strom-mcp-server; \
else \
    echo "==> Native build for $TARGETPLATFORM"; \
    cargo build --release --package strom --features no-gui,efp && \
    cargo build --release --package strom-mcp-server; \
fi && \
    { command -v sccache >/dev/null && [ -n "$RUSTC_WRAPPER" ] && sccache --show-stats || true; }

# Stage 3: Runtime - Minimal runtime image with Ubuntu 25.10 (questing) for GStreamer 1.26
FROM ubuntu:questing AS runtime
WORKDIR /app

# TARGETARCH is set automatically by docker buildx (e.g. amd64, arm64).
# We use it to fetch the right architecture of our patched GStreamer plugins.
ARG TARGETARCH

# Install GStreamer runtime dependencies
# Note: Ubuntu Plucky (25.04) reached EOL before the nvcodec fix (Bug #2109413) was released.
# Ubuntu Questing (25.10) includes the fix in gstreamer1.0-plugins-bad 1.26.3+.
#
# IMPORTANT: gstreamer1.0-gl and EGL/GBM libraries are required for CUDA-GL interop:
# - gstreamer1.0-gl: GL plugins (glupload, gldownload, glcolorconvert)
# - libegl1, libegl-mesa0, libgbm1: Headless EGL/GBM rendering (no X11 required)
# - libgl1-mesa-dri: Mesa DRI drivers for software fallback
# The nvidia-container-toolkit mounts NVIDIA GL libraries at runtime via --gpus all
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
        libgstreamer1.0-0 \
        libgstreamer-plugins-base1.0-0 \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        libgstreamer-plugins-bad1.0-0 \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-ugly \
        gstreamer1.0-libav \
        gstreamer1.0-nice \
        gstreamer1.0-tools \
        gstreamer1.0-gl \
        libegl1 \
        libegl-mesa0 \
        libgbm1 \
        libgl1-mesa-dri \
        graphviz \
        ca-certificates \
        dbus \
        avahi-daemon \
    && rm -rf /var/lib/apt/lists/*

# Override the distro-shipped decklink plugin with our patched build, which
# adds the `capture-group` property to decklinkvideosrc for synchronized
# capture group support (see tools/patched-gstreamer-plugins/README.md).
# The patched .so is forward-ABI-compatible with the runtime gstreamer 1.26
# in this image because it was built against the 1.22 ABI.
ARG PATCHED_PLUGINS_TAG=patched-plugins-v1.0-gst1.22.12
ARG PATCHED_PLUGINS_REPO=Eyevinn/strom
RUN apt-get update && apt-get install -y --no-install-recommends curl \
    && curl -fsSL \
        "https://github.com/${PATCHED_PLUGINS_REPO}/releases/download/${PATCHED_PLUGINS_TAG}/libgstdecklink-linux-${TARGETARCH}.so" \
        -o "/usr/lib/$(uname -m)-linux-gnu/gstreamer-1.0/libgstdecklink.so" \
    && apt-get remove -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binaries from backend-builder to /app
COPY --from=backend-builder /app/target/release/strom /app/strom
COPY --from=backend-builder /app/target/release/strom-mcp-server /app/strom-mcp-server

# Copy setup scripts for optional host/container configuration (NDI, NVIDIA, etc.)
COPY scripts/setup /app/scripts/setup

# Set environment variables
ENV RUST_LOG=info
ENV STROM_PORT=8080
ENV STROM_DATA_DIR=/data

# Enable all NVIDIA driver capabilities (needed for NVENC/NVDEC video encoding/decoding)
ENV NVIDIA_DRIVER_CAPABILITIES=all

# Headless GStreamer GL configuration for CUDA-GL interop
# GST_GL_WINDOW=egl-device: Use EGL device extension - direct GPU access without display server
# GST_GL_PLATFORM=egl: Use EGL platform (not GLX which requires X11)
# This enables true zero-copy CUDA-GL interop for glupload/gldownload/glcolorconvert
# The nvidia-container-toolkit mounts NVIDIA's EGL libraries at runtime via --gpus all
ENV GST_GL_WINDOW=egl-device
ENV GST_GL_PLATFORM=egl

# Add NVIDIA EGL vendor config (nvidia-container-toolkit mounts libEGL_nvidia.so but not the ICD file)
# This tells libglvnd to use NVIDIA's EGL implementation for CUDA-GL interop
RUN mkdir -p /usr/share/glvnd/egl_vendor.d && \
    echo '{"file_format_version":"1.0.0","ICD":{"library_path":"libEGL_nvidia.so.0"}}' \
    > /usr/share/glvnd/egl_vendor.d/10_nvidia.json

# Create data directory for persistent storage
RUN mkdir -p /data

# Copy entrypoint script that starts dbus/avahi for NDI discovery
COPY docker/strom/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Expose the server port
EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
CMD ["/app/strom"]
