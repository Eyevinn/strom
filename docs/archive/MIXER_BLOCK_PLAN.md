# Stereo Mixer Block - Implementation Plan

> **Archived — built.** This is a historical design document from the planning phase; the mixer has long since been implemented. The code is the source of truth. For usage, see the [Audio Mixer operator guide](../AUDIO_MIXER_OPERATOR_GUIDE.md); the older block writeup is at [MIXER_BLOCK.md](MIXER_BLOCK.md).

## Overview

Build a digital audio mixer block for Strom, similar to professional digital consoles
like Behringer X32/M32, Yamaha TF series. The mixer provides:

- Configurable number of input channels
- Per-channel processing (gate, compressor, EQ, pan, fader)
- Aux sends (pre/post fader) for monitor mixes and effects
- Subgroups for bus processing
- Main stereo output with master processing
- PFL (Pre-Fader Listen) monitoring
- Real-time parameter control without pipeline restart
- Fullscreen GUI (Live Audio view)

## Architecture

### Signal Flow per Channel

```
Input
  │
  ├── PFL tap ──────────────────────────────────┐
  │                                              │
  ▼                                              │
[Gate] ─── threshold, attack, release, range     │
  │                                              │
  ▼                                              │
[Compressor] ─── threshold, ratio, attack,       │
  │              release, makeup, knee           │
  │                                              │
  ▼                                              │
[EQ] ─── 4-band parametric                       │
  │      (low shelf, 2x mid bell, high shelf)    │
  │      each: freq, gain, Q                     │
  │                                              │
  ├── Aux Sends (Pre-fader) ────────────────────┼──► Aux Buses
  │                                              │
  ▼                                              │
[Pan] ─── -1.0 (L) to +1.0 (R)                  │
  │                                              │
  ▼                                              │
[Fader] ─── 0.0 to 1.0 (or dB scale)            │
  │                                              │
  ├── Aux Sends (Post-fader) ───────────────────┼──► Aux Buses
  │                                              │
  ▼                                              │
[Mute] ─── boolean                              │
  │                                              │
  ├── Subgroup routing ─────────────────────────┼──► Subgroups
  │                                              │
  └── Main routing ─────────────────────────────┼──► Main Bus
                                                 │
                                          PFL Bus ◄──┘
```

### Bus Structure

```
                    ┌─────────────────────────────────────────────┐
                    │              MIXER BLOCK                     │
                    ├─────────────────────────────────────────────┤
                    │                                             │
  Input 1 ─────────►│  Channel Strip 1 ──┬── Aux 1 Send ────────►├──► Aux 1 Out
  Input 2 ─────────►│  Channel Strip 2 ──┼── Aux 2 Send ────────►├──► Aux 2 Out
  Input 3 ─────────►│  Channel Strip 3 ──┼── ...                 │
  ...               │  ...               │                        │
  Input N ─────────►│  Channel Strip N ──┴──┬─────────────────┐  │
                    │                       │                 │  │
                    │                       ▼                 ▼  │
                    │              ┌─────────────┐    ┌──────────┐│
                    │              │ Subgroup 1  │    │Subgroup 2││
                    │              │ [Gate/Comp/ │    │[Gate/... ││
                    │              │  EQ/Fader]  │    │          ││
                    │              └──────┬──────┘    └────┬─────┘│
                    │                     │                │      │
                    │                     └────────┬───────┘      │
                    │                              ▼              │
                    │                     ┌─────────────────┐     │
                    │                     │    MAIN BUS     │     │
                    │                     │ [Comp/EQ/Limit] │     │
                    │                     │    [Fader]      │     │
                    │                     └────────┬────────┘     │
                    │                              │              │
                    │                              ▼              │
                    │                        Main L/R ───────────►├──► Main Out
                    │                                             │
                    │    ┌───────────────────────────────┐       │
                    │    │          PFL BUS              │       │
                    │    │  (receives PFL taps from      │       │
                    │    │   any channel with PFL on)    │       │
                    │    └──────────────┬────────────────┘       │
                    │                   │                        │
                    │                   ▼                        │
                    │              PFL Out ─────────────────────►├──► PFL Out
                    │                                             │
                    └─────────────────────────────────────────────┘
```

## GStreamer Implementation

### Elements Used

| Function | GStreamer Element | Notes |
|----------|-------------------|-------|
| Input routing | `deinterleave` + `queue` | Split stereo to mono for per-channel processing |
| Gate | `lv2` (LSP Gate) | `http://lsp-plug.in/plugins/lv2/gate_stereo` |
| Compressor | `lv2` (LSP Compressor) | `http://lsp-plug.in/plugins/lv2/compressor_stereo` |
| EQ | `lv2` (LSP Parametric EQ) | `http://lsp-plug.in/plugins/lv2/para_equalizer_x8_stereo` |
| Pan | `audiopanorama` | Built-in GStreamer element |
| Volume/Fader | `volume` | Built-in GStreamer element |
| Mixing | `audiomixer` | Built-in, supports per-pad volume |
| Output routing | `interleave` | Combine mono to stereo |
| Metering | `level` | For UI feedback |

### Alternative: Native GStreamer Elements

If LSP/LV2 is not available or desired, we can use built-in elements:

| Function | Alternative Element | Limitations |
|----------|---------------------|-------------|
| Gate | Custom with `cutter` | Basic, no soft knee |
| Compressor | `audiodynamic` | Mode=compressor, less control |
| EQ | `equalizer-nbands` | Works well, parametric |

### Per-Channel Pipeline (Conceptual)

```
input_N
    │
    ▼
audioconvert ─► audioresample ─► capsfilter (ensure stereo float)
    │
    ▼
tee (for PFL tap) ──────────────────────────────────────► queue ──► pfl_mixer
    │
    ▼
lv2_gate_N (LSP Gate Stereo)
    │
    ▼
lv2_comp_N (LSP Compressor Stereo)
    │
    ▼
lv2_eq_N (LSP Parametric EQ Stereo)
    │
    ▼
tee (for pre-fader aux sends) ──► queue ──► aux_1_mixer (pre-fader)
    │                          ──► queue ──► aux_2_mixer (pre-fader)
    │
    ▼
audiopanorama_N (pan control)
    │
    ▼
volume_N (fader)
    │
    ▼
tee (for post-fader aux sends) ──► queue ──► aux_1_mixer (post-fader)
    │                           ──► queue ──► aux_2_mixer (post-fader)
    │
    ▼
valve_N (mute - or use volume=0)
    │
    ├──► subgroup_1_mixer (if assigned)
    ├──► subgroup_2_mixer (if assigned)
    └──► main_mixer (if direct-to-main)
```

## Block Definition

### Properties

```yaml
# Global Configuration
num_channels: 1-32 (default: 8)
num_aux_buses: 0-8 (default: 2)
num_subgroups: 0-4 (default: 0)

# Per Channel (channel_N_*)
channel_N_label: string (default: "Ch N")
channel_N_gain: float, -20.0 to +20.0 dB (default: 0.0)

# Gate
channel_N_gate_enabled: bool (default: false)
channel_N_gate_threshold: float, -80.0 to 0.0 dB (default: -40.0)
channel_N_gate_attack: float, 0.1 to 100.0 ms (default: 1.0)
channel_N_gate_release: float, 10.0 to 1000.0 ms (default: 100.0)
channel_N_gate_range: float, -80.0 to 0.0 dB (default: -80.0)

# Compressor
channel_N_comp_enabled: bool (default: false)
channel_N_comp_threshold: float, -60.0 to 0.0 dB (default: -20.0)
channel_N_comp_ratio: float, 1.0 to 20.0 (default: 4.0)
channel_N_comp_attack: float, 0.1 to 100.0 ms (default: 10.0)
channel_N_comp_release: float, 10.0 to 1000.0 ms (default: 100.0)
channel_N_comp_makeup: float, 0.0 to 30.0 dB (default: 0.0)
channel_N_comp_knee: float, 0.0 to 12.0 dB (default: 3.0)

# EQ (4 bands)
channel_N_eq_enabled: bool (default: false)
channel_N_eq_low_freq: float, 20.0 to 500.0 Hz (default: 80.0)
channel_N_eq_low_gain: float, -15.0 to +15.0 dB (default: 0.0)
channel_N_eq_low_q: float, 0.1 to 10.0 (default: 0.7)
channel_N_eq_lowmid_freq: float, 100.0 to 2000.0 Hz (default: 400.0)
channel_N_eq_lowmid_gain: float, -15.0 to +15.0 dB (default: 0.0)
channel_N_eq_lowmid_q: float, 0.1 to 10.0 (default: 1.0)
channel_N_eq_highmid_freq: float, 500.0 to 8000.0 Hz (default: 2500.0)
channel_N_eq_highmid_gain: float, -15.0 to +15.0 dB (default: 0.0)
channel_N_eq_highmid_q: float, 0.1 to 10.0 (default: 1.0)
channel_N_eq_high_freq: float, 2000.0 to 20000.0 Hz (default: 12000.0)
channel_N_eq_high_gain: float, -15.0 to +15.0 dB (default: 0.0)
channel_N_eq_high_q: float, 0.1 to 10.0 (default: 0.7)

# Routing
channel_N_pan: float, -1.0 to +1.0 (default: 0.0)
channel_N_fader: float, 0.0 to 1.0 (default: 0.75, ~-6dB)
channel_N_mute: bool (default: false)
channel_N_pfl: bool (default: false)
channel_N_main_assign: bool (default: true)
channel_N_subgroup_assign: int, 0=none, 1-4=subgroup (default: 0)

# Aux Sends (per channel, per aux)
channel_N_aux_M_send: float, 0.0 to 1.0 (default: 0.0)
channel_N_aux_M_pre: bool (default: true for aux 1-2, false for aux 3+)

# Subgroup (subgroup_N_*)
subgroup_N_label: string
subgroup_N_comp_enabled: bool
subgroup_N_comp_*: (same as channel compressor)
subgroup_N_eq_enabled: bool
subgroup_N_eq_*: (same as channel EQ)
subgroup_N_fader: float
subgroup_N_mute: bool
subgroup_N_pfl: bool

# Main Bus
main_comp_enabled: bool (default: false)
main_comp_*: (same as channel compressor)
main_eq_enabled: bool (default: false)
main_eq_*: (same as channel EQ)
main_limiter_enabled: bool (default: true)
main_limiter_threshold: float, -20.0 to 0.0 dB (default: -0.3)
main_fader: float, 0.0 to 1.0 (default: 1.0)

# Aux Bus Masters
aux_M_fader: float, 0.0 to 1.0 (default: 1.0)
aux_M_mute: bool (default: false)

# PFL
pfl_level: float, 0.0 to 1.0 (default: 1.0)
```

### External Pads (Dynamic)

```
Inputs:
  - input_1 (stereo audio)
  - input_2 (stereo audio)
  - ...
  - input_N (based on num_channels)

Outputs:
  - main_out (stereo audio) - always present
  - aux_1_out (stereo audio) - if num_aux_buses >= 1
  - aux_2_out (stereo audio) - if num_aux_buses >= 2
  - ...
  - pfl_out (stereo audio) - always present
```

## GUI Design

### Fullscreen Mixer View

```
┌─────────────────────────────────────────────────────────────────────────────────────┐
│ [≡ Menu]  MIXER - Flow Name                           [PFL] [AFL]  [Close X]        │
├─────────────────────────────────────────────────────────────────────────────────────┤
│                                                                                      │
│ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ │ ┌────────┐ ┌────────┐ │ ┌────────────┐│
│ │ Ch 1   │ │ Ch 2   │ │ Ch 3   │ │ Ch 4   │ │ │ Sub 1  │ │ Sub 2  │ │ │   MAIN     ││
│ ├────────┤ ├────────┤ ├────────┤ ├────────┤ │ ├────────┤ ├────────┤ │ ├────────────┤│
│ │ [GATE] │ │ [GATE] │ │ [GATE] │ │ [GATE] │ │ │        │ │        │ │ │            ││
│ │ [COMP] │ │ [COMP] │ │ [COMP] │ │ [COMP] │ │ │ [COMP] │ │ [COMP] │ │ │  [COMP]    ││
│ │ [EQ]   │ │ [EQ]   │ │ [EQ]   │ │ [EQ]   │ │ │ [EQ]   │ │ [EQ]   │ │ │  [EQ]      ││
│ ├────────┤ ├────────┤ ├────────┤ ├────────┤ │ ├────────┤ ├────────┤ │ │  [LIMIT]   ││
│ │AUX 1[▓]│ │AUX 1[▓]│ │AUX 1[▓]│ │AUX 1[▓]│ │ │        │ │        │ │ ├────────────┤│
│ │AUX 2[▓]│ │AUX 2[▓]│ │AUX 2[▓]│ │AUX 2[▓]│ │ │        │ │        │ │ │            ││
│ ├────────┤ ├────────┤ ├────────┤ ├────────┤ │ ├────────┤ ├────────┤ │ │    ┌──┐    ││
│ │ ◄──●──►│ │ ◄──●──►│ │ ◄──●──►│ │ ◄──●──►│ │ │        │ │        │ │ │    │  │    ││
│ │  PAN   │ │  PAN   │ │  PAN   │ │  PAN   │ │ │        │ │        │ │ │    │▓▓│    ││
│ ├────────┤ ├────────┤ ├────────┤ ├────────┤ │ ├────────┤ ├────────┤ │ │    │▓▓│    ││
│ │  ┌──┐  │ │  ┌──┐  │ │  ┌──┐  │ │  ┌──┐  │ │ │  ┌──┐  │ │  ┌──┐  │ │ │    │▓▓│    ││
│ │  │▓▓│  │ │  │▓▓│  │ │  │  │  │ │  │▓▓│  │ │ │  │▓▓│  │ │  │▓▓│  │ │ │    │▓▓│    ││
│ │  │▓▓│  │ │  │▓▓│  │ │  │  │  │ │  │▓▓│  │ │ │  │▓▓│  │ │  │▓▓│  │ │ │    │▓▓│    ││
│ │  │▓▓│  │ │  │▓▓│  │ │  │  │  │ │  │▓▓│  │ │ │  │▓▓│  │ │  │▓▓│  │ │ │    │▓▓│    ││
│ │ [METER]│ │ [METER]│ │ [METER]│ │ [METER]│ │ │ [METER]│ │ [METER]│ │ │ [L METER R]││
│ │  └──┘  │ │  └──┘  │ │  └──┘  │ │  └──┘  │ │ │  └──┘  │ │  └──┘  │ │ │    └──┘    ││
│ ├────────┤ ├────────┤ ├────────┤ ├────────┤ │ ├────────┤ ├────────┤ │ ├────────────┤│
│ │ [MUTE] │ │ [MUTE] │ │ [MUTE] │ │ [MUTE] │ │ │ [MUTE] │ │ [MUTE] │ │ │   [MUTE]   ││
│ │ [PFL]  │ │ [PFL]  │ │ [PFL]  │ │ [PFL]  │ │ │ [PFL]  │ │ [PFL]  │ │ │            ││
│ │ [1→2]  │ │ [1→2]  │ │ [1→2]  │ │ [MAIN] │ │ │        │ │        │ │ │   0.0 dB   ││
│ └────────┘ └────────┘ └────────┘ └────────┘ │ └────────┘ └────────┘ │ └────────────┘│
│                                             │                       │               │
│    Channels                                 │    Subgroups          │    Master     │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

### Channel Strip Detail (when clicking a channel)

```
┌─────────────────────────────────────────────────────┐
│ Channel 1 - "Vocal"                        [X Close]│
├─────────────────────────────────────────────────────┤
│                                                     │
│ ┌─── GATE ───────────────────────────────────────┐ │
│ │ [ON/OFF]                                       │ │
│ │ Threshold: ════════●════════  -25.3 dB        │ │
│ │ Attack:    ══●══════════════   2.1 ms         │ │
│ │ Release:   ════════●════════  85.0 ms         │ │
│ │ Range:     ════●════════════  -60.0 dB        │ │
│ │                                                │ │
│ │ [GR Meter: ▓▓▓▓▓▓░░░░░░░░]                    │ │
│ └────────────────────────────────────────────────┘ │
│                                                     │
│ ┌─── COMPRESSOR ─────────────────────────────────┐ │
│ │ [ON/OFF]                                       │ │
│ │ Threshold: ════════●════════  -18.0 dB        │ │
│ │ Ratio:     ════●════════════   4.0:1          │ │
│ │ Attack:    ══●══════════════   5.0 ms         │ │
│ │ Release:   ════════●════════  120.0 ms        │ │
│ │ Makeup:    ════════●════════   6.0 dB         │ │
│ │ Knee:      ════●════════════   3.0 dB         │ │
│ │                                                │ │
│ │ [GR Meter: ▓▓▓▓▓▓▓▓░░░░░░]                    │ │
│ └────────────────────────────────────────────────┘ │
│                                                     │
│ ┌─── EQ ─────────────────────────────────────────┐ │
│ │ [ON/OFF]                                       │ │
│ │                                                │ │
│ │      ___/\___                                  │ │
│ │   __/        \__          Visual EQ curve     │ │
│ │ _/              \_____/\__                    │ │
│ │                                                │ │
│ │ LOW    LOMID   HIMID   HIGH                   │ │
│ │ 80Hz   400Hz   2.5kHz  12kHz                  │ │
│ │ +3dB   0dB     -2dB    +1dB                   │ │
│ │ Q:0.7  Q:1.0   Q:1.5   Q:0.7                  │ │
│ └────────────────────────────────────────────────┘ │
│                                                     │
│ ┌─── ROUTING ────────────────────────────────────┐ │
│ │ Aux 1: ════●════════════  -12.0 dB  [PRE]     │ │
│ │ Aux 2: ════════●════════   -6.0 dB  [POST]    │ │
│ │                                                │ │
│ │ Assign: [✓ MAIN] [  Sub1] [  Sub2]            │ │
│ └────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────┘
```

### Keyboard Shortcuts (in fullscreen mode)

| Key | Action |
|-----|--------|
| 1-9, 0 | Select channel 1-10 |
| Shift+1-9 | Select channel 11-19 |
| M | Mute selected channel |
| P | Toggle PFL on selected channel |
| ↑/↓ | Adjust fader |
| ←/→ | Adjust pan |
| G | Open gate detail |
| C | Open compressor detail |
| E | Open EQ detail |
| Esc | Close detail / exit fullscreen |
| Space | (reserved for potential use) |

## Implementation Phases

### Phase 1: Core Pipeline (MVP)
1. Create basic mixer block definition
2. Implement channel strip with:
   - Volume (fader)
   - Pan
   - Mute
   - Basic metering
3. Implement audiomixer for main bus
4. Single stereo output

### Phase 2: Processing
1. Add gate (using audiodynamic or LV2)
2. Add compressor
3. Add parametric EQ
4. Bypass switches for each

### Phase 3: Routing
1. Aux sends (pre/post configurable)
2. Subgroups
3. PFL bus

### Phase 4: GUI
1. Compact mixer view (for graph node)
2. Fullscreen mixer view
3. Channel detail panels
4. Real-time metering

### Phase 5: Polish
1. Gain reduction meters
2. EQ visualization
3. Presets/scenes
4. DCA groups (optional)

## Technical Considerations

### Real-time Parameter Updates
All parameters must be changeable without pipeline restart:
- GStreamer properties are already real-time safe
- LV2 plugins expose parameters as GObject properties via gst-plugins-lv2
- Use `g_object_set()` for immediate changes
- For automation: use GstControlSource/GstControlBinding

### Latency
- Keep processing chain minimal to reduce latency
- Consider adding latency compensation for aux returns
- Monitor with `latency` element if needed

### Thread Safety
- Property updates from GUI thread
- Meter data from streaming thread
- Use EventBroadcaster pattern (already in codebase)

### Memory
- Pre-allocate all elements at pipeline creation
- Don't create/destroy elements during playback
- Use valve element for muting instead of removing elements

## Files to Create/Modify

### Backend
- `backend/src/blocks/builtin/mixer.rs` - Main mixer block implementation
- `backend/src/blocks/builtin/mod.rs` - Add mixer module
- `backend/src/blocks/registry.rs` - Register mixer block

### Frontend
- `frontend/src/mixer.rs` - Mixer GUI (fullscreen view)
- `frontend/src/mixer_channel.rs` - Channel strip widget
- `frontend/src/mixer_detail.rs` - Detail panels (gate, comp, EQ)
- `frontend/src/app.rs` - Add mixer view handling
- `frontend/src/graph.rs` - Add double-click handler for mixer

### Types
- `types/src/events.rs` - Add mixer meter events
- `types/src/api.rs` - Add mixer-specific API types (if needed)

## Dependencies

### Required GStreamer Plugins
- `gst-plugins-base`: audioconvert, audioresample, volume, audiomixer
- `gst-plugins-good`: audiopanorama, level
- `gst-plugins-bad`: lv2 (for LSP plugins)

### LSP Plugins (recommended)
- `lsp-plugins-lv2` package
- Gate: `http://lsp-plug.in/plugins/lv2/gate_stereo`
- Compressor: `http://lsp-plug.in/plugins/lv2/compressor_stereo`
- Parametric EQ: `http://lsp-plug.in/plugins/lv2/para_equalizer_x8_stereo`
- Limiter: `http://lsp-plug.in/plugins/lv2/limiter_stereo`

### Fallback (if LV2 not available)
- Gate: Manual implementation with `cutter` or threshold logic
- Compressor: `audiodynamic mode=compressor`
- EQ: `equalizer-nbands`
- Limiter: `audiodynamic mode=limiter`
