# CEF SIGILL Crash (MemoryInfra / PartitionAlloc)

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

MemoryInfra periodically collects memory statistics from **PartitionAlloc**
(Chromium's memory allocator since ~M116, replacing tcmalloc).
`MallocDumpProvider::OnMemoryDump()` walks PartitionAlloc's internal metadata
to gather allocation stats. In long-running, high-throughput rendering processes
(like gstcefsrc), the metadata can end up in an inconsistent state. When the
dump provider encounters this, the CHECK fails and Chromium crashes.

Core dump files are named `core.MemoryInfra.*`, confirming the crashing thread.

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

Reported on the CEF Forum for CEF 127+ (which introduced the Chrome runtime by
default, changing threading and process models). No upstream fix exists as of
2026-03.

References:
- CEF Forum: "Process hangs after switching to chrome runtime" (MemoryInfra SIGILL)
- SharedImageManager::ProduceMemory errors reported around Chromium 124

## Fix: Disable MemoryInfra periodic dumps

Since the MemoryInfra dump is not needed for production rendering, the fix is to
prevent the periodic memory dump system from running.

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
