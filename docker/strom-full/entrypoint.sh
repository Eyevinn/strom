#!/bin/bash
# Entrypoint for strom-full Docker image
#
# Starts Xvfb (X Virtual Framebuffer) for headless CEF rendering.
# CEF requires an X server to render HTML content, even in headless mode.
#
# GPU handling:
# The base strom image sets GST_GL_WINDOW=egl-device for headless GPU access.
# strom-full uses Xvfb (X11) for CEF, so we need to adjust GL settings:
# - With GPU: Keep egl-device for GStreamer GL (CUDA-GL interop), fully isolate CEF from GPU
# - Without GPU: Override to x11/glx so GStreamer GL falls back via Xvfb/Mesa
#
# CEF GPU mode (opt-in via STROM_CEF_GPU=1):
# Default is software rendering — safe, portable, near-zero CPU for idle/static
# pages. Set STROM_CEF_GPU=1 to route CEF through ANGLE/Vulkan on the NVIDIA GPU.
# GPU mode has a ~50% CPU floor per 1080p30 cefsrc regardless of page content
# but greatly reduces renderer CPU for heavy canvas/WebGL work (e.g. 95% → 57%
# on a 1080p30 canvas-heavy page). Recommended only for such workloads.
# Requires host + docker run:
#   --gpus all -e NVIDIA_DRIVER_CAPABILITIES=all
#   -v /usr/share/vulkan/icd.d/nvidia_icd.json:/usr/share/vulkan/icd.d/nvidia_icd.json:ro

# Start dbus and avahi-daemon for NDI network discovery
# NDI uses mDNS (Avahi) to discover streams on the local network.
rm -f /run/dbus/pid
mkdir -p /run/dbus
dbus-daemon --system 2>/dev/null
rm -f /run/avahi-daemon/pid
avahi-daemon -D 2>/dev/null

# Chromium flag sets and the Xvfb/cache/shim setup, shared with CI
. /usr/local/lib/strom/cef-env.sh

cef_start_xvfb

# Detect GPU availability (container must be launched with --gpus all)
HAS_GPU=no
if nvidia-smi > /dev/null 2>&1; then HAS_GPU=yes; fi

# Opt-in CEF GPU path via ANGLE/Vulkan (see header comment for the bind-mount).
if [ "${STROM_CEF_GPU:-0}" = "1" ] && [ "$HAS_GPU" = "yes" ]; then
    echo "CEF GPU mode enabled (STROM_CEF_GPU=1) - ANGLE/Vulkan on NVIDIA"
    export GST_CEF_GPU_ENABLED=set
    export GST_CEF_CHROME_EXTRA_FLAGS="$CEF_GPU_FLAGS"
elif [ "$HAS_GPU" = "yes" ]; then
    if [ "${STROM_CEF_GPU:-0}" = "1" ]; then
        echo "WARNING: STROM_CEF_GPU=1 but nvidia-smi unavailable - falling back to software"
    else
        echo "GPU detected - GStreamer uses egl-device; CEF in software (set STROM_CEF_GPU=1 to enable)"
    fi
    # Fully isolate CEF from GPU to prevent SharedImageManager crashes.
    export GST_CEF_CHROME_EXTRA_FLAGS="$CEF_SOFTWARE_FLAGS"
else
    if [ "${STROM_CEF_GPU:-0}" = "1" ]; then
        echo "WARNING: STROM_CEF_GPU=1 but no GPU visible in container (pass --gpus all) - falling back to software"
    else
        echo "No GPU detected - using software rendering for both GStreamer and CEF"
    fi
    # Override base image GL settings to use Xvfb (X11/Mesa software renderer)
    # Without GPU, egl-device will fail since there's no EGL device available
    export GST_GL_WINDOW=x11
    export GST_GL_PLATFORM=glx
    export GST_CEF_CHROME_EXTRA_FLAGS="$CEF_SOFTWARE_FLAGS"
fi

cef_setup_cache

# Enable CEF debug logging
export GST_CEF_LOG_SEVERITY="verbose"

# See cef-env.sh for why
cef_preload_mallinfo_shim

# Wait briefly for Xvfb to initialize
sleep 0.5

# Execute the command (defaults to /app/strom via CMD)
exec "$@"
