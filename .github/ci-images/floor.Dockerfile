# The image check-floor runs in (ci.yml): the GStreamer floor, 1.22 from
# Debian 12 bookworm, and the Rust toolchain rust-toolchain.toml pins.
#
# ci.yml tags it with a hash of this file and rust-toolchain.toml, and builds
# and pushes it when that tag is missing. Any change here gets a new image;
# the base image and the archive are only read again when this file changes.
#
# The check-linux package list, as far as bookworm has it (all of it, today),
# plus what a bare container lacks. No trunk: the build script then embeds a
# placeholder page instead of the WASM frontend, as in check-linux.
FROM debian:bookworm

RUN apt-get update -qq \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates curl git build-essential gstreamer1.0-tools \
    libunwind-dev libssl-dev libcairo2-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-base gstreamer1.0-gl gstreamer1.0-plugins-good \
    libsoup-3.0-0 gsettings-desktop-schemas gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav gstreamer1.0-nice libegl-mesa0 libgl1-mesa-dri pkg-config cmake libclang-dev \
 && rm -rf /var/lib/apt/lists/* \
 && gst-inspect-1.0 --version \
 && pkg-config --modversion gstreamer-1.0

# The channel rust-toolchain.toml pins (ci.yml passes it), without the
# rustfmt, clippy and wasm32 extras it lists: check-floor only builds and
# tests the backend. RUSTUP_TOOLCHAIN overrides the file, so rustup installs
# nothing more when the job runs cargo in the checkout.
ARG RUST_CHANNEL
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    RUSTUP_TOOLCHAIN=$RUST_CHANNEL
RUN test -n "$RUST_CHANNEL" \
 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "$RUST_CHANNEL" \
 && rustc --version
