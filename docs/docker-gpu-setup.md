# Docker GPU Setup (NVIDIA)

This guide covers setting up NVIDIA GPU support for Strom in Docker, enabling hardware-accelerated video encoding (NVENC), decoding (NVDEC), and GPU-accelerated video processing with CUDA-GL interop.

## Prerequisites

- NVIDIA GPU (GeForce, Quadro, or Tesla)
- NVIDIA drivers installed on the host
- Docker installed
- Linux host (native or VM with GPU passthrough)

Verify GPU access on the host:
```bash
nvidia-smi
```

## Install NVIDIA Container Toolkit

The NVIDIA Container Toolkit enables GPU access inside Docker containers.

```bash
# Install prerequisites
sudo apt-get update && sudo apt-get install -y curl gnupg2

# Add GPG key and repository
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey | \
  sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg

curl -s -L https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list | \
  sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' | \
  sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list

# Install
sudo apt-get update
sudo apt-get install -y nvidia-container-toolkit

# Configure Docker
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
```

Or use the setup script:
```bash
./scripts/setup/nvidia/install-nvidia-container-toolkit.sh
```

## Basic Usage

```bash
# Run with all GPUs
docker run --gpus all <image>

# Run with specific GPU by ID
docker run --gpus '"device=0"' <image>

# Run with specific number of GPUs
docker run --gpus 2 <image>
```

## Running Strom with GPU

### Basic

```bash
docker run -d \
  --gpus all \
  -p 8080:8080 \
  --name strom \
  eyevinntechnology/strom:latest
```

### Production

```bash
docker run -d \
  --gpus all \
  -e STROM_MEDIA_PATH=/media \
  -v ./media:/media \
  -v ./data:/data \
  --network host \
  --name strom \
  eyevinntechnology/strom:latest
```

## GPU Acceleration in Strom

### What Gets Accelerated

| Feature | GPU Element | Fallback |
|---------|-------------|----------|
| Video Encoding | `nvh264enc`, `nvh265enc`, `nvav1enc` | `x264enc`, `x265enc` |
| Video Decoding | `nvh264dec`, `nvh265dec` | `avdec_h264` |
| Color Conversion | `autovideoconvert` (glcolorconvert) | `videoconvert` |
| Video Scaling | `glvideomixer` | `videoscale` |
| Compositing | `glvideomixer` | `compositor` |

### Runtime Detection

Strom automatically detects GPU capabilities at startup and selects the optimal pipeline:

**GPU interop works:**
```
INFO  CUDA-GL interop works - using GPU-accelerated video conversion
INFO  ✓ NVML initialized successfully - found 1 GPU(s)
```

**GPU interop not available (falls back gracefully):**
```
WARN  CUDA-GL interop failed: ... - using software video conversion
INFO  ✓ NVML initialized successfully - found 1 GPU(s)
```

Even when CUDA-GL interop fails, hardware encoding (NVENC) is still used - only color conversion falls back to CPU.

## CUDA-GL Interop (Zero-Copy)

### What is CUDA-GL Interop?

CUDA-GL interop allows video frames to stay in GPU memory throughout the entire processing pipeline, eliminating expensive CPU-GPU memory transfers:

```
┌──────────────────────────────────────────────────────────────┐
│                    With CUDA-GL Interop                       │
│                                                                │
│  Video Source → glupload → glcolorconvert → nvh264enc → Output│
│                     │              │              │            │
│                  GPU Mem        GPU Mem        GPU Mem         │
│                     └──────── Zero Copy ────────┘             │
└──────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────┐
│                    Without CUDA-GL Interop                    │
│                                                                │
│  Video Source → videoconvert → nvh264enc → Output             │
│                     │              │                           │
│                  CPU Mem   ──Copy──▶ GPU Mem                   │
└──────────────────────────────────────────────────────────────┘
```

### Headless Docker Configuration

For CUDA-GL interop to work in headless Docker (no display server), specific environment variables are required:

| Variable | Value | Purpose |
|----------|-------|---------|
| `GST_GL_WINDOW` | `egl-device` | Direct GPU access without X11/Wayland |
| `GST_GL_PLATFORM` | `egl` | Use EGL instead of GLX |
| `NVIDIA_DRIVER_CAPABILITIES` | `all` | Enable graphics + compute capabilities |

The Strom Docker image sets these automatically.

### Testing CUDA-GL Interop

Inside the container:

```bash
# Test GL context creation
GST_DEBUG=glcontext:4 gst-launch-1.0 \
  videotestsrc num-buffers=1 ! glupload ! gldownload ! fakesink 2>&1 | \
  grep -E "GL_VENDOR|GL_RENDERER"

# Expected for working interop:
# GL_VENDOR: NVIDIA Corporation
# GL_RENDERER: NVIDIA GeForce RTX 3090/PCIe/SSE2

# Test full interop pipeline
GST_DEBUG=nvenc:3 gst-launch-1.0 \
  videotestsrc num-buffers=10 ! video/x-raw,width=1920,height=1080 ! \
  glupload ! glcolorconvert ! "video/x-raw(memory:GLMemory),format=NV12" ! \
  nvh264enc ! fakesink

# No CUDA_ERROR_OPERATING_SYSTEM means interop works
```

## Platform Compatibility

| Platform | NVENC | CUDA-GL Interop | Notes |
|----------|-------|-----------------|-------|
| Linux Native (X11) | Yes | Yes | Full support |
| Linux Native (Headless) | Yes | Yes | Requires `egl-device` |
| Docker `--gpus all` | Yes | Yes | Requires `egl-device` |
| WSL2 | Yes | **No** | D3D layer blocks interop |
| macOS | No | No | No NVIDIA support |

## Troubleshooting

### GPU not visible in container

```bash
# Error: could not select device driver "" with capabilities: [[gpu]]

# Solution: Install nvidia-container-toolkit
./scripts/setup/nvidia/install-nvidia-container-toolkit.sh
```

### CUDA works but GL uses Mesa

```bash
# Symptom: GL_RENDERER shows "llvmpipe" instead of NVIDIA

# Check EGL vendor config
cat /usr/share/glvnd/egl_vendor.d/10_nvidia.json
# Should contain: {"file_format_version":"1.0.0","ICD":{"library_path":"libEGL_nvidia.so.0"}}

# Check if NVIDIA EGL library exists
ls -la /usr/lib/x86_64-linux-gnu/libEGL_nvidia.so*
```

### CUDA_ERROR_OPERATING_SYSTEM

```bash
# Symptom in logs:
# CUDA call failed: CUDA_ERROR_OPERATING_SYSTEM

# Causes:
# 1. WSL2 - CUDA-GL interop not supported (use software fallback)
# 2. Wrong GL backend - ensure GST_GL_WINDOW=egl-device
# 3. Missing EGL device - check /dev/dri/card* permissions
```

### Wrong GPU selected

```bash
# Specify exact GPU
docker run --gpus '"device=0"' ...

# Or via CUDA
docker run --gpus all -e CUDA_VISIBLE_DEVICES=0 ...
```

## Verify Installation

```bash
# Test nvidia-smi in container
docker run --rm --gpus all ubuntu nvidia-smi

# Test GStreamer NVENC
docker run --rm --gpus all \
  eyevinntechnology/strom:latest \
  gst-inspect-1.0 nvh264enc

# Test full pipeline
docker run --rm --gpus all \
  eyevinntechnology/strom:latest \
  gst-launch-1.0 videotestsrc num-buffers=30 ! \
    video/x-raw,width=1920,height=1080 ! \
    nvh264enc ! fakesink
```

## Additional Resources

- [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/)
- [NVIDIA Setup Scripts](../scripts/setup/nvidia/README.md) - Detailed setup guide
- [Strom Docker Guide](DOCKER.md) - General Docker deployment
- [GStreamer NVCODEC](https://gstreamer.freedesktop.org/documentation/nvcodec/)
