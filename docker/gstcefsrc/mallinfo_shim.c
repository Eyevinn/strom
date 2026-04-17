/*
 * mallinfo_shim — LD_PRELOAD interposer for glibc `mallinfo()`.
 *
 * Chromium's MallocDumpProvider::OnMemoryDump calls the legacy int-based
 * mallinfo() from libcef.so (built against an old sysroot without mallinfo2).
 * When the process arena grows beyond 2 GiB, the int fields overflow to
 * negative values. Chromium then checked_cast<size_t> them, fails the
 * narrowing check, and CHECK()s — producing a SIGILL on the MemoryInfra
 * thread that kills the whole CEF process.
 *
 * This shim replaces mallinfo() with a zero-filled result. The memory dump
 * just records zero bytes (we don't use memory profiling in production),
 * no overflow, no CHECK() failure, no crash.
 *
 * Workaround reference:
 *   https://github.com/chromiumembedded/cef/issues/3963
 *   https://issues.chromium.org/issues/401168177
 *
 * Build:  gcc -shared -fPIC -o libmallinfo_shim.so mallinfo_shim.c
 * Use:    LD_PRELOAD=/path/to/libmallinfo_shim.so <cef-process>
 */

#include <malloc.h>
#include <string.h>

struct mallinfo mallinfo(void) {
    struct mallinfo mi;
    memset(&mi, 0, sizeof(mi));
    return mi;
}
