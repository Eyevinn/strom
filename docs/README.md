# Strom Documentation

Start here. For a quick overview of what Strom is, see the [root README](../README.md).
Common questions are answered in the [FAQ](FAQ.md).

## Getting started & deployment

- [OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md) — guided setup for running a local Strom instance (Docker, GPU, ICE, auth). Good first stop for operators.
- [DOCKER.md](DOCKER.md) — full Docker / Docker Compose deployment reference.
- [docker-gpu-setup.md](docker-gpu-setup.md) — NVIDIA GPU acceleration (NVENC/NVDEC, CUDA-GL interop, container toolkit).
- [AUTHENTICATION.md](AUTHENTICATION.md) — session login and API keys.
- [POSTGRESQL.md](POSTGRESQL.md) — PostgreSQL storage backend for production.

## Using Strom

- [VISION_MIXER_OPERATOR_GUIDE.md](VISION_MIXER_OPERATOR_GUIDE.md) — broadcast PVW/PGM switcher: transitions, DSK, PiP, multiview.
- [AUDIO_MIXER_OPERATOR_GUIDE.md](AUDIO_MIXER_OPERATOR_GUIDE.md) — audio mixing console signal flow and operation.
- [HTML_RENDER.md](HTML_RENDER.md) — render web pages as video sources (CEF / `strom-full`).
- [STREAM_SYNCHRONIZATION.md](STREAM_SYNCHRONIZATION.md) — aligning multiple inputs with PTP/NTP clocks.
- [COMPOSITOR_EDITOR.md](COMPOSITOR_EDITOR.md) — first-generation compositor layout editor (legacy; prefer the Vision Mixer).

## Blocks & API reference

- [BLOCKS_IMPLEMENTATION.md](BLOCKS_IMPLEMENTATION.md) — block system architecture and how to add a block.
- [MIXER_BLOCK.md](MIXER_BLOCK.md) — Audio Mixer block reference.
- [VIDEO_ENCODER_BLOCK.md](VIDEO_ENCODER_BLOCK.md) — Video Encoder block reference.
- [MCP.md](MCP.md) — Model Context Protocol server (AI assistant integration).
- [INTEGRATION.md](INTEGRATION.md) — MCP / OpenAPI integration overview.

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
- [design/](design/) — architecture/design notes (AES67 discovery, app navigation).
- [archive/](archive/) — solved-problem postmortems and built design specs, kept for reference.

## Design notes

- [design/aes67-discovery.md](design/aes67-discovery.md) — AES67/SAP discovery design.
- [design/app-navigation.md](design/app-navigation.md) — frontend page architecture.
