#!/bin/sh
# Install the prebuilt gstcefsrc plugin (cefsrc) into /usr/local.
#
# Shared by the strom-full image and the CI job that tests against a real
# cefsrc, so both get the same layout. The version is the GSTCEFSRC_VERSION
# ARG in docker/strom-full/Dockerfile; CI reads it from there.
#
# Usage:
#   GSTCEFSRC_VERSION=144.0.21 install-gstcefsrc.sh            # stream the download
#   GSTCEFSRC_VERSION=144.0.21 install-gstcefsrc.sh FILE.tar.gz  # extract FILE, downloading it first if absent
#
# TARGETARCH (amd64/arm64) defaults to the host's Debian architecture.
#
# After installing, the environment needs:
#   GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0
#   LD_LIBRARY_PATH=/usr/local/lib/cef
#   GST_CEF_SUBPROCESS_PATH=/usr/local/lib/gstreamer-1.0/gstcefsubprocess
set -eu

: "${GSTCEFSRC_VERSION:?GSTCEFSRC_VERSION must be set}"
arch="${TARGETARCH:-$(dpkg --print-architecture)}"
url="https://github.com/Eyevinn/strom/releases/download/gstcefsrc-deps/gstcefsrc-${GSTCEFSRC_VERSION}-linux-${arch}.tar.gz"
prefix=/usr/local

if [ $# -eq 0 ]; then
    curl -L "$url" | tar -xz -C "$prefix/"
else
    if [ ! -s "$1" ]; then
        curl -L --fail --retry 3 --retry-all-errors -o "$1" "$url"
    fi
    tar -xz -C "$prefix/" -f "$1"
fi

# CEF plugin expects resources relative to the plugin directory
# Create symlinks so cefsrc can find locales, subprocess, and resource files
ln -sf "$prefix/lib/cef/locales" "$prefix/lib/gstreamer-1.0/locales"
ln -sf "$prefix/lib/cef/gstcefsubprocess" "$prefix/lib/gstreamer-1.0/gstcefsubprocess"
ln -sf "$prefix"/lib/cef/*.pak "$prefix/lib/gstreamer-1.0/"
ln -sf "$prefix"/lib/cef/*.dat "$prefix/lib/gstreamer-1.0/"
ln -sf "$prefix"/lib/cef/*.bin "$prefix/lib/gstreamer-1.0/"
