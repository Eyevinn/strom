# Cross-Compiling Strom for ARM64

This document describes the ARM64 cross-compilation setup for Strom, including lessons learned during implementation.

## Overview

Strom can be cross-compiled from x86_64 Linux to ARM64 targets (aarch64). Two build approaches are available:

1. **Zig-based build** (`cargo-zigbuild`) - **RECOMMENDED** - Uses Zig for cross-compilation with specific glibc version targeting
2. **Traditional glibc build** (`aarch64-unknown-linux-gnu`) - Dynamic linking with glibc (requires complex multi-arch setup)

## Quick Start

### Zig-based Cross-Compilation (Recommended)

```bash
# One-time setup - Run BOTH scripts (in order):
./scripts/cross-compile/setup-zig-cross.sh       # 1. Install Zig and cargo-zigbuild
./scripts/cross-compile/setup-arm64-cross.sh     # 2. Install ARM64 GStreamer libraries (REQUIRED!)

# Build for ARM64 targeting specific glibc version
./scripts/cross-compile/build-zig-arm64.sh 2.36  # For Raspberry Pi OS 12
./scripts/cross-compile/build-zig-arm64.sh 2.31  # For older Debian/Ubuntu
./scripts/cross-compile/build-zig-arm64.sh 2.17  # Maximum compatibility
```

**Key advantage**: You can target **any glibc version** without needing that version installed on your build system!

**Important:** Both setup scripts are required. Zig handles the cross-compilation toolchain, but ARM64 GStreamer libraries are still needed for pkg-config during the build process.

### Traditional Cross-Compilation

```bash
# One-time setup (installs cross-compiler and ARM64 libraries)
./scripts/cross-compile/setup-arm64-cross.sh

# Build for ARM64 with glibc (uses build system's glibc version)
./scripts/cross-compile/build-arm64.sh
```

Binaries are output to:
- `target/aarch64-unknown-linux-gnu/release/strom`
- `target/aarch64-unknown-linux-gnu/release/strom-mcp-server`

## Prerequisites

- Ubuntu 24.04 (or compatible Debian-based distribution)
- Rust toolchain installed via rustup
- Trunk for frontend builds: `cargo install trunk`
- sudo access for installing system packages

## Architecture Decision: Zig vs Traditional

### Zig-based Build (Recommended)

**Use when:**
- You need to target a specific glibc version different from your build system
- You want simpler setup without multi-arch apt complexity
- You need maximum control over glibc compatibility

**Advantages:**
- **Target specific glibc versions** (e.g., build for glibc 2.36 on a system with 2.39)
- **No complex multi-arch setup** - avoids Python package conflicts
- **Simple installation** - just install Zig and cargo-zigbuild
- **Better reproducibility** - explicit version targeting

**How it works:**
- Uses Zig's built-in cross-compilation toolchain
- Specify glibc version via target suffix: `aarch64-unknown-linux-gnu.2.36`
- Zig provides glibc headers/libs - no need to install them on build system

**Limitations:**
- Still need ARM64 GStreamer libraries for pkg-config (can use multi-arch setup for this)
- Slightly newer tool (but actively maintained and widely used)

### Traditional glibc Build

**Use when:**
- You're already set up with multi-arch and it's working
- Target system has matching or newer glibc version than build system
- Need best compatibility with GStreamer ecosystem

**Limitations:**
- **Inherits build system's glibc version** - Ubuntu 24.04 uses glibc 2.39
- Will fail with "version GLIBC_X.XX not found" on older systems (e.g., Raspberry Pi OS with 2.36)
- **Complex setup** - requires multi-arch apt, potential Python conflicts

## When to Use Which Approach

| Build Method | Best For | glibc Version Control | Setup Complexity |
|--------------|----------|----------------------|------------------|
| **Zig** | Most users, production builds | ✅ Full control (target 2.17-2.39+) | ⭐ Low |
| **Traditional glibc** | Already set up, matching glibc | ❌ Uses build system's version | ⭐⭐⭐ High |

## Setup Process Explained

### Zig Setup (`setup-zig-cross.sh`)

The Zig-based setup is much simpler than traditional cross-compilation:

1. **Download and install Zig**
   - Downloads Zig from ziglang.org
   - Extracts to `~/.local/zig`
   - Adds to PATH in `~/.bashrc`

2. **Install cargo-zigbuild**
   ```bash
   cargo install --locked cargo-zigbuild
   ```

3. **Add Rust ARM64 target**
   ```bash
   rustup target add aarch64-unknown-linux-gnu
   ```

4. **For GStreamer support (optional)**
   - Run `setup-arm64-cross.sh` to get ARM64 GStreamer pkg-config files
   - Or build in Docker with ARM64 GStreamer pre-installed

That's it! No multi-arch apt configuration, no Python conflicts, no complex toolchain setup.

### Traditional Setup (`setup-arm64-cross.sh`)

The `setup-arm64-cross.sh` script performs the following:

### 1. Add ARM64 Architecture

```bash
sudo dpkg --add-architecture arm64
```

Enables multi-arch support in dpkg/apt.

### 2. Configure Package Sources

**Challenge**: Ubuntu 24.04 uses deb822 format in `/etc/apt/sources.list.d/ubuntu.sources`

**Solution**: Add `Architectures: amd64` to existing sources, add ARM64 sources separately:

```
# /etc/apt/sources.list.d/ubuntu.sources gets:
Types: deb
Architectures: amd64  # <- Added
URIs: http://archive.ubuntu.com/ubuntu/
...

# /etc/apt/sources.list.d/arm64-cross.list gets:
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble main universe
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble-updates main universe
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble-security main universe
```

**Key insight**: ARM64 packages are on `ports.ubuntu.com`, not `archive.ubuntu.com`.

### 3. Block ARM64 Python Packages

**Problem**: GStreamer libraries depend on Python. When installing `libgstreamer1.0-dev:arm64`, apt tries to satisfy dependencies by:
1. Removing `python3:amd64` (your working Python)
2. Installing `python3:arm64` (which can't run on x86_64)
3. This breaks your system and the installation fails

**Solution**: Create `/etc/apt/preferences.d/block-arm64-python`:

```
Package: python3*:arm64
Pin: release *
Pin-Priority: -1
```

This tells apt to **never** install ARM64 Python packages. Python dependencies are satisfied by existing amd64 Python.

### 4. Install Cross-Compilation Tools

```bash
gcc-aarch64-linux-gnu    # ARM64 C/C++ cross-compiler
g++-aarch64-linux-gnu    # ARM64 C++ cross-compiler
pkg-config               # For finding library paths
```

### 5. Install ARM64 Development Libraries

```bash
libgstreamer1.0-dev:arm64
libgstreamer-plugins-base1.0-dev:arm64
libgstreamer-plugins-bad1.0-dev:arm64
```

These are **header files and .so libraries** for ARM64, not executables.

### 6. Configure Rust

```bash
rustup target add aarch64-unknown-linux-gnu
rustup target add aarch64-unknown-linux-musl
```

Creates `.cargo/config.toml` with linker settings:

```toml
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"

[target.aarch64-unknown-linux-musl]
linker = "aarch64-linux-gnu-gcc"
rustflags = ["-C", "target-feature=-crt-static", "-C", "link-arg=-lm"]

[env]
PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu = "/usr/aarch64-linux-gnu"
PKG_CONFIG_PATH_aarch64_unknown_linux_gnu = "/usr/lib/aarch64-linux-gnu/pkgconfig"
```

## Build Process

### Frontend (WASM)

```bash
cd frontend
trunk build --release
```

WASM is architecture-independent, built once for all targets.

### Backend (ARM64)

```bash
export PKG_CONFIG_SYSROOT_DIR=/usr/aarch64-linux-gnu
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig

cargo build --release --package strom --target aarch64-unknown-linux-gnu
```

## Lessons Learned

### 1. Ubuntu 24.04 Package Source Format

**Issue**: Script assumed old `/etc/apt/sources.list` format with simple `deb http://...` lines.

**Reality**: Ubuntu 24.04+ uses deb822 format in `/etc/apt/sources.list.d/ubuntu.sources`:

```
Types: deb
URIs: http://archive.ubuntu.com/ubuntu/
Suites: noble noble-updates
Components: main universe
```

**Solution**: Detect and handle deb822 format by adding `Architectures: amd64` field.

### 2. Python Multi-Arch Conflict

**Issue**: Most complex problem encountered. Installing ARM64 libraries triggers Python removal:

```
The following packages will be REMOVED:
  python3 python3-minimal python3.12 python3.12-minimal
  # ... and 738 other packages including desktop environment!
```

**Root cause**:
- `libgstreamer-dev:arm64` depends on `python3:any`
- apt interprets this as "any architecture"
- Decides to "upgrade" by removing `python3:amd64` and installing `python3:arm64`
- ARM64 Python can't execute on x86_64, installation fails

**Solution**: apt pinning to block ARM64 Python entirely. This forces apt to satisfy dependencies with existing amd64 Python.

### 3. glibc Version Compatibility

**Issue**: Binaries compiled on Ubuntu 24.04 (glibc 2.39) won't run on older systems (e.g., Raspberry Pi OS 12 with glibc 2.36):

```
./strom: /lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found
```

**Solutions**:
1. **Use Zig** (recommended) - Target specific glibc version (e.g., 2.36 for Raspberry Pi)
2. Use Docker with older base (Debian 12 Bookworm = glibc 2.36)
3. Compile on system with matching glibc version

## Cleanup

To remove cross-compilation setup:

```bash
./scripts/cross-compile/cleanup-arm64-cross.sh
```

This will:
- Remove ARM64 package sources
- Remove Python blocking preferences
- Restore original ubuntu.sources (from backup)
- Optionally remove arm64 architecture and packages

## Troubleshooting

### "version GLIBC_X.XX not found" on target

Your build system has newer glibc than target. Use Zig to target specific glibc version:
```bash
./scripts/cross-compile/build-zig-arm64.sh 2.36
```

### "cannot find -lgstreamer-1.0"

ARM64 GStreamer dev packages not installed. Run setup script again.

### Python package conflicts

If you see Python removal warnings, the Python blocking isn't working. Check:

```bash
cat /etc/apt/preferences.d/block-arm64-python
```

Should show `Pin-Priority: -1` for `python3*:arm64`.

### Builds taking forever after changing .cargo/config.toml

Changing `rustflags` invalidates entire build cache. This is one-time; subsequent builds will be incremental.

## Distribution-Specific Notes

This setup is designed for **Ubuntu 24.04**. For other distributions:

- **Ubuntu 22.04/23.04**: Change "noble" to "jammy"/"lunar" in sources
- **Debian 12/13**: Use Debian repositories instead of Ubuntu
- **Fedora/RHEL**: Use `dnf`, different package names, no multi-arch
- **Arch Linux**: Use AUR for cross-compilers, different approach

## Alternative: Docker Cross-Compilation

For glibc version compatibility, use Docker with matching Debian version:

```bash
# Modify Dockerfile to use Debian Bookworm (glibc 2.36)
docker buildx build --platform linux/arm64 -t strom:arm64 .

# Extract binary
docker create --name temp strom:arm64
docker cp temp:/app/strom ./strom-arm64
docker rm temp
```

## References

- [Rust Cross Compilation](https://rust-lang.github.io/rustup/cross-compilation.html)
- [Debian Multi-Arch](https://wiki.debian.org/Multiarch/HOWTO)
- [Ubuntu Ports](https://wiki.ubuntu.com/ARM/Server/PortsArchiveHowto)
- [Musl Dynamic Linking](https://doc.rust-lang.org/rustc/targets/custom.html)

## Contributing

Improvements to cross-compilation setup welcome! Please test thoroughly on target hardware before submitting PRs.
