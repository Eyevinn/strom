# HTML Rendering with CEF (Chromium Embedded Framework)

> **Code is the source of truth.** This guide describes intended behaviour and may have
> drifted from the current implementation. When in doubt, read the code and check the in-app UI.

Strom supports rendering HTML content as video sources using the `cefsrc` GStreamer element from [gstcefsrc](https://github.com/centricular/gstcefsrc). This enables:

- Dynamic HTML/CSS/JavaScript overlays
- Web-based graphics and animations
- Real-time data visualization
- Chromium-powered web content as video input

## Docker Image

HTML rendering requires Chromium Embedded Framework (CEF), which adds significant size to the image. To keep the base image lightweight, this functionality is available in a separate extended image:

| Image | Arch | Compressed | Uncompressed | Use Case |
|-------|------|------------|--------------|----------|
| `strom` | amd64 | ~410 MB | ~1.1 GB | Standard pipelines (no HTML rendering) |
| `strom` | arm64 | ~400 MB | ~1.1 GB | |
| `strom-full` | amd64 | ~820 MB | ~2.7 GB | Full functionality including HTML rendering |
| `strom-full` | arm64 | ~930 MB | ~3.5 GB | |

*Note: Compressed size is what you download via `docker pull`. Uncompressed size is disk usage after extraction. Sizes measured from v0.3.12 (2026-01-22).*

### Quick Start

```bash
# Pull the full image
docker pull eyevinntechnology/strom-full:latest

# Run with host networking (recommended for multicast/AES67)
docker run --network host eyevinntechnology/strom-full:latest

# Or with port mapping
docker run -p 8080:8080 eyevinntechnology/strom-full:latest
```

## Using cefsrc in Pipelines

The `cefsrc` element renders a URL to video frames. Basic properties:

| Property | Type | Description |
|----------|------|-------------|
| `url` | string | URL to render (http://, https://, file://, or data:) |

### Example: Import via gst-launch

In the Strom UI, use "Import gst-launch" to add a cefsrc pipeline:

```bash
cefsrc url=https://example.com ! videoconvert ! autovideosink
```

### Example: API

```bash
# Parse pipeline to flow elements
curl -X POST http://localhost:8080/api/gst-launch/parse \
  -H "Content-Type: application/json" \
  -d '{"pipeline": "cefsrc url=https://example.com ! videoconvert ! fakesink"}'
```

### Example: Transparent Overlay (Data URL)

This example renders a bouncing ball on a transparent background, useful for overlays:

```json
{
  "id": "00000000-0000-0000-0000-000000000002",
  "name": "ball overlay",
  "elements": [
    {
      "id": "cefsrc_0",
      "element_type": "cefsrc",
      "properties": {
        "url": "data:text/html,<style>body{margin:0;background:transparent}</style><canvas id=c></canvas><script>const c=document.getElementById('c'),x=c.getContext('2d');c.width=1920;c.height=1080;let bx=100,by=100,dx=4,dy=3;function d(){x.clearRect(0,0,1920,1080);x.beginPath();x.arc(bx,by,60,0,Math.PI*2);x.fillStyle='%23ff6b6b';x.fill();x.strokeStyle='%23fff';x.lineWidth=4;x.stroke();bx+=dx;by+=dy;if(bx>1860||bx<60)dx=-dx;if(by>1020||by<60)dy=-dy;requestAnimationFrame(d)}d()</script>"
      },
      "position": [100.0, 200.0]
    }
  ],
  "blocks": [],
  "links": []
}
```

### Example: Import Flow JSON

Import this flow via the UI (Import → JSON) to render a live wind map with WHEP output:

```json
{
  "id": "00000000-0000-0000-0000-000000000001",
  "name": "html render",
  "elements": [
    {
      "id": "cefsrc_0",
      "element_type": "cefsrc",
      "properties": {
        "url": "https://earth.nullschool.net/#current/wind/surface/level/orthographic=13.01,61.06,1232"
      },
      "position": [100.0, 200.0]
    }
  ],
  "blocks": [
    {
      "id": "whep_0",
      "block_definition_id": "builtin.whep_output",
      "properties": {
        "mode": "video",
        "endpoint_id": "html render"
      },
      "position": {"x": 400.0, "y": 200.0}
    }
  ],
  "links": [
    {
      "from": "cefsrc_0:src",
      "to": "whep_0:video_in"
    }
  ]
}
```

## How It Works

The `strom-full` Docker image includes:

1. **gstcefsrc plugin** - GStreamer plugin providing `cefsrc`, `cefdemux`, and `cefbin` elements
2. **Xvfb** - X Virtual Framebuffer for headless rendering
3. **CEF runtime** - Chromium libraries, locales, and resources

### Automatic Configuration

The entrypoint script automatically:

- Starts Xvfb on display `:99`
- Disables CEF sandbox (required for Docker root user)
- Uses software rendering for CEF by default (see GPU mode below for opt-in)
- Configures CEF cache and logging

No manual configuration is needed - just run the container and use `cefsrc` in your pipelines.

### GPU mode (opt-in, experimental)

CEF can be routed through the host NVIDIA GPU via ANGLE/Vulkan by setting
`STROM_CEF_GPU=1`. The software default is kept because:

- GPU mode has a roughly 50% CPU floor per `cefsrc` at 1080p30, independent of
  page content (continuous Vulkan command-buffer submits and compositor work).
- Software mode is near-zero-cost for idle or static pages — Chromium elides
  paint when nothing changes, and `cefsrc` emits duplicate buffers cheaply.

GPU mode pays off when the renderer is the bottleneck: canvas-heavy animations,
WebGL/3D scenes, or very high resolutions. For example, a 1080p30 wind-map
(canvas + continuous simulation) drops from ~95% CPU to ~57% CPU with GPU mode
on an RTX 3090; the same simple static page goes from ~1% CPU to ~53%.

**Enabling GPU mode:**

```bash
docker run --gpus all \
  -e STROM_CEF_GPU=1 \
  -e NVIDIA_DRIVER_CAPABILITIES=all \
  -v /usr/share/vulkan/icd.d/nvidia_icd.json:/usr/share/vulkan/icd.d/nvidia_icd.json:ro \
  --network host \
  eyevinntechnology/strom-full:latest
```

Requirements:

- NVIDIA driver on the host and `nvidia-container-toolkit` installed
- `--gpus all` to pass the device into the container
- `NVIDIA_DRIVER_CAPABILITIES=all` so the toolkit mounts the full lib set
  (including `libGLX_nvidia.so.0`)
- Bind-mount of the host's `nvidia_icd.json` — the container toolkit does not
  mount the Vulkan ICD JSON automatically on all setups

The entrypoint prints `CEF GPU mode enabled (STROM_CEF_GPU=1) - ANGLE/Vulkan
on NVIDIA` when the GPU path activates, and warns if `STROM_CEF_GPU=1` is set
but no GPU is visible in the container.

**GPU mode does not give a page hardware video decode or encode.** It moves
painting and compositing to the GPU, nothing more. Chromium on Linux does
hardware video only through VA-API, and NVIDIA GPUs have no VA-API driver in
the Docker images, so a page's video (YouTube, a video call) is still decoded
and encoded in software by Chromium. NVDEC and NVENC go unused.

## Troubleshooting

### "Missing X server or $DISPLAY"

The Xvfb server may not have started. Check container logs:

```bash
docker logs <container_id>
```

Verify Xvfb is running:

```bash
docker exec <container_id> ps aux | grep Xvfb
```

### "locale_file_path.empty() for locale"

CEF can't find its locale files. This is fixed in strom-full:0.3.12+. Ensure you're using the latest image:

```bash
docker pull eyevinntechnology/strom-full:latest
```

### DBus errors in logs

Messages like "Failed to connect to the bus" are benign warnings - DBus is not available in the container but CEF works without it.

### High CPU usage

CEF renders pages continuously. For software mode the biggest levers are:
- **Resolution** — rendering at the target output size instead of 1080p is the
  single biggest win for software mode. 640x360 uses roughly 3x less CPU than
  1920x1080 because paint, compositor and BGRA transport all scale with pixel
  count. Pass width/height via `cefsrc` or a downstream capsfilter.
- **Framerate** — dropping from 30 to 15 fps roughly halves compositor and
  transport cost, but page-internal JS loops continue at the browser's own
  cadence unless the page is strictly `requestAnimationFrame`-driven.
- **Content complexity** — simpler HTML/CSS. Canvas simulations and heavy
  WebGL are CPU-bound in software mode.

For genuinely canvas/WebGL-heavy pages, consider GPU mode (see above) instead.

## Building gstcefsrc

The gstcefsrc plugin is pre-built and included in the strom-full image. For manual builds:

```bash
# Build the gstcefsrc plugin
cd docker/gstcefsrc
docker build --platform linux/amd64 -t gstcefsrc-builder:amd64 .

# Extract built files
docker run --rm -v $(pwd)/output:/export gstcefsrc-builder:amd64
```

The build uses Ubuntu Questing to match the strom base image's glibc version.

## HTML Input block

An HTML source is a block: set the URL, the viewport size and the framerate,
and pick whether the page's audio comes out as a second pad. Internally it is
`cefsrc` feeding `cefdemux`, with `cefdemux` built only when audio is asked
for. Raw `cefsrc` pipelines still work — the block just spares you the caps.

## Which pages work

The CEF binaries come from Spotify's CEF builds, which gstcefsrc downloads when
it is built. Those builds leave out the proprietary codecs, so **Chromium in
`strom-full` has no H.264**. A page that offers H.264 only, or that negotiates
it for WebRTC, cannot play its video.

Tested in `strom-full`:

| Page | Result |
|------|--------|
| YouTube | Works, audio and video |
| Google Meet | Works, audio and video |
| Microsoft Teams | Audio only, no video. The missing H.264 is the likely cause |

Video calls need the fake camera and microphone (see
[Camera, microphone, and what else a page cannot do](#camera-microphone-and-what-else-a-page-cannot-do)),
and WebRTC needs UDP to reach the call's servers. A source only takes media
out of a call: the block has no audio or video inputs, so nothing from Strom
goes back into the call.

## Strict Network Access

A page an HTML source renders goes on air, so a page on the server itself
would put the server's own services on screen: `http://127.0.0.1:9222/json/list`
lists every page the browser has open, and on a cloud VM
`http://169.254.169.254/` is the instance's metadata. **Strict Network Access**
on the HTML Input block keeps a source off the server and its local network,
and it is **on by default**.

With it on:

- The block's URL, a URL changed on air, remote control's address bar and the
  pin button refuse loopback, private, link-local and `localhost` addresses.
- With Strom's gstcefsrc build, Chromium also refuses the page's own
  requests to them (Local Network Access): fetches, frames, workers,
  navigations and WebSockets, checked against the address actually connected
  to.
- Raw `cefsrc` elements are strict too, unless the flow sets `strict-network`.

Turn it off only on your own machine, to render your own local pages.

> **Never expose this setting to anyone who is not the server's operator.** A
> system that lets customers edit flows must not let them switch it off.

What it does not cover:

- **WebRTC.** A page can have the browser send WebRTC connectivity checks
  (small STUN packets) over UDP to any address, internal ones included, and
  tell from the answers which ports are open. It cannot send data of its own
  that way, and Local Network Access in this Chromium does not look at WebRTC.
  UDP stays allowed, because blocking it breaks most WebRTC pages, which are
  a real use: a video call or a WebRTC player rendered as a source.
- A hostname that only resolves to an internal address, when it is the page's
  own URL. That, and anything Chromium itself might get
wrong, needs the browser's network locked down from outside, for instance by
not running with `--network host` and by dropping `169.254.169.254` for the
container.

## Camera, microphone, and what else a page cannot do

A page is never given the server's own cameras or microphones, which on a
broadcast server may be capture cards. With Strom's gstcefsrc build it gets a
camera and a microphone all the same, because some pages will not start
without them - a video call joined to be watched, for one. They are
synthetic: the microphone is silent and the camera shows a black frame.
Strom writes both into the CEF cache directory when it starts.

Nor does a page get anything that would need someone at the server: file
dialogs are cancelled, downloads refused, `alert`, `confirm` and `prompt`
answered as dismissed, printing cancelled, the right-click menu empty, and
drops refused. Left to CEF, a file dialog was built inside Strom and aborted
it, and `print()` froze the page.

A page's console messages go to the `cef_console` GStreamer debug category
(`GST_DEBUG=cef_console:5`), not to the container log.

## Browser profiles

Upstream gstcefsrc creates every browser in one shared context. Every HTML
source in a Strom process then shares one cookie jar, one local storage and one
HTTP cache: a login made for one source is a login for all of them, whichever
flow they are in.

Strom builds its own gstcefsrc for the `strom-full` image, with a patch that
adds a browser context per source (`isolated-context`, `context-cache-path` and
`persist-session-cookies` on `cefsrc`). When the plugin has it:

- **Every HTML Input block gets a profile of its own**, kept in the CEF cache
  directory, so a login survives a flow restart. The `strom-full` entrypoint
  clears that directory when the container starts, so it does not survive a
  container restart unless the cache is mounted elsewhere. Set **Browser Profile** to the
  same name on several blocks to let them share one — for example, several
  graphics from one logged-in dashboard.
- **Every raw `cefsrc` element gets one too**, keyed by its flow and element
  id, unless the flow sets `isolated-context` on it itself.
- **A popup the page opens shares its opener's profile**, so a "Sign in
  with…" window logs in the page that opened it.

Without it, Strom logs a warning for every HTML source it builds, and a remote
control link says that a login made through it reaches every HTML source in the
instance.

## Running HTML sources for several customers

A page an HTML source renders is code of the customer's choosing, running on
the server. Strict Network Access, Local Network Access and browser profiles
all hold against a page that plays by the browser's rules. None of them holds
against a page that exploits a bug in Chromium itself, and **Chromium's sandbox
is off in the `strom-full` image**: the image runs as root, which Chromium's
sandbox refuses, so the entrypoint passes `no-sandbox`. A renderer bug in one
customer's page is then code running as root in that Strom's container, with
every flow, credential and network that Strom can reach.

So the rule for customers who must not reach each other is:

**One Strom container per customer, on a locked network.**

- **One container per customer.** Each gets its own process, its own browser,
  its own debug port, its own API key and its own cache directory, and a CPU
  or memory limit of its own. A page that breaks out of the browser reaches
  that customer's own Strom and nothing of anyone else's.
- **No `--network host`.** With host networking, "loopback" and "local
  network" are the host's, and every service on it is in reach.
- **Containers cannot reach each other.** Put each customer's container on a
  network of its own, or drop traffic between them.
- **No cloud metadata.** Drop `169.254.169.254` (and the rest of
  `169.254.0.0/16`) for the containers, and on AWS require IMDSv2 with a hop
  limit of 1. The metadata service hands out the host's cloud credentials, and
  a hostname that resolves to it gets past Strict Network Access.

What one Strom shared between customers still gives, with Strom's gstcefsrc
build: sessions and cookies kept apart per source, and a filtered remote
control link that reaches only its own page and that page's profile. That is
enough against a customer who is curious or careless. It is not enough against
one who is hostile, because what separates them is one browser process
without a sandbox. If you share a Strom anyway:

- **Full DevTools must stay off.** It is not filtered and reaches every page in
  the process.
- **Strict Network Access must stay on**, and customers must never be able to
  change it.
- **Profile names are one namespace per Strom.** Strom does not know who owns a
  flow, so two customers who pick the same Browser Profile name share one. The
  system in front of Strom has to make the names its own, for instance by
  prefixing them with a tenant id, and has to scope who may mint a remote
  control link for which source, because Strom's authentication is per
  instance, not per customer.

Without Strom's gstcefsrc build, every HTML source in a Strom shares one cookie
jar, so a shared Strom is not an option at all.

## Remote control (logging in to a page)

An HTML source renders on the server, so a page behind a login shows its login
screen for as long as the flow runs. Strom can hand out a link that shows that
page as it is being rendered and passes clicks and keystrokes back to it, so an
operator can log in, clear a consent dialog or click a tab — in the browser that
is actually on air. Nothing about the session is copied anywhere.

The link carries the page and nothing else. Chromium's debug protocol is how
this works underneath, and that protocol is full control of the browser
process, so the proxy forwards only what a picture and an input device need and
refuses the rest.

> **A link is still worth guarding.** Whoever holds it sees and can type into a
> page that is on air, can point it at any http, https or data address, and can
> make that the block's URL, until the link expires or you revoke it. From
> inside your network an http address reaches whatever the server can.

An HTML source renders http, https and data URLs only, whether set on the block
or reached through a link. A bare address such as `example.com` is read as
`https://`. Everything else — `file:`, `chrome:`, `view-source:`, `javascript:`
and the rest — is refused.

Remote control needs Strom's own authentication configured. With none, minting
a link would take no credentials at all, so Strom refuses to open the debug
port and says so at startup. Enable it with a port that nothing else on the
host uses:

```toml
[cef]
debug_port = 9222
```

or `STROM_CEF_DEBUG_PORT=9222`. Two Strom instances on one host need two
different ports, the same way they already need two CEF profile directories.
Chromium binds the port to loopback; leave it there and never publish it.

With a flow running, ask which pages are available:

```bash
curl -H "Authorization: Bearer $STROM_API_KEY" \
  http://localhost:8080/api/devtools/targets
```

Mint a link for the one you want:

```bash
curl -X POST -H "Authorization: Bearer $STROM_API_KEY" \
  http://localhost:8080/api/devtools/targets/<target-id>/link
```

What comes back is a path, an id, and nothing else — one random key, no API
token, no target id, no address or port:

```json
{"id": "4b1e…", "path": "/devtools/7f3c…", "expires_in_seconds": 1800, "warning": "…"}
```

Open the path in your own browser and the page appears; click and type into it
as if it were yours, and paste works for a password manager. On a phone, tap to
click, drag to scroll, and use the keyboard button to type. Back, forward,
reload and a home button that returns to the block's URL are there, and the
address bar takes any http, https or data address. The pin button makes the page
you are on the block's URL, so the source starts there from then on. The key in
the path is the credential, so the link is handed to a person rather than
published, and it dies after half an hour of disuse.

The `id` is not a credential — it is the name you use to take the link back:

```bash
# What is still live, without handing any key back out
curl -H "Authorization: Bearer $STROM_API_KEY" \
  http://localhost:8080/api/devtools/links

# Kill one link, and any session already open on it
curl -X DELETE -H "Authorization: Bearer $STROM_API_KEY" \
  http://localhost:8080/api/devtools/links/<id>

# Kill all of them
curl -X DELETE -H "Authorization: Bearer $STROM_API_KEY" \
  http://localhost:8080/api/devtools/links
```

Revoking ends sessions that are already open, not just the next one — it is the
emergency stop, so it has to reach whoever is holding the socket.

A login survives a flow restart: the source's profile keeps the cookies, session
cookies included (see [Browser profiles](#browser-profiles)).
Chromium writes them on a timer, so a login made seconds before the process is
killed can still be lost.

A link stays alive while you work in it: opening it, and every click, keystroke
or navigation in a session, starts its half hour over. A tab left open and only
watched is closed when the time runs out.

### Full DevTools

For troubleshooting a page rather than operating it, Strom can serve Chromium's
DevTools application instead, with the protocol unfiltered:

```toml
[cef]
debug_port = 9222
full_devtools = true
```

or `STROM_CEF_FULL_DEVTOOLS=1`. This is not a richer version of the same thing.
DevTools needs exactly the parts of the protocol the filter exists to refuse,
so the two cannot be combined.

> **With this on, a link is control of the host, and it is instance-wide.** It
> runs arbitrary JavaScript, navigates anywhere including `file://`, and reads
> every cookie it can reach. One browser process serves every HTML source in
> a Strom instance, and the unfiltered protocol can attach to any of them, so a
> link reaches all of them, every page they are logged in to, and the files
> this process can read. Separate browser profiles do not change that. Give it
> only to someone you would trust with the instance itself, and never turn it
> on for a Strom shared between customers.

## Running with upstream gstcefsrc

The `strom-full` image ships Strom's own gstcefsrc build: upstream
[gstcefsrc](https://github.com/centricular/gstcefsrc) at a pinned commit, plus
four patches. The HTML Input block, raw `cefsrc` elements and remote control
all run on an upstream build too, for instance a native Linux install with a
gstcefsrc you built yourself. Strom checks which properties the plugin has and
logs a warning for each one that is missing, rather than failing the flow.

Remote control itself needs nothing from the patches. Its picture, input and
navigation use Chromium's own debug protocol, and the debug port is a plain
Chromium switch.

What you lose without each patch:

| Patch | With it | Without it |
|-------|---------|------------|
| Popup close | Closing a popup leaves the page that opened it alone | Closing a popup is taken for the source's own browser closing. Later URL changes on air do nothing, and stopping the flow leaves the page running, still logged in and on the network, until Strom exits. Remote control closes popups when it follows a login, so this happens in normal use |
| Browser context per source | Each source has its own cookies, storage and cache ([Browser profiles](#browser-profiles)) | Every HTML source in the process shares one cookie jar. A login made in one is a login in all of them, and the Browser Profile setting has no effect |
| Strict network | Chromium refuses a strict page's own requests to this machine and its network | Strom still refuses an internal address as the page's URL, but what the page itself fetches, frames or connects to is not checked. Of the Local Network Access checks Strom switches on, only the last one takes effect |
| Offscreen handlers | File dialogs, downloads, printing, JavaScript dialogs, the context menu and drops are all refused, and a page gets a fake camera and microphone | **A click on a file input through remote control can abort Strom, with every pipeline in it.** `print()` can freeze the page, a download is written to the server's disk without asking, and a page asking for a camera or microphone is given the server's real ones, which may be capture cards |

An upstream build is reasonable on your own machine, rendering pages you
trust, with an operator who knows not to click file inputs or print. It is not
an option for a Strom that renders pages for anyone else.

## Limitations

- **`strom-full` image only**: `cefsrc` comes from the gstcefsrc plugin, which Strom ships only in the `strom-full` image. The plain `strom` image and the native release builds (Linux, macOS, Windows) do not include it.
- **Linux: X11 required**: CEF needs an X server on Linux, which the strom-full image provides via Xvfb. This is why `strom-full` is the supported way to run HTML sources.
- **macOS: no native support yet**: CEF renders offscreen through its own macOS path, so Xvfb is not involved and the X11 requirement above does not apply. Native macOS support is tracked in [centricular/gstcefsrc#110](https://github.com/centricular/gstcefsrc/pull/110) (macOS build fixes) and [Eyevinn/strom#669](https://github.com/Eyevinn/strom/pull/669) (a Cocoa run loop on the main thread, needed in headless mode). CEF on macOS also refuses to initialise unless the host process is inside an `.app` bundle, and the macOS release ships a bare executable rather than a bundle.
- **Software rendering by default**: CEF uses CPU rendering; opt in to GPU with `STROM_CEF_GPU=1` (see above)
- **No H.264 in the page**: the CEF build has no proprietary codecs, so Teams shows no video (see [Which pages work](#which-pages-work))
- **No hardware video in the page on Linux**: even in GPU mode, Chromium decodes and encodes a page's video in software, because it needs VA-API and NVIDIA has none in the Docker images
- **Memory usage**: CEF spawns multiple processes (browser, renderer, GPU process)
- **No audio by default**: Use `cefbin` or `cefdemux` if you need audio from web content
- **No Chromium sandbox in `strom-full`**: the image runs as root and the entrypoint passes `no-sandbox`, so a Chromium bug in a page is code running in Strom's container. See [Running HTML sources for several customers](#running-html-sources-for-several-customers)
- **One browser process per instance**: every `cefsrc` shares one CEF process and one debugging port. Cookies and storage are per source only with Strom's gstcefsrc build (see [Browser profiles](#browser-profiles))
- **A removed block's profile goes at the next restart**: deleting a flow removes its sources' profiles at once. A block or element removed from a flow that is kept, and a named profile no block uses any more, stay until Strom next starts

## References

- [gstcefsrc GitHub](https://github.com/centricular/gstcefsrc) - GStreamer CEF plugin
- [CEF Project](https://bitbucket.org/chromiumembedded/cef) - Chromium Embedded Framework
