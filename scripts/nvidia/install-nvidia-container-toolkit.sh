#!/bin/bash
set -e

echo "=== Installing NVIDIA Container Toolkit ==="

# Add NVIDIA GPG key
echo "Adding NVIDIA GPG key..."
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey | \
  sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg

# Add NVIDIA repository
echo "Adding NVIDIA repository..."
curl -s -L https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list | \
  sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' | \
  sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list

# Update and install
echo "Installing nvidia-container-toolkit..."
sudo apt update
sudo apt install -y nvidia-container-toolkit

# Configure Docker runtime
echo "Configuring Docker runtime..."
sudo nvidia-ctk runtime configure --runtime=docker

# Restart Docker
echo "Restarting Docker..."
sudo systemctl restart docker

# Verify installation
echo "=== Verifying installation ==="
echo "Testing nvidia-smi in Docker..."
#docker run --rm --gpus all nvidia/cuda:12.0.0-base-ubuntu22.04 nvidia-smi
docker run --rm --gpus all ubuntu nvidia-smi

echo "=== Done! ==="
