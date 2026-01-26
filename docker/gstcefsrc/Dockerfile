# Dockerfile for building gstcefsrc - GStreamer CEF (Chromium Embedded Framework) plugin
#
# This builds the cefsrc/cefdemux/cefbin GStreamer elements for HTML overlay rendering.
# Supports both amd64 and arm64 architectures.
#
# Uses Ubuntu 25.10 (Questing) to match the strom base image. This ensures the CEF
# binaries are compatible with the same glibc and system libraries.
#
# Usage:
#   docker build --platform linux/amd64 -t gstcefsrc-builder:amd64 .
#   docker build --platform linux/arm64 -t gstcefsrc-builder:arm64 .
#
# Extract built plugin:
#   docker run --rm -v $(pwd)/output:/export gstcefsrc-builder:amd64

ARG TARGETARCH

# Latest stable CEF version (as of 2026-01)
ARG CEF_VERSION=144.0.11+ge135be2+chromium-144.0.7559.97

FROM ubuntu:questing AS builder

ARG TARGETARCH
ARG CEF_VERSION

ENV DEBIAN_FRONTEND=noninteractive

# Install build dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    ninja-build \
    pkg-config \
    git \
    curl \
    ca-certificates \
    # GStreamer development packages
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    # CEF dependencies
    libgtk-3-dev \
    libnss3-dev \
    libnspr4-dev \
    libasound2-dev \
    libxcomposite-dev \
    libxdamage-dev \
    libxrandr-dev \
    libxkbcommon-dev \
    libatk1.0-dev \
    libatk-bridge2.0-dev \
    libcups2-dev \
    libdrm-dev \
    libgbm-dev \
    libpango1.0-dev \
    libcairo2-dev \
    libx11-xcb-dev \
    libxcb-dri3-dev \
    # X11 for runtime (needed for CEF)
    xvfb \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Clone gstcefsrc from Centricular
RUN git clone --depth 1 https://github.com/centricular/gstcefsrc.git

WORKDIR /build/gstcefsrc

# Build gstcefsrc
# CEF binaries are automatically downloaded from Spotify's CDN during cmake configure
# The CMake script automatically detects the architecture
RUN mkdir build && cd build && \
    cmake -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DCEF_VERSION="${CEF_VERSION}" \
        .. && \
    ninja

# Create output directory with the built plugin and CEF resources
# The output structure:
#   /output/
#     lib/gstreamer-1.0/libgstcef.so  - The GStreamer plugin
#     lib/cef/                         - CEF runtime files (subprocess, resources, libs)
#     version.txt                      - Build metadata
RUN mkdir -p /output/lib/gstreamer-1.0 && \
    mkdir -p /output/lib/cef && \
    # Copy the GStreamer plugin
    cp /build/gstcefsrc/build/Release/libgstcef.so /output/lib/gstreamer-1.0/ && \
    # Copy the CEF subprocess helper (required at runtime)
    cp /build/gstcefsrc/build/Release/gstcefsubprocess /output/lib/cef/ && \
    # Copy CEF resources (pak files, locales, etc.)
    cp /build/gstcefsrc/build/Release/*.pak /output/lib/cef/ 2>/dev/null || true && \
    cp /build/gstcefsrc/build/Release/*.dat /output/lib/cef/ 2>/dev/null || true && \
    cp /build/gstcefsrc/build/Release/*.bin /output/lib/cef/ 2>/dev/null || true && \
    cp -r /build/gstcefsrc/build/Release/locales /output/lib/cef/ 2>/dev/null || true && \
    # Copy CEF shared libraries
    cp /build/gstcefsrc/build/Release/libcef.so /output/lib/cef/ 2>/dev/null || true && \
    cp /build/gstcefsrc/build/Release/libEGL.so /output/lib/cef/ 2>/dev/null || true && \
    cp /build/gstcefsrc/build/Release/libGLESv2.so /output/lib/cef/ 2>/dev/null || true && \
    cp /build/gstcefsrc/build/Release/libvk_swiftshader.so /output/lib/cef/ 2>/dev/null || true && \
    cp /build/gstcefsrc/build/Release/libvulkan.so.1 /output/lib/cef/ 2>/dev/null || true && \
    cp /build/gstcefsrc/build/Release/vk_swiftshader_icd.json /output/lib/cef/ 2>/dev/null || true && \
    # Create version info file
    echo "CEF_VERSION=${CEF_VERSION}" > /output/version.txt && \
    echo "TARGETARCH=${TARGETARCH}" >> /output/version.txt && \
    echo "BUILD_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> /output/version.txt && \
    # List what was built
    echo "=== Built files ===" && \
    find /output -type f -exec ls -lh {} \;

# Final stage - minimal image for artifact extraction
FROM ubuntu:questing AS export

COPY --from=builder /output /output

# Default command copies files to mounted volume and fixes ownership
# Uses the UID/GID of the target directory to set ownership
CMD ["/bin/sh", "-c", "cp -r /output/. /export/ && chown -R $(stat -c '%u:%g' /export) /export/"]
