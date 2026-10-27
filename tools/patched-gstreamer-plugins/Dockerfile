# Reproducible build environment for patched GStreamer plugins.
#
# One plugin per build. build.sh passes the plugin's row from plugins.conf:
# the base image, the GStreamer source tag, and the plugin's directory in
# gst-plugins-bad. The base image provides the GStreamer dev packages the
# plugin links against, so it sets the oldest runtime the plugin loads in
# (GStreamer keeps forward ABI compat within 1.x).
#
# build.sh builds both architectures in one `docker buildx build`, with
# QEMU emulating the non-native one.

ARG BASE_IMAGE=debian:bookworm
FROM ${BASE_IMAGE} AS builder

ARG GSTREAMER_VERSION
ARG PLUGIN
ARG PLUGIN_DIR

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
# Pinned on every base so the build does not change with the distro's meson.
RUN pip3 install --break-system-packages --no-cache-dir 'meson==1.5.2'

WORKDIR /work

# Pinned source tree from upstream GStreamer monorepo. We only need the
# gst-plugins-bad subdirectory plus its meson wraps; cloning the full
# monorepo at the tag is the simplest reproducible path.
RUN git clone --depth 1 --branch ${GSTREAMER_VERSION} \
        https://gitlab.freedesktop.org/gstreamer/gstreamer.git src \
    && cd src \
    && git rev-parse HEAD > /work/gstreamer-rev.txt

COPY patches/${PLUGIN}/ /work/patches/

# Apply the plugin's patches in lexical order. Patches must be in unified
# diff format relative to the GStreamer monorepo root.
RUN cd src && for p in $(ls /work/patches/*.patch | sort); do \
        echo "Applying $p" && \
        git apply --check "$p" && git apply "$p" \
    ; done

# Standalone gst-plugins-bad build (linked against the base image's system
# gstreamer), this plugin only. Auto-features=disabled keeps the build small.
RUN cd src/subprojects/gst-plugins-bad \
    && meson setup /work/build . \
        --buildtype=release \
        --auto-features=disabled \
        -D${PLUGIN}=enabled \
        -Dtests=disabled \
        -Dexamples=disabled \
        -Dnls=disabled \
        -Dintrospection=disabled \
        -Ddoc=disabled \
        -Dorc=disabled \
        -Dgpl=enabled \
    && meson compile -C /work/build \
    && mkdir -p /out \
    && cp /work/build/${PLUGIN_DIR}/libgst${PLUGIN}.so /work/gstreamer-rev.txt /out/

# Final stage: scratch image whose only role is to expose the built
# artifacts to `docker buildx build --output`.
FROM scratch AS export
COPY --from=builder /out/ /
