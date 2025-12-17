#!/bin/bash
set -e

echo "=== Installing NVIDIA Driver ==="

# Install ubuntu-drivers utility
echo "Installing ubuntu-drivers-common..."
sudo apt update
sudo apt install -y ubuntu-drivers-common

# Show detected GPUs and recommended drivers
echo ""
echo "=== Detected GPU(s) ==="
ubuntu-drivers devices

# Install recommended driver
echo ""
echo "Installing recommended driver..."
sudo ubuntu-drivers autoinstall

echo ""
echo "=== Driver installed ==="
echo "A reboot is required for the driver to load."
echo ""
read -p "Reboot now? [y/N] " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    sudo reboot
else
    echo "Run 'sudo reboot' when ready, then verify with 'nvidia-smi'"
fi
