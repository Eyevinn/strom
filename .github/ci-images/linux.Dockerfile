# The image check-linux runs in (ci.yml): the GStreamer baseline (#964) from
# Ubuntu 26.04 and the Rust toolchain rust-toolchain.toml pins.
#
# ci.yml tags it with a hash of this file and rust-toolchain.toml, and builds
# and pushes it when that tag is missing. Any change here gets a new image;
# the base image and the archive are only read again when this file changes.
#
# libegl-mesa0 and libgl1-mesa-dri give the GL tests a surfaceless EGL
# context (llvmpipe). libsoup-3.0-0 and gsettings-desktop-schemas are what
# souphttpsrc loads and looks up the system proxy in. zstd is what the
# actions cache compresses with.
FROM ubuntu:26.04

RUN apt-get update -qq \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git pkg-config cmake libclang-dev procps \
    libunwind-dev libssl-dev libcairo2-dev \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-nice gstreamer1.0-gl \
    gstreamer1.0-tools libsoup-3.0-0 gsettings-desktop-schemas libegl-mesa0 libgl1-mesa-dri \
    zstd \
 && rm -rf /var/lib/apt/lists/*

# The channel rust-toolchain.toml pins (ci.yml passes it), with the rustfmt
# and clippy check-linux uses but without the wasm32 target the file also
# lists (check-wasm's). RUSTUP_TOOLCHAIN overrides the file, so rustup
# installs nothing more when the job runs cargo in the checkout.
ARG RUST_CHANNEL
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    RUSTUP_TOOLCHAIN=$RUST_CHANNEL
RUN test -n "$RUST_CHANNEL" \
 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal \
      --default-toolchain "$RUST_CHANNEL" --component rustfmt,clippy \
 && rustc --version && cargo fmt --version && cargo clippy --version
