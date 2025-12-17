# Media Player Interactive Test Harness

A standalone Rust binary (`mediaplayer-test`) for interactive testing of the MediaPlayer block.

## Overview

The test harness:
- Starts the Strom server as a subprocess
- Captures and displays server logs in real-time
- Runs test scenarios sequentially via API
- Streams output to SRT for human verification
- Shows interactive prompts describing expected behavior
- Human confirms pass/fail at each step

## Quick Start

```bash
# Run all tests
cargo run --bin mediaplayer-test

# Start from a specific phase
cargo run --bin mediaplayer-test -- --phase 3

# Custom configuration
TEST_MEDIA_DIR=/path/to/media cargo run --bin mediaplayer-test
```

Then open your SRT player:
```bash
ffplay srt://127.0.0.1:5100
```

## Configuration

| Env Variable | Default | Description |
|--------------|---------|-------------|
| `TEST_SRT_URI` | `srt://:5100?mode=listener` | SRT output endpoint |
| `TEST_MEDIA_DIR` | `./media` | Path to media files |
| `STROM_PORT` | `8080` | Server port |

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  mediaplayer-test binary                                │
│  ┌─────────────────┐  ┌──────────────────────────────┐ │
│  │ Server Manager  │  │ Test Runner                  │ │
│  │ - spawn server  │  │ - API client (reqwest)       │ │
│  │ - capture logs  │  │ - test scenarios             │ │
│  │ - health check  │  │ - interactive prompts        │ │
│  └────────┬────────┘  └──────────────┬───────────────┘ │
│           │                          │                  │
│           ▼                          ▼                  │
│  ┌─────────────────┐  ┌──────────────────────────────┐ │
│  │ Log Display     │  │ Flow Builder                 │ │
│  │ - prefix [SVR]  │  │ - MediaPlayer block          │ │
│  │ - color coded   │  │ - MPEGTSSRT block            │ │
│  └─────────────────┘  │ - VideoEncoder (decode mode) │ │
│                       └──────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
         │
         ▼
   Human watches SRT stream (VLC/ffplay)
```

## File Structure

```
backend/src/bin/mediaplayer_test/
├── main.rs           # Entry point, CLI args, test cases
├── server.rs         # Server subprocess management
├── api.rs            # HTTP client for Strom API
├── flow_builder.rs   # Build test flows (MediaPlayer + MPEGTSSRT)
└── prompt.rs         # Interactive prompts with colored output
```

## Test Phases

### Phase 1: Basic Playback (Passthrough)
| Test | Description |
|------|-------------|
| 1.1 | Basic video+audio playback |
| 1.2 | Pause |
| 1.3 | Resume |

### Phase 2: Playlist Navigation
| Test | Description |
|------|-------------|
| 2.1 | Start with multiple files |
| 2.2 | Next file |
| 2.3 | Next file again |
| 2.4 | Previous file |
| 2.5 | Go to first file |

### Phase 3: Decode Mode (MediaPlayer → VideoEncoder → MPEGTSSRT)
| Test | Description |
|------|-------------|
| 3.1 | Basic playback with re-encoding |
| 3.2 | Pause |
| 3.3 | Resume |
| 3.4 | Playlist with multiple files |
| 3.5 | Next file |
| 3.6 | Previous file |

### Phase 4: Edge Cases
| Test | Description |
|------|-------------|
| 4.1 | Start playlist |
| 4.2 | Previous at start (should reject) |
| 4.3 | Go to last file |
| 4.4 | Next at end (should reject) |

### Phase 5: Loop/Wrap Playlist
| Test | Description |
|------|-------------|
| 5.1 | Start playlist with loop enabled |
| 5.2 | Go to last file |
| 5.3 | Next should wrap to first |
| 5.4 | Previous should wrap to last |

## Test Media Files

Required files in `TEST_MEDIA_DIR`:
- `BigBuckBunny.mp4` - Primary test file (~10 min)
- `ElephantsDream.mp4` - Secondary file (~11 min)
- `Sintel.mp4` - Third file (~15 min)

## Implementation Status

### Completed
- [x] Server manager (spawn, logs, health check, shutdown)
- [x] API client (all player control methods)
- [x] Flow builder (passthrough and decode modes)
- [x] Interactive prompts (colored output, y/n/s/q)
- [x] CLI `--phase N` argument
- [x] Basic playback tests
- [x] Playlist navigation tests
- [x] Decode mode tests (with VideoEncoder)
- [x] Edge case tests
- [x] Loop/wrap tests
- [x] Cleanup of stale flows (prevents SRT port conflicts)

### Not Implemented / Deferred
- [ ] **Seek tests** - Seeking doesn't work properly with `sync=true` live sinks
- [ ] **Media type variations** - Audio-only, video-only, H.265 files
- [ ] **Empty playlist handling**
- [ ] **Invalid file path handling**
- [ ] **Very short file handling**

## Example Output

```
╔══════════════════════════════════════════════════════════╗
║  MEDIA PLAYER TEST HARNESS                               ║
╚══════════════════════════════════════════════════════════╝

[TEST] Starting Strom server...
[SERVER] GStreamer initialized
[SERVER] Server listening on 0.0.0.0:8080
[TEST] ✓ Server ready!

[TEST] Open your SRT player:
       ffplay srt://127.0.0.1:5100

Press Enter when your player is ready...

═══════════════════════════════════════════════════════════
TEST 1.1: Basic video+audio playback (passthrough mode)
═══════════════════════════════════════════════════════════

[TEST] Creating flow: MediaPlayer → MPEGTSSRT
[TEST] Starting flow...

┌─────────────────────────────────────────────────────────┐
│ EXPECTED OBSERVATION:                                   │
│   • Video should be playing smoothly                    │
│   • Audio should be in sync with video                  │
│   • No artifacts or glitches                            │
└─────────────────────────────────────────────────────────┘

Did you observe the expected behavior? [Y]es/[n]o/[s]kip/[q]uit: y

[TEST] ✓ Test 1.1 PASSED
```

## Known Issues

1. **Seek not working**: Seeking with `sync=true` (live sink mode) causes issues.
   The MediaPlayer seek functionality works, but the MPEGTSSRT output block
   doesn't handle the seek properly when configured for real-time streaming.

2. **Transition glitches**: File transitions in decode mode may have brief
   glitches as the VideoEncoder restarts encoding for the new stream.
