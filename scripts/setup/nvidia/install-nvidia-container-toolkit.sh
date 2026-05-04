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
echo "Installing nvidia-container-toolkit and jq..."
sudo apt update
sudo apt install -y nvidia-container-toolkit jq

# Configure Docker runtime
echo "Configuring Docker runtime..."
sudo nvidia-ctk runtime configure --runtime=docker

# Apply NVML cgroup-reload workaround.
#
# Without this, containers lose GPU access ("Failed to initialize NVML:
# Unknown Error") after any `systemctl daemon-reload` on the host because
# Docker's default systemd cgroup driver lets systemd revoke the device
# cgroup permissions that the container was started with.
#
# Switching Docker to the cgroupfs driver is NVIDIA's primary recommended
# workaround. See: https://github.com/NVIDIA/nvidia-container-toolkit/issues/48
echo "Applying cgroupdriver=cgroupfs to /etc/docker/daemon.json..."
DAEMON_JSON="/etc/docker/daemon.json"
if [ ! -f "$DAEMON_JSON" ]; then
  echo "{}" | sudo tee "$DAEMON_JSON" >/dev/null
fi
sudo cp -a "$DAEMON_JSON" "${DAEMON_JSON}.bak.$(date +%Y%m%d-%H%M%S)"
TMP=$(mktemp)
sudo jq '."exec-opts" = (((."exec-opts" // []) | map(select(startswith("native.cgroupdriver=") | not))) + ["native.cgroupdriver=cgroupfs"])' \
  "$DAEMON_JSON" > "$TMP"
sudo mv "$TMP" "$DAEMON_JSON"
sudo chmod 0644 "$DAEMON_JSON"

# Install udev rule for /dev/char NVIDIA symlinks. Newer runc versions
# require these symlinks to inject device nodes into containers correctly;
# the NVIDIA driver does not create them itself. The rule re-runs nvidia-ctk
# every time the nvidia driver binds to a PCI device (boot, module reload).
echo "Installing /dev/char symlink udev rule..."
sudo tee /etc/udev/rules.d/71-nvidia-dev-char.rules >/dev/null <<'RULE'
# Create /dev/char symlinks for NVIDIA devices so runc can inject them
# into containers correctly.
ACTION=="add", DEVPATH=="/bus/pci/drivers/nvidia", RUN+="/usr/bin/nvidia-ctk system create-dev-char-symlinks --create-all"
RULE
sudo udevadm control --reload-rules
sudo /usr/bin/nvidia-ctk system create-dev-char-symlinks --create-all

# Restart Docker
echo "Restarting Docker..."
sudo systemctl restart docker

# Verify installation
echo "=== Verifying installation ==="
echo "Cgroup driver:"
docker info 2>/dev/null | grep -E "Cgroup Driver|Cgroup Version"
echo
echo "Testing nvidia-smi in Docker..."
docker run --rm --gpus all ubuntu nvidia-smi

echo "=== Done! ==="
