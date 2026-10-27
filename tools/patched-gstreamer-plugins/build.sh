#!/usr/bin/env bash
#
# Build patched GStreamer plugins for linux/amd64 and linux/arm64.
#
# Usage: ./build.sh [plugin...]   (default: every plugin in plugins.conf)
#
# Run this once when patches change. Output, per plugin:
#   dist/<plugin>/linux_amd64/libgst<plugin>.so
#   dist/<plugin>/linux_arm64/libgst<plugin>.so
#   dist/<plugin>/linux_<arch>/gstreamer-rev.txt
#
# Then upload the dist/ files to a GitHub release in this repo (see
# README.md for the tag convention). The strom Dockerfile pulls the right
# .so for its target architecture from that release URL.
#
# Requires: docker with buildx + QEMU (for cross-arch emulation).

set -euo pipefail

cd "$(dirname "$0")"

PLATFORMS="linux/amd64,linux/arm64"
BUILDER_NAME="patched-gstreamer-plugins-builder"
WANTED=("$@")

# Ensure buildx + QEMU multi-arch emulation are available. Idempotent.
if ! docker buildx inspect "${BUILDER_NAME}" >/dev/null 2>&1; then
    echo "== Creating buildx builder '${BUILDER_NAME}' =="
    docker buildx create --name "${BUILDER_NAME}" --use
    docker run --privileged --rm tonistiigi/binfmt --install all >/dev/null
else
    docker buildx use "${BUILDER_NAME}"
fi

while read -r plugin base_image gst_version plugin_dir; do
    case "$plugin" in ''|'#'*) continue ;; esac
    if [ ${#WANTED[@]} -gt 0 ] && [[ ! " ${WANTED[*]} " =~ " ${plugin} " ]]; then
        continue
    fi

    echo "== ${plugin}: GStreamer ${gst_version} on ${base_image} =="
    rm -rf "dist/${plugin}"

    docker buildx build \
        --platform "${PLATFORMS}" \
        --build-arg "BASE_IMAGE=${base_image}" \
        --build-arg "GSTREAMER_VERSION=${gst_version}" \
        --build-arg "PLUGIN=${plugin}" \
        --build-arg "PLUGIN_DIR=${plugin_dir}" \
        --target export \
        --output "type=local,dest=dist/${plugin}" \
        .
done < plugins.conf

echo
echo "== Done =="
find dist -type f -exec ls -l {} \;
echo
echo "sha256 (for the strom Dockerfile):"
find dist -name '*.so' -exec shasum -a 256 {} \;
