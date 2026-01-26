#!/usr/bin/env bash
#
# Install NDI SDK for Linux
#
# This script downloads and installs the NDI SDK v6 from the official source.
# The SDK provides the core NDI runtime libraries needed for NDI streaming.
#

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${BLUE}==>${NC} $1"
}

log_success() {
    echo -e "${GREEN}==>${NC} $1"
}

log_error() {
    echo -e "${RED}Error:${NC} $1"
    exit 1
}

log_warning() {
    echo -e "${YELLOW}==>${NC} $1"
}

# Configuration
NDI_SDK_VERSION="${NDI_SDK_VERSION:-6}"
DOWNLOAD_DIR="${DOWNLOAD_DIR:-/tmp}"
INSTALL_PREFIX="${INSTALL_PREFIX:-/usr}"

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        LIB_DIR="x86_64-linux-gnu"
        ;;
    aarch64|arm64)
        LIB_DIR="aarch64-linux-gnu"
        ;;
    *)
        log_error "Unsupported architecture: $ARCH"
        ;;
esac

log_info "NDI SDK Installer for Linux"
echo ""
log_info "Architecture: $ARCH"
log_info "SDK Version: $NDI_SDK_VERSION"
log_info "Install prefix: $INSTALL_PREFIX"
echo ""

# Check if already installed
NDI_EXISTING=$(ldconfig -p | grep libndi.so || true)
if [ -n "$NDI_EXISTING" ]; then
    log_warning "NDI SDK appears to be already installed:"
    echo "$NDI_EXISTING"
    echo ""
    read -p "Do you want to reinstall? [y/N] " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        log_info "Skipping installation"
        exit 0
    fi
fi

# Install dependencies
log_info "Installing dependencies..."
if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update
    sudo apt-get install -y wget libavahi-common3 libavahi-client3
else
    log_warning "apt-get not found. Please ensure libavahi-common3 and libavahi-client3 are installed."
fi

# Download NDI SDK
cd "$DOWNLOAD_DIR"
NDI_SDK_ARCHIVE="Install_NDI_SDK_v${NDI_SDK_VERSION}_Linux.tar.gz"
NDI_SDK_URL="https://downloads.ndi.tv/SDK/NDI_SDK_Linux/$NDI_SDK_ARCHIVE"

log_info "Downloading NDI SDK from: $NDI_SDK_URL"

if [ -f "$NDI_SDK_ARCHIVE" ]; then
    log_warning "Archive already exists, skipping download"
else
    if ! wget -O "$NDI_SDK_ARCHIVE" "$NDI_SDK_URL"; then
        log_error "Failed to download NDI SDK. Please check the URL or download manually from https://ndi.video/for-developers/ndi-sdk/download/"
    fi
fi

# Extract archive
log_info "Extracting archive..."
tar -xzf "$NDI_SDK_ARCHIVE"

# NDI SDK v6 extracts to a single installer script
INSTALLER_SCRIPT="Install_NDI_SDK_v${NDI_SDK_VERSION}_Linux.sh"

if [ -f "$INSTALLER_SCRIPT" ]; then
    log_info "Found NDI SDK installer: $INSTALLER_SCRIPT"
    chmod +x "$INSTALLER_SCRIPT"

    # The installer script extracts files to a directory
    log_info "Running NDI SDK installer..."
    log_warning "You will be asked to accept the NDI SDK license agreement"
    echo ""
    "./$INSTALLER_SCRIPT" || true

    # The installer creates a directory
    SDK_DIR="NDI SDK for Linux"
    if [ ! -d "$SDK_DIR" ]; then
        log_error "SDK extraction failed - directory not found: $SDK_DIR"
    fi

    log_info "Found extracted SDK at: $SDK_DIR"

    # Install libraries
    log_info "Installing libraries to $INSTALL_PREFIX/lib/$LIB_DIR/..."
    if [ -d "$SDK_DIR/lib/$LIB_DIR" ]; then
        # Find the actual library file (version may vary)
        NDI_LIB=$(find "$SDK_DIR/lib/$LIB_DIR" -name "libndi.so.6.*" -type f | head -1)
        if [ -z "$NDI_LIB" ]; then
            log_error "Could not find libndi.so.6.* in $SDK_DIR/lib/$LIB_DIR"
        fi
        NDI_LIB_NAME=$(basename "$NDI_LIB")
        log_info "Found library: $NDI_LIB_NAME"

        # Copy the actual library file
        sudo cp -v "$NDI_LIB" "$INSTALL_PREFIX/lib/$LIB_DIR/"

        # Create symlinks
        sudo ln -sf "$NDI_LIB_NAME" "$INSTALL_PREFIX/lib/$LIB_DIR/libndi.so.6"
        sudo ln -sf libndi.so.6 "$INSTALL_PREFIX/lib/$LIB_DIR/libndi.so"

        log_success "Libraries installed"
    else
        log_error "Library directory not found: $SDK_DIR/lib/$LIB_DIR"
    fi

    # Install headers (optional but useful)
    if [ -d "$SDK_DIR/include" ]; then
        log_info "Installing headers to $INSTALL_PREFIX/include/ndi/..."
        sudo mkdir -p "$INSTALL_PREFIX/include/ndi"
        sudo cp -v "$SDK_DIR/include/"* "$INSTALL_PREFIX/include/ndi/"
        log_success "Headers installed"
    fi

elif [ -f "ndi-sdk-installer.sh" ]; then
    # Fallback for older SDK versions
    log_info "Found alternative installer: ndi-sdk-installer.sh"
    chmod +x ndi-sdk-installer.sh
    sudo ./ndi-sdk-installer.sh

else
    log_error "Could not find NDI SDK installer script. Archive structure may have changed."
    log_info "Contents of extracted archive:"
    ls -la
    exit 1
fi

# Update library cache
log_info "Updating library cache..."
sudo ldconfig

# Verify installation
echo ""
log_info "Verifying installation..."
NDI_CHECK=$(ldconfig -p | grep libndi.so || true)
if [ -n "$NDI_CHECK" ]; then
    log_success "NDI SDK installed successfully!"
    echo ""
    echo "Installed libraries:"
    echo "$NDI_CHECK"
else
    log_error "Installation verification failed. NDI library not found in ldconfig cache."
fi

# Cleanup option
echo ""
read -p "Do you want to remove the downloaded files? [Y/n] " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Nn]$ ]]; then
    log_info "Cleaning up..."
    rm -f "$DOWNLOAD_DIR/$NDI_SDK_ARCHIVE" "$DOWNLOAD_DIR/$INSTALLER_SCRIPT"
    rm -rf "$DOWNLOAD_DIR/NDI SDK for Linux"
    log_success "Cleanup complete"
fi

echo ""
log_success "NDI SDK installation complete!"
log_info "Next step: Run 2-install-gstreamer-ndi-plugin.sh"
