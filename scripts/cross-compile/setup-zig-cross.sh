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

    # Download Zig — prefer community mirror (faster), fall back to upstream
    ZIG_VERSION="0.13.0"
    ZIG_TARBALL="zig-linux-${ZIG_ARCH}-${ZIG_VERSION}.tar.xz"
    ZIG_PRIMARY="https://zigmirror.hryx.net/zig/${ZIG_TARBALL}"
    ZIG_FALLBACK="https://ziglang.org/download/${ZIG_VERSION}/${ZIG_TARBALL}"
    if [ "$ZIG_ARCH" = "x86_64" ]; then
        ZIG_SHA256="d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea"
    else
        ZIG_SHA256="041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556"
    fi

    echo "Downloading Zig ${ZIG_VERSION} for ${ZIG_ARCH}..."
    curl -L --fail --retry 3 --retry-all-errors --retry-delay 2 \
        "$ZIG_PRIMARY" -o "/tmp/${ZIG_TARBALL}" || \
    curl -L --fail --retry 5 --retry-all-errors --retry-delay 3 \
        "$ZIG_FALLBACK" -o "/tmp/${ZIG_TARBALL}"
    echo "${ZIG_SHA256}  /tmp/${ZIG_TARBALL}" | sha256sum -c

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

# 2. Install cargo-zigbuild — use prebuilt binary (avoids ~10 min compile)
echo "Installing cargo-zigbuild..."
if command -v cargo-zigbuild &> /dev/null; then
    echo "✓ cargo-zigbuild already installed"
else
    CZB_VERSION="0.22.3"
    CZB_TARGET="${ZIG_ARCH}-unknown-linux-gnu"
    if [ "$ZIG_ARCH" = "x86_64" ]; then
        CZB_SHA256="6a014d41ba41ca4b69ca4c4819b9f78a41b0197b5d486904e31c1244e3686190"
    else
        CZB_SHA256="6f86a78cf8be222ac08a68a944ffd8a1ef9d455c504097f0ffbd8bcfbe434a55"
    fi
    CZB_URL="https://github.com/rust-cross/cargo-zigbuild/releases/download/v${CZB_VERSION}/cargo-zigbuild-${CZB_TARGET}.tar.xz"
    curl -L --fail --retry 5 --retry-all-errors --retry-delay 3 \
        "$CZB_URL" -o /tmp/cargo-zigbuild.tar.xz
    echo "${CZB_SHA256}  /tmp/cargo-zigbuild.tar.xz" | sha256sum -c
    tar -xf /tmp/cargo-zigbuild.tar.xz -C /tmp
    mkdir -p ~/.cargo/bin
    install -m 0755 "/tmp/cargo-zigbuild-${CZB_TARGET}/cargo-zigbuild" ~/.cargo/bin/cargo-zigbuild
    rm -rf /tmp/cargo-zigbuild.tar.xz "/tmp/cargo-zigbuild-${CZB_TARGET}"
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
