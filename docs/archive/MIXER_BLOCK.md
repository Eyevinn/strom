# Audio Mixer Block

> **Archived — code is the source of truth.** This describes the original design and intent
> of the audio mixer block. The code has very likely drifted since; treat this as a high-level
> pointer, not an accurate spec — read the code for the current implementation. The
> [Audio Mixer operator guide](../AUDIO_MIXER_OPERATOR_GUIDE.md) is the maintained usage reference.

Stereo audio mixer with per-channel processing, aux sends, subgroups,
and main bus mastering. Modeled after professional digital consoles
(Behringer X32, Yamaha TF series).

## Signal Flow

### Channel Strip

Each input channel passes through the following chain in order:

```
Input (stereo)
  │
  ▼
audioconvert → capsfilter (F32LE stereo)
  │
  ▼
Gain (+/- 20 dB)
  │
  ▼
HPF (high-pass filter, 20-500 Hz)
  │
  ▼
Gate (LSP Gate Stereo)
  │
  ▼
Compressor (LSP Compressor Stereo)
  │
  ▼
EQ (LSP Parametric EQ x8 Stereo, 4 bands used)
  │
  ▼
Level meter (pre-fader, 100ms interval)
  │
  ▼
pre_fader_tee ──────────┬──► Pre-fader aux sends (aux 1-2 default)
  │                      └──► PFL/AFL tap (configurable)
  ▼
Pan (audiopanorama)
  │
  ▼
Volume (fader + mute combined)
  │
  ▼
post_fader_tee ─────────────► Post-fader aux sends (aux 3-4 default)
  │
  ▼
routing_tee ────────────┬──► Main mixer (via to_main_vol → queue)
                        ├──► Group 1 mixer (via to_grp1_vol → queue)
                        └──► Group 2 mixer (via to_grp2_vol → queue)
```

Key points:
- **Meter position**: Pre-fader (after EQ/dynamics, before pan/fader/mute).
  The channel meter shows the signal hitting the fader regardless of fader
  position or mute — convention is *inputs are metered pre-fader, outputs
  (buses) are metered post-master*. Use PFL/AFL when you want to hear or
  read the post-fader signal.
- **Mute**: Implemented by setting the volume element to 0.0 (not a
  separate element).
- **Routing**: Each destination (main, groups) has its own volume element
  acting as an on/off gate (1.0 or 0.0). A channel can feed multiple
  destinations simultaneously.
- **PFL/AFL**: Solo tap point is configurable. PFL (default) taps from
  pre_fader_tee, AFL taps from post_fader_tee. Controlled by `solo_mode`
  property.

### Aux Bus

No processing. Receives sends from channel pre_fader_tee or
post_fader_tee depending on per-channel aux pre/post setting.

```
Channel aux sends ──► audiomixer ──► Volume (fader+mute) ──► Level meter ──► aux_out_tee ──► Output
```

- Aux 1-2 default to pre-fader sends
- Aux 3-4 default to post-fader sends
- Each channel has an independent send level per aux bus

### Subgroup (Group) Bus

No processing. Receives routed channels from their routing_tee.

```
Channel routing ──► audiomixer ──► Volume (fader+mute) ──► Level meter ──► group_out_tee ──┬──► Output
                                                                                           └──► queue ──► Main mixer
```

Groups always feed into the main mixer in addition to their external
output. This means groups act as parallel submixes — they sum into
main alongside any channels routed directly to main.

### Main Bus

Receives summed audio from channels routed to main and from all group
buses.

```
Channel routing ──┐
Group buses ──────┤
                  ▼
            audiomixer
                  │
                  ▼
            Compressor (LSP Compressor Stereo)
                  │
                  ▼
            EQ (LSP Parametric EQ x8 Stereo, 4 bands)
                  │
                  ▼
            Limiter (LSP Limiter Stereo)
                  │
                  ▼
            Volume (master fader)
                  │
                  ▼
            Level meter (100ms interval)
                  │
                  ▼
            main_out_tee ──► Output
```

### PFL Bus

Receives solo taps from channels with PFL enabled.

```
Channel PFL taps ──► audiomixer ──► Volume (master) ──► Level meter ──► pfl_out_tee ──► Output
```

## External Pads

### Inputs (dynamic)

| Pad | Description |
|-----|-------------|
| `input_1` .. `input_N` | Stereo audio, one per channel |

### Outputs (dynamic)

| Pad | Description |
|-----|-------------|
| `main_out` | Main stereo mix |
| `pfl_out` | PFL/solo bus output |
| `aux_out_1` .. `aux_out_M` | Aux bus outputs |
| `group_out_1` .. `group_out_K` | Subgroup outputs |

All output pads use `tee` elements with `allow-not-linked=true`, so
unconnected outputs do not stall the pipeline.

## GStreamer Elements

| Function | Element | Plugin |
|----------|---------|--------|
| Format conversion | `audioconvert` | gst-plugins-base |
| Format enforcement | `capsfilter` | core |
| Gain / Fader / Mute | `volume` | gst-plugins-base |
| High-pass filter | `audiocheblimit` or `audiowsinclimit` | gst-plugins-good / -bad |
| Gate | LSP Gate Stereo (LV2) | lsp-plugins-lv2 |
| Compressor | LSP Compressor Stereo (LV2) | lsp-plugins-lv2 |
| Parametric EQ | LSP Parametric EQ x8 Stereo (LV2) | lsp-plugins-lv2 |
| Limiter | LSP Limiter Stereo (LV2) | lsp-plugins-lv2 |
| Pan | `audiopanorama` | gst-plugins-good |
| Mixing | `audiomixer` | gst-plugins-base |
| Metering | `level` | gst-plugins-good |
| Splitting | `tee` | core |
| Isolation | `queue` | core |

### LSP LV2 Plugin URIs

| Plugin | URI |
|--------|-----|
| Gate Stereo | `http://lsp-plug.in/plugins/lv2/gate_stereo` |
| Compressor Stereo | `http://lsp-plug.in/plugins/lv2/compressor_stereo` |
| Parametric EQ x8 Stereo | `http://lsp-plug.in/plugins/lv2/para_equalizer_x8_stereo` |
| Limiter Stereo | `http://lsp-plug.in/plugins/lv2/limiter_stereo` |

LSP plugins use the `enabled` property (bool) for bypass control. When
`enabled=false`, the plugin passes audio through unprocessed.

### Fallback Elements

If LV2 plugins are not available, the mixer falls back to native
GStreamer elements:

| Function | Fallback | Limitations |
|----------|----------|-------------|
| Gate | `audiodynamic` (mode=expander) | Less control |
| Compressor | `audiodynamic` (mode=compressor) | No knee control |
| EQ | `equalizer-nbands` | Works well |
| Limiter | `audiodynamic` (mode=compressor, hard ratio) | Basic |

## Properties

### Global

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `num_channels` | int | 8 | Number of input channels (1-32) |
| `num_aux_buses` | int | 2 | Number of aux buses (0-4) |
| `num_groups` | int | 0 | Number of subgroups (0-4) |
| `solo_mode` | string | `"pfl"` | Solo tap point: `"pfl"` or `"afl"` |
| `force_live` | bool | true | Force audiomixer live mode |
| `latency` | float | 30.0 | Audiomixer latency (ms) |
| `min_upstream_latency` | float | 30.0 | Minimum upstream latency (ms) |

### Per Channel (`ch{N}_*`, N = 1-based)

| Property | Type | Default | Range | Description |
|----------|------|---------|-------|-------------|
| `ch{N}_gain` | float | 0.0 | -20.0 to +20.0 dB | Input gain |
| `ch{N}_hpf_enabled` | bool | false | | High-pass filter on/off |
| `ch{N}_hpf_freq` | float | 80.0 | 20-500 Hz | HPF cutoff frequency |
| `ch{N}_gate_enabled` | bool | false | | Gate on/off |
| `ch{N}_gate_threshold` | float | -40.0 | dB | Gate threshold |
| `ch{N}_gate_attack` | float | 5.0 | ms | Gate attack time |
| `ch{N}_gate_release` | float | 100.0 | ms | Gate release time |
| `ch{N}_gate_range` | float | -80.0 | dB | Gate range (attenuation) |
| `ch{N}_comp_enabled` | bool | false | | Compressor on/off |
| `ch{N}_comp_threshold` | float | -20.0 | dB | Compressor threshold |
| `ch{N}_comp_ratio` | float | 4.0 | 1.0-20.0 | Compression ratio |
| `ch{N}_comp_attack` | float | 10.0 | ms | Compressor attack |
| `ch{N}_comp_release` | float | 100.0 | ms | Compressor release |
| `ch{N}_comp_makeup` | float | 0.0 | dB | Makeup gain |
| `ch{N}_comp_knee` | float | -6.0 | dB | Compressor knee |
| `ch{N}_eq_enabled` | bool | false | | EQ on/off |
| `ch{N}_eq{B}_freq` | float | * | Hz | EQ band B frequency |
| `ch{N}_eq{B}_gain` | float | 0.0 | dB | EQ band B gain |
| `ch{N}_eq{B}_q` | float | 1.0 | | EQ band B Q factor |
| `ch{N}_pan` | float | 0.0 | -1.0 to +1.0 | Pan (L/R) |
| `ch{N}_fader` | float | 1.0 | 0.0-2.0 (linear) | Channel fader (1.0 = 0 dB) |
| `ch{N}_mute` | bool | false | | Channel mute |
| `ch{N}_pfl` | bool | false | | Pre-fader listen |
| `ch{N}_to_main` | bool | true | | Route to main bus |
| `ch{N}_to_grp{G}` | bool | false | | Route to group G |

EQ default frequencies per band: Band 1 = 80 Hz, Band 2 = 400 Hz,
Band 3 = 2000 Hz, Band 4 = 8000 Hz.

### Per Aux Send (`ch{N}_aux{M}_*`)

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `ch{N}_aux{M}_level` | float | 0.0 | Send level (linear) |
| `ch{N}_aux{M}_pre` | bool | * | Pre-fader send (default: true for aux 1-2, false for 3-4) |

### Aux Bus Master (`aux{M}_*`, M = 1-based)

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `aux{M}_fader` | float | 1.0 | Aux bus master fader (linear) |
| `aux{M}_mute` | bool | false | Aux bus mute |

### Group (`group{G}_*`, G = 1-based)

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `group{G}_fader` | float | 1.0 | Group fader (linear) |
| `group{G}_mute` | bool | false | Group mute |

### Main Bus

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `main_fader` | float | 1.0 | Master fader (linear) |
| `main_comp_enabled` | bool | false | Main compressor on/off |
| `main_comp_threshold` | float | -20.0 | dB |
| `main_comp_ratio` | float | 4.0 | |
| `main_comp_attack` | float | 10.0 | ms |
| `main_comp_release` | float | 100.0 | ms |
| `main_comp_makeup` | float | 0.0 | dB |
| `main_comp_knee` | float | -6.0 | dB |
| `main_eq_enabled` | bool | false | Main EQ on/off |
| `main_eq{B}_freq` | float | * | Hz (same defaults as channel EQ) |
| `main_eq{B}_gain` | float | 0.0 | dB |
| `main_eq{B}_q` | float | 1.0 | |
| `main_limiter_enabled` | bool | false | Main limiter on/off |
| `main_limiter_threshold` | float | -3.0 | dB |

### PFL

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `pfl_level` | float | 1.0 | PFL bus master volume (linear) |

## GUI

The mixer GUI runs as a fullscreen view in the Strom frontend.

### Layout

```
┌─────────────────────────────────────────────────────────────┐
│ Row 1: [Ch1][Ch2][Ch3]...[ChN]     ← horizontal scroll     │
├─────────────────────────────────────────────────────────────┤
│ Row 2: [AUX1][AUX2] | [GRP1][GRP2] | [MAIN]  ← compact    │
├─────────────────────────────────────────────────────────────┤
│ Row 3: Detail panel (Gain / HPF / Gate / Comp / EQ)         │
├─────────────────────────────────────────────────────────────┤
│ Status bar: [Save] [Reset] status text                      │
└─────────────────────────────────────────────────────────────┘
```

- **Row 1**: Full channel strips with processing buttons, aux knobs,
  routing, LCD, pan, fader, meter, mute, PFL
- **Row 2**: Compact bus master strips (fader + meter + mute only)
- **Row 3**: Detail panel for the selected strip. Channels show
  Gain/HPF/Gate/Comp/EQ. Main shows Comp/EQ/Limiter.
- **Status bar**: Save (Ctrl+S) persists all settings to the flow
  definition. Reset returns all parameters to defaults.

### Channel Strip (top to bottom)

| Element | Description |
|---------|-------------|
| Label | Channel number |
| H G C E | Toggle buttons for HPF, Gate, Compressor, EQ |
| Aux knobs | Send level per aux bus (drag to adjust) |
| Routing | M + group number buttons (multi-destination) |
| LCD | Shows pan + level (or active control value) |
| Pan knob | Left/right panning |
| Fader + Meter | Vertical fader with stereo level meter and dB scale |
| MUTE | Mute toggle |
| PFL | Pre-fader listen toggle |

### Interactions

- **Click strip background**: Select channel/main for detail panel
- **Double-click fader**: Toggle between 0 dB and -inf (silence)
- **Drag fader**: Adjust level (-60 to +6 dB range)
- **Ctrl+S**: Save all mixer settings to flow

### Persistence

Mixer state is saved as block properties in the flow definition via the
`update_flow` API. On flow start, the backend reads all properties and
configures the GStreamer pipeline accordingly. The GUI also supports
runtime parameter changes via `update_element_property` without
restarting the pipeline.
