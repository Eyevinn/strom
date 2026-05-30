# CEF SIGILL Crash (MemoryInfra / PartitionAlloc)

> **Archived — solved.** Fixed in our `strom-full` Docker image via the `mallinfo`
> LD_PRELOAD shim (see `docker/gstcefsrc/Dockerfile` and `docker/strom-full/entrypoint.sh`).
> Kept as a reference for anyone running gstcefsrc in a container who hits the same crash.

## Summary

gstcefsrc running in Docker with NVIDIA GPU crashes intermittently with
exit code 132 (SIGILL). The crash interval is unpredictable: sometimes hours,
sometimes weeks. Starting and stopping flows with CEF sources increases the
likelihood of triggering the crash.

## Root Cause

The SIGILL is not a real illegal instruction. It is Chromium's `CHECK()` macro
deliberately executing `ud2` when an internal assertion fails.

The crash occurs in Chromium's **MemoryInfra** background tracing thread:

```
SIGILL, Illegal instruction
#0  OnMemoryDump() at malloc_dump_provider.cc:465
#1  InvokeOnMemoryDump() at memory_dump_manager.cc:460
#2  ContinueAsyncProcessDump() at memory_dump_manager.cc:377
```

### The actual bug (identified 2025-10, CEF issue [#3963](https://github.com/chromiumembedded/cef/issues/3963))

`MallocDumpProvider::OnMemoryDump()` calls glibc's **legacy `mallinfo()`**,
not `mallinfo2()`. Spotify's official CEF builds compile against a Debian
bullseye sysroot (glibc 2.31) which lacks `mallinfo2`, so the int-based
API is what ends up baked into `libcef.so` regardless of the host's glibc.

Once the CEF process's arena exceeds **2 GiB** (INT_MAX ≈ 2.147 GB), the
int fields overflow to negative values. Chromium narrows them via
`checked_cast<size_t>(int)`, the narrowing check fails, and Chromium
CHECKs — executing `ud2` → SIGILL.

Core dump files are named `core.MemoryInfra.*`, confirming the crashing thread.

This is not truly a Chrome-runtime regression, even though Chrome runtime
(CEF 127+) made it much more visible by running more long-lived allocations.
Both Alloy- and Chrome-runtime builds can hit it given enough memory.

## Previous symptoms

Before the SIGILL crash, Chromium's GPU process logs:
```
SharedImageManager::ProduceMemory: Trying to Produce a Memory representation from a non-existent mailbox.
```
These are a separate GPU process race condition (shared textures destroyed before
consumers finish), but indicate the kind of internal instability that can leave
allocator state inconsistent.

The final lines before the crash typically show GPU probing:
```
pci id for fd 9: 10de:2204, driver (null)
```

## Known issue

Tracked in CEF issue [#3963](https://github.com/chromiumembedded/cef/issues/3963)
(closed 2025-10 as "not planned") and upstream Chromium bug
[401168177](https://issues.chromium.org/issues/401168177) (open, no progress
as of 2026-04).

## Fix: LD_PRELOAD mallinfo shim

Since the bug is an int overflow of `mallinfo()`'s return fields, the simplest
fix is to interpose `mallinfo()` and return zeroed values. Chromium then
narrows 0 to size_t without any CHECK() failure, and the memory dump records
zero bytes for the CEF process (we don't use MemoryInfra profiling in
production).

The shim source is `docker/gstcefsrc/mallinfo_shim.c`. It is compiled to
`libmallinfo_shim.so` during the gstcefsrc build and shipped alongside the
CEF binaries in the release tarball. `docker/strom-full/entrypoint.sh`
injects it via `LD_PRELOAD` before `exec`ing the strom binary.

### Why this is safe for the rest of the stack

`LD_PRELOAD` only replaces the specific symbol `mallinfo()`; all other
allocator entry points (malloc/free/calloc/realloc) are untouched. GStreamer,
GLib, Rust's allocator interface, and our own code do not call `mallinfo()`.
The only consumer in our process tree is Chromium's MemoryInfra thread —
which is exactly what we want to silence.

### This was confirmed by another CEF user

From CEF [#3963](https://github.com/chromiumembedded/cef/issues/3963#issuecomment-3677232632):
> "We are working around it for now by LD_PRELOADing a small lib which
> interposes mallinfo and basically does pad the reported values, working
> fine for now."

### Why not downgrade to an older CEF?

Earlier attempts downgraded to CEF 122 / 126 (pre-Chrome-runtime), believing
this was a Chrome-runtime regression. That does avoid the crash, but:

- Gives up ~20 Chromium versions of security patches and web platform features.
- Pins to an old gstcefsrc commit (`0e470f51fd`, 2024-10); no bugfixes.
- CEF 123–126 introduced ABI changes (e.g. `OnRenderProcessTerminated` added
  `error_code`/`error_string` params in CEF 126) that break that gstcefsrc
  pin, forcing CEF 122 specifically.

The shim targets the real bug at a lower level and keeps us on modern CEF.

### Defense-in-depth flags

The Chromium flags in `entrypoint.sh` reduce how often MemoryInfra runs,
which lowers the probability of hitting the overflow path even without the
shim. They are retained because they're harmless and provide a second line
of defense:

### Important: `disable-background-tracing` does not exist

The flag `--disable-background-tracing` was used in earlier attempts but **does
not exist as a Chromium switch**. Verified by:
1. Checking `components/tracing/common/tracing_switches.cc` in Chromium source
   (only `enable-background-tracing` exists, as an opt-in flag)
2. Binary string search of `libcef.so` (Chromium 144) confirms the string
   `disable-background-tracing` is absent

Chromium silently ignores unknown switches, so this flag had no effect.

### Working flags

The correct approach uses three mechanisms to prevent MemoryInfra from running:

| Flag | Purpose |
|------|---------|
| `disable-features=BackgroundTracing` | Disables the BackgroundTracing feature flag, preventing automatic trace sessions |
| `no-periodic-tasks` | Prevents periodic task scheduling, including MemoryDumpScheduler ticks |
| `force-fieldtrials=` | Clears all field trial configurations that could enable tracing |
| `disable-field-trial-config` | Prevents field trials from being loaded |
| `disable-breakpad` | Disables crash reporting (not needed in production) |
| `disable-crash-reporter` | Same as above |
| `disable-dev-shm-usage` | Avoids Docker's limited /dev/shm (default 64MB) |
| `disable-background-networking` | Reduces background activity |
| `disable-component-update` | Disables component updater |

For gstcefsrc, set via environment variable (without `--` prefix):
```bash
export GST_CEF_CHROME_EXTRA_FLAGS="no-sandbox,disable-gpu,disable-gpu-compositing,use-gl=disabled,disable-features=BackgroundTracing,no-periodic-tasks,force-fieldtrials=,disable-field-trial-config,disable-breakpad,disable-crash-reporter,disable-dev-shm-usage,disable-background-networking,disable-component-update,enable-logging=stderr"
```

## Investigation commands

Check for core dumps:
```bash
find /tmp -name 'core.*' -type f
```

Analyze with gdb (install in container if needed):
```bash
gdb -batch -ex 'bt' /app/strom /tmp/core.MemoryInfra.*
```

Check for mailbox errors:
```bash
docker logs <container> 2>&1 | grep -i mailbox
```
