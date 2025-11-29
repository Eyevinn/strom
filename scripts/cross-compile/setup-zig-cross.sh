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
rustup target add aarch64-unknown-linux-musl
echo "✓ Rust ARM64 targets added"

# 4. Install minimal dependencies for GStreamer pkg-config
# We only need pkg-config and the .pc files, not the full cross-compilation toolchain!
echo "Installing pkg-config (for GStreamer detection)..."
sudo apt-get update
sudo apt-get install -y pkg-config

# 5. For GStreamer: We need the ARM64 development packages for pkg-config
#    BUT we use a simpler approach - just copy the .pc files we need
echo ""
echo "Note: For GStreamer cross-compilation, you have two options:"
echo ""
echo "Option 1 (Simple): Build in Docker with ARM64 GStreamer installed"
echo "  This is recommended for production builds"
echo ""
echo "Option 2 (Local): Install ARM64 GStreamer packages via multi-arch"
echo "  Run ./setup-arm64-cross.sh first to set up multi-arch apt"
echo "  Then Zig will use the ARM64 libraries for linking"
echo ""

echo "✓ Zig-based cross-compilation setup complete!"
echo ""
echo "To build for ARM64 with specific glibc version:"
echo "  ./scripts/cross-compile/build-zig-arm64.sh [glibc_version]"
echo ""
echo "Examples:"
echo "  ./scripts/cross-compile/build-zig-arm64.sh 2.36  # For Raspberry Pi OS 12"
echo "  ./scripts/cross-compile/build-zig-arm64.sh 2.31  # For older Debian/Ubuntu"
echo "  ./scripts/cross-compile/build-zig-arm64.sh 2.17  # Maximum compatibility"
echo ""
