#!/bin/bash
# Verify DeckLink installation and Docker compatibility
# Run this on the host to check if DeckLink is ready for Docker use

set -e

echo "=== DeckLink Installation Verification ==="
echo

# Check for DeckLink devices
echo "1. Checking for DeckLink device nodes..."
if [ -d "/dev/blackmagic" ]; then
    echo "   ✓ /dev/blackmagic exists"
    ls -la /dev/blackmagic/
else
    echo "   ✗ /dev/blackmagic not found"
    echo "   Install Desktop Video software and ensure cards are detected"
    exit 1
fi
echo

# Check for DeckLink API libraries
echo "2. Checking for DeckLink API libraries..."
if [ -f "/usr/lib/libDeckLinkAPI.so" ]; then
    echo "   ✓ /usr/lib/libDeckLinkAPI.so exists"
else
    echo "   ✗ libDeckLinkAPI.so not found"
    echo "   Install Desktop Video software from Blackmagic Design"
    exit 1
fi

if [ -f "/usr/lib/libDeckLinkPreviewAPI.so" ]; then
    echo "   ✓ /usr/lib/libDeckLinkPreviewAPI.so exists"
else
    echo "   ⚠ libDeckLinkPreviewAPI.so not found (may be optional)"
fi
echo

# Check for blackmagic support directory
echo "3. Checking for Blackmagic support files..."
if [ -d "/usr/lib/blackmagic" ]; then
    echo "   ✓ /usr/lib/blackmagic exists"
else
    echo "   ⚠ /usr/lib/blackmagic not found (may not be required)"
fi
echo

# Check for Desktop Video utilities
echo "4. Checking for Desktop Video utilities..."
if command -v BlackmagicFirmwareUpdater &> /dev/null; then
    echo "   ✓ BlackmagicFirmwareUpdater found"
    echo "   Firmware status:"
    BlackmagicFirmwareUpdater status 2>/dev/null || echo "   (unable to query firmware status)"
else
    echo "   ⚠ BlackmagicFirmwareUpdater not found"
fi
echo

# Summary
echo "=== Docker Run Command ==="
echo
echo "Use these options to run Strom with DeckLink support:"
echo
echo "docker run -d \\"
echo "  --privileged \\"
echo "  -v /dev/blackmagic:/dev/blackmagic \\"
echo "  -v /usr/lib/libDeckLinkAPI.so:/lib/libDeckLinkAPI.so:ro \\"
echo "  -v /usr/lib/libDeckLinkPreviewAPI.so:/lib/libDeckLinkPreviewAPI.so:ro \\"
echo "  -v /usr/lib/blackmagic:/lib/blackmagic:ro \\"
echo "  -p 8080:8080 \\"
echo "  --name strom \\"
echo "  eyevinntechnology/strom:latest"
echo
echo "=== Verification Complete ==="
