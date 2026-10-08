# NDI Testing Scripts

This directory contains scripts for installing and testing NDI (Network Device Interface) support in Strom.

## Overview

NDI allows you to send and receive broadcast-quality video and audio over a standard IP network. These scripts help you set up the required components and test the NDI blocks in Strom.

## Components

The NDI integration requires two main components:

1. **NDI SDK** - Core NDI runtime libraries from Vizrt/NDI
2. **GStreamer NDI Plugin** - GStreamer plugin that provides `ndisrc` and `ndisink` elements

## Scripts

Run these scripts **in order**:

### 1. Install NDI SDK
```bash
./1-install-ndi-sdk.sh
```

Downloads and installs the NDI SDK v6 from the official source. This provides the core NDI runtime libraries.

**Environment variables:**
- `NDI_SDK_VERSION` - SDK version to install (default: 6)
- `INSTALL_PREFIX` - Installation prefix (default: /usr)
- `DOWNLOAD_DIR` - Temporary download directory (default: /tmp)

**Example:**
```bash
NDI_SDK_VERSION=5 ./1-install-ndi-sdk.sh
```

### 2. Install GStreamer NDI Plugin
```bash
./2-install-gstreamer-ndi-plugin.sh
```

Builds and installs the `gst-plugin-ndi` from source. This provides the GStreamer elements needed for NDI streaming.

**Environment variables:**
- `BUILD_TYPE` - Build type: `release` or `debug` (default: release)
- `PLUGIN_REPO` - Git repository URL (default: https://github.com/teltek/gst-plugin-ndi.git)
- `PLUGIN_BRANCH` - Git branch to build (default: master)
- `WORK_DIR` - Build directory (default: /tmp/gst-plugin-ndi-build)

**Example:**
```bash
BUILD_TYPE=debug ./2-install-gstreamer-ndi-plugin.sh
```

### 3. Verify Installation
```bash
./3-verify-ndi-installation.sh
```

Comprehensive verification script that checks:
- NDI SDK library installation
- GStreamer installation
- NDI plugin registration
- Required dependencies
- Element availability (ndisrc, ndisink)

Returns exit code 0 if all checks pass, 1 if there are errors.

### 4. Test NDI Output (Sender)
```bash
./4-test-ndi-output.sh
```

Creates a test NDI sender that broadcasts a test pattern video and audio tone. This can be received by any NDI-compatible receiver.

**Environment variables:**
- `NDI_NAME` - NDI stream name (default: Strom-Test-Output)
- `VIDEO_PATTERN` - Test pattern number (default: 0 = SMPTE bars)
- `AUDIO_WAVE` - Audio waveform (default: 0 = sine wave)

**Examples:**
```bash
# Send bouncing ball with pink noise
VIDEO_PATTERN=18 AUDIO_WAVE=4 ./4-test-ndi-output.sh

# Custom stream name
NDI_NAME="My Test Stream" ./4-test-ndi-output.sh
```

**Available video patterns:**
- 0 = SMPTE color bars
- 1 = Snow (random)
- 18 = Bouncing ball
- 20 = Circular gradient

### 5. Test NDI Input (Receiver)
```bash
./5-test-ndi-input.sh
```

Interactive script to receive NDI streams. Provides a menu to:
- List available NDI sources on the network
- Receive from a specific source
- Receive from the default test source

**Environment variables:**
- `NDI_SOURCE` - NDI source name (if set, skips menu)
- `SHOW_VIDEO` - Show video (default: true)
- `SHOW_AUDIO` - Show audio (default: true)

**Examples:**
```bash
# Receive specific source directly (include hostname)
NDI_SOURCE="DESKTOP-ABC123 (Strom-Test-Output)" ./5-test-ndi-input.sh

# Receive video only
SHOW_AUDIO=false ./5-test-ndi-input.sh

# Receive audio only
SHOW_VIDEO=false ./5-test-ndi-input.sh
```

**Note:** NDI source names include the hostname in the format `HOSTNAME (Stream Name)`. The script automatically constructs the correct name when using option 3.

## Quick Start

```bash
# Make scripts executable
chmod +x *.sh

# Run all installation steps
./1-install-ndi-sdk.sh
./2-install-gstreamer-ndi-plugin.sh
./3-verify-ndi-installation.sh

# Test NDI output in one terminal
./4-test-ndi-output.sh

# Test NDI input in another terminal
./5-test-ndi-input.sh
```

## Cleanup

To completely remove NDI SDK and plugin installations:

```bash
./cleanup-ndi.sh
```

This will remove:
- All NDI SDK files from `/tmp/`
- Installed libraries from `/usr/lib/` and `/lib/`
- Headers from `/usr/include/ndi/`
- Update the library cache

**Warning:** This is destructive and cannot be undone. Use this if you want to start fresh.

## Testing NDI Blocks in Strom

After installation, you can test the NDI blocks in Strom. Strom provides two unified NDI blocks with configurable modes:

**Block Modes:**
- `combined` (default) - Handles both audio and video in a single block
- `video` - Video-only streaming
- `audio` - Audio-only streaming

### Combined Audio+Video Test (Most Common)
1. Start Strom: `cargo run --release`
2. Open `http://localhost:8080`
3. Create a sender flow:
   - Add `videotestsrc` element
   - Add `audiotestsrc` element
   - Add "NDI Output" block
   - Set mode to "combined" (default)
   - Set NDI stream name (e.g., "My Stream")
   - Connect videotestsrc → NDI Output video_in
   - Connect audiotestsrc → NDI Output audio_in
   - Click "Start"
4. Create a receiver flow:
   - Add "NDI Input" block
   - Set mode to "combined" (default)
   - Set NDI source name to "HOSTNAME (My Stream)" (use your actual hostname)
   - Add `autovideosink` element
   - Add `autoaudiosink` element
   - Connect NDI Input video_out → autovideosink
   - Connect NDI Input audio_out → autoaudiosink
   - Click "Start"

### Video-Only Test
1. Add "NDI Output" block, set mode to "video"
   - Only `video_in` pad will be available
   - Connect videotestsrc → video_in
2. Add "NDI Input" block, set mode to "video"
   - Only `video_out` pad will be available
   - Connect video_out → autovideosink

### Audio-Only Test
1. Add "NDI Output" block, set mode to "audio"
   - Only `audio_in` pad will be available
   - Connect audiotestsrc → audio_in
2. Add "NDI Input" block, set mode to "audio"
   - Only `audio_out` pad will be available
   - Connect audio_out → autoaudiosink

### Using Test Scripts
Alternatively, use the test scripts:
- Sender: `./4-test-ndi-output.sh` (creates "Strom-Test-Output" stream)
- Receiver: `./5-test-ndi-input.sh` (prompts for source selection)

## Troubleshooting

### NDI library not found
```bash
# Check if library is in ldconfig cache
ldconfig -p | grep libndi

# If not found, verify installation
ls -l /usr/lib/*/libndi.so*

# Update cache
sudo ldconfig
```

### GStreamer can't find NDI plugin
```bash
# Clear plugin cache
rm -rf ~/.cache/gstreamer-1.0/

# Check if plugin exists
ls -l /usr/lib/*/gstreamer-1.0/libgstndi.so

# Inspect with debug output
GST_DEBUG=2 gst-inspect-1.0 ndi
```

### NDI source not discovered
- Ensure sender and receiver are on the same network
- Check firewall settings (NDI uses TCP port 5960 and multicast)
- Some networks block multicast traffic
- Try using `url-address` property instead of `ndi-name` with direct IP

### Build failures
```bash
# Update Rust
rustup update

# Install missing dependencies
sudo apt-get install build-essential pkg-config

# Check GStreamer development files
pkg-config --modversion gstreamer-1.0
```

## Architecture Support

These scripts support:
- **x86_64** (Intel/AMD 64-bit)
- **aarch64** (ARM 64-bit)

The scripts automatically detect your architecture and install the appropriate libraries.

## Dependencies

### Runtime Dependencies
- NDI SDK (libndi.so)
- GStreamer 1.0
- libavahi-common3
- libavahi-client3

### Build Dependencies
- Rust and Cargo
- GStreamer development packages
- pkg-config
- Git

## References

- [NDI Official Website](https://ndi.video/)
- [NDI SDK Download](https://ndi.video/for-developers/ndi-sdk/download/)
- [GStreamer NDI Plugin (teltek)](https://github.com/teltek/gst-plugin-ndi)
- [GStreamer Documentation](https://gstreamer.freedesktop.org/documentation/)
- [Strom Documentation](../../../README.md)

## License

These scripts are part of the Strom project and follow the same license (MIT OR Apache-2.0).

The NDI SDK has its own license terms - see https://ndi.video/ for details.
