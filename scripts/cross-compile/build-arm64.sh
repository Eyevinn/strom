#!/bin/bash
# Build Strom for ARM64 using glibc (dynamic linking)

set -e

echo "Building Strom for ARM64 with glibc (dynamic linking)..."

# Set environment variables for cross-compilation
export PKG_CONFIG_SYSROOT_DIR=/usr/aarch64-linux-gnu
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

# Build frontend first (WASM - architecture independent)
echo "Building frontend (WASM)..."
cd frontend
trunk build --release
cd ..

# Build backend for ARM64
echo "Building backend for ARM64..."
cargo build --release --package strom --target aarch64-unknown-linux-gnu

# Build MCP server for ARM64
echo "Building MCP server for ARM64..."
cargo build --release --package strom-mcp-server --target aarch64-unknown-linux-gnu

echo ""
echo "✓ Build complete!"
echo ""
echo "Binaries location (dynamically linked with glibc):"
echo "  Backend:    target/aarch64-unknown-linux-gnu/release/strom"
echo "  MCP Server: target/aarch64-unknown-linux-gnu/release/strom-mcp-server"
echo ""
echo "NOTE: These binaries use your build system's glibc version."
echo "If you get 'version GLIBC_X.XX not found' errors on the target, use Zig instead:"
echo "  ./build-zig-arm64.sh 2.36  # Target specific glibc version"
echo ""
echo "Copy to target ARM64 system with:"
echo "  scp target/aarch64-unknown-linux-gnu/release/strom user@host:~/"
echo ""
