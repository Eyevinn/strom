#!/bin/bash
# Build Strom for ARM64 using Zig cross-compilation with specific glibc version targeting

set -e

# Default to glibc 2.36 (Raspberry Pi OS 12 / Debian 12 Bookworm)
GLIBC_VERSION="${1:-2.36}"

echo "Building Strom for ARM64 with Zig (targeting glibc ${GLIBC_VERSION})..."

# Verify zig and cargo-zigbuild are installed
if ! command -v zig &> /dev/null; then
    echo "Error: zig not found. Run ./scripts/cross-compile/setup-zig-cross.sh first"
    exit 1
fi

if ! command -v cargo-zigbuild &> /dev/null; then
    echo "Error: cargo-zigbuild not found. Run ./scripts/cross-compile/setup-zig-cross.sh first"
    exit 1
fi

# Check if we need GStreamer libraries for cross-compilation
# Option 1: Multi-arch setup with ARM64 libraries
if [ -d "/usr/lib/aarch64-linux-gnu/pkgconfig" ]; then
    echo "✓ Found ARM64 GStreamer libraries (multi-arch setup)"
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
    # Tell OpenSSL build where to find headers and libraries
    export AARCH64_UNKNOWN_LINUX_GNU_OPENSSL_LIB_DIR=/usr/lib/aarch64-linux-gnu
    # Need both include paths: generic headers + arch-specific headers
    export CFLAGS="-I/usr/include -I/usr/include/aarch64-linux-gnu"
    # Tell linker where to find ARM64 libraries
    export RUSTFLAGS="-L /usr/lib/aarch64-linux-gnu"
else
    echo ""
    echo "Error: ARM64 GStreamer libraries not found."
    echo ""
    echo "Run the ARM64 setup script first:"
    echo "  ./scripts/cross-compile/setup-arm64-cross.sh"
    echo ""
    echo "This installs ARM64 GStreamer libraries needed for pkg-config."
    echo ""
    exit 1
fi

# Build frontend first (WASM - architecture independent)
echo "Building frontend (WASM)..."
cd frontend
trunk build --release
cd ..

# Build backend for ARM64 with specific glibc version
# The magic: appending .X.XX to the target tells Zig which glibc to target!
TARGET="aarch64-unknown-linux-gnu.${GLIBC_VERSION}"

echo "Building backend for ARM64 (target: ${TARGET})..."
cargo zigbuild --release --package strom --target "$TARGET"

echo "Building MCP server for ARM64 (target: ${TARGET})..."
cargo zigbuild --release --package strom-mcp-server --target "$TARGET"

# Binaries go to the standard target directory (without the glibc version suffix)
OUTPUT_DIR="target/aarch64-unknown-linux-gnu/release"

echo ""
echo "✓ Build complete!"
echo ""
echo "Binaries location (dynamically linked with glibc ${GLIBC_VERSION}):"
echo "  Backend:    ${OUTPUT_DIR}/strom"
echo "  MCP Server: ${OUTPUT_DIR}/strom-mcp-server"
echo ""
echo "These binaries will run on any ARM64 Linux system with glibc ${GLIBC_VERSION} or newer."
echo ""
echo "Common glibc versions:"
echo "  2.17 - CentOS 7, Amazon Linux 2 (maximum compatibility)"
echo "  2.28 - Ubuntu 18.04 LTS"
echo "  2.31 - Ubuntu 20.04 LTS, Debian 11 Bullseye"
echo "  2.36 - Ubuntu 22.04 LTS, Debian 12 Bookworm, Raspberry Pi OS 12"
echo "  2.38 - Ubuntu 24.04 LTS"
echo ""
echo "To verify glibc on target system:"
echo "  ldd --version"
echo ""
echo "Copy to target ARM64 system with:"
echo "  scp ${OUTPUT_DIR}/strom user@host:~/"
echo ""
