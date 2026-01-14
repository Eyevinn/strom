# NVIDIA GPU Setup for Strom

This directory contains scripts for setting up NVIDIA GPU support for Strom, enabling hardware-accelerated video encoding/decoding (NVENC/NVDEC) and GPU-accelerated video processing.

## Overview

Strom uses NVIDIA GPUs for:
- **NVENC/NVDEC** - Hardware video encoding/decoding (H.264/H.265/AV1)
- **CUDA-GL Interop** - Zero-copy video processing between OpenGL and CUDA
- **GPU Compositing** - Hardware-accelerated video mixing/compositing

## Scripts

### 1. Install NVIDIA Driver

```bash
./install-nvidia-driver.sh
```

Installs the recommended NVIDIA driver using `ubuntu-drivers`. This script:
- Detects your GPU model
- Installs the recommended driver version
- Prompts for reboot to load the new driver

**Important:** Do NOT install the `nvidia-headless` driver variant - it lacks the OpenGL/EGL capabilities required for CUDA-GL interop.

### 2. Install NVIDIA Container Toolkit

```bash
./install-nvidia-container-toolkit.sh
```

Installs the NVIDIA Container Toolkit for Docker, enabling GPU access inside containers. This script:
- Adds the NVIDIA GPG key and repository
- Installs `nvidia-container-toolkit`
- Configures the Docker runtime
- Verifies the installation with a test container

## Quick Start

```bash
# Make scripts executable
chmod +x *.sh

# Install driver (requires reboot)
./install-nvidia-driver.sh

# After reboot, verify driver
nvidia-smi

# Install container toolkit
./install-nvidia-container-toolkit.sh

# Verify GPU in Docker
docker run --rm --gpus all ubuntu nvidia-smi
```

## Running Strom with GPU Support

### Basic Usage

```bash
docker run -d \
  --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=all \
  -p 8080:8080 \
  --name strom \
  eyevinntechnology/strom:latest
```

### Production Setup

```bash
docker run -d \
  --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=all \
  -e STROM_MEDIA_PATH=/media \
  -v ./media:/media \
  -v ./data:/data \
  --network host \
  --name strom \
  eyevinntechnology/strom:latest
```

## How GPU Acceleration Works

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Strom Pipeline                                │
│                                                                   │
│  ┌──────────┐    ┌─────────────────┐    ┌──────────────────┐   │
│  │ Video    │    │ autovideoconvert │    │  nvh264enc       │   │
│  │ Source   │───▶│ (glupload +     │───▶│  (NVENC HW)      │   │
│  │          │    │  glcolorconvert) │    │                  │   │
│  └──────────┘    └─────────────────┘    └──────────────────┘   │
│                          │                        │              │
│                    GPU Memory                GPU Memory          │
│                   (GL Textures)              (CUDA)             │
│                          │                        │              │
│                          └────── Zero-Copy ──────┘              │
│                           (CUDA-GL Interop)                      │
└─────────────────────────────────────────────────────────────────┘
```

### CUDA-GL Interop

When CUDA-GL interop works, video frames flow through the GPU without copying to system memory:

1. **glupload** - Uploads video to GPU as OpenGL texture
2. **glcolorconvert** - Converts color space on GPU (e.g., NV12)
3. **CUDA-GL Interop** - Registers GL texture as CUDA resource (zero-copy)
4. **nvh264enc** - Encodes directly from CUDA memory

This provides significant performance benefits for high-resolution video processing.

### Runtime Detection

Strom automatically detects at startup whether CUDA-GL interop works:

```
INFO  GStreamer initialized
INFO  CUDA-GL interop works - using GPU-accelerated video conversion
INFO  ✓ NVML initialized successfully - found 1 GPU(s)
```

Or if interop is not available:

```
WARN  CUDA-GL interop failed: CUDA_ERROR_OPERATING_SYSTEM - using software video conversion
```

When interop fails, Strom falls back to CPU-based color conversion (`videoconvert`) while still using GPU encoding (`nvh264enc`).

## Headless Docker (No Display)

### The Challenge

Running GPU-accelerated video processing in Docker without a display server (X11/Wayland) requires special configuration. The key insight is using **EGL device** mode instead of GBM for OpenGL context creation.

### Solution

The Strom Docker image is preconfigured with:

```dockerfile
# Use EGL device for headless NVIDIA GPU access
ENV GST_GL_WINDOW=egl-device
ENV GST_GL_PLATFORM=egl
```

This tells GStreamer to use NVIDIA's EGL device extension, which provides direct GPU access without requiring a display server.

### Environment Variables

| Variable | Value | Description |
|----------|-------|-------------|
| `GST_GL_WINDOW` | `egl-device` | Use EGL device for headless GL |
| `GST_GL_PLATFORM` | `egl` | Use EGL instead of GLX |
| `NVIDIA_DRIVER_CAPABILITIES` | `all` | Enable all NVIDIA capabilities |

### Verifying Headless GPU Access

To test if CUDA-GL interop works in your Docker container:

```bash
# Inside the container
GST_DEBUG=glcontext:4 gst-launch-1.0 \
  videotestsrc num-buffers=1 ! \
  glupload ! gldownload ! \
  fakesink 2>&1 | grep -E "GL_VENDOR|GL_RENDERER"
```

Expected output for working interop:
```
GL_VENDOR: NVIDIA Corporation
GL_RENDERER: NVIDIA GeForce RTX 3090/PCIe/SSE2
```

If you see `Mesa` or `llvmpipe`, the NVIDIA GPU is not being used.

## Troubleshooting

### nvidia-smi works but GL doesn't

**Symptom:** `nvidia-smi` shows the GPU, but GStreamer uses Mesa/llvmpipe.

**Cause:** Missing NVIDIA EGL vendor configuration.

**Solution:** Verify `/usr/share/glvnd/egl_vendor.d/10_nvidia.json` exists with:
```json
{"file_format_version":"1.0.0","ICD":{"library_path":"libEGL_nvidia.so.0"}}
```

### CUDA_ERROR_OPERATING_SYSTEM

**Symptom:**
```
CUDA call failed: CUDA_ERROR_OPERATING_SYSTEM
```

**Cause:** CUDA cannot register OpenGL resources. Common in WSL2 or with incorrect GL backend.

**Solutions:**
1. Use `GST_GL_WINDOW=egl-device` (not `gbm`)
2. Ensure `NVIDIA_DRIVER_CAPABILITIES=all` is set
3. WSL2: CUDA-GL interop is not supported - use software conversion

### Wrong GPU selected

**Symptom:** Multiple GPUs present, wrong one used for encoding.

**Solution:** Set specific GPU:
```bash
docker run --gpus '"device=0"' ...
```

Or for CUDA device selection:
```bash
-e CUDA_VISIBLE_DEVICES=0
```

### Container can't access GPU

**Symptom:**
```
docker: Error response from daemon: could not select device driver "" with capabilities: [[gpu]]
```

**Solution:** Install and configure nvidia-container-toolkit:
```bash
./install-nvidia-container-toolkit.sh
```

### GStreamer GL plugins not found

**Symptom:** `glupload`, `gldownload` elements not available.

**Solution:** Ensure `gstreamer1.0-gl` is installed:
```bash
apt-get install gstreamer1.0-gl
gst-inspect-1.0 glupload
```

## Platform Compatibility

| Platform | CUDA | NVENC | CUDA-GL Interop |
|----------|------|-------|-----------------|
| Linux Native (X11) | Yes | Yes | Yes |
| Linux Native (Headless) | Yes | Yes | Yes (with egl-device) |
| Docker (--gpus all) | Yes | Yes | Yes (with egl-device) |
| WSL2 | Yes | Yes | No (fallback to software) |
| Windows | N/A | N/A | N/A |
| macOS | No | No | No |

## Performance Comparison

| Mode | Color Conversion | Encoding | Memory Copies |
|------|-----------------|----------|---------------|
| GPU Interop | GPU (glcolorconvert) | GPU (nvenc) | 0 (zero-copy) |
| Software Fallback | CPU (videoconvert) | GPU (nvenc) | 1 (CPU→GPU) |
| Full Software | CPU (videoconvert) | CPU (x264) | 0 |

## Additional Resources

- [NVIDIA Container Toolkit Documentation](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/)
- [GStreamer GL Documentation](https://gstreamer.freedesktop.org/documentation/gl/)
- [CUDA-OpenGL Interop Guide](https://docs.nvidia.com/cuda/cuda-runtime-api/group__CUDART__OPENGL.html)
- [Strom Docker Guide](../../docs/DOCKER.md)
- [Strom Docker GPU Setup](../../docs/docker-gpu-setup.md)

## References

- [GStreamer GstGLDisplay](https://gstreamer.freedesktop.org/documentation/gl/gstgldisplay.html) - GL environment variables
- [NVIDIA EGL Device Extension](https://www.khronos.org/registry/EGL/extensions/EXT/EGL_EXT_device_query.txt)
- [nvidia-docker FAQ](https://github.com/NVIDIA/nvidia-docker/wiki/Frequently-Asked-Questions)
