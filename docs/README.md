# Strom Documentation

Start here. For a quick overview of what Strom is, see the [root README](../README.md).
Common questions are answered in the [FAQ](FAQ.md).

## Getting started & deployment

- [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) — guided setup for running a local Strom instance (Docker, GPU, ICE, auth). Good first stop for operators.
- [DOCKER.md](DOCKER.md) — generic Docker deployment reference.
- [DOCKER_GPU_SETUP.md](DOCKER_GPU_SETUP.md) — NVIDIA GPU acceleration (NVENC/NVDEC, CUDA-GL interop, container toolkit).
- [AUTHENTICATION.md](AUTHENTICATION.md) — session login and API keys.
- [POSTGRESQL.md](POSTGRESQL.md) — PostgreSQL storage backend for production.

## Using Strom

- [VISION_MIXER_OPERATOR_GUIDE.md](VISION_MIXER_OPERATOR_GUIDE.md) — broadcast PVW/PGM switcher: transitions, DSK, PiP, multiview.
- [AUDIO_MIXER_OPERATOR_GUIDE.md](AUDIO_MIXER_OPERATOR_GUIDE.md) — audio mixing console signal flow and operation.
- [HTML_RENDER.md](HTML_RENDER.md) — render web pages as video sources (CEF / `strom-full`).
- [STREAM_SYNCHRONIZATION.md](STREAM_SYNCHRONIZATION.md) — aligning multiple inputs with PTP/NTP clocks.

The full set of built-in blocks and their properties is best browsed in the app's element
palette and inspector — the code is the source of truth. Older block design writeups live in
[archive/](archive/).

## API & integration

- [MCP.md](MCP.md) — Model Context Protocol server (AI assistant integration).
- [INTEGRATION.md](INTEGRATION.md) — MCP / OpenAPI integration overview.
- Interactive OpenAPI docs are served at `/swagger-ui` on a running instance.

## Host setup scripts

Ready-to-run scripts for preparing a host, under [`scripts/setup/`](../scripts/setup)
(also bundled in the Docker images at `/app/scripts/setup/`):
[nvidia](../scripts/setup/nvidia/README.md) ·
[decklink](../scripts/setup/decklink/README.md) ·
[ndi](../scripts/setup/ndi/README.md) ·
[ntp](../scripts/setup/ntp/README.md).

## Contributing & building

- [DEVELOPMENT.md](DEVELOPMENT.md) — build, run, and develop locally.
- [CONTRIBUTING.md](CONTRIBUTING.md) — contribution guidelines.
- [CROSS_COMPILE_ARM64.md](CROSS_COMPILE_ARM64.md) — cross-compiling for ARM64 (Raspberry Pi etc.).
- [DEBUGGING_SEGFAULTS_WSL2.md](DEBUGGING_SEGFAULTS_WSL2.md) — debugging segfaults (especially on WSL2).

## History & ideas

- [CHANGELOG.md](CHANGELOG.md) — release history.
- [FEATURE_SUGGESTIONS.md](FEATURE_SUGGESTIONS.md) — unordered idea list (not a roadmap).
- [archive/](archive/) — historical material kept for reference: solved-problem postmortems,
  completed audits, and original design/implementation writeups that have likely drifted from
  the code (block design, compositor editor, AES67 discovery, app navigation, …).
