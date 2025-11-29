#!/bin/bash
# Fix the broken ARM64 python package

set -e

echo "Fixing broken ARM64 Python package..."

# Force remove the broken ARM64 python packages
sudo dpkg --remove --force-remove-reinstreq \
    python3.12-minimal:arm64 \
    python3-minimal:arm64 \
    python3:arm64 2>/dev/null || true

# Mark python packages to only use amd64 architecture
echo "Configuring apt to prefer amd64 for Python..."
cat <<EOF | sudo tee /etc/apt/preferences.d/prefer-amd64-python
Package: python3*
Pin: release a=*
Pin-Priority: -1

Package: python3*:amd64
Pin: release a=*
Pin-Priority: 500
EOF

# Clean up
sudo apt-get -f install -y
sudo apt-get autoremove -y

echo "✓ Fixed! Python will now stay as amd64."
