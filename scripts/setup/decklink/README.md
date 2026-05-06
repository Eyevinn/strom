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
# List DeckLink device nodes
ls -la /dev/blackmagic/

# Check firmware status (also works as a quick "is the card alive?" probe)
BlackmagicFirmwareUpdater status

# Or, more verbosely:
DesktopVideoUpdateTool --list
```

You should see device nodes like `/dev/blackmagic/io0`, `/dev/blackmagic/io1`, etc.

### 4. Configure the Card (Connector Mapping & Profile)

The base `desktopvideo` package only ships CLI firmware tools. To inspect or
change the **configuration profile** (e.g. how many sub-devices, which BNCs
are inputs vs. outputs, half-duplex vs. full-duplex on multi-channel cards
like the DeckLink Quad 2 / 8K Pro) you need the GUI utility, which lives in a
separate package:

```bash
sudo apt-get install desktopvideo-gui
```

This installs `BlackmagicDesktopVideoSetup`, a Qt application that exposes
the same per-sub-device tabs you would see in the macOS/Windows version
(`Video Output`, `Conversions`, `Connectors`, `About`).

#### Running the GUI on a headless host (X11 forwarding)

When the DeckLink host has no local display, run the GUI from your laptop
over SSH X11 forwarding:

```bash
# From your local machine
ssh -X <host> BlackmagicDesktopVideoSetup
```

If SSH complains about a missing `~/.Xauthority` on the remote
(`No xauth data; using fake authentication data for X11 forwarding`),
seed the file once on the remote host and try again:

```bash
ssh <host> 'touch ~/.Xauthority && xauth generate $DISPLAY . trusted 2>/dev/null'
ssh -X <host> BlackmagicDesktopVideoSetup
```

`-X` requires the remote sshd to allow `X11Forwarding yes`. If your local
display is unhappy with the warning, use `-Y` (trusted forwarding) instead.

#### What to look at in the GUI

For multi-channel cards (Quad 2, 8K Pro, etc.) the most relevant places are:

- **Top-level device list**: shows one row per sub-device. On a Quad 2 in
  the `Four sub-devices, half duplex` profile you will see eight entries —
  the first four have output formats configured (e.g. `1080p50`) and use
  pairs of BNCs (`SDI 1 & 2`, `SDI 3 & 4`, `SDI 5 & 6`, `SDI 7 & 8`); the
  last four have `Connectors: none` and are inactive in this profile.
- **Connectors tab → Connector mapping** dropdown: picks which physical
  BNCs back this sub-device. This is what determines whether
  `decklinkvideosrc device-number=N` will see anything at all.
- **Configuration profile** (top-right or under a `Setup`-style menu, depending
  on the card): changes the entire profile — e.g. switching from
  `Four sub-devices, half duplex` (4 active sub-devices, each using 2 BNCs
  for in+out) to `One sub-device, half duplex` (1 sub-device with 4 dedicated
  inputs on BNC 1–4 and 4 dedicated outputs on BNC 5–8).

The mapping in the GUI translates to GStreamer's zero-indexed `device-number`:
`DeckLink Quad (1)` is `device-number=0`, `DeckLink Quad (2)` is
`device-number=1`, and so on. `device-number` values that point at
sub-devices with `Connectors: none` will fail at pipeline start
(`decklinkvideosink`) or report SDK errors (`decklinkvideosrc`).

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

Strom exposes two DeckLink blocks — one input, one output — each combined for
video and audio. Pick `audio_video`, `video`, or `audio` via the `Stream Mode`
property; the block exposes only the pads relevant to the selected mode.

Neither block performs format conversion. Peer blocks must accept (input) or
deliver (output) the card's native pixel format — `8bit-yuv` (UYVY), `10bit-yuv`
(v210), `8bit-argb`, `8bit-bgra`, or `10bit-rgb` — and 48 kHz S16LE/S32LE audio.
Use a downstream `Video Format` / `Audio Format` block to convert if needed.

### DeckLink Input Block

Add a "DeckLink Input" block and configure:

- **Stream Mode**: `Audio + Video`, `Video only`, or `Audio only`
- **Device Number**: Which DeckLink device to use (0, 1, 2, ...)
- **Video Mode**: Video format (e.g., `1080p50`, `1080i50`, `2160p50`, `auto`)
- **Video Connection**: Input connector (`auto`, `sdi`, `hdmi`, `optical-sdi`, ...)
- **Video Format**: Pixel format the card delivers (`auto`, `8bit-yuv`, `10bit-yuv`, ...)
- **Drop No-Signal Frames**: Drop frames flagged as no-signal instead of forwarding black
- **Audio Connection**: Audio source (`auto`, `embedded`, `aes`, `analog`, `analog-xlr`, `analog-rca`)
- **Audio Channels**: `2`, `8`, `16`, or `max`

In `Audio only` mode `decklinkvideosrc` is still created internally and drained
to a `fakesink` — required for `decklinkaudiosrc` to operate.

### DeckLink Output Block

Add a "DeckLink Output" block and configure:

- **Stream Mode**: `Audio + Video`, `Video only`, or `Audio only`
- **Device Number**: Which DeckLink device to use
- **Video Mode**: Output video format (e.g., `1080p25`, `1080p50`, `2160p50`)
- **Video Format**: Pixel format the card expects (`8bit-yuv`, `10bit-yuv`, ...)

In `Audio only` mode `decklinkvideosink` is still created internally and fed
black frames at 1080p25/UYVY — required for `decklinkaudiosink` to operate. The
`Video Mode` and `Video Format` properties are ignored in this case.

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

### Detecting Which Inputs Have Signal

Use the bundled `probe-signal.sh` script to scan every DeckLink
device/connection combination and print which inputs have a live signal
(plus the auto-detected video mode). Run it from inside the strom
container — it relies on the GStreamer DeckLink plugin and works
without any extra dependencies.

```bash
# One-shot via stdin (no copy needed)
docker exec -i strom bash < scripts/setup/decklink/probe-signal.sh

# Or copy in and run repeatedly
docker cp scripts/setup/decklink/probe-signal.sh strom:/tmp/probe-signal.sh
docker exec strom bash /tmp/probe-signal.sh
```

Sample output:

```
=== DeckLink devices visible to GStreamer ==="
…
=== Probing inputs for signal (timeout 3s per probe) ===

 device | connection    | result
 -------+---------------+-------------------
   0    | sdi           | SIGNAL 1080i50
   1    | sdi           | no signal
   2    | sdi           | SIGNAL 2160p50
```

Tunable via env vars (see top of the script):

| Variable | Default | Purpose |
|----------|---------|---------|
| `DECKLINK_DEVICES` | `0 1 2 3 4 5 6 7` | Device numbers to probe |
| `DECKLINK_CONNECTIONS` | `sdi hdmi optical-sdi component composite svideo` | Connections to try per device |
| `DECKLINK_PROBE_TIMEOUT` | `3` | Seconds to wait for a buffer before declaring no signal |
| `VERBOSE` | `0` | Set `1` to dump raw `gst-launch` output for each probe |

The script auto-skips device-numbers that don't exist on the host and
connections that aren't valid for the card model, so the output stays
clean even on cards that only expose `sdi`.

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

List all cards with: `DesktopVideoUpdateTool --list` (firmware/serial only),
`BlackmagicFirmwareUpdater status`, or `BlackmagicDesktopVideoSetup` (GUI,
shows the connector mapping per sub-device — see "Configure the Card" above).

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
