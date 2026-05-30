# Setting Up a Local Strom Instance for Open Live

This section walks through deploying a local [Strom](https://github.com/Eyevinn/strom) instance that Open Live can connect to.

[Open Live](https://github.com/Eyevinn/open-live) is an open-source live production platform, with [Open Live Studio](https://github.com/Eyevinn/open-live-studio) as its web-based production interface. Strom is a standalone GStreamer flow engine that handles the actual media pipelines — it is not tied to Open Live, but Open Live uses it as a backend, driving it over its REST and WebSocket APIs. This guide covers running your own Strom instance for Open Live to connect to.

Linux + Docker is the primary supported target — that combination is what we run in production and test against. Native Linux binaries, macOS, and Windows builds exist, but the instructions below assume the Docker path.

For full details on any topic below, follow the links to the upstream Strom documentation.

---

## 1. Prerequisites

A Linux host (Ubuntu 22.04+ or equivalent) with:

- Docker Engine and `docker compose`
- Network reachability from the machine running Open Live to this host on the Strom port (default `8080`)
- An NVIDIA GPU — **highly preferred** for hardware-accelerated encode/decode and GPU compositing. Optional, but production deployments should plan for one; software fallbacks do not scale.
- Optional: a Blackmagic DeckLink card for SDI I/O

---

## 2. Pull and Run Strom

Two multi-arch images are published on Docker Hub:

- **`eyevinntechnology/strom-full:latest`** — recommended default. Bundles CEF/Chromium so HTML pages can be used as video sources (graphics, lower-thirds, scoreboards, web-based overlays). See [HTML_RENDER.md on GitHub](https://github.com/Eyevinn/strom/blob/main/docs/HTML_RENDER.md).
- **`eyevinntechnology/strom:latest`** — smaller image without CEF/HTML rendering. Use this if you do not need browser-rendered graphics and want a leaner footprint.

Minimal run:

```bash
mkdir -p data
docker run -d \
  --name strom \
  --restart unless-stopped \
  -p 8080:8080 \
  -v "$(pwd)/data:/data" \
  eyevinntechnology/strom-full:latest
```

Open `http://<host>:8080` in a browser to confirm the web UI loads. The `/data` volume persists flows, blocks, and other configuration across restarts.

### Getting the Host Setup Scripts From the Image

The host-side helper scripts (NVIDIA, DeckLink, NDI, NTP) are bundled inside both Docker images at `/app/scripts/setup/`. You do not need to clone the GitHub repo — extract them straight out of the image:

```bash
# Pull the image first (if not already)
docker pull eyevinntechnology/strom-full:latest

# Extract /app/scripts/setup from the image into ./strom-setup on the host
id=$(docker create eyevinntechnology/strom-full:latest)
docker cp "$id":/app/scripts/setup ./strom-setup
docker rm "$id"

chmod +x ./strom-setup/nvidia/*.sh ./strom-setup/decklink/*.sh 2>/dev/null
```

The rest of this guide assumes the scripts live in `./strom-setup/`. Adjust paths if you extracted them elsewhere (or if you have a clone of the repo, use `scripts/setup/` from there directly).

---

## 3. NVIDIA GPU Setup (Highly Preferred, Optional)

Strom uses NVENC/NVDEC and CUDA-GL interop for hardware video encoding, decoding, and compositing. An NVIDIA GPU is **highly preferred** — without one, encoding falls back to software (x264 etc.), which works for small flows but does not scale. Production deployments should plan to have one.

You can skip this section for a CPU-only trial install, but expect to revisit it before going live.

Two helper scripts are bundled in the image (see "Getting the Host Setup Scripts From the Image" above), under `nvidia/`:

```bash
# Install the recommended NVIDIA driver (requires reboot)
sudo ./strom-setup/nvidia/install-nvidia-driver.sh

# After reboot, verify the driver
nvidia-smi

# Install the NVIDIA Container Toolkit so Docker can see the GPU
sudo ./strom-setup/nvidia/install-nvidia-container-toolkit.sh

# Sanity check
docker run --rm --gpus all ubuntu nvidia-smi
```

Important:

- Do **not** install the `nvidia-headless` driver variant — it lacks the OpenGL/EGL bits needed for CUDA-GL interop.
- The toolkit script also pins Docker's cgroup driver to `cgroupfs` and installs a udev rule that prevents containers from losing GPU access on `systemctl daemon-reload` (a known NVIDIA/Docker interaction).

When you then run Strom, add `--gpus all` and `NVIDIA_DRIVER_CAPABILITIES=all`:

```bash
docker run -d \
  --name strom \
  --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=all \
  -p 8080:8080 \
  -v "$(pwd)/data:/data" \
  eyevinntechnology/strom-full:latest
```

Strom logs whether interop succeeded at startup:

```
INFO  CUDA-GL interop works - using GPU-accelerated video conversion
INFO  NVML initialized successfully - found 1 GPU(s)
```

For a deeper dive (headless EGL, troubleshooting, WSL2 caveats), see `./strom-setup/nvidia/README.md` (also [on GitHub](https://github.com/Eyevinn/strom/blob/main/scripts/setup/nvidia/README.md)) and the [Strom Docker GPU guide](https://github.com/Eyevinn/strom/blob/main/docs/DOCKER_GPU_SETUP.md).

---

## 4. DeckLink SDI I/O Setup (Optional)

Skip this section if you do not have Blackmagic DeckLink cards.

DeckLink support requires:

1. The Blackmagic **Desktop Video** package installed on the host (compiles a DKMS kernel module — a reboot is required after install).
2. The container to run with `--privileged` and the DeckLink device nodes and SDK libraries mounted in.

Helper scripts are bundled in the image under `decklink/` (see "Getting the Host Setup Scripts From the Image" above):

- `./strom-setup/decklink/probe-signal.sh` — scan every device/connection combination and report which inputs have a live signal.
- `./strom-setup/decklink/verify-decklink.sh` — sanity-check that the card is visible from inside the container.

The README at `./strom-setup/decklink/README.md` (also viewable [on GitHub](https://github.com/Eyevinn/strom/blob/main/scripts/setup/decklink/README.md)) has the full host install walkthrough.

The minimum Docker flags for a DeckLink-enabled Strom container:

```bash
docker run -d \
  --name strom \
  --privileged \
  --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=all \
  -v /dev/blackmagic:/dev/blackmagic \
  -v /usr/lib/libDeckLinkAPI.so:/lib/libDeckLinkAPI.so:ro \
  -v /usr/lib/libDeckLinkPreviewAPI.so:/lib/libDeckLinkPreviewAPI.so:ro \
  -v /usr/lib/blackmagic:/lib/blackmagic:ro \
  -p 8080:8080 \
  -v "$(pwd)/data:/data" \
  eyevinntechnology/strom-full:latest
```

For card profile configuration (which BNCs are inputs vs outputs, half- vs full-duplex on Quad 2 / 8K Pro cards), see the [DeckLink setup README on GitHub](https://github.com/Eyevinn/strom/blob/main/scripts/setup/decklink/README.md).

---

## 5. Docker Compose Example

A `docker-compose.yml` for a typical production-style deployment with GPU, authentication, persistent data, and host networking:

```yaml
services:
  strom:
    image: eyevinntechnology/strom-full:latest
    container_name: strom
    network_mode: host
    restart: unless-stopped
    environment:
      - TZ=Europe/Stockholm
      # Authentication (see section 7)
      - STROM_ADMIN_USER=admin
      - STROM_ADMIN_PASSWORD_HASH=$$2b$$12$$REPLACE_WITH_YOUR_OWN_BCRYPT_HASH
      - STROM_API_KEY=REPLACE_WITH_A_LONG_RANDOM_KEY
      # GPU
      - NVIDIA_VISIBLE_DEVICES=all
      - NVIDIA_DRIVER_CAPABILITIES=all
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: [gpu]
    cap_add:
      - SYS_NICE
    logging:
      driver: json-file
      options:
        max-size: "50m"
        max-file: "5"
    volumes:
      - ./data:/data
```

Notes:

- `network_mode: host` is recommended when running WHEP/WHIP, AES67, NDI, or SRT — these protocols are easier to operate without Docker NAT.
- The `$$` in `STROM_ADMIN_PASSWORD_HASH` is required in `docker compose` to escape the literal `$` characters in the bcrypt hash. If you set the variable directly via `docker run -e`, use a single `$`.
- Add the DeckLink volume mounts from section 4 if applicable.
- To terminate TLS in Strom itself, mount your certificates and set `STROM_TLS_CERT` / `STROM_TLS_KEY`. See [README — HTTPS/TLS](https://github.com/Eyevinn/strom#httpstls).

Bring it up:

```bash
docker compose up -d
docker compose logs -f strom
```

---

## 6. Networking: What Open Live Needs to Reach

Strom exposes a single HTTP/WebSocket endpoint:

| Port    | Protocol  | Purpose                                                     |
|---------|-----------|-------------------------------------------------------------|
| `8080`  | HTTP(S)   | Web UI, REST API (`/api/...`), WebSocket (`/api/ws`), SSE (`/api/events`), MCP (`/api/mcp`), OpenAPI (`/swagger-ui`) |

This is the port Open Live needs reachable. Override it with `STROM_PORT` or `--port` if `8080` is taken on the host.

Media-plane ports (RTP/SRT/WHIP/WHEP/AES67/NDI) are determined by the flows you build inside Strom and are independent of the control port — open those on the firewall as needed for each flow.

---

## 7. ICE Servers (STUN / TURN) for WebRTC

WHIP and WHEP blocks (and the built-in WHEP player) need ICE servers to traverse NAT. Strom ships with a single public STUN server (`stun:stun.l.google.com:19302`) as the default — this is fine for quick demos on permissive networks, but **any real deployment should configure its own STUN and at least one TURN server**.

### Configuration

Set the list either via environment variable (comma-separated) or in `.strom.toml`.

URL format follows RFC 7064/7065:

```
stun:<host>:<port>
turn:<user>:<password>@<host>:<port>
turns:<user>:<password>@<host>:<port>     # TLS-secured TURN
```

`stun://` / `turn://` / `turns://` forms (with the slashes) are accepted and normalized.

**Environment variable** (recommended for Docker):

```bash
STROM_SERVER_ICE_SERVERS=stun:stun.example.com:3478,turn:alice:secret@turn.example.com:3478,turns:alice:secret@turn.example.com:5349
```

**Config file** (`.strom.toml`):

```toml
[server]
ice_servers = [
  "stun:stun.example.com:3478",
  "turn:alice:secret@turn.example.com:3478",
  "turns:alice:secret@turn.example.com:5349",
]
```

### Transport Policy

By default Strom offers all candidate types (host, server-reflexive, relay). To force every WebRTC connection through a TURN relay — useful when the publisher/viewer might be on networks that block direct UDP, or when you do not want endpoints to learn each other's IPs — set:

```bash
STROM_SERVER_ICE_TRANSPORT_POLICY=relay
```

or in `.strom.toml`:

```toml
[server]
ice_transport_policy = "relay"   # "all" (default) or "relay"
```

### Docker Compose Example

Add to the `environment:` block in section 5:

```yaml
      # ICE servers for WHIP/WHEP NAT traversal
      - STROM_SERVER_ICE_SERVERS=stun:stun.example.com:3478,turn:alice:secret@turn.example.com:3478
      # Optional: force relay-only candidates
      # - STROM_SERVER_ICE_TRANSPORT_POLICY=relay
```

### Verifying

The configured list is exposed at `GET /api/ice-servers` (subject to auth if enabled):

```bash
curl -H "Authorization: Bearer $STROM_API_KEY" http://<host>:8080/api/ice-servers
```

The same list is what the WHEP player and WHIP blocks hand to their `webrtcbin` instances at flow start.

### Notes

- For self-hosted TURN, [coturn](https://github.com/coturn/coturn) is the common open-source option. Use long random shared secrets, not human-friendly passwords.
- TURN traffic goes via the TURN server's ports, not through Strom — Strom only needs reachability to the TURN server's STUN/TURN listener.
- Restart Strom after changing the list — it is read at startup.

---

## 8. Security: Authentication When Exposing to the Internet

Strom runs **unauthenticated by default**. This is acceptable only on a fully trusted network. If the Strom port is reachable from the public internet — or from any network you don't fully trust — you **must** enable both a session login and an API key.

### Generate a bcrypt password hash

```bash
docker run --rm -it eyevinntechnology/strom-full:latest hash-password
# Enter your desired password when prompted
# Copy the resulting $2b$12$... hash
```

### Configure the container

Set three environment variables:

```bash
STROM_ADMIN_USER=admin
STROM_ADMIN_PASSWORD_HASH='$2b$12$...'      # the hash from above; single quotes
STROM_API_KEY='a-long-random-string'        # used by Open Live and other API clients
```

Once these are set:

- The web UI requires login at `/login`.
- All API endpoints (except `/health`, `/api/login`, `/api/logout`, `/api/auth/status`) require either a valid session cookie or `Authorization: Bearer <STROM_API_KEY>`.

Generate a strong API key, for example:

```bash
openssl rand -base64 32
```

If you publish Strom over the public internet, also terminate TLS — either via Strom's built-in TLS (`STROM_TLS_CERT` / `STROM_TLS_KEY`) or a reverse proxy (nginx, Caddy, Traefik).

Full reference: [docs/AUTHENTICATION.md on GitHub](https://github.com/Eyevinn/strom/blob/main/docs/AUTHENTICATION.md).

---

## 9. Verifying the Installation

From the Strom host or any client that can reach it:

```bash
# Health endpoint — no auth required
curl http://<host>:8080/health

# Authenticated check (when STROM_API_KEY is set)
curl -H "Authorization: Bearer $STROM_API_KEY" http://<host>:8080/api/flows
```

Open the web UI at `http://<host>:8080`, log in, and confirm that:

- The element palette loads.
- The topbar shows CPU / memory / GPU usage (the GPU row appears only if NVIDIA is wired up correctly).
- `/swagger-ui` renders the OpenAPI documentation.

Open Live can now be pointed at `http://<host>:8080` (or `https://...`) using the configured API key.

---

## Further Reading

- [Strom on GitHub](https://github.com/Eyevinn/strom) — source, releases, issue tracker
- [docs/DOCKER.md](https://github.com/Eyevinn/strom/blob/main/docs/DOCKER.md) — full Docker deployment guide, including reverse-proxy and production tips
- [docs/AUTHENTICATION.md](https://github.com/Eyevinn/strom/blob/main/docs/AUTHENTICATION.md) — session login and API key details
- [scripts/setup/nvidia/README.md](https://github.com/Eyevinn/strom/blob/main/scripts/setup/nvidia/README.md) — GPU driver and container toolkit setup
- [scripts/setup/decklink/README.md](https://github.com/Eyevinn/strom/blob/main/scripts/setup/decklink/README.md) — Blackmagic DeckLink setup
- [docs/POSTGRESQL.md](https://github.com/Eyevinn/strom/blob/main/docs/POSTGRESQL.md) — switching from JSON file storage to PostgreSQL for production
- [docs/MCP.md](https://github.com/Eyevinn/strom/blob/main/docs/MCP.md) — using the built-in Model Context Protocol server
