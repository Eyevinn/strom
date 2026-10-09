# Dockerfile for strom-full - Extended image with HTML overlay support (gstcefsrc/CEF)
#
# This image extends the base strom image with:
# - gstcefsrc GStreamer plugin (Chromium Embedded Framework)
# - xvfb for headless X11 rendering
#
# CEF adds ~400MB compressed (~800MB uncompressed) to the image, hence the separate "full" variant.
#
# Usage:
#   docker run -p 8080:8080 eyevinntechnology/strom-full:latest

ARG VERSION=latest
FROM eyevinntechnology/strom:${VERSION}

# gstcefsrc build: the CEF version, plus the suffix of Strom's patched build
# (docker/gstcefsrc: popup-close, isolated-context and strict-network). Each
# archive is pinned
# by sha256, so a rebuild published under this name cannot reach an image
# unnoticed. Changing the version means changing both hashes with it; the
# build workflow prints them, and the release lists them as asset digests.
ARG GSTCEFSRC_VERSION=144.0.21-strom5
ARG GSTCEFSRC_SHA256_AMD64=f369134420ad51c6e85c6b3a75818f3eb004bb8c2002f1f1e830ee774c275bbc
ARG GSTCEFSRC_SHA256_ARM64=d97c0f19bc6a7ebcd5b3a3dc009366c29a20e264937780deab62de3b412b051b
ARG TARGETARCH

ENV DEBIAN_FRONTEND=noninteractive

# Install xvfb for headless X11 and download gstcefsrc
RUN --mount=type=bind,source=install-gstcefsrc.sh,target=/tmp/install-gstcefsrc.sh \
    apt-get update && apt-get install -y \
    curl \
    xvfb \
    && case "${TARGETARCH}" in \
         amd64) GSTCEFSRC_SHA256="${GSTCEFSRC_SHA256_AMD64}" ;; \
         arm64) GSTCEFSRC_SHA256="${GSTCEFSRC_SHA256_ARM64}" ;; \
         *) echo "No gstcefsrc build for ${TARGETARCH}" >&2; exit 1 ;; \
       esac \
    && GSTCEFSRC_VERSION="${GSTCEFSRC_VERSION}" GSTCEFSRC_SHA256="${GSTCEFSRC_SHA256}" \
       TARGETARCH="${TARGETARCH}" sh /tmp/install-gstcefsrc.sh \
    && apt-get remove -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# CEF environment variables
ENV GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0:$GST_PLUGIN_PATH
ENV LD_LIBRARY_PATH=/usr/local/lib/cef:$LD_LIBRARY_PATH
ENV GST_CEF_SUBPROCESS_PATH=/usr/local/lib/gstreamer-1.0/gstcefsubprocess

# Copy entrypoint script that auto-starts Xvfb for headless CEF rendering, and
# the CEF runtime settings it shares with CI
COPY cef-env.sh /usr/local/lib/strom/cef-env.sh
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
CMD ["/app/strom"]
