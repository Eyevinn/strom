# ARM64 Cross-Compilation Scripts

Scripts for cross-compiling Strom from x86_64 to ARM64 (aarch64) targets.

## Recommended: Zig-based Cross-Compilation

**Why Zig?** Target specific glibc versions without complex multi-arch setup!

```bash
# Setup (one-time) - BOTH scripts required!
./setup-zig-cross.sh         # Install Zig and cargo-zigbuild
./setup-arm64-cross.sh       # Install ARM64 GStreamer libraries (required!)

# Build for specific glibc version
./build-zig-arm64.sh 2.36  # Raspberry Pi OS 12
./build-zig-arm64.sh 2.31  # Older Debian/Ubuntu
./build-zig-arm64.sh 2.17  # Maximum compatibility
```

**Advantages:**
- ✅ Target specific glibc versions (2.17 - 2.39+)
- ✅ Build on Ubuntu 24.04 (glibc 2.39) → run on Raspberry Pi (glibc 2.36)
- ✅ Simpler than traditional cross-compilation (Zig handles the toolchain)

**Note:** While Zig handles cross-compilation, ARM64 GStreamer libraries are still needed for pkg-config during build.

## Scripts

### Zig-based Scripts

#### `setup-zig-cross.sh`
**Step 1 of 2** - Install Zig and cargo-zigbuild.

**What it does:**
- Downloads and installs Zig
- Installs cargo-zigbuild
- Adds Rust ARM64 targets

**Usage:**
```bash
./setup-zig-cross.sh
# Then run setup-arm64-cross.sh (required for GStreamer libraries)
```

**Important:** This alone is not sufficient. You must also run `setup-arm64-cross.sh` to install ARM64 GStreamer libraries.

#### `build-zig-arm64.sh [glibc_version]`
Builds Strom for ARM64 targeting a specific glibc version.

**Outputs:**
- `target/aarch64-unknown-linux-gnu/release/strom`
- `target/aarch64-unknown-linux-gnu/release/strom-mcp-server`

**Usage:**
```bash
./build-zig-arm64.sh 2.36  # Specify glibc version (default: 2.36)
```

**Common glibc versions:**
- `2.17` - CentOS 7, Amazon Linux 2 (max compatibility)
- `2.31` - Ubuntu 20.04, Debian 11
- `2.36` - Ubuntu 22.04, Debian 12, Raspberry Pi OS 12
- `2.38` - Ubuntu 24.04

### Traditional Scripts

#### `setup-arm64-cross.sh`
One-time setup script that installs cross-compilation toolchain and ARM64 libraries.

**What it does:**
- Adds arm64 architecture to dpkg
- Configures apt sources for ARM64 packages
- Blocks ARM64 Python to prevent conflicts
- Installs cross-compiler (gcc-aarch64-linux-gnu)
- Installs ARM64 GStreamer development libraries
- Adds Rust ARM64 targets
- Creates `.cargo/config.toml` with linker configuration

**Usage:**
```bash
./setup-arm64-cross.sh
```

**Note:** Idempotent - safe to run multiple times.

#### `build-arm64.sh`
Builds Strom for ARM64 using glibc (standard dynamic linking).

**Outputs:**
- `target/aarch64-unknown-linux-gnu/release/strom`
- `target/aarch64-unknown-linux-gnu/release/strom-mcp-server`

**Usage:**
```bash
./build-arm64.sh
```

**Note:** Uses build system's glibc version. For targeting specific glibc versions, use Zig build instead.

#### `cleanup-arm64-cross.sh`
Removes cross-compilation setup and restores system to original state.

**What it does:**
- Removes Python blocking preferences
- Removes ARM64 package sources
- Restores ubuntu.sources from backup
- Optionally removes arm64 architecture and packages

**Usage:**
```bash
./cleanup-arm64-cross.sh
```

## Documentation

For detailed information about the cross-compilation process, see:
- [Cross-Compilation Guide](../../docs/CROSS_COMPILE_ARM64.md)

## Quick Start

### Zig-based (Recommended)

```bash
# 1. Setup (one time) - Run BOTH scripts!
cd /path/to/strom
./scripts/cross-compile/setup-zig-cross.sh       # Install Zig
./scripts/cross-compile/setup-arm64-cross.sh     # Install ARM64 libraries (required!)

# 2. Build for specific glibc version
./scripts/cross-compile/build-zig-arm64.sh 2.36

# 3. Copy to target
scp target/aarch64-unknown-linux-gnu/release/strom user@arm64-host:~/
```

### Traditional

```bash
# 1. Setup (one time)
cd /path/to/strom
./scripts/cross-compile/setup-arm64-cross.sh

# 2. Build
./scripts/cross-compile/build-arm64.sh

# 3. Copy to target
scp target/aarch64-unknown-linux-gnu/release/strom user@arm64-host:~/
```

## Requirements

- Ubuntu 24.04 (or compatible Debian-based distribution)
- Rust toolchain (rustup)
- Trunk (for frontend builds)
- sudo access
