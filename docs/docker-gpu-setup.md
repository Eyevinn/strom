# Docker GPU Setup (NVIDIA)

## Prerequisites

- NVIDIA GPU drivers installed (on Windows host for WSL2)
- Verify GPU access: `nvidia-smi`

## Install NVIDIA Container Toolkit

```bash
# Install prerequisites
sudo apt-get update && sudo apt-get install -y curl gnupg2

# Add GPG key and repository
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey | sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg \
  && curl -s -L https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list | \
    sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' | \
    sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list

# Install
sudo apt-get update
sudo apt-get install -y nvidia-container-toolkit

# Configure Docker
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
```

## Usage

```bash
# Run with all GPUs
docker run --gpus all <image>

# Run with specific number of GPUs
docker run --gpus 2 <image>

# Run with specific GPU by ID
docker run --gpus '"device=0,1"' <image>
```

## Example: Strom with GPU

```bash
docker run --gpus all -p 8080:8080 eyevinntechnology/strom:0.2.8
```

## Verify

```bash
docker run --rm --gpus all nvidia/cuda:12.0-base nvidia-smi
```
