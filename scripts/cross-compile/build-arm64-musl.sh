#!/bin/bash
# Build Strom for ARM64 using musl (static linking - portable)

set -e

echo "Building Strom for ARM64 with musl (static, portable binary)..."

# Set environment variables for cross-compilation with musl
# Point to ARM64 glibc libraries (musl will statically link, but GStreamer pkg-config files are there)
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR=/usr/aarch64-linux-gnu
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc

# Build frontend first (WASM - architecture independent)
echo "Building frontend (WASM)..."
cd frontend
trunk build --release
cd ..

# Build backend for ARM64 with musl
echo "Building backend for ARM64 (musl - static)..."
cargo build --release --package strom --target aarch64-unknown-linux-musl

# Build MCP server for ARM64 with musl
echo "Building MCP server for ARM64 (musl - static)..."
cargo build --release --package strom-mcp-server --target aarch64-unknown-linux-musl

echo ""
echo "✓ Build complete!"
echo ""
echo "Binaries location (statically linked with musl):"
echo "  Backend:    target/aarch64-unknown-linux-musl/release/strom"
echo "  MCP Server: target/aarch64-unknown-linux-musl/release/strom-mcp-server"
echo ""
echo "These binaries are fully static and will run on ANY ARM64 Linux system,"
echo "regardless of glibc version (works on Alpine, Debian, Ubuntu, etc.)"
echo ""
echo "Copy to target ARM64 system with:"
echo "  scp target/aarch64-unknown-linux-musl/release/strom user@host:~/"
echo ""
