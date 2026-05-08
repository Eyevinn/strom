# Reproducible build environment for patched GStreamer plugins.
#
# Debian bookworm has gstreamer 1.22.x dev packages out of the box, which
# is the ABI we pin to (see ../gstreamer-version.txt). Plugins built here
# load in any GStreamer 1.22+ runtime via forward ABI compat.
#
# The image is multi-arch via docker buildx + QEMU. TARGETARCH is set by
# buildx to "amd64" or "arm64" and is consumed by the build steps.

FROM debian:bookworm AS builder

ARG GSTREAMER_VERSION=1.22.12
ARG TARGETARCH

RUN apt-get update && apt-get install --no-install-recommends -y \
        build-essential \
        ca-certificates \
        git \
        ninja-build \
        pkg-config \
        python3 \
        python3-pip \
        libgstreamer1.0-dev \
        libgstreamer-plugins-base1.0-dev \
    && rm -rf /var/lib/apt/lists/*

# Bookworm ships meson 1.0; we need >= 1.4 for the gst-plugins-bad meson.build.
RUN pip3 install --break-system-packages --no-cache-dir 'meson==1.5.2'

WORKDIR /work

# Pinned source tree from upstream GStreamer monorepo. We only need the
# gst-plugins-bad subdirectory plus its meson wraps; cloning the full
# monorepo at the tag is the simplest reproducible path.
RUN git clone --depth 1 --branch ${GSTREAMER_VERSION} \
        https://gitlab.freedesktop.org/gstreamer/gstreamer.git src \
    && cd src \
    && git rev-parse HEAD > /work/gstreamer-rev.txt

COPY patches/ /work/patches/

# Apply every patch in lexical order. Patches must be in unified diff
# format relative to the GStreamer monorepo root.
RUN cd src && for p in $(ls /work/patches/*.patch | sort); do \
        echo "Applying $p" && \
        git apply --check "$p" && git apply "$p" \
    ; done

# Standalone gst-plugins-bad build (linked against system gstreamer 1.22),
# decklink only. Auto-features=disabled keeps the build small.
RUN cd src/subprojects/gst-plugins-bad \
    && meson setup /work/build . \
        --buildtype=release \
        --auto-features=disabled \
        -Ddecklink=enabled \
        -Dtests=disabled \
        -Dexamples=disabled \
        -Dnls=disabled \
        -Dintrospection=disabled \
        -Ddoc=disabled \
        -Dorc=disabled \
        -Dgpl=enabled \
    && meson compile -C /work/build

# Final stage: scratch image whose only role is to expose the built
# artifacts to `docker buildx build --output`.
FROM scratch AS export
COPY --from=builder /work/build/sys/decklink/libgstdecklink.so /libgstdecklink.so
COPY --from=builder /work/gstreamer-rev.txt /gstreamer-rev.txt
