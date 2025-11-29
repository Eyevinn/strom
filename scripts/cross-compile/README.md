# ARM64 Cross-Compilation Scripts

Scripts for cross-compiling Strom from x86_64 to ARM64 (aarch64) targets.

## Scripts

### `setup-arm64-cross.sh`
One-time setup script that installs cross-compilation toolchain and ARM64 libraries.

**What it does:**
- Adds arm64 architecture to dpkg
- Configures apt sources for ARM64 packages
- Blocks ARM64 Python to prevent conflicts
- Installs cross-compiler (gcc-aarch64-linux-gnu)
- Installs ARM64 GStreamer development libraries
- Adds Rust ARM64 targets (glibc and musl)
- Creates `.cargo/config.toml` with linker configuration

**Usage:**
```bash
./setup-arm64-cross.sh
```

**Note:** Idempotent - safe to run multiple times.

### `build-arm64.sh`
Builds Strom for ARM64 using glibc (standard dynamic linking).

**Outputs:**
- `target/aarch64-unknown-linux-gnu/release/strom`
- `target/aarch64-unknown-linux-gnu/release/strom-mcp-server`

**Usage:**
```bash
./build-arm64.sh
```

**Note:** Requires matching glibc version on target system.

### `build-arm64-musl.sh`
Builds Strom for ARM64 using musl (experimental portable build).

**Outputs:**
- `target/aarch64-unknown-linux-musl/release/strom`
- `target/aarch64-unknown-linux-musl/release/strom-mcp-server`

**Usage:**
```bash
./build-arm64-musl.sh
```

**Note:** More portable across different Linux distributions.

### `cleanup-arm64-cross.sh`
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

### `fix-python-issue.sh`
Emergency script to fix broken ARM64 Python packages.

**When to use:** If setup fails with ARM64 Python installation errors.

**Usage:**
```bash
./fix-python-issue.sh
```

## Documentation

For detailed information about the cross-compilation process, see:
- [Cross-Compilation Guide](../../docs/CROSS_COMPILE_ARM64.md)

## Quick Start

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
