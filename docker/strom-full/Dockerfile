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
# (docker/gstcefsrc: popup-close, isolated-context and strict-network). Each archive is pinned
# by sha256, so a rebuild published under this name cannot reach an image
# unnoticed. Changing the version means changing both hashes with it; the
# build workflow prints them, and the release lists them as asset digests.
ARG GSTCEFSRC_VERSION=144.0.21-strom2
ARG GSTCEFSRC_SHA256_AMD64=ad10ea6f3e3ef566c56636d00d8f0edbd9e202fcb8206aa617b8a483451d7eda
ARG GSTCEFSRC_SHA256_ARM64=810a4c4da4f1a829b0e68accd7d5f763eef61163eea2beed787754ecdf719294
ARG TARGETARCH

ENV DEBIAN_FRONTEND=noninteractive

# Install xvfb for headless X11 and download gstcefsrc
RUN apt-get update && apt-get install -y \
    curl \
    xvfb \
    && case "${TARGETARCH}" in \
         amd64) GSTCEFSRC_SHA256="${GSTCEFSRC_SHA256_AMD64}" ;; \
         arm64) GSTCEFSRC_SHA256="${GSTCEFSRC_SHA256_ARM64}" ;; \
         *) echo "No gstcefsrc build for ${TARGETARCH}" >&2; exit 1 ;; \
       esac \
    && curl -fL -o /tmp/gstcefsrc.tar.gz "https://github.com/Eyevinn/strom/releases/download/gstcefsrc-deps/gstcefsrc-${GSTCEFSRC_VERSION}-linux-${TARGETARCH}.tar.gz" \
    && echo "${GSTCEFSRC_SHA256}  /tmp/gstcefsrc.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/gstcefsrc.tar.gz -C /usr/local/ \
    && rm /tmp/gstcefsrc.tar.gz \
    && apt-get remove -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/* \
    # CEF plugin expects resources relative to the plugin directory
    # Create symlinks so cefsrc can find locales, subprocess, and resource files
    && ln -sf /usr/local/lib/cef/locales /usr/local/lib/gstreamer-1.0/locales \
    && ln -sf /usr/local/lib/cef/gstcefsubprocess /usr/local/lib/gstreamer-1.0/gstcefsubprocess \
    && ln -sf /usr/local/lib/cef/*.pak /usr/local/lib/gstreamer-1.0/ \
    && ln -sf /usr/local/lib/cef/*.dat /usr/local/lib/gstreamer-1.0/ \
    && ln -sf /usr/local/lib/cef/*.bin /usr/local/lib/gstreamer-1.0/

# CEF environment variables
ENV GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0:$GST_PLUGIN_PATH
ENV LD_LIBRARY_PATH=/usr/local/lib/cef:$LD_LIBRARY_PATH
ENV GST_CEF_SUBPROCESS_PATH=/usr/local/lib/gstreamer-1.0/gstcefsubprocess

# Copy entrypoint script that auto-starts Xvfb for headless CEF rendering
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
CMD ["/app/strom"]
