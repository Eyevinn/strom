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

# gstcefsrc version (pinned to known-good version)
ARG GSTCEFSRC_VERSION=144.0.21
ARG TARGETARCH

ENV DEBIAN_FRONTEND=noninteractive

# Install xvfb for headless X11 and download gstcefsrc
RUN --mount=type=bind,source=install-gstcefsrc.sh,target=/tmp/install-gstcefsrc.sh \
    apt-get update && apt-get install -y \
    curl \
    xvfb \
    && GSTCEFSRC_VERSION="${GSTCEFSRC_VERSION}" TARGETARCH="${TARGETARCH}" sh /tmp/install-gstcefsrc.sh \
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
