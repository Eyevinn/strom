# Blackmagic DeckLink Setup for Strom

This directory contains documentation and scripts for setting up Blackmagic DeckLink cards with Strom in Docker, enabling professional SDI video input/output.

## Overview

Strom supports Blackmagic DeckLink cards for:
- **SDI Input** - Capture video from SDI sources (cameras, routers, etc.)
- **SDI Output** - Output video to SDI destinations (monitors, routers, etc.)
- **Multiple Cards** - Support for multiple DeckLink cards in a single system

## Prerequisites

### Host Requirements

- Linux host (Ubuntu 20.04+ recommended)
- Blackmagic DeckLink card installed (PCIe)
- Blackmagic Desktop Video software installed on host
- Docker installed

### Supported Cards

Most DeckLink cards are supported, including:
- DeckLink Duo 2
- DeckLink Quad 2
- DeckLink 8K Pro
- DeckLink Mini Monitor/Recorder
- And others from the DeckLink family

## Host Setup

### 1. Install Desktop Video Software

Download the Blackmagic Desktop Video package from:
https://www.blackmagicdesign.com/support/family/capture-and-playback

**Note:** The download page may prompt for registration, but direct download links are available.

```bash
# Download the latest Desktop Video package for your distribution
# Example: desktopvideo_15.3.1a4_amd64.deb

# Install the package
sudo dpkg -i desktopvideo_*.deb

# Fix any dependency issues
sudo apt-get install -f

# The installation compiles DKMS kernel modules - this may take a few minutes
# A reboot is required after installation
sudo reboot
```

The installation process:
1. Installs the DeckLink SDK libraries (`libDeckLinkAPI.so`, `libDeckLinkPreviewAPI.so`)
2. Compiles DKMS kernel modules for your running kernel
3. Creates device nodes at `/dev/blackmagic/`

### 2. Update Card Firmware (if needed)

After installing Desktop Video, check if firmware updates are available:

```bash
# List DeckLink devices and their firmware status
BlackmagicFirmwareUpdater status

# Update firmware if needed (requires reboot)
sudo BlackmagicFirmwareUpdater update
sudo reboot
```

### 3. Verify Installation

```bash
# Check that DeckLink devices are detected
BlackmagicDesktopVideoStatusUtility

# Or list devices
ls -la /dev/blackmagic/
```

You should see device nodes like `/dev/blackmagic/io0`, `/dev/blackmagic/io1`, etc.

## Running Strom with DeckLink in Docker

### Required Docker Options

DeckLink cards require specific Docker options to work inside containers:

| Option | Purpose |
|--------|---------|
| `--privileged` | Required for direct hardware access |
| `-v /dev/blackmagic:/dev/blackmagic` | Mount DeckLink device nodes |
| `-v /usr/lib/libDeckLinkAPI.so:/lib/libDeckLinkAPI.so:ro` | Mount SDK API library |
| `-v /usr/lib/libDeckLinkPreviewAPI.so:/lib/libDeckLinkPreviewAPI.so:ro` | Mount SDK Preview API library |
| `-v /usr/lib/blackmagic:/lib/blackmagic:ro` | Mount SDK support files |

### Basic Usage

```bash
docker run -d \
  --privileged \
  -v /dev/blackmagic:/dev/blackmagic \
  -v /usr/lib/libDeckLinkAPI.so:/lib/libDeckLinkAPI.so:ro \
  -v /usr/lib/libDeckLinkPreviewAPI.so:/lib/libDeckLinkPreviewAPI.so:ro \
  -v /usr/lib/blackmagic:/lib/blackmagic:ro \
  -p 8080:8080 \
  --name strom \
  eyevinntechnology/strom:latest
```

### Production Setup (with GPU and Network)

```bash
docker run -d \
  --privileged \
  --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=all \
  -v /dev/blackmagic:/dev/blackmagic \
  -v /usr/lib/libDeckLinkAPI.so:/lib/libDeckLinkAPI.so:ro \
  -v /usr/lib/libDeckLinkPreviewAPI.so:/lib/libDeckLinkPreviewAPI.so:ro \
  -v /usr/lib/blackmagic:/lib/blackmagic:ro \
  -v ./media:/media \
  -v ./data:/data \
  --network host \
  --name strom \
  eyevinntechnology/strom:latest
```

### Docker Compose Example

```yaml
version: '3.8'
services:
  strom:
    image: eyevinntechnology/strom:latest
    privileged: true
    volumes:
      - /dev/blackmagic:/dev/blackmagic
      - /usr/lib/libDeckLinkAPI.so:/lib/libDeckLinkAPI.so:ro
      - /usr/lib/libDeckLinkPreviewAPI.so:/lib/libDeckLinkPreviewAPI.so:ro
      - /usr/lib/blackmagic:/lib/blackmagic:ro
      - ./data:/data
    ports:
      - "8080:8080"
    restart: unless-stopped
```

## Using DeckLink in Strom

### DeckLink Video Input Block

In the Strom UI, add a "DeckLink Video Input" block and configure:

- **Device Number**: Which DeckLink device to use (0, 1, 2, ...)
- **Mode**: Video format (e.g., `1080p50`, `1080i50`, `2160p50`)
- **Connection**: Input connector (`sdi`, `hdmi`, `optical-sdi`)

### DeckLink Video Output Block

Add a "DeckLink Video Output" block and configure:

- **Device Number**: Which DeckLink device to use
- **Mode**: Output video format

### Testing Inside Container

```bash
# Enter the container
docker exec -it strom bash

# List available DeckLink devices
gst-device-monitor-1.0 Video/Source

# Test capture from first DeckLink input
gst-launch-1.0 decklinkvideosrc device-number=0 mode=1080p50 ! \
  videoconvert ! autovideosink

# Test output to first DeckLink output
gst-launch-1.0 videotestsrc ! \
  video/x-raw,width=1920,height=1080,framerate=50/1 ! \
  decklinkvideosink device-number=0 mode=1080p50
```

## Troubleshooting

### DeckLink devices not visible in container

**Symptom:** No devices found when running `gst-device-monitor-1.0`

**Solutions:**
1. Verify devices exist on host: `ls -la /dev/blackmagic/`
2. Ensure `--privileged` flag is used
3. Check device mount: `-v /dev/blackmagic:/dev/blackmagic`

### "Failed to load DeckLink drivers"

**Symptom:** Error about missing DeckLink libraries

**Solution:** Mount all SDK libraries and support files:
```bash
-v /usr/lib/libDeckLinkAPI.so:/lib/libDeckLinkAPI.so:ro
-v /usr/lib/libDeckLinkPreviewAPI.so:/lib/libDeckLinkPreviewAPI.so:ro
-v /usr/lib/blackmagic:/lib/blackmagic:ro
```

### Card requires firmware update

**Symptom:** Card detected but not working properly

**Solution:** Update firmware on the host:
```bash
sudo BlackmagicFirmwareUpdater update
sudo reboot
```

### Wrong video mode

**Symptom:** Black output or distorted video

**Solution:** Ensure the mode matches your signal:
- Check input signal format
- Use matching mode in Strom block configuration
- Common modes: `1080p50`, `1080p5994`, `1080i50`, `1080i5994`, `2160p50`

### Multiple cards - selecting the right one

**Symptom:** Wrong card being used

**Solution:** Use the `device-number` property:
- `device-number=0` for first card
- `device-number=1` for second card
- etc.

List all cards with: `BlackmagicDesktopVideoStatusUtility`

## Platform Compatibility

| Platform | DeckLink Support | Notes |
|----------|------------------|-------|
| Linux Native | Yes | Full support |
| Docker (Linux) | Yes | Requires privileged mode + mounts |
| Windows | No | Docker on Windows doesn't support PCIe passthrough |
| macOS | No | Docker on macOS doesn't support PCIe passthrough |
| WSL2 | No | No PCIe passthrough |

## Additional Resources

- [Blackmagic Design Support](https://www.blackmagicdesign.com/support)
- [Desktop Video Downloads](https://www.blackmagicdesign.com/support/family/capture-and-playback)
- [GStreamer DeckLink Plugin](https://gstreamer.freedesktop.org/documentation/decklink/)
- [Strom Docker Guide](../../docs/DOCKER.md)
