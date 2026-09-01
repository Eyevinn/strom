# Hardware Requirements (Work in Progress)

> **Status: work in progress.** We have not load-tested Strom across a range of machines, so
> unless a section says otherwise the numbers below are derived from what the code requires and
> from vendor-published capabilities rather than measured. Treat them as starting points for
> sizing.
>
> If you have run Strom on real hardware, please add a line to
> [Field reports](#field-reports) — collecting real experience here is the main point of this
> document.

## Strom and Open Live

Strom is a standalone GStreamer flow engine with its own UI and API — see the
[root README](../README.md) for what it is on its own.
[Open Live](https://github.com/Eyevinn/open-live) is one of the things that use it: it keeps its
own configuration in its own database and drives Strom over REST/WebSocket, pushing flows across
at runtime. Strom holds those flows in memory and runs the pipelines.

Either way, a Strom host is sized by the media it processes — not by the number of operators
using the UI, and not by how the flows got there. Many of the examples here come from Open Live
setups, since that is where most of our numbers come from.

For setup instructions see [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) and
[DOCKER_GPU_SETUP.md](DOCKER_GPU_SETUP.md).

**Primary target: Linux + Docker with an NVIDIA GPU.** That is what runs live and what gets
tested. Native Linux, macOS, and Windows builds exist and are useful for development, but the
platform caveats in [Known limits](#known-limits) apply. **H.264 is the codec that runs in
production today** — HEVC and AV1 are supported by the code but are not part of the live path,
so do not size a machine around them.

### Two deployment profiles: cloud and facility

Where the media comes from changes what the host needs, so it helps to know which of these you
are building.

**Cloud instance — network only.** Media arrives and leaves over the internet: SRT contribution
in, WHEP/WHIP and SRT out. No SDI, NDI, AES67, multicast or house clock involved. The host is a
cloud VM or rack server with a GPU and decent connectivity. Open Live has so far focused on this
profile.

**Facility instance — house I/O.** Strom running in a facility, using its SDI (DeckLink), NDI,
AES67 and multicast support. That brings a few extra requirements: a local media network with
room for the traffic, PTP/NTP clock discipline, host networking for multicast, and vendor
drivers on the host.

Compute sizing (GPU, CPU) is much the same for both. What differs is networking, clocking and
host access, so the sections below note which requirements are facility-only.

## What actually drives the load

These four things say more about sizing than any spec table, since they decide where the work
lands.

1. **Video encoding — one encoder per encoded output, not per viewer.** Encoders are explicit
   `videoenc` blocks in the flow, so a typical Open Live setup encodes twice: once for multiview
   and once for PGM. Those encoded streams are then *shared* — a WHEP output fed from
   `encoded_out` only payloads what it is given, and an SRT output can hang off the same
   encoder. A hundred WHEP viewers still cost one encode — they cost uplink bandwidth instead,
   which is the real audience-scaling cost on a cloud instance. Video encode sessions scale with
   the number of encoded outputs, which is why NVENC session limits are rarely the ceiling in
   practice.
2. **HTML/CEF sources are the expensive part.** `cefsrc` renders in software against a virtual
   framebuffer (Xvfb) by default, and it is the one area where we do have measurements — see
   [HTML/CEF sources](#htmlcef-sources). Budget CPU per HTML source, not per flow.
3. **Decoding contribution feeds.** Hardware decode is used where available, with automatic
   software fallback. 4:2:2 sources always decode on the CPU — see
   [Known limits](#known-limits).
4. **GPU compositing is cheap.** The vision mixer's OpenGL path is texture blitting with alpha
   and geometry, not heavy shader work. Any hardware GL is enough; Strom deliberately falls back
   to the CPU compositor when only software GL (Mesa llvmpipe) is present, because llvmpipe is
   slower than the CPU compositor.

## Reference workload — a simple flow

One of the simpler flows we run: two SRT inputs, no DSK, no HTML graphics. It works well as a
lower bound to scale up from rather than as a picture of a full production:

| Stage | Count | Where the work lands |
|---|---|---|
| MPEG-TS/SRT inputs (1080p25, 125 ms latency) | 2 | Hardware decode when available, CPU otherwise |
| Vision mixer (2 inputs, 2 PiP) | 1 block, 2 composites | GPU: PGM mix + 1080p25 multiview mosaic |
| Video encoders (`videoenc`, H.264 by default) | **2** | GPU: one for PGM, one for multiview |
| WHEP outputs (4 audio tracks each) | 2 | Video is payloaded, not re-encoded; audio see below |
| SRT output | 1 | Shares the PGM encoder — no third encode |
| Audio mixer (2 ch, 2 groups, 2 aux) + loudness | 1 each | CPU, modest |

So this minimal two-camera flow is **2 decodes, 2 GPU composites, and 2 H.264 encodes** —
flat, regardless of how many people watch.

A real show grows from there, and the growth is not uniform: more cameras add a decode each,
extra PiPs and DSK layers add compositing work (cheap on the GPU), and HTML graphics add CPU per
source — that last one is usually the largest single increase, see
[HTML/CEF sources](#htmlcef-sources). The encode count only grows if you add separately encoded
outputs.

Two caveats on this shape:

- **Audio is the part that does scale with viewers.** Raw audio into a WHEP output is encoded
  inside `whepserversink`'s per-consumer pipeline, so Opus cost is roughly tracks x viewers —
  and the reference flow carries 4 audio tracks per output. Opus is cheap per instance, but this
  is unmeasured and a good candidate for a field report.
- **Flow-level settings matter for stability, not throughput**: the reference flow runs
  `thread_priority: high` with a monotonic clock and 100 ms mixer latency / 125 ms upstream
  latency.

## Baseline host

| | Minimum | Recommended | Notes |
|---|---|---|---|
| OS | Ubuntu 22.04+ or equivalent | Ubuntu 24.04+ | Docker images are built on Ubuntu 25.10 with GStreamer 1.26 |
| Arch | x86-64 | x86-64 | `arm64` images are published; see [CROSS_COMPILE_ARM64.md](CROSS_COMPILE_ARM64.md) |
| CPU | 4 cores | 8–16 cores | Add headroom per HTML/CEF source and for any 4:2:2 or software-encode path |
| RAM | 8 GB | 16–32 GB | Not measured; scale with concurrent flows. CEF spawns several processes per source |
| Disk | 4 GB | 20 GB + recording space | Images are ~1.1 GB (`strom`) / ~2.7 GB (`strom-full`) uncompressed. Size for recordings and media, not for flow config |
| Network | 1 GbE | Cloud: sized by internet egress. Facility: 10 GbE | Cloud egress scales with WHEP viewers (see below). Facility: NDI is roughly 100–200 Mbit/s per 1080p stream, so several feeds outgrow 1 GbE quickly |
| GPU | none (software fallback) | NVIDIA, see below | Optional for a trial, expected for production |

Host-level requirements that are easy to miss:

**Cloud instance:**

- **Internet bandwidth is the thing that scales with audience.** Encoding does not grow with
  viewers, but egress does: every WHEP viewer pulls a full copy of the stream. Size uplink as
  bitrate x concurrent viewers, and expect that to become the constraint long before the GPU
  does.
- **ICE servers.** WHIP/WHEP need STUN, and any real deployment needs TURN as well — see
  section 7 of [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md). If you force
  `ice_transport_policy = relay`, all media traverses the TURN server, so that host needs the
  bandwidth, not this one.
- **Firewall.** The control port (default `8080`) plus whatever media-plane ports the flows use.

**Facility instance:**

- **`--network host`** for multicast and AES67.
- **Clock discipline** for AES67 and multi-input synchronisation — see
  [STREAM_SYNCHRONIZATION.md](STREAM_SYNCHRONIZATION.md) and
  [`scripts/setup/ntp/`](../scripts/setup/ntp/README.md).
- **DeckLink** cards need the host driver installed, and the container needs `--privileged`
  with the device nodes mounted — see
  [`scripts/setup/decklink/`](../scripts/setup/decklink/README.md).
- **NDI** needs the runtime installed — see [`scripts/setup/ndi/`](../scripts/setup/ndi/README.md).

## GPU

### Suggested tiers

| Tier | Example GPU | What it covers |
|---|---|---|
| Trial / development | None — software fallback | One or two 1080p flows. Works, does not scale. |
| Small production | RTX 3050 / 2060 / A2000, 6–12 GB | A 1080p mix with a couple of encoded outputs. Fine for H.264. |
| Production | NVIDIA L4, RTX A4000 / RTX 4000 Ada, 16–24 GB | More encode/decode engines, no session cap, and built for continuous duty. L4 is 72 W, single-slot, passively cooled — a good rack fit. |

Since encoders scale with outputs rather than viewers, the argument for a professional card is
mostly **engine count, thermals, and 24/7 duty cycle**, not raw capability. A consumer card with
NVENC does the same encoding work at the same quality.

### Requirements, regardless of card

- The **full NVIDIA driver — not `nvidia-headless`**. The headless package omits the OpenGL/EGL
  components, which disables GPU compositing and CUDA-GL interop.
- `nvidia-container-toolkit`, and `--gpus all` on the container.
- `NVIDIA_DRIVER_CAPABILITIES=all`, `GST_GL_WINDOW=egl-device`, `GST_GL_PLATFORM=egl` for
  headless GL. The Docker images set these already.

### Generation capabilities

H.264 is what we run live, and every generation below handles it. The remaining columns are
informational.

| Generation | H.264 encode | HEVC encode | AV1 encode | AV1 decode | 4:2:2 |
|---|---|---|---|---|---|
| Pascal (GTX 10xx) | Yes, older quality | Yes | No | No | No |
| Turing (RTX 20xx, T4) | Yes | Yes | No | No | No |
| Ampere (RTX 30xx, A2000) | Yes | Yes | No | Yes | No |
| Ada (RTX 40xx, L4) | Yes | Yes | Yes | Yes | No |
| Blackwell | Yes | Yes | Yes | Yes | Hardware yes, **unusable** — see limits |

`nvcodec` registers its elements from what the driver reports the card can do, so
`gst-inspect-1.0 nvcodec` on the target machine is an authoritative capability report — better
than this table.

### Non-NVIDIA

Intel (QSV, VA-API) and AMD (VA-API, AMF) hardware encoders are supported and selected
automatically when present, and GL compositing works on any hardware GL driver including Intel
iGPUs. What is NVIDIA-only is the zero-copy CUDA-GL interop path. These combinations get far
less testing than NVIDIA — field reports especially welcome.

## HTML/CEF sources

The one area with measured numbers. Full detail in [HTML_RENDER.md](HTML_RENDER.md); the sizing
consequences:

- **Software rendering is the default**, via Xvfb. It is near-free for idle or static pages —
  Chromium elides paint when nothing changes — and CPU-bound for canvas/WebGL content.
- **GPU mode (`STROM_CEF_GPU=1`) is opt-in and not a general win.** It has a roughly **50% CPU
  floor per `cefsrc` at 1080p30** regardless of page content, from continuous Vulkan
  command-buffer submits. Measured on an RTX 3090: a canvas-heavy 1080p30 wind-map goes from
  ~95% to ~57% CPU, but a simple static page goes from ~1% to ~53%. Enable it only for pages
  where the renderer is genuinely the bottleneck.
- **Resolution is the biggest lever in software mode** — 640x360 costs roughly 3x less CPU than
  1920x1080, since paint, compositing, and BGRA transport all scale with pixel count. Render at
  the size you actually composite at.
- **Framerate**: 30 → 15 fps roughly halves compositor and transport cost.

So: a graphics-heavy production is sized by its HTML sources, and the fix is usually smaller
render sizes rather than a bigger GPU.

## Known limits

Worth knowing before settling on hardware:

- **No measured capacity figures exist** for flows. We cannot currently tell you how many 1080p
  mixes a given GPU sustains. See [Field reports](#field-reports).
- **4:2:2 never uses the GPU.** GStreamer's `nvcodec` has no 4:2:2 support even on hardware that
  has it (issue #711), so 10-bit 4:2:2 contribution decodes on the CPU. If 4:2:2 is in your
  workflow, the answer is CPU cores rather than a bigger GPU.
- **Pre-encoded H.264 into WHEP needs a short GOP.** `webrtcsink` runs codec discovery per
  client with a fresh `h264parse` that needs SPS/PPS from a keyframe; starting mid-GOP can time
  out. Roughly 1 second (30 frames) is the working recommendation.
- **AV1 encode requires Ada or newer**, and with the default encoder preference an AV1 request
  on older hardware silently falls back to `svtav1enc` on the CPU (the log says
  `Using software fallback encoder`). Not currently part of the live path.
- **NVENC session limits on consumer cards** exist (historically 3, then 5, then 8 depending on
  driver version). Rarely reached with per-output encoding, but worth knowing if you build flows
  with many separately encoded outputs.
- **WSL2**: NVENC works, CUDA-GL interop does not (the D3D layer blocks it).
- **macOS**: no NVENC/NVDEC at all; VideoToolbox encoders only.
- **Encoder/decoder engine utilisation is not reported** by Strom's system monitor — GPU
  utilisation, memory, temperature, and power are. Use `nvidia-smi dmon -s u` on the host to see
  encode/decode load.

## Checking a specific machine

```bash
# Driver, card, memory
nvidia-smi

# What this GPU can actually encode/decode (nvcodec registers per hardware capability)
docker exec strom gst-inspect-1.0 nvcodec

# Is GPU compositing active? "llvmpipe" here means software GL — check the driver package
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

The startup log is explicit about every choice made: `Found available encoder: …`,
`Using software fallback encoder: …`, `CUDA-GL interop works …`, and the selected compositor
backend.

## Field reports

Add what you actually ran. Rough numbers are fine — "cloud, eight 1080p25 SRT inputs, one mix,
two encoded outputs, three HTML overlays, GPU at 40%, CPU at 60%" is far more useful than
silence. One row per setup, `cloud` or `facility` in the profile column; put detail in a footnote
if needed.

| Date | Profile | GPU | CPU / RAM | Workload | Result | Reported by |
|---|---|---|---|---|---|---|
| _no entries yet_ | | | | | | |
