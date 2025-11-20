# Windows Troubleshooting Guide

## GStreamer Plugin Panics on Startup

### Problem

When running `strom-backend.exe` on Windows, you may encounter a panic error like:

```
thread '<unnamed>' panicked at net\hlssink3\src\hlssink3\imp.rs:113:14:
Could not make element mpegtsmux: BoolError { message: "Failed to find element factory with name 'mpegtsmux'..."
thread caused non-unwinding panic. aborting.
```

### Cause

Some GStreamer Rust plugins (like `hlssink3`, `awstranscriber`, etc.) panic during initialization if their required dependencies aren't available in your GStreamer installation. For example:

- `hlssink3` requires `mpegtsmux` (from gst-plugins-bad)
- Other plugins may require additional muxers, parsers, or codecs

### Solutions

#### Solution 1: Install Complete GStreamer Runtime (Recommended)

Install the complete GStreamer runtime with all plugins:

1. Download GStreamer from https://gstreamer.freedesktop.org/download/
2. Install both:
   - **MSVC 64-bit runtime installer** (includes all plugins)
   - **MSVC 64-bit development installer** (if building from source)
3. Choose "Complete" installation (not "Typical")
4. Verify installation: `gst-inspect-1.0 mpegtsmux`

#### Solution 2: Set Environment Variables

Strom automatically sets `GST_REGISTRY_FORK=no` on Windows to help isolate plugin loading issues. The backend will attempt to continue running even if some plugins fail to load.

If you continue to have issues, you can:

1. **Skip registry update**: Set `GST_REGISTRY_UPDATE=no` before running
   ```
   $env:GST_REGISTRY_UPDATE = "no"
   .\strom-backend.exe
   ```

2. **Use a custom plugin path**: Point to a directory with only working plugins
   ```
   $env:GST_PLUGIN_PATH = "C:\path\to\working\plugins"
   .\strom-backend.exe
   ```

#### Solution 3: Remove Problematic Plugins

As a workaround, you can temporarily rename problematic plugin DLLs to prevent them from loading:

1. Find your GStreamer plugin directory (e.g., `D:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0\`)
2. Rename problematic plugins:
   ```
   cd "D:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0\"
   ren gsthlssink3.dll gsthlssink3.dll.disabled
   ren gstaws.dll gstaws.dll.disabled
   ```
3. Run Strom backend
4. Restore plugins when needed:
   ```
   ren gsthlssink3.dll.disabled gsthlssink3.dll
   ren gstaws.dll.disabled gstaws.dll
   ```

**Note**: This will make these plugins unavailable for use, but Strom will start successfully.

### Known Problematic Plugins on Windows

- `hlssink3` - Requires mpegtsmux
- `awstranscriber` - May have dependency issues
- `gesdemux`, `gessrc` - GES initialization issues

---

## WebRTC Plugin Type Registration Collision

### Problem

You may encounter a panic after GStreamer initializes successfully:

```
INFO GStreamer initialized
thread 'main' panicked at glib\src\subclass\types.rs:1030:13:
assertion `left == right` failed: Type GstWebRTCSinkPad has already been registered
  left: 2573685070512
  right: 0
thread 'main' panicked at backend\src\main.rs:68:43:
Could not register webrtc plugins: BoolError
```

### Cause

Windows GStreamer installations often include `gstrswebrtc.dll` in the plugin directory. When GStreamer initializes, it dynamically loads this DLL and registers the WebRTC types. If the backend then tries to register the same types statically, it causes a type collision panic.

This is a classic static vs. dynamic linking conflict.

### Solution

**This issue is now fixed** in Strom v0.1.0+. The backend automatically detects if the WebRTC plugin is already loaded dynamically and skips static registration to avoid the collision.

If you still encounter this issue:

1. **Update to the latest version** of Strom
2. **Or temporarily remove the DLL**: Rename `gstrswebrtc.dll` in your GStreamer plugin directory:
   ```powershell
   cd "D:\gstreamer\1.0\msvc_x86_64\lib\gstreamer-1.0\"
   ren gstrswebrtc.dll gstrswebrtc.dll.disabled
   ```
   The backend will then register the plugin statically instead.

### Technical Details

The fix checks the GStreamer registry before attempting static registration:

```rust
// Check if rswebrtc plugin is already registered
if registry.find_plugin("rswebrtc").is_some() {
    // Already loaded dynamically, skip static registration
    return;
}
// Not found, register it statically
gstrswebrtc::plugin_register_static()?;
```

---

## Verifying Your Fix

After applying a solution, verify Strom starts correctly:

```powershell
.\strom-backend.exe
```

You should see:
```
GStreamer initialized
Server listening on 0.0.0.0:3000
```

If you still see panics, please report them on our GitHub issues page with:
- Full error message
- GStreamer version (`gst-inspect-1.0 --version`)
- Windows version
- Output of `gst-inspect-1.0 --print-all > plugins.txt`
