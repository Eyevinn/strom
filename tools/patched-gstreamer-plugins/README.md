# Patched GStreamer plugins

This directory holds patches we apply on top of upstream GStreamer plugins
plus a reproducible Docker-based build environment that produces them as
shared libraries (`.so`) for `linux/amd64` and `linux/arm64`.

The output binaries are uploaded as assets on a GitHub release in this
repo. Strom's main Docker image fetches the right one for its target
architecture and drops it into the GStreamer plugin path, overriding the
distro-shipped plugin.

This setup is intended for patches that we don't (yet) plan to upstream
and that change rarely. Right now there is exactly one patch — see
[Current patches](#current-patches).

## Why we do it this way

* **Docker-based build** — pinned base image, pinned GStreamer source
  version, pinned build tool versions. Anyone on the team can rebuild and
  get a byte-identical (or close-to-identical) artifact.
* **Build once, ship binary** — patches change very rarely. We don't want
  every strom CI build to spend the time recompiling GStreamer plugins.
  After a manual one-shot build + GitHub release upload, the strom build
  pipeline just does `curl ... | install`.
* **GStreamer ABI stability** — GStreamer maintains forward ABI compat
  within the 1.x line, so a plugin built against 1.22 loads cleanly in
  1.22, 1.24, 1.26 and beyond.

## Layout

```
tools/patched-gstreamer-plugins/
├── Dockerfile              # multi-arch builder
├── build.sh                # one-shot wrapper around docker buildx
├── gstreamer-version.txt   # pinned upstream GStreamer source version
├── patches/                # unified diffs applied to the upstream tree
│   └── 0001-…patch
└── dist/                   # build output, .gitignore'd
    ├── linux_amd64/
    └── linux_arm64/
```

## Building

Requires Docker with `buildx` available (modern Docker Desktop / Linux
with `docker-ce` + `docker-buildx-plugin`).

```sh
./build.sh
```

The first run sets up a buildx builder named
`patched-gstreamer-plugins-builder` and installs QEMU emulators for
cross-arch builds. Subsequent runs reuse them.

The arm64 build runs under QEMU emulation on x86_64 hosts, which is slow
(typically 10–20 minutes). On native arm64 hosts both builds are fast.

Output: `dist/linux_amd64/libgstdecklink.so` and
`dist/linux_arm64/libgstdecklink.so` (plus a `gstreamer-rev.txt` per arch
recording the exact upstream commit the plugin was built from).

## Releasing

1. Run `./build.sh`.
2. Verify the output by loading it locally with `gst-inspect-1.0`:

   ```sh
   GST_PLUGIN_PATH=$PWD/dist/linux_amd64 gst-inspect-1.0 decklinkvideosrc \
     | grep -A2 capture-group
   ```

3. Create a GitHub release on this repo. Tag convention:
   `patched-plugins-v<X.Y>-gst<gst-version>` — e.g.
   `patched-plugins-v1.0-gst1.22.12`. Bump `<X.Y>` whenever any patch in
   `patches/` changes; keep the `gst<version>` suffix in sync with
   `gstreamer-version.txt`.

4. Upload the four artifacts:

   ```
   libgstdecklink-linux-amd64.so   ← dist/linux_amd64/libgstdecklink.so
   libgstdecklink-linux-arm64.so   ← dist/linux_arm64/libgstdecklink.so
   gstreamer-rev-linux-amd64.txt   ← dist/linux_amd64/gstreamer-rev.txt
   gstreamer-rev-linux-arm64.txt   ← dist/linux_arm64/gstreamer-rev.txt
   ```

   Renaming with the platform suffix avoids name collisions in the
   release UI.

## Consuming from strom's Dockerfile

Strom's main `Dockerfile` overrides the distro-shipped decklink plugin
with our patched build, picking the right architecture automatically:

```Dockerfile
ARG TARGETARCH
ARG PATCHED_PLUGINS_TAG=patched-plugins-v1.0-gst1.22.12
ARG PATCHED_PLUGINS_REPO=eyevinntechnology/strom

RUN curl -fsSL \
        "https://github.com/${PATCHED_PLUGINS_REPO}/releases/download/${PATCHED_PLUGINS_TAG}/libgstdecklink-linux-${TARGETARCH}.so" \
        -o "/usr/lib/$(dpkg-architecture -q DEB_HOST_MULTIARCH)/gstreamer-1.0/libgstdecklink.so"
```

`TARGETARCH` is set automatically by `docker buildx` and matches our
release naming (`amd64` / `arm64`).

## Adding a new patch

1. Edit the GStreamer source locally however you like (recommended: a
   monorepo checkout at the version pinned in `gstreamer-version.txt`).
2. `git diff > tools/patched-gstreamer-plugins/patches/NNNN-short-description.patch`
   from the gstreamer repo root. Patches must be unified diffs relative
   to the GStreamer monorepo root and must apply cleanly with
   `git apply --check`.
3. Run `./build.sh` and confirm the resulting `.so` loads in
   `gst-inspect-1.0`.
4. Bump the release tag and re-release as above.

## Current patches

### `0001-decklink-add-capture-group-property-and-sync-control.patch`

Exposes BMD's *synchronized capture group* feature
(`bmdVideoInputSynchronizeToCaptureGroup` flag,
`bmdDeckLinkConfigCaptureGroup` config ID) as a `capture-group` int
property on `decklinkvideosrc`. When two or more inputs share a
non-negative `capture-group` value, BMD arms them atomically at the same
hardware vblank, so their captured frames have a common time origin and
audio/video can be sample-aligned across SDI ports.

The plugin's element model otherwise calls `StartStreams()` /
`StopStreams()` once per element. With sync enabled, the BMD SDK manual
specifies these should be called exactly once per group, on any one
member. The patch adds a refcounted leader-tracking layer so the same
`IDeckLinkInput` instance that calls `StartStreams` also calls
`StopStreams`, regardless of which element's state-change handler runs
first.

A known BMD driver quirk: `StopStreams()` on an input that was enabled
with `bmdVideoInputSynchronizeToCaptureGroup` blocks for ~12 s before
returning (reproducible in BMD's own `SynchronizedCapture` SDK example;
see [forum thread 190441][forum]). We accept the wait — bypassing
`StopStreams()` would violate the documented teardown order
(`StopStreams` → `DisableVideoInput`) and break subsequent pipeline
restarts with caps not-negotiated errors.

[forum]: https://forum.blackmagicdesign.com/viewtopic.php?f=12&t=190441

The strom-side glue (block property exposure, runtime activation) lives
in `backend/src/blocks/builtin/decklink.rs`.
