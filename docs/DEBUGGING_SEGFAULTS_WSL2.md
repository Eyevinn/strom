# Debugging Segfaults in Strom

This document describes how to debug segfaults in the Strom application, particularly when running under WSL2.

## Quick Reference

When you encounter a segfault:

1. **Check kernel log**: `dmesg | tail -50`
2. **Find core dumps**: Check WSL crash directory
3. **Get backtrace**: Use gdb with the core dump
4. **Analyze**: Look at the crashing thread's stack

## WSL2 Core Dump Locations

WSL2 captures crash dumps in Windows temp directory:

```bash
# Find recent strom crashes
find /mnt/c/Users/*/AppData/Local/Temp/wsl-crashes -name "*strom*.dmp" 2>/dev/null | tail -10

# List crashes with timestamps
ls -lh /mnt/c/Users/$USER/AppData/Local/Temp/wsl-crashes/*.dmp | tail -20
```

**Note**: These `.dmp` files are actually ELF core dumps (despite the extension), not Windows minidumps.

## Getting the Backtrace

### 1. Identify the crash

Check dmesg for the crash message:

```bash
dmesg | grep -A 20 "segfault" | tail -50
```

Look for:
- **Process name**: `tokio-runtime-w[PID]`
- **Segfault address**: `segfault at 25` (the memory address that was accessed)
- **Instruction pointer**: `ip 0x...`
- **Error code**: `error 4` (read access to unmapped memory)

### 2. Find the matching core dump

The PID from dmesg should match the filename:

```bash
# Example: if dmesg shows PID 101341
ls -lh /mnt/c/Users/$USER/AppData/Local/Temp/wsl-crashes/*101341*.dmp
```

### 3. Get the backtrace with gdb

```bash
# Basic backtrace (30 frames)
gdb -batch \
  -ex "bt 30" \
  ./target/debug/strom \
  "/mnt/c/Users/$USER/AppData/Local/Temp/wsl-crashes/wsl-crash-*-PID-*.dmp"

# Full backtrace with all threads
gdb -batch \
  -ex "thread apply all bt" \
  ./target/debug/strom \
  "/mnt/c/Users/$USER/AppData/Local/Temp/wsl-crashes/wsl-crash-*-PID-*.dmp" 2>&1 | head -500

# Get info about all threads first
gdb -batch \
  -ex "info threads" \
  -ex "bt 30" \
  ./target/debug/strom \
  "/mnt/c/Users/$USER/AppData/Local/Temp/wsl-crashes/wsl-crash-*-PID-*.dmp" 2>&1 | tail -100
```

**Important**: Use `target/debug/strom` (not `target/release/strom`) for better stack traces with symbols.

### 4. Analyze the backtrace

The crashing thread will show:
- **Frame 0**: The exact instruction that crashed (often in libc functions like `__strcmp_avx2`, `memcpy`, etc.)
- **Frame 1-3**: GStreamer or system library calls
- **Frame 4+**: Your Rust code

Example crash at `pipeline.rs:1090`:

```
#0  __strcmp_avx2 () at ../sysdeps/x86_64/multiarch/strcmp-avx2.S:283
#1  gst_element_class_get_pad_template ()
#2  gst_element_request_pad_simple ()
#3  gst_element_link_pads_full ()
#4  gstreamer::element::ElementExtManual::link_pads
#5  strom::gst::pipeline::PipelineManager::try_link_elements at backend/src/gst/pipeline.rs:1090
```

**Key indicators**:
- Crash in string comparison (`__strcmp_avx2`, `strlen`) → NULL pointer passed to C function
- Crash in `memcpy`/`memset` → Buffer overflow or NULL buffer
- Crash in GStreamer pad functions → Element not initialized or wrong state

## Understanding Register Values

From dmesg output:

```
RIP: 0033:0x7663ef98afeb          # Instruction pointer (where it crashed)
RSP: 002b:00007663edca6af8        # Stack pointer
RDI: 0000000000000025              # First argument (NULL pointer!)
```

**Common patterns**:
- `RDI: 0x25` or other small value → NULL/invalid pointer dereference
- `RAX: 0x...500000` → Looks like size or flags
- Error code 4 → Read from unmapped memory
- Error code 6 → Write to unmapped memory

## Common GStreamer Segfault Causes

### 1. Element not in correct state

**Symptom**: Crash in `gst_element_request_pad_simple()` or `gst_element_class_get_pad_template()`

**Cause**: Trying to request pads before element is in READY state

**Fix**: Set element or pipeline to READY before requesting pads or linking with `link_pads()`

### 2. Accessing element after pipeline cleanup

**Symptom**: Crash in `g_object_get_property()` or `gst_object_ref()`

**Cause**: Holding Rust reference to GStreamer object after pipeline has been destroyed

**Fix**: Ensure proper lifetime management, drop references before pipeline cleanup

### 3. NULL pad template

**Symptom**: Crash in string comparison when looking up pad templates

**Cause**: Aggregator elements need proper initialization sequence

**Fix**: Use element-level linking or ensure READY state before pad operations

### 4. Race conditions in bus message handling

**Symptom**: Sporadic crashes in `g_main_context_*` or bus message callbacks

**Cause**: Concurrent access to GStreamer objects from multiple threads

**Fix**: Use GStreamer's thread-safe message posting, avoid direct object access from callbacks

## Debug Build vs Release Build

**Always use debug builds when investigating segfaults**:

```bash
# Build debug version (with symbols)
cargo build

# Run with backtrace
RUST_BACKTRACE=full ./target/debug/strom
```

Release builds strip symbols and optimize code, making backtraces harder to read.

## Additional Debug Tools

### GStreamer debug logs

Set GST_DEBUG before running:

```bash
GST_DEBUG=3 \
GST_DEBUG_FILE=gst-debug.log \
RUST_LOG=debug \
cargo run
```

### Strom logging configuration

Edit `.strom.toml`:

```toml
[logging]
file = "strom.log"
level = "debug"  # or "trace" for verbose output
```

### Enable core dumps (alternative to WSL handler)

```bash
# Check current setting
cat /proc/sys/kernel/core_pattern

# If it shows "|/wsl-capture-crash", WSL is handling crashes
# To generate local core files instead:
ulimit -c unlimited
echo "core.%e.%p" | sudo tee /proc/sys/kernel/core_pattern
```

## Checklist for Segfault Investigation

- [ ] Check `dmesg` for crash details (address, error code, PID)
- [ ] Find corresponding core dump in WSL crash directory
- [ ] Run gdb with debug binary and core dump
- [ ] Identify crashing thread and function
- [ ] Look at register values (especially RDI, RSI for function arguments)
- [ ] Check if crash is in GStreamer, libc, or Rust code
- [ ] Examine recent code changes that might affect the crash location
- [ ] Check if element state management changed
- [ ] Verify initialization sequence for GStreamer elements
- [ ] Test with minimal reproduction case

## Reference: Past Segfault Fixes

### Video Compositor NULL Pointer in link_pads (Dec 2025)

**Symptom**:
```
segfault at 25 in libc.so.6
__strcmp_avx2 -> gst_element_class_get_pad_template
```

**Root cause**: Removed pipeline READY state transition before linking, causing pad templates to be uninitialized when `link_pads()` was called.

**Fix**: Restore pipeline READY state transition before element linking for aggregator elements like `glvideomixerelement`.

**Commit**: See commit 54e5b14 for working state.

---

## Quick Commands Summary

```bash
# 1. Find recent crashes
find /mnt/c/Users -name "*strom*.dmp" 2>/dev/null | tail -5

# 2. Check dmesg for PID
dmesg | grep segfault | tail -5

# 3. Get backtrace
gdb -batch -ex "bt 30" \
  ./target/debug/strom \
  "/mnt/c/Users/$USER/AppData/Local/Temp/wsl-crashes/wsl-crash-TIMESTAMP-PID-path-11.dmp" \
  2>&1 | tail -100
```
