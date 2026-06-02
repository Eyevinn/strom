#!/bin/bash
set -e

# Reboot behavior. For automation (e.g. golden-image builds) set REBOOT
# explicitly so the script never blocks on a prompt:
#   REBOOT=no   install the driver but do NOT reboot (the caller reboots later)
#   REBOOT=yes  reboot immediately once the driver is installed
# Left unset, the script asks interactively, and skips the reboot when there is
# no terminal to ask on.
REBOOT="${REBOOT:-}"

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
case "$REBOOT" in
    1 | y | Y | yes | YES | true | TRUE)
        echo "REBOOT=$REBOOT: rebooting now..."
        sudo reboot
        ;;
    0 | n | N | no | NO | false | FALSE)
        echo "REBOOT=$REBOOT: skipping reboot. Run 'sudo reboot' when ready, then verify with 'nvidia-smi'."
        ;;
    "")
        if [ -t 0 ]; then
            read -p "Reboot now? [y/N] " -n 1 -r
            echo
            if [[ $REPLY =~ ^[Yy]$ ]]; then
                sudo reboot
            else
                echo "Run 'sudo reboot' when ready, then verify with 'nvidia-smi'"
            fi
        else
            echo "No terminal and REBOOT unset; skipping reboot. Set REBOOT=yes to reboot automatically, or run 'sudo reboot' manually."
        fi
        ;;
    *)
        echo "Unrecognized REBOOT value '$REBOOT' (expected yes/no); skipping reboot." >&2
        ;;
esac
