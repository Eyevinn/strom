#!/bin/sh
# Install the prebuilt gstcefsrc plugin (cefsrc) into /usr/local.
#
# Shared by the strom-full image and the CI job that tests against a real
# cefsrc, so both get the same layout. The version and the per-architecture
# hashes are the GSTCEFSRC_* ARGs in docker/strom-full/Dockerfile; CI reads
# them from there.
#
# Usage:
#   GSTCEFSRC_VERSION=... GSTCEFSRC_SHA256=... install-gstcefsrc.sh            # download to a temporary file
#   GSTCEFSRC_VERSION=... GSTCEFSRC_SHA256=... install-gstcefsrc.sh FILE.tar.gz  # extract FILE, downloading it first if absent
#
# The archive is checked against GSTCEFSRC_SHA256 before it is extracted,
# including a FILE that was already there.
#
# TARGETARCH (amd64/arm64) defaults to the host's Debian architecture.
#
# After installing, the environment needs:
#   GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0
#   LD_LIBRARY_PATH=/usr/local/lib/cef
#   GST_CEF_SUBPROCESS_PATH=/usr/local/lib/gstreamer-1.0/gstcefsubprocess
set -eu

: "${GSTCEFSRC_VERSION:?GSTCEFSRC_VERSION must be set}"
: "${GSTCEFSRC_SHA256:?GSTCEFSRC_SHA256 must be set}"
arch="${TARGETARCH:-$(dpkg --print-architecture)}"
url="https://github.com/Eyevinn/strom/releases/download/gstcefsrc-deps/gstcefsrc-${GSTCEFSRC_VERSION}-linux-${arch}.tar.gz"
prefix=/usr/local

if [ $# -eq 0 ]; then
    archive=/tmp/gstcefsrc.tar.gz
    keep=no
else
    archive="$1"
    keep=yes
fi

if [ ! -s "$archive" ]; then
    curl -fL --retry 3 --retry-all-errors -o "$archive" "$url"
fi
echo "${GSTCEFSRC_SHA256}  ${archive}" | sha256sum -c -
tar -xz -C "$prefix/" -f "$archive"
if [ "$keep" = no ]; then
    rm "$archive"
fi

# CEF plugin expects resources relative to the plugin directory
# Create symlinks so cefsrc can find locales, subprocess, and resource files
ln -sf "$prefix/lib/cef/locales" "$prefix/lib/gstreamer-1.0/locales"
ln -sf "$prefix/lib/cef/gstcefsubprocess" "$prefix/lib/gstreamer-1.0/gstcefsubprocess"
ln -sf "$prefix"/lib/cef/*.pak "$prefix/lib/gstreamer-1.0/"
ln -sf "$prefix"/lib/cef/*.dat "$prefix/lib/gstreamer-1.0/"
ln -sf "$prefix"/lib/cef/*.bin "$prefix/lib/gstreamer-1.0/"
