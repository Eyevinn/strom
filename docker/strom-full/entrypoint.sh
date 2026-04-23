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

# Clean up stale X server lock files from previous runs/crashes
rm -f /tmp/.X99-lock /tmp/.X11-unix/X99 2>/dev/null

# Start Xvfb on display :99 with 1920x1080 resolution
Xvfb :99 -screen 0 1920x1080x24 &
export DISPLAY=:99

# Detect GPU availability (container must be launched with --gpus all)
HAS_GPU=no
if nvidia-smi > /dev/null 2>&1; then HAS_GPU=yes; fi

# Opt-in CEF GPU path via ANGLE/Vulkan.
# ANGLE-over-Vulkan bypasses X11/DRI3 (which Xvfb lacks); it needs NVIDIA's
# Vulkan ICD visible in the container (see header comment for the bind-mount).
if [ "${STROM_CEF_GPU:-0}" = "1" ] && [ "$HAS_GPU" = "yes" ]; then
    echo "CEF GPU mode enabled (STROM_CEF_GPU=1) - ANGLE/Vulkan on NVIDIA"
    export GST_CEF_GPU_ENABLED=set
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,use-gl=angle,use-angle=vulkan,enable-gpu-rasterization,ignore-gpu-blocklist,enable-zero-copy,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
elif [ "$HAS_GPU" = "yes" ]; then
    if [ "${STROM_CEF_GPU:-0}" = "1" ]; then
        echo "WARNING: STROM_CEF_GPU=1 but nvidia-smi unavailable - falling back to software"
    else
        echo "GPU detected - GStreamer uses egl-device; CEF in software (set STROM_CEF_GPU=1 to enable)"
    fi
    # Fully isolate CEF from GPU to prevent SharedImageManager crashes.
    # disable-gpu alone is not enough - Chromium still starts a GPU subprocess that
    # probes the NVIDIA driver and initializes SharedImage mailboxes.
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
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
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
fi

# Set CEF cache location to avoid singleton behavior warning
# Clean up stale CEF cache/locks from previous runs/crashes
export GST_CEF_CACHE_LOCATION="/tmp/cef-cache"
rm -rf /tmp/cef-cache
mkdir -p /tmp/cef-cache

# Enable CEF debug logging
export GST_CEF_LOG_SEVERITY="verbose"

# LD_PRELOAD the mallinfo shim to neutralise the MemoryInfra SIGILL crash.
# libcef.so was built against an old sysroot and calls glibc's int-based
# mallinfo(); when the CEF process arena exceeds 2 GiB, the ints overflow to
# negative values, Chromium checked_casts them to size_t, and CHECK()s -> SIGILL.
# The shim returns zeroed values so the cast succeeds harmlessly.
# Reference: https://github.com/chromiumembedded/cef/issues/3963
if [ -f /usr/local/lib/cef/libmallinfo_shim.so ]; then
    export LD_PRELOAD="/usr/local/lib/cef/libmallinfo_shim.so${LD_PRELOAD:+:$LD_PRELOAD}"
fi

# Wait briefly for Xvfb to initialize
sleep 0.5

# Execute the command (defaults to /app/strom via CMD)
exec "$@"
