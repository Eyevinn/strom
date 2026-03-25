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

# Detect GPU availability and configure GL accordingly
if nvidia-smi > /dev/null 2>&1; then
    echo "GPU detected - GStreamer will use egl-device, CEF uses software rendering"
    # Keep GST_GL_WINDOW=egl-device and GST_GL_PLATFORM=egl from base image
    # GStreamer GL elements (glvideomixer, glupload, etc.) use NVIDIA EGL directly

    # Fully isolate CEF from GPU to prevent SharedImageManager crashes.
    # disable-gpu alone is not enough - Chromium still starts a GPU subprocess that
    # probes the NVIDIA driver and initializes SharedImage mailboxes.
    #
    # Prevent MemoryInfra SIGILL crashes (exit code 132):
    # Chromium's MemoryInfra thread periodically dumps PartitionAlloc stats;
    # in long-running processes the allocator metadata can become inconsistent,
    # causing a CHECK() failure (ud2/SIGILL).
    #
    # NOTE: "disable-background-tracing" does NOT exist as a Chromium switch
    # (verified in Chromium 144 source and libcef.so binary). The correct
    # approach is to disable the BackgroundTracing feature flag and prevent
    # periodic tasks from scheduling memory dumps:
    #   disable-features=BackgroundTracing  - disables the feature entirely
    #   no-periodic-tasks                   - prevents periodic dump scheduling
    #   force-fieldtrials=                  - clears all field trial configs
    # See docs/CEF_SIGILL_CRASH.md for full investigation.
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
else
    echo "No GPU detected - using software rendering for both GStreamer and CEF"
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

# Wait briefly for Xvfb to initialize
sleep 0.5

# Execute the command (defaults to /app/strom via CMD)
exec "$@"
