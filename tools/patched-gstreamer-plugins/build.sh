#!/usr/bin/env bash
#
# Build patched GStreamer plugins for linux/amd64 and linux/arm64.
#
# Run this once when patches change. Output:
#   dist/linux_amd64/libgstdecklink.so
#   dist/linux_arm64/libgstdecklink.so
#   dist/linux_amd64/gstreamer-rev.txt
#   dist/linux_arm64/gstreamer-rev.txt
#
# Then upload the dist/ files to a GitHub release in this repo, tagged
# `patched-plugins-vX.Y-gst<gst-version>`. The strom Dockerfile pulls
# the right .so for its target architecture from that release URL.
#
# Requires: docker with buildx + QEMU (for cross-arch emulation).

set -euo pipefail

cd "$(dirname "$0")"

GSTREAMER_VERSION="$(cat gstreamer-version.txt)"
PLATFORMS="linux/amd64,linux/arm64"
BUILDER_NAME="patched-gstreamer-plugins-builder"

echo "== Patched GStreamer plugins build =="
echo "Pinned GStreamer source: ${GSTREAMER_VERSION}"
echo "Target platforms:        ${PLATFORMS}"
echo

# Ensure buildx + QEMU multi-arch emulation are available. Idempotent.
if ! docker buildx inspect "${BUILDER_NAME}" >/dev/null 2>&1; then
    echo "== Creating buildx builder '${BUILDER_NAME}' =="
    docker buildx create --name "${BUILDER_NAME}" --use
    docker run --privileged --rm tonistiigi/binfmt --install all >/dev/null
else
    docker buildx use "${BUILDER_NAME}"
fi

rm -rf dist
mkdir -p dist

echo "== Building =="
docker buildx build \
    --platform "${PLATFORMS}" \
    --build-arg "GSTREAMER_VERSION=${GSTREAMER_VERSION}" \
    --target export \
    --output "type=local,dest=dist" \
    .

echo
echo "== Done =="
find dist -type f -printf '%p (%s bytes)\n'
echo
echo "Next: upload these files to a GitHub release in this repo,"
echo "      e.g. tagged patched-plugins-v1.0-gst${GSTREAMER_VERSION}"
