# Hardware Requirements (Work in Progress)

> **Status: work in progress. Code is the source of truth — this may have drifted; read the
> code for the current implementation.** We have not load-tested Strom across a range of
> machines, so unless a section says otherwise the numbers here are derived from what the code
> requires and from vendor-published capabilities rather than measured.
>
> If you have run Strom on real hardware, please add a line to
> [Field reports](#field-reports) — collecting real experience is the main point of this
> document.

**Sizing in three lines:**

- **Cloud production**: roughly 8 cores, 16 GB, an L4-class NVIDIA GPU. Uplink is
  bitrate x concurrent viewers, and usually binds before the GPU does.
- **Graphics-heavy production**: budget CPU per HTML source. That is normally the largest single
  cost in the system.
- **4:2:2 contribution**: CPU cores, not a bigger GPU. It never touches the hardware decoder.

Everything below is the reasoning behind those numbers.

## Strom and Open Live

Strom is a standalone GStreamer flow engine with its own UI and API (see the
[root README](../README.md)). [Open Live](https://github.com/Eyevinn/open-live) is one of the
things that use it: it holds its own configuration in its own database and pushes flows to Strom
over REST/WebSocket at runtime, which Strom keeps in memory and runs.

Either way the host is sized by the media it processes, not by how the flows got there. Most
examples here come from Open Live setups, since that is where our operational experience is.

**Primary target: Linux + Docker with an NVIDIA GPU** — what runs live and what gets tested; see
[OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) and [DOCKER_GPU_SETUP.md](DOCKER_GPU_SETUP.md). Other
platforms build and suit development, with the caveats in [Known limits](#known-limits). H.264
is what runs in production, so do not size around HEVC or AV1.

### Two deployment profiles

**Cloud instance — network only.** Media arrives and leaves over the internet: SRT in,
WHEP/WHIP and SRT out. No SDI, NDI, AES67, multicast or house clock. A cloud VM or rack server
with a GPU and decent connectivity. Open Live has so far focused on this profile.

**Facility instance — house I/O.** Strom in a facility, using its SDI (DeckLink), NDI, AES67 and
multicast support, which adds a local media network, PTP/NTP clock discipline, host networking
and vendor drivers on the host.

Compute sizing is much the same for both; networking, clocking and host access differ.

## What drives the load

1. **Video encoding — one encoder per encoded output, not per viewer.** Encoders are explicit
   `videoenc` blocks, so a typical setup encodes twice: multiview and PGM. Those streams are
   shared: a WHEP output fed from `encoded_out` payloads what it is given, and an SRT output can
   hang off the same encoder. A hundred viewers still cost one encode — what they cost is
   uplink.
2. **HTML/CEF sources are the expensive part.** Budget CPU per HTML source, not per flow — see
   [HTML/CEF sources](#htmlcef-sources), the one area where we have measurements.
3. **Decoding contribution feeds.** Hardware decode where available, software fallback
   otherwise; 4:2:2 always on the CPU.
4. **GPU compositing is cheap.** The vision mixer's OpenGL path is texture blitting with alpha
   and geometry, not heavy shader work. Any hardware GL is enough — and Strom falls back to the
   CPU compositor when only software GL (Mesa llvmpipe) is present, since llvmpipe is slower than
   compositing on the CPU.

## Reference workload — a simple flow

One of the simpler flows we run: two SRT inputs, no DSK, no HTML graphics. A lower bound to
scale up from, not a picture of a full production.

| Stage | Count | Where the work lands |
|---|---|---|
| MPEG-TS/SRT inputs (1080p25, 125 ms latency) | 2 | Hardware decode when available, CPU otherwise |
| Vision mixer (2 inputs, 2 PiP) | 1 block, 2 composites | GPU: PGM mix + 1080p25 multiview mosaic |
| Video encoders (`videoenc`, H.264 by default) | **2** | GPU: one for PGM, one for multiview |
| WHEP outputs (4 audio tracks each) | 2 | Video is payloaded, not re-encoded; audio see below |
| SRT output | 1 | Shares the PGM encoder — no third encode |
| Audio mixer (2 ch, 2 groups, 2 aux) + loudness | 1 each | CPU, modest |

So: **2 decodes, 2 GPU composites, 2 H.264 encodes** — flat, regardless of audience. Growth is
uneven: each camera adds a decode, extra PiPs and DSK layers are cheap GPU compositing, HTML
graphics add CPU per source and are usually the largest increase, and the encode count only grows
with separately encoded outputs.

The exception to "flat" is audio. Raw audio into a WHEP output is encoded inside
`whepserversink`'s per-consumer pipeline, so Opus cost is roughly tracks x viewers, and this flow
carries 4 tracks per output. Cheap per instance, unmeasured in total — a good field report.

## Baseline host

| | Minimum | Recommended | Notes |
|---|---|---|---|
| OS | Ubuntu 22.04+ or equivalent | Ubuntu 24.04+ | Images are built on Ubuntu 25.10 with GStreamer 1.26 |
| Arch | x86-64 | x86-64 | `arm64` images are published; see [CROSS_COMPILE_ARM64.md](CROSS_COMPILE_ARM64.md) |
| CPU | 4 cores | 8–16 cores | Add headroom per HTML/CEF source, for 4:2:2 or software encode, and for Opus (tracks x viewers) |
| RAM | 8 GB | 16–32 GB | Scale with concurrent flows; CEF spawns several processes per source |
| Disk | 10 GB | 20 GB + recording space | Images are ~1.1 GB (`strom`) / ~2.7 GB (`strom-full`) uncompressed, and a pull transiently needs both compressed and extracted copies. Size for recordings and media, not for flow config |
| Network | 1 GbE | Cloud: sized by egress. Facility: 10 GbE | See the profile notes below |
| GPU | none (software fallback) | NVIDIA, see below | Optional for a trial, expected for production |

**Cloud instance:**

- **Uplink bandwidth is what scales with audience.** Every WHEP viewer pulls a full copy, so
  size as bitrate x concurrent viewers and expect this to bind before the GPU does.
- **ICE servers.** WHIP/WHEP need STUN, and any real deployment needs TURN too — see section 7
  of [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md). With `ice_transport_policy = relay` all media
  traverses the TURN server, so that host needs the bandwidth, not this one.
- **Firewall.** The control port (default `8080`) plus the media-plane ports your flows use.

**Facility instance:**

- **`--network host`** for multicast and AES67.
- **Clock discipline** for AES67 and multi-input synchronisation — see
  [STREAM_SYNCHRONIZATION.md](STREAM_SYNCHRONIZATION.md) and
  [`scripts/setup/ntp/`](../scripts/setup/ntp/README.md).
- **NDI** needs the runtime installed ([`scripts/setup/ndi/`](../scripts/setup/ndi/README.md)),
  and is roughly 100–200 Mbit/s per 1080p stream — several feeds outgrow 1 GbE.
- **DeckLink** needs the host driver, plus `--privileged` and the device nodes mounted — see
  [`scripts/setup/decklink/`](../scripts/setup/decklink/README.md).

## GPU

| Tier | Example GPU | What it covers |
|---|---|---|
| Trial / development | None — software fallback | One or two 1080p flows. Works, does not scale. |
| Small production | RTX 3050 / 2060 / A2000, 6–12 GB | A 1080p mix with a couple of encoded outputs. Fine for H.264. |
| Production | NVIDIA L4, RTX A4000 / RTX 4000 Ada, 16–24 GB | More encode/decode engines, no session cap, built for continuous duty. L4 is 72 W, single-slot, passively cooled — a good rack fit. |

The case for a professional card is engine count, thermals and duty cycle rather than raw
capability: a consumer card with NVENC does the same encoding work at the same quality.

Required regardless of card:

- The **full NVIDIA driver — not `nvidia-headless`**, which omits the OpenGL/EGL components and
  so disables GPU compositing and CUDA-GL interop.
- `nvidia-container-toolkit`, and `--gpus all` on the container.
- `NVIDIA_DRIVER_CAPABILITIES=all`, `GST_GL_WINDOW=egl-device`, `GST_GL_PLATFORM=egl` for
  headless GL. The Docker images set these already.

Every generation below handles H.264, which is what we run live; the rest is informational.

| Generation | H.264 encode | HEVC encode | AV1 encode | AV1 decode | 4:2:2 |
|---|---|---|---|---|---|
| Pascal (GTX 10xx) | Yes, older quality | Yes | No | No | No |
| Turing (RTX 20xx, T4) | Yes | Yes | No | No | No |
| Ampere (RTX 30xx, A2000) | Yes | Yes | No | Yes | No |
| Ada (RTX 40xx, L4) | Yes | Yes | Yes | Yes | No |
| Blackwell | Yes | Yes | Yes | Yes | Hardware yes, unusable — see limits |

`nvcodec` registers its elements from what the driver reports the card can do, so
`gst-inspect-1.0 nvcodec` on the target machine beats this table.

**Non-NVIDIA:** Intel (QSV, VA-API) and AMD (VA-API, AMF) encoders are supported and selected
automatically when present, and GL compositing works on any hardware GL driver including Intel
iGPUs. NVIDIA-only is the zero-copy CUDA-GL interop path. These combinations get far less
testing — field reports especially welcome.

## HTML/CEF sources

The one area with measured numbers, all from a single machine (an RTX 3090) — treat them as one
data point. Full detail in [HTML_RENDER.md](HTML_RENDER.md).

- **Software rendering is the default**, via Xvfb: near-free for idle or static pages (Chromium
  elides paint when nothing changes), CPU-bound for canvas/WebGL.
- **GPU mode (`STROM_CEF_GPU=1`) is opt-in and not a general win** — roughly a **50% CPU floor
  per `cefsrc` at 1080p30** whatever the page, from continuous Vulkan command-buffer submits. A
  canvas-heavy wind-map went ~95% → ~57% CPU, but a static page went ~1% → ~53%. Enable it only
  where the renderer is the bottleneck.
- **Resolution is the biggest lever in software mode**: 640x360 costs roughly 3x less CPU than
  1920x1080, since paint, compositing and BGRA transport scale with pixel count. Render at the
  size you composite at. Dropping 30 → 15 fps roughly halves compositor and transport cost.

So a graphics-heavy production is sized by its HTML sources, and the fix is usually smaller
render sizes rather than a bigger GPU.

## Known limits

- **No measured capacity figures exist** for flows — we cannot tell you how many 1080p mixes a
  given GPU sustains. See [Field reports](#field-reports).
- **4:2:2 never uses the GPU.** GStreamer's `nvcodec` has no 4:2:2 support even on hardware that
  has it ([issue #711](https://github.com/Eyevinn/strom/issues/711)), so 10-bit 4:2:2
  contribution decodes on the CPU.
- **Pre-encoded H.264 into WHEP needs a short GOP.** `webrtcsink` runs codec discovery per client
  with a fresh `h264parse` that needs SPS/PPS from a keyframe, so starting mid-GOP can time out.
  Roughly 1 second (30 frames) works.
- **AV1 encode requires Ada or newer**, and on older hardware an AV1 request silently falls back
  to `svtav1enc` on the CPU (the log says `Using software fallback encoder`). Not part of the
  live path today.
- **NVENC session limits apply to consumer cards** — see NVIDIA's
  [GPU support matrix](https://developer.nvidia.com/video-encode-and-decode-gpu-support-matrix-new)
  for the current per-card figures. Rarely reached with per-output encoding, but relevant if you
  build many separately encoded outputs.
- **Platforms**: on WSL2 NVENC works but CUDA-GL interop does not (the D3D layer blocks it); on
  macOS there is no NVENC/NVDEC at all, only VideoToolbox.
- **Encoder/decoder engine utilisation is not reported** by Strom's system monitor, which covers
  GPU utilisation, memory, temperature and power. Use `nvidia-smi dmon -s u` on the host.

## Checking a specific machine

```bash
# Driver, card, memory
nvidia-smi

# What this GPU can actually encode/decode
docker exec strom gst-inspect-1.0 nvcodec

# Is GPU compositing active? "llvmpipe" means software GL — check the driver package
docker exec strom sh -c 'GST_DEBUG=glcontext:4 gst-launch-1.0 videotestsrc num-buffers=1 \
  ! glupload ! gldownload ! fakesink 2>&1' | grep -E "GL_VENDOR|GL_RENDERER"

# Every codec element Strom found (encoders and decoders share the "Codec" category)
curl -s localhost:8080/api/elements \
  | jq -r '.elements[] | select(.category=="Codec") | .name' | sort

# Which encoder a running flow actually got
curl -s localhost:8080/api/flows/<flow-id>/debug-graph \
  | grep -oE "(nv|va|qsv|x26|svt)[a-z0-9_]*(enc|dec)" | sort | uniq -c

# Live encode/decode engine load (host, not container)
nvidia-smi dmon -s u
```

The startup log states every choice made: `Found available encoder: …`,
`Using software fallback encoder: …`, `CUDA-GL interop works …`, and the compositor backend.

## Field reports

Rough numbers are fine — "cloud, eight 1080p25 SRT inputs, one mix, two encoded outputs, three
HTML overlays, GPU at 40%, CPU at 60%" is far more useful than silence. One row per setup,
`cloud` or `facility` in the profile column.

To add one: open a PR against this file, or an issue titled `field report: <setup>` and we will
add it for you.

| Date | Profile | GPU | CPU / RAM | Workload | Result | Reported by |
|---|---|---|---|---|---|---|
| _no entries yet_ | | | | | | |
