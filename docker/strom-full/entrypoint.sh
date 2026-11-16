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
# Requires NVIDIA's Vulkan ICD in the container. With Docker:
#   --gpus all -e NVIDIA_DRIVER_CAPABILITIES=all
#   -v /usr/share/vulkan/icd.d/nvidia_icd.json:/usr/share/vulkan/icd.d/nvidia_icd.json:ro
# On Kubernetes nodes whose device plugin mounts the driver at /usr/local/nvidia,
# the ICD in /usr/local/nvidia/vulkan/icd.d is added below. Without an ICD,
# CEF falls back to software.

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

# Detect GPU availability: Docker with --gpus all, or Kubernetes with an
# nvidia.com/gpu limit. The strom image puts /usr/local/nvidia/bin on PATH for
# device plugins that mount the driver there. /dev/nvidiactl also counts, so a
# GPU is found where nvidia-smi is not on PATH (a strom-full built on an older
# strom image).
HAS_GPU=no
if nvidia-smi > /dev/null 2>&1 || [ -e /dev/nvidiactl ]; then HAS_GPU=yes; fi

# Kubernetes device plugins without the NVIDIA container toolkit mount the
# Vulkan ICD at /usr/local/nvidia/vulkan/icd.d, where the Vulkan loader does
# not look. Add it. VK_ADD_DRIVER_FILES needs Vulkan loader 1.3.234 or later;
# the loader CEF bundles (Chromium 144) knows it.
for icd in /usr/local/nvidia/vulkan/icd.d/*.json; do
    [ -f "$icd" ] && VK_ADD_DRIVER_FILES="${VK_ADD_DRIVER_FILES:+$VK_ADD_DRIVER_FILES:}$icd"
done
[ -n "$VK_ADD_DRIVER_FILES" ] && export VK_ADD_DRIVER_FILES

# True if the Vulkan loader can see an NVIDIA ICD: a manifest named in the
# loader's variables, or one in its default search directories.
has_nvidia_vulkan_icd() {
    local f
    for f in $(echo "${VK_DRIVER_FILES:-}:${VK_ICD_FILENAMES:-}:${VK_ADD_DRIVER_FILES:-}" | tr ':' ' ') \
             /etc/xdg/vulkan/icd.d/*.json /etc/vulkan/icd.d/*.json \
             /usr/local/share/vulkan/icd.d/*.json /usr/share/vulkan/icd.d/*.json; do
        [ -f "$f" ] && grep -qi nvidia "$f" && return 0
    done
    return 1
}

# Opt-in CEF GPU path via ANGLE/Vulkan.
# ANGLE-over-Vulkan bypasses X11/DRI3 (which Xvfb lacks); it needs NVIDIA's
# Vulkan ICD visible in the container (see header comment). Without one,
# CEF stays in software and GStreamer keeps the GPU.
CEF_VULKAN=no
if [ "${STROM_CEF_GPU:-0}" = "1" ] && [ "$HAS_GPU" = "yes" ]; then
    if has_nvidia_vulkan_icd; then
        CEF_VULKAN=yes
    else
        echo "WARNING: STROM_CEF_GPU=1 but no NVIDIA Vulkan ICD found (see docs/DOCKER_GPU_SETUP.md) - CEF falls back to software"
    fi
fi

if [ "$CEF_VULKAN" = "yes" ]; then
    echo "CEF GPU mode enabled (STROM_CEF_GPU=1) - ANGLE/Vulkan on NVIDIA"
    export GST_CEF_GPU_ENABLED=set
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,use-gl=angle,use-angle=vulkan,enable-gpu-rasterization,ignore-gpu-blocklist,enable-zero-copy,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
elif [ "$HAS_GPU" = "yes" ]; then
    if [ "${STROM_CEF_GPU:-0}" != "1" ]; then
        echo "GPU detected - GStreamer uses egl-device; CEF in software (set STROM_CEF_GPU=1 to enable)"
    fi
    # Fully isolate CEF from GPU to prevent SharedImageManager crashes.
    # disable-gpu alone is not enough - Chromium still starts a GPU subprocess that
    # probes the NVIDIA driver and initializes SharedImage mailboxes.
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
else
    if [ "${STROM_CEF_GPU:-0}" = "1" ]; then
        echo "WARNING: STROM_CEF_GPU=1 but no GPU visible in container (Docker: pass --gpus all; Kubernetes: request nvidia.com/gpu; see docs/DOCKER_GPU_SETUP.md) - falling back to software"
    else
        echo "No GPU detected - using software rendering for both GStreamer and CEF"
    fi
    # Override base image GL settings to use Xvfb (X11/Mesa software renderer)
    # Without GPU, egl-device will fail since there's no EGL device available
    export GST_GL_WINDOW=x11
    export GST_GL_PLATFORM=glx
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
fi

# CEF cache directory. It holds the browser profiles HTML sources keep their
# logins in, so it is kept across a restart of the container; mount a volume
# here (or point GST_CEF_CACHE_LOCATION at one) to keep it across a
# replacement too. It is not wiped; Strom removes the profiles no flow uses
# at startup.
export GST_CEF_CACHE_LOCATION="${GST_CEF_CACHE_LOCATION:-/tmp/cef-cache}"
mkdir -p "$GST_CEF_CACHE_LOCATION"

# A restarted container keeps its hostname and often its pids, so Chromium's
# lock from the previous run (a SingletonLock symlink to "<host>-<pid>") can
# look live. Nothing in this container runs Chromium yet, so a lock naming
# this host is stale: drop it here. A lock naming another host is left for
# Strom, which clears it before CEF starts.
CEF_LOCK="$GST_CEF_CACHE_LOCATION/SingletonLock"
if [ -L "$CEF_LOCK" ] && [ "$(readlink "$CEF_LOCK" | sed 's/-[0-9]*$//')" = "$(uname -n)" ]; then
    rm -f "$CEF_LOCK" \
          "$GST_CEF_CACHE_LOCATION/SingletonSocket" \
          "$GST_CEF_CACHE_LOCATION/SingletonCookie"
fi

# CEF's own logging, to stderr. Warnings and errors only: verbose writes
# everything Chromium does into the container log. Chromium logs a page's
# console messages at info, console.error included, so warning also keeps a
# page logging in a loop out of the log. GST_CEF_LOG_SEVERITY=info shows them.
export GST_CEF_LOG_SEVERITY="${GST_CEF_LOG_SEVERITY:-warning}"

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
