# Runtime environment for cefsrc (gstcefsrc) on headless Linux.
#
# Sourced, not executed. Shared by the strom-full entrypoint and the CI job
# that tests against a real cefsrc, so the Chromium flags CI runs with are the
# ones production runs with.

# Chromium flags common to every mode.
CEF_COMMON_FLAGS="disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"

# Software rendering. Fully isolates CEF from any GPU: disable-gpu alone is not
# enough - Chromium still starts a GPU subprocess that probes the NVIDIA driver
# and initializes SharedImage mailboxes, which crashes in SharedImageManager.
CEF_SOFTWARE_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,${CEF_COMMON_FLAGS}"

# ANGLE-over-Vulkan on NVIDIA. Bypasses X11/DRI3 (which Xvfb lacks); needs
# NVIDIA's Vulkan ICD visible in the container.
CEF_GPU_FLAGS="no-sandbox,use-gl=angle,use-angle=vulkan,enable-gpu-rasterization,ignore-gpu-blocklist,enable-zero-copy,${CEF_COMMON_FLAGS}"

# Start Xvfb on display :99. CEF requires an X server to render HTML content,
# even in windowless mode.
cef_start_xvfb() {
    # Clean up stale X server lock files from previous runs/crashes
    rm -f /tmp/.X99-lock /tmp/.X11-unix/X99 2>/dev/null
    Xvfb :99 -screen 0 1920x1080x24 &
    export DISPLAY=:99
}

# CEF cache directory. It holds the browser profiles HTML sources keep their
# logins in, so it is kept across a restart of the container; mount a volume
# here (or point GST_CEF_CACHE_LOCATION at one) to keep it across a
# replacement too. It is not wiped; Strom removes the profiles no flow uses
# at startup.
#
# A restarted container keeps its hostname and often its pids, so Chromium's
# lock from the previous run (a SingletonLock symlink to "<host>-<pid>") can
# look live. Nothing in this container runs Chromium yet, so a lock naming
# this host is stale: drop it here. A lock naming another host is left for
# Strom, which clears it before CEF starts.
cef_setup_cache() {
    export GST_CEF_CACHE_LOCATION="${GST_CEF_CACHE_LOCATION:-/tmp/cef-cache}"
    mkdir -p "$GST_CEF_CACHE_LOCATION"

    CEF_LOCK="$GST_CEF_CACHE_LOCATION/SingletonLock"
    if [ -L "$CEF_LOCK" ] && [ "$(readlink "$CEF_LOCK" | sed 's/-[0-9]*$//')" = "$(uname -n)" ]; then
        rm -f "$CEF_LOCK" \
              "$GST_CEF_CACHE_LOCATION/SingletonSocket" \
              "$GST_CEF_CACHE_LOCATION/SingletonCookie"
    fi
}

# LD_PRELOAD the mallinfo shim to neutralise the MemoryInfra SIGILL crash.
# libcef.so was built against an old sysroot and calls glibc's int-based
# mallinfo(); when the CEF process arena exceeds 2 GiB, the ints overflow to
# negative values, Chromium checked_casts them to size_t, and CHECK()s -> SIGILL.
# The shim returns zeroed values so the cast succeeds harmlessly.
# Reference: https://github.com/chromiumembedded/cef/issues/3963
cef_preload_mallinfo_shim() {
    if [ -f /usr/local/lib/cef/libmallinfo_shim.so ]; then
        export LD_PRELOAD="/usr/local/lib/cef/libmallinfo_shim.so${LD_PRELOAD:+:$LD_PRELOAD}"
    fi
}
