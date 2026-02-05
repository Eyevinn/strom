#!/bin/bash
# Entrypoint for strom-full Docker image
#
# Starts Xvfb (X Virtual Framebuffer) for headless CEF rendering.
# CEF requires an X server to render HTML content, even in headless mode.

# Clean up stale X server lock files from previous runs/crashes
rm -f /tmp/.X99-lock /tmp/.X11-unix/X99 2>/dev/null

# Start Xvfb on display :99 with 1920x1080 resolution
Xvfb :99 -screen 0 1920x1080x24 &
export DISPLAY=:99

# Disable CEF sandbox (required when running as root in Docker)
# GST_CEF_SANDBOX doesn't work reliably, pass --no-sandbox directly to Chromium
# Also enable logging to stderr for debugging
export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,disable-gpu,enable-logging=stderr"

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
