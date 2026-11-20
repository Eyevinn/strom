#!/bin/bash
# Entrypoint for strom-full Docker image
#
# CEF needs an X server to render HTML, even offscreen. Which one decides
# whether Chromium renders on the GPU:
#
# - NVIDIA GPU visible (Docker --gpus all, Kubernetes nvidia.com/gpu): start
#   Xorg on the GPU with the NVIDIA X driver the NVIDIA container toolkit
#   mounts (display capability; the image sets NVIDIA_DRIVER_CAPABILITIES=all).
#   That X server has DRI3, so Chromium renders on the GPU and hands each
#   frame to cefsrc as a dma-buf, which cefsrc copies into GL memory: an HTML
#   source feeds a GL mixer without a round trip through the CPU. This is the
#   default; STROM_CEF_GPU=0 turns it off.
# - Otherwise, or when Xorg does not come up: Xvfb, and Chromium paints in
#   software, as before. cefsrc's output is the same either way; it also falls
#   back by itself if GPU frames stop arriving or cannot be imported.
#
# GStreamer GL keeps egl-device with a GPU (CUDA-GL interop); without one it
# goes through Xvfb/Mesa (x11/glx).
#
# STROM_CEF_GPU=1 additionally allows ANGLE/Vulkan under Xvfb when Xorg cannot
# start (needs NVIDIA's Vulkan ICD in the container; on Kubernetes nodes whose
# device plugin mounts the driver at /usr/local/nvidia, the ICD there is added
# below). Frames then come back through the CPU.

# Start dbus and avahi-daemon for NDI network discovery
# NDI uses mDNS (Avahi) to discover streams on the local network.
rm -f /run/dbus/pid
mkdir -p /run/dbus
dbus-daemon --system 2>/dev/null
rm -f /run/avahi-daemon/pid
avahi-daemon -D 2>/dev/null

# Clean up stale X server lock files from previous runs/crashes
rm -f /tmp/.X99-lock /tmp/.X11-unix/X99 /tmp/.X98-lock /tmp/.X11-unix/X98 2>/dev/null

# Detect GPU availability: Docker with --gpus all, or Kubernetes with an
# nvidia.com/gpu limit. The strom image puts /usr/local/nvidia/bin on PATH for
# device plugins that mount the driver there. A device node alone does not
# count: without the driver libraries, egl-device fails where the software
# branch below would fall back to Mesa.
HAS_GPU=no
if nvidia-smi > /dev/null 2>&1; then HAS_GPU=yes; fi

# Kubernetes device plugins without the NVIDIA container toolkit mount the
# Vulkan ICD at /usr/local/nvidia/vulkan/icd.d, where the Vulkan loader does
# not look. Add it. VK_ADD_DRIVER_FILES needs Vulkan loader 1.3.234 or later;
# the loader CEF bundles knows it.
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

# Start Xorg on the NVIDIA GPU, headless, on display :98. The X driver comes
# from the host through the container toolkit, so it always matches the
# kernel driver; it lands under the host's module path, which Xorg is told.
# Returns non-zero, with the reason on stdout, when that is not possible.
start_nvidia_xorg() {
    # Rendering on the GPU pays only when cefsrc takes Chromium's frames as GPU
    # buffers (Strom's gl-output patch). An older gstcefsrc would have them
    # read back through the CPU, which costs more than software on most pages.
    grep -q '^STROM_PATCHES=.*gl-output' /usr/local/version.txt 2>/dev/null \
        || { echo "this gstcefsrc build has no GPU frame handover (gl-output)"; return 1; }
    command -v Xorg > /dev/null || { echo "no Xorg in the image"; return 1; }
    local drv="" d
    for d in /usr/lib64/xorg/modules /usr/lib/xorg/modules /usr/lib/x86_64-linux-gnu/xorg/modules \
             /usr/local/nvidia/lib64/xorg/modules /usr/local/nvidia/lib/xorg/modules; do
        [ -f "$d/drivers/nvidia_drv.so" ] && { drv="$d"; break; }
    done
    [ -n "$drv" ] || { echo "no NVIDIA X driver (nvidia_drv.so) in the container: the NVIDIA container toolkit mounts it with the display capability (NVIDIA_DRIVER_CAPABILITIES=all)"; return 1; }
    # The first GPU nvidia-smi lists (00000000:00:1E.0 = domain:bus:device.function).
    # GStreamer's egl-device display takes EGL device 0; on a pod with several
    # GPUs these may differ, and cefsrc then falls back to CPU frames.
    local bus dom b dev fn busid
    bus=$(nvidia-smi --query-gpu=pci.bus_id --format=csv,noheader 2>/dev/null | head -1)
    IFS=':.' read -r dom b dev fn <<< "$bus"
    [ -n "$fn" ] || { echo "could not read the GPU's PCI bus id"; return 1; }
    busid="PCI:$((16#$b)):$((16#$dev)):$((16#$fn))"
    [ "$((16#$dom))" != 0 ] && busid="PCI:$((16#$b))@$((16#$dom)):$((16#$dev)):$((16#$fn))"
    cat > /tmp/xorg-strom.conf <<CONF
Section "Files"
  ModulePath "$drv"
  ModulePath "/usr/lib/xorg/modules"
EndSection
Section "ServerLayout"
  Identifier "Layout"
  Screen 0 "Screen"
EndSection
Section "Device"
  Identifier "GPU"
  Driver "nvidia"
  BusID "$busid"
  Option "AllowEmptyInitialConfiguration" "True"
  Option "UseDisplayDevice" "None"
EndSection
Section "Screen"
  Identifier "Screen"
  Device "GPU"
  DefaultDepth 24
  SubSection "Display"
    Depth 24
    Virtual 1920 1080
  EndSubSection
EndSection
CONF
    Xorg :98 -ac -noreset -nolisten tcp -config /tmp/xorg-strom.conf -logfile /tmp/Xorg.98.log > /dev/null 2>&1 &
    local pid=$! i
    for i in $(seq 50); do
        [ -S /tmp/.X11-unix/X98 ] && return 0
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    kill "$pid" 2>/dev/null
    echo "Xorg did not start: $(grep -m1 '(EE)' /tmp/Xorg.98.log 2>/dev/null | sed 's/^\[[^]]*\] //')"
    return 1
}

# NVIDIA's GBM backend, which Chromium allocates its GPU frames through. The
# toolkit mounts it under the host's library path, where Ubuntu's libgbm does
# not look.
for d in /usr/lib64/gbm /usr/local/nvidia/lib64/gbm /usr/lib/x86_64-linux-gnu/gbm; do
    [ -f "$d/nvidia-drm_gbm.so" ] && { export GBM_BACKENDS_PATH="$d"; break; }
done

CEF_BASE_FLAGS="disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
CEF_SOFTWARE_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,$CEF_BASE_FLAGS"

CEF_MODE=software
if [ "$HAS_GPU" = "yes" ] && [ "${STROM_CEF_GPU:-auto}" != "0" ]; then
    if XORG_WHY=$(start_nvidia_xorg); then
        CEF_MODE=xorg
    else
        echo "GPU detected, but HTML stays in software: $XORG_WHY"
        if [ "${STROM_CEF_GPU:-auto}" = "1" ]; then
            if has_nvidia_vulkan_icd; then
                CEF_MODE=vulkan
            else
                echo "WARNING: STROM_CEF_GPU=1 but no NVIDIA Vulkan ICD found either (see docs/DOCKER_GPU_SETUP.md)"
            fi
        fi
    fi
fi

if [ "$CEF_MODE" != "xorg" ]; then
    # Start Xvfb on display :99 with 1920x1080 resolution
    Xvfb :99 -screen 0 1920x1080x24 &
    export DISPLAY=:99
fi

case "$CEF_MODE" in
xorg)
    echo "GPU detected - HTML renders on the GPU (Xorg on the NVIDIA driver), frames stay in GPU memory"
    export DISPLAY=:98
    export GST_CEF_GPU_ENABLED=set
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,use-gl=angle,use-angle=gl-egl,ozone-platform=x11,enable-gpu-rasterization,ignore-gpu-blocklist,enable-zero-copy,$CEF_BASE_FLAGS"
    ;;
vulkan)
    echo "CEF GPU mode (STROM_CEF_GPU=1) - ANGLE/Vulkan on NVIDIA under Xvfb, frames come back through the CPU"
    export GST_CEF_GPU_ENABLED=set
    # Xvfb has no DRI3: Chromium cannot allocate frames to share.
    export GST_CEF_SHARED_TEXTURE=0
    export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,use-gl=angle,use-angle=vulkan,enable-gpu-rasterization,ignore-gpu-blocklist,enable-zero-copy,$CEF_BASE_FLAGS"
    ;;
*)
    if [ "$HAS_GPU" = "yes" ]; then
        [ "${STROM_CEF_GPU:-auto}" = "0" ] && echo "GPU detected - GStreamer uses egl-device; HTML in software (STROM_CEF_GPU=0)"
        # Fully isolate CEF from GPU to prevent SharedImageManager crashes.
        # disable-gpu alone is not enough - Chromium still starts a GPU subprocess that
        # probes the NVIDIA driver and initializes SharedImage mailboxes.
    else
        if [ "${STROM_CEF_GPU:-auto}" = "1" ]; then
            echo "WARNING: STROM_CEF_GPU=1 but no GPU visible in container (Docker: pass --gpus all; Kubernetes: request nvidia.com/gpu; see docs/DOCKER_GPU_SETUP.md) - falling back to software"
        else
            echo "No GPU detected - using software rendering for both GStreamer and CEF"
        fi
        # Override base image GL settings to use Xvfb (X11/Mesa software renderer)
        # Without GPU, egl-device will fail since there's no EGL device available
        export GST_GL_WINDOW=x11
        export GST_GL_PLATFORM=glx
    fi
    export GST_CEF_CHROME_EXTRA_FLAGS="$CEF_SOFTWARE_FLAGS"
    ;;
esac

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
