# Block Feature Implementation Guide

> **Archived — code is the source of truth.** This describes the original design and intent
> of the block system. The code has very likely drifted since; treat this as a high-level
> pointer to what was once intended, not as an accurate spec — read the code for the current
> implementation.

This document describes the block system architecture and how to add new built-in blocks.

## Overview

Blocks are reusable pipeline components that encapsulate multiple GStreamer elements with a simplified interface. They provide:

- **Abstraction**: Hide complex GStreamer configurations behind simple properties
- **Reusability**: Common patterns packaged as drag-and-drop components
- **Discoverability**: Organized by category with descriptions and icons

## Architecture

### Block Types

- **Built-in blocks** (`builtin.*`): Shipped with Strom, read-only
- **User blocks** (`user.*`): Custom blocks created by users

### Key Components

| Component | Location | Purpose |
|-----------|----------|---------|
| Block types | `types/src/block.rs` | Type definitions |
| Block registry | `backend/src/blocks/registry.rs` | Block discovery and lookup |
| Block builders | `backend/src/blocks/builtin/*.rs` | Pipeline construction |
| Block API | `backend/src/api/blocks.rs` | REST endpoints |

## Built-in Blocks

Current built-in blocks in `backend/src/blocks/builtin/`:

| Block | File | Description |
|-------|------|-------------|
| AES67 Input/Output | `aes67.rs` | RTP audio over IP with PTP sync |
| WHIP Input/Output | `whip.rs` | WebRTC ingestion (WHIP server and client) |
| WHEP Input/Output | `whep.rs` | WebRTC egress (WHEP client and server with built-in player) |
| Audio/Video Meter | `meter.rs` | Level monitoring with visualization |
| Audio Format | `audioformat.rs` | Sample rate, channels, format conversion |
| Video Format | `videoformat.rs` | Resolution, framerate, pixel format conversion |
| Video Encoder | `videoenc.rs` | Auto hardware encoder selection (H.264/H.265/AV1/VP9) |
| MPEG-TS/SRT Input | `mpegtssrt_input.rs` | MPEG-TS demuxing from SRT with decode/passthrough |
| MPEG-TS/SRT Output | `mpegtssrt.rs` | MPEG-TS muxing with SRT transport |
| Video Compositor | `compositor.rs` | OpenGL video mixing with layout editor |
| DeckLink Input/Output | `decklink.rs` | Blackmagic SDI/HDMI capture and playback |
| NDI Input/Output | `ndi.rs` | NewTek NDI video over IP |
| Media Player | `mediaplayer.rs` | File playback with playlist support |
| Audio Mixer | `mixer/` | Stereo mixer with per-channel processing, aux sends, subgroups |
| Audio Router | `audiorouter.rs` | Multi-stream channel routing matrix with mixing and fan-out |
| Loudness Meter | `loudness.rs` | EBU R128 real-time loudness measurement with reset |
| Spectrum Analyzer | `spectrum.rs` | Real-time audio spectrum visualization |
| Audio Analyzer | `audioanalyzer.rs` | Real-time waveform and vectorscope visualization |
| Recorder | `recorder.rs` | Write audio/video streams to file with splitmuxsink |

See [MIXER_BLOCK.md](MIXER_BLOCK.md) and [VIDEO_ENCODER_BLOCK.md](VIDEO_ENCODER_BLOCK.md) for detailed documentation.

## Adding a New Block

### 1. Create the block file

Create `backend/src/blocks/builtin/myblock.rs`:

```rust
use super::*;

pub struct MyBlockBuilder;

impl BlockBuilder for MyBlockBuilder {
    fn id(&self) -> &'static str {
        "builtin.myblock"
    }

    fn build(&self, ctx: &mut BlockBuildContext, properties: &Properties) -> Result<BlockBuildResult> {
        // Create GStreamer elements
        // Set up internal links
        // Return result with elements and pads
    }
}
```

### 2. Register in mod.rs

Add to `backend/src/blocks/builtin/mod.rs`:

```rust
mod myblock;
pub use myblock::MyBlockBuilder;

// In get_builtin_blocks():
builders.push(Box::new(MyBlockBuilder));
```

### 3. Define block metadata

Implement `BlockBuilder::definition()` to provide:
- Name and description
- Category for palette organization
- Exposed properties with types and defaults
- External pads (inputs/outputs)
- UI metadata (color, icon, size)

## Testing

```bash
# Run backend tests
cargo test --package strom

# Test block registry
cargo test --package strom --lib blocks::registry

# Test via Swagger UI
cargo run -p strom
# Visit http://localhost:8080/swagger-ui → Blocks endpoints
```

## Notes

- Block expansion happens at pipeline creation time
- Blocks are purely a configuration abstraction
- At runtime, everything becomes native GStreamer elements
- Block IDs with `builtin.` prefix are read-only
- Block IDs with `user.` prefix are user-defined (future feature)
