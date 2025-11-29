#!/bin/bash
# Cleanup ARM64 cross-compilation setup
# Reverts system changes made by setup-arm64-cross.sh

set -e

echo "Cleaning up ARM64 cross-compilation setup..."

# 1. Remove Python blocking
if [ -f /etc/apt/preferences.d/block-arm64-python ]; then
    echo "Removing Python blocking..."
    sudo rm /etc/apt/preferences.d/block-arm64-python
fi

# 2. Remove ARM64 sources
if [ -f /etc/apt/sources.list.d/arm64-cross.list ]; then
    echo "Removing ARM64 sources..."
    sudo rm /etc/apt/sources.list.d/arm64-cross.list
fi

# 3. Restore ubuntu.sources (if backup exists)
BACKUP_DIR="/var/backups/strom-cross-compile"
if [ -d "$BACKUP_DIR" ]; then
    LATEST_BACKUP=$(ls -t "$BACKUP_DIR"/ubuntu.sources.backup-* 2>/dev/null | head -1)
    if [ -n "$LATEST_BACKUP" ]; then
        echo "Restoring ubuntu.sources from backup..."
        sudo cp "$LATEST_BACKUP" /etc/apt/sources.list.d/ubuntu.sources
    fi
fi

# 4. Remove ARM64 architecture (optional - may fail if ARM64 packages installed)
read -p "Remove arm64 architecture? This will fail if ARM64 packages are installed (y/N): " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    echo "Removing arm64 architecture..."
    sudo dpkg --remove-architecture arm64 || echo "Warning: Could not remove arm64 architecture (packages still installed)"
fi

# 5. Optionally remove ARM64 packages
read -p "Remove ARM64 development packages? (y/N): " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    echo "Removing ARM64 packages..."
    sudo apt-get purge -y \
        'libgstreamer*:arm64' \
        'libglib*:arm64' \
        'libssl*:arm64' 2>/dev/null || true
    sudo apt-get autoremove -y
fi

# 6. Update apt
echo "Updating package lists..."
sudo apt-get update

echo ""
echo "✓ Cleanup complete!"
echo ""
echo "Note: Rust ARM64 target and .cargo/config.toml were NOT removed."
echo "To remove Rust target: rustup target remove aarch64-unknown-linux-gnu"
echo "To remove cargo config: rm .cargo/config.toml"
echo ""
