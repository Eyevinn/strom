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
ls -lh /mnt/c/Users/$(whoami)/AppData/Local/Temp/wsl-crashes/*.dmp | tail -20
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
ls -lh /mnt/c/Users/$(whoami)/AppData/Local/Temp/wsl-crashes/*101341*.dmp
```

### 3. Get the backtrace with gdb

```bash
# Basic backtrace (30 frames)
gdb -batch \
  -ex "bt 30" \
  ./target/debug/strom \
  "/mnt/c/Users/$(whoami)/AppData/Local/Temp/wsl-crashes/wsl-crash-*-PID-*.dmp"

# Full backtrace with all threads
gdb -batch \
  -ex "thread apply all bt" \
  ./target/debug/strom \
  "/mnt/c/Users/$(whoami)/AppData/Local/Temp/wsl-crashes/wsl-crash-*-PID-*.dmp" 2>&1 | head -500

# Get info about all threads first
gdb -batch \
  -ex "info threads" \
  -ex "bt 30" \
  ./target/debug/strom \
  "/mnt/c/Users/$(whoami)/AppData/Local/Temp/wsl-crashes/wsl-crash-*-PID-*.dmp" 2>&1 | tail -100
```

**Important**: Use `target/debug/strom` (not `target/release/strom`) for better stack traces with symbols.

