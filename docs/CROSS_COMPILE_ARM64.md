# Cross-Compiling Strom for ARM64

> Code is the source of truth — this may have drifted; read the code for the current implementation.

How to build an ARM64 (aarch64) Strom binary on an x86_64 Linux machine. The scripts live in
`scripts/cross-compile/`.

## Prerequisites

- Ubuntu 24.04 (or a compatible Debian-based distribution)
- Rust toolchain installed via rustup
- Trunk for the frontend: `cargo install trunk --locked`
- sudo access for installing system packages

## Zig-based build (recommended)

Zig lets you target a specific glibc version without having it installed, so the binary runs
on systems older than your build machine.

```bash
# One-time setup - run BOTH scripts, in this order:
./scripts/cross-compile/setup-zig-cross.sh       # 1. Install Zig and cargo-zigbuild (needs >= 0.23; an older one is kept, upgrade it yourself)
./scripts/cross-compile/setup-arm64-cross.sh     # 2. Install ARM64 GStreamer libraries (required)

# Build for ARM64, targeting a glibc version
./scripts/cross-compile/build-zig-arm64.sh 2.36  # Raspberry Pi OS 12 / Debian 12
./scripts/cross-compile/build-zig-arm64.sh 2.31  # Older Debian/Ubuntu
./scripts/cross-compile/build-zig-arm64.sh 2.17  # Maximum compatibility
```

Both setup scripts are required: Zig provides the toolchain and glibc, but pkg-config still
needs the ARM64 GStreamer development packages during the build.

## Traditional build

```bash
# One-time setup (cross-compiler and ARM64 libraries)
./scripts/cross-compile/setup-arm64-cross.sh

# Build for ARM64 against the build machine's glibc
./scripts/cross-compile/build-arm64.sh
```

The binary is written to `target/aarch64-unknown-linux-gnu/release/strom`. It inherits the
build machine's glibc (2.39 on Ubuntu 24.04), so it will not run on systems with an older
glibc. Use the Zig build for those.

`setup-arm64-cross.sh` enables the arm64 dpkg architecture, adds `ports.ubuntu.com` sources,
pins ARM64 Python packages out (otherwise apt tries to replace your amd64 Python), installs
the cross-compiler and ARM64 GStreamer development packages, and writes the linker and
pkg-config settings for `aarch64-unknown-linux-gnu` to `.cargo/config.toml`.

## Docker

The published Docker images are multi-arch (amd64 and arm64). The `Dockerfile` already
cross-compiles the backend with Zig against glibc 2.36, so an ARM64 image can be built with:

```bash
docker buildx build --platform linux/arm64 -t strom:arm64 .

# Extract the binary
docker create --name temp strom:arm64
docker cp temp:/app/strom ./strom-arm64
docker rm temp
```

## Cleanup

```bash
./scripts/cross-compile/cleanup-arm64-cross.sh
```

Removes the ARM64 package sources and the Python pin, restores the original `ubuntu.sources`
from backup, and optionally removes the arm64 architecture and its packages.

## Troubleshooting

### "version GLIBC_X.XX not found" on the target

The build machine has a newer glibc than the target. Use the Zig build and pass the target's
glibc version:

```bash
./scripts/cross-compile/build-zig-arm64.sh 2.36
```

### "cannot find -lgstreamer-1.0"

The ARM64 GStreamer development packages are not installed. Run `setup-arm64-cross.sh` again.

### apt wants to remove Python

The ARM64 Python pin is missing. Check that `/etc/apt/preferences.d/block-arm64-python`
contains `Pin-Priority: -1` for `python3*:arm64`.

### Builds take a long time after changing `.cargo/config.toml`

Changing `rustflags` invalidates the build cache. Later builds are incremental again.

## Other distributions

The setup script targets Ubuntu 24.04 (noble). On Ubuntu 22.04 change `noble` to `jammy` in
the sources; on Debian use Debian repositories. Fedora/RHEL and Arch need a different
approach (no dpkg multi-arch).
