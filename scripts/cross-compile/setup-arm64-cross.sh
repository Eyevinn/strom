#!/bin/bash
# Safe ARM64 cross-compilation setup for Strom
# Updates ubuntu.sources to use amd64 only, adds ARM64 sources separately

set -e

echo "Setting up ARM64 cross-compilation for Strom..."

# 1. Add ARM64 architecture
echo "Adding arm64 architecture..."
sudo dpkg --add-architecture arm64

# 2. Backup and update ubuntu.sources to specify amd64 architecture
echo "Updating ubuntu.sources to specify amd64 architecture..."
UBUNTU_SOURCES="/etc/apt/sources.list.d/ubuntu.sources"
BACKUP_DIR="/var/backups/strom-cross-compile"
if [ -f "$UBUNTU_SOURCES" ]; then
    # Check if already has Architectures line
    if ! grep -q "^Architectures:" "$UBUNTU_SOURCES"; then
        sudo mkdir -p "$BACKUP_DIR"
        sudo cp "$UBUNTU_SOURCES" "$BACKUP_DIR/ubuntu.sources.backup-$(date +%Y%m%d-%H%M%S)"
        # Add "Architectures: amd64" after each "Types: deb" line
        sudo sed -i '/^Types: deb$/a Architectures: amd64' "$UBUNTU_SOURCES"
        echo "  Backup saved to: $BACKUP_DIR/"
    else
        echo "  Already configured (skipping)"
    fi
fi

# Clean up any old backup files in sources.list.d to avoid warnings
sudo rm -f /etc/apt/sources.list.d/*.backup-* 2>/dev/null || true

# 3. Block ARM64 Python packages (the key fix we discovered!)
if [ ! -f /etc/apt/preferences.d/block-arm64-python ]; then
    echo "Blocking ARM64 Python packages..."
    sudo sh -c 'cat > /etc/apt/preferences.d/block-arm64-python << "PREFEOF"
Package: python3*:arm64
Pin: release *
Pin-Priority: -1
PREFEOF'
else
    echo "ARM64 Python already blocked (skipping)"
fi

# 4. Add ARM64 package sources (ports.ubuntu.com hosts ARM packages)
echo "Adding ARM64 package sources..."
cat <<EOF | sudo tee /etc/apt/sources.list.d/arm64-cross.list
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble main universe
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble-updates main universe
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble-security main universe
EOF

# 5. Update package lists
echo "Updating package lists..."
sudo apt-get update

# 6. Install cross-compilation toolchain (amd64 versions)
echo "Installing cross-compilation toolchain..."
sudo apt-get install -y \
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu \
    pkg-config

# 7. Install ARM64 development libraries (only libraries, no binaries)
echo "Installing ARM64 GStreamer libraries..."
# Install only library packages that don't have executables/scripts
sudo apt-get install -y --no-install-recommends \
    libssl-dev:arm64 \
    libglib2.0-dev:arm64 \
    libgstreamer1.0-dev:arm64 \
    libgstreamer-plugins-base1.0-dev:arm64 \
    libgstreamer-plugins-bad1.0-dev:arm64

# 8. Add ARM64 Rust target
echo "Adding Rust ARM64 target..."
rustup target add aarch64-unknown-linux-gnu

# 9. Create .cargo/config.toml with linker configuration
echo "Creating .cargo/config.toml..."
mkdir -p .cargo
cat > .cargo/config.toml << 'EOF'
# ARM64 cross-compilation configuration
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"

[env]
PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu = "/usr/aarch64-linux-gnu"
PKG_CONFIG_PATH_aarch64_unknown_linux_gnu = "/usr/lib/aarch64-linux-gnu/pkgconfig"
EOF

echo ""
echo "✓ ARM64 libraries and toolchain installed!"
echo ""
echo "You can now build for ARM64 using:"
echo ""
echo "Zig-based (Recommended - target specific glibc versions):"
echo "  ./build-zig-arm64.sh 2.36   # For Raspberry Pi OS 12"
echo "  ./build-zig-arm64.sh 2.17   # Maximum compatibility"
echo ""
echo "Traditional (uses build system's glibc version):"
echo "  ./build-arm64.sh"
echo ""
