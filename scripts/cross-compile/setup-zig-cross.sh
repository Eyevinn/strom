#!/bin/bash
# Setup Zig-based cross-compilation for Strom
# Much simpler than traditional cross-compilation - no multi-arch apt complexity!

set -e

echo "Setting up Zig-based cross-compilation for Strom..."

# 1. Check if zig is already installed
if command -v zig &> /dev/null; then
    ZIG_VERSION=$(zig version)
    echo "✓ Zig already installed: $ZIG_VERSION"
else
    echo "Installing Zig..."

    # Detect architecture
    ARCH=$(uname -m)
    if [ "$ARCH" = "x86_64" ]; then
        ZIG_ARCH="x86_64"
    elif [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
        ZIG_ARCH="aarch64"
    else
        echo "Error: Unsupported architecture: $ARCH"
        exit 1
    fi

    # Download latest Zig (or specify a version)
    ZIG_VERSION="0.13.0"
    ZIG_TARBALL="zig-linux-${ZIG_ARCH}-${ZIG_VERSION}.tar.xz"
    ZIG_URL="https://ziglang.org/download/${ZIG_VERSION}/${ZIG_TARBALL}"

    echo "Downloading Zig ${ZIG_VERSION} for ${ZIG_ARCH}..."
    curl -L "$ZIG_URL" -o "/tmp/${ZIG_TARBALL}"

    echo "Extracting to ~/.local/zig..."
    mkdir -p ~/.local
    tar -xf "/tmp/${ZIG_TARBALL}" -C ~/.local
    mv ~/.local/zig-linux-${ZIG_ARCH}-${ZIG_VERSION} ~/.local/zig

    # Add to PATH if not already there
    if ! grep -q '~/.local/zig' ~/.bashrc; then
        echo 'export PATH="$HOME/.local/zig:$PATH"' >> ~/.bashrc
        echo "Added Zig to PATH in ~/.bashrc"
    fi

    export PATH="$HOME/.local/zig:$PATH"

    echo "✓ Zig installed: $(zig version)"
fi

# 2. Install cargo-zigbuild
echo "Installing cargo-zigbuild..."
if command -v cargo-zigbuild &> /dev/null; then
    echo "✓ cargo-zigbuild already installed"
else
    cargo install --locked cargo-zigbuild
    echo "✓ cargo-zigbuild installed"
fi

# 3. Add Rust ARM64 target (still needed for rustc)
echo "Adding Rust ARM64 target..."
rustup target add aarch64-unknown-linux-gnu
echo "✓ Rust ARM64 target added"

echo ""
echo "✓ Zig and cargo-zigbuild installed!"
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "IMPORTANT: You must also install ARM64 GStreamer libraries!"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Zig provides cross-compilation, but still needs ARM64 libraries"
echo "for pkg-config to find GStreamer dependencies."
echo ""
echo "Run this next:"
echo "  ./scripts/cross-compile/setup-arm64-cross.sh"
echo ""
echo "Then you can build with:"
echo "  ./scripts/cross-compile/build-zig-arm64.sh 2.36  # Raspberry Pi OS 12"
echo "  ./scripts/cross-compile/build-zig-arm64.sh 2.31  # Older Debian/Ubuntu"
echo "  ./scripts/cross-compile/build-zig-arm64.sh 2.17  # Maximum compatibility"
echo ""
