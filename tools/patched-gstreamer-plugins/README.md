# Patched GStreamer plugins

This directory holds patches we apply on top of upstream GStreamer plugins
plus a reproducible Docker-based build environment that produces them as
shared libraries (`.so`) for `linux/amd64` and `linux/arm64`.

The output binaries are uploaded as assets on a GitHub release in this
repo. Strom's main Docker image fetches the right one for its target
architecture and drops it into the GStreamer plugin path, overriding the
distro-shipped plugin.

This setup is for patches that change rarely: patches we don't (yet) plan
to upstream, and upstream fixes the runtime image's distro has not shipped
yet. See [Current patches](#current-patches).

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
  1.22, 1.24, 1.26 and beyond. A plugin that replaces a distro plugin with
  a fixed copy of the same release is built from that exact release, on
  the runtime image's base.

## Layout

```
tools/patched-gstreamer-plugins/
├── Dockerfile              # multi-arch builder, one plugin per build
├── build.sh                # one-shot wrapper around docker buildx
├── plugins.conf            # per plugin: base image, GStreamer tag, build dir
├── patches/<plugin>/       # unified diffs applied to the upstream tree
├── testdata/               # files the strom Dockerfile checks the plugins with
└── dist/<plugin>/          # build output, .gitignore'd
    ├── linux_amd64/
    └── linux_arm64/
```

## Building

Requires Docker with `buildx` available (modern Docker Desktop / Linux
with `docker-ce` + `docker-buildx-plugin`).

```sh
./build.sh               # every plugin in plugins.conf
./build.sh mpegtsdemux   # one plugin
```

The first run sets up a buildx builder named
`patched-gstreamer-plugins-builder` and installs QEMU emulators for
cross-arch builds. Subsequent runs reuse them.

The non-native architecture builds under QEMU emulation, which is slow
(typically 10–20 minutes per plugin).

Output: `dist/<plugin>/linux_amd64/libgst<plugin>.so` and
`dist/<plugin>/linux_arm64/libgst<plugin>.so` (plus a `gstreamer-rev.txt`
per arch recording the exact upstream commit the plugin was built from).
The script prints each `.so`'s sha256 for the strom Dockerfile.

## Releasing

1. Run `./build.sh <plugin>`.
2. Verify the output in a container of the plugin's base image, e.g. that
   `gst-inspect-1.0` loads it and that it behaves as the patch intends.
3. Create a GitHub release on this repo. Tag convention:
   `patched-plugins-<plugin>-v<X.Y>-gst<gst-version>` — e.g.
   `patched-plugins-mpegtsdemux-v1.0-gst1.28.2`. Bump `<X.Y>` whenever the
   plugin's patches change; keep the `gst<version>` suffix in sync with
   `plugins.conf`. (decklink predates this and is released as
   `patched-plugins-v1.0-gst1.22.12`.)
4. Upload the four artifacts:

   ```
   libgst<plugin>-linux-amd64.so   ← dist/<plugin>/linux_amd64/libgst<plugin>.so
   libgst<plugin>-linux-arm64.so   ← dist/<plugin>/linux_arm64/libgst<plugin>.so
   gstreamer-rev-linux-amd64.txt   ← dist/<plugin>/linux_amd64/gstreamer-rev.txt
   gstreamer-rev-linux-arm64.txt   ← dist/<plugin>/linux_arm64/gstreamer-rev.txt
   ```

   Renaming with the platform suffix avoids name collisions in the
   release UI.

5. Pin the new sha256 digests in strom's `Dockerfile`.

## Consuming from strom's Dockerfile

Strom's main `Dockerfile` downloads each patched plugin for its
`TARGETARCH` (set by `docker buildx`, matching the `amd64` / `arm64`
release naming), checks it against a pinned sha256, and installs it over
the distro plugin. Read the Dockerfile for the current steps.

## Adding a new patch

1. Edit the GStreamer source locally however you like (recommended: a
   monorepo checkout at the version pinned for the plugin in `plugins.conf`).
2. `git diff > tools/patched-gstreamer-plugins/patches/<plugin>/NNNN-short-description.patch`
   from the gstreamer repo root. Patches must be unified diffs relative
   to the GStreamer monorepo root and must apply cleanly with
   `git apply --check`.
3. For a new plugin, add its row to `plugins.conf`.
4. Run `./build.sh <plugin>` and confirm the resulting `.so` loads in
   `gst-inspect-1.0`.
5. Bump the release tag and re-release as above.

## Current patches

### `decklink/0001-decklink-add-capture-group-property-and-sync-control.patch`

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

### `mpegtsdemux/0001-mpegtsdemux-improve-pts-rollover-handling-in-ignore-pcr-mode.patch`

Upstream commit a972093b (gstreamer!11905, in 1.28.5 and later), with its
test hunks dropped, applied to 1.28.2 — the release Ubuntu 26.04 ships.
In `ignore-pcr` mode, which `hlsdemux2` uses, tsdemux put every timestamp
after a 33-bit PTS wrap a whole 26.5 h epoch ahead. A live MPEG-TS HLS
source then froze once per 26.5 h with no error.

`testdata/test_pcr_rollover.ts` is upstream's test file for this fix. The
strom Dockerfile runs it through `tsdemux ignore-pcr=true` and fails the
image build if the PTS jumps across the wrap. Remove this patch once the
runtime image's distro ships GStreamer 1.28.5 or later.
