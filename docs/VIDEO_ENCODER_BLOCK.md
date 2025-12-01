# Video Encoder Block - Design & Implementation

## Overview

The Video Encoder block (`builtin.videoenc`) provides automatic hardware-accelerated video encoding with intelligent encoder selection and graceful software fallback. It supports H.264, H.265, AV1, and VP9 codecs across all major hardware acceleration platforms.

**Block ID**: `builtin.videoenc`
**Category**: Video
**Implementation**: `backend/src/blocks/builtin/videoenc.rs`

## Features

- **Automatic Hardware Encoder Selection**: Intelligently detects and selects the best available hardware encoder for the chosen codec
- **Graceful Software Fallback**: Automatically falls back to software encoding when hardware acceleration is unavailable
- **Multi-Platform Support**: Works with NVIDIA NVENC, Intel QSV, VA-API, and AMD AMF hardware encoders
- **Comprehensive Codec Support**: H.264/AVC, H.265/HEVC, AV1, VP9
- **Flexible Configuration**: Bitrate, quality presets, rate control modes, and keyframe intervals
- **Property Mapping**: Translates common properties to encoder-specific implementations

## Architecture

### Block Structure

```
┌──────────────┐    ┌─────────┐    ┌────────────┐
│ videoconvert │───▶│ encoder │───▶│ capsfilter │
└──────────────┘    └─────────┘    └────────────┘
      ▲                                    │
      │                                    ▼
 video_in                           encoded_out
```

**Elements:**
1. **videoconvert**: Ensures compatible pixel format for the selected encoder
2. **encoder**: Dynamically selected hardware or software encoder element
3. **capsfilter**: Sets output caps for proper codec negotiation

### Encoder Selection Algorithm

The block uses a priority-based selection algorithm:

1. **Build Priority List**: Generate ordered list of encoders based on codec and preference
2. **Probe Availability**: Use GStreamer Registry to check if each encoder exists
3. **Select First Available**: Return the first encoder found in the priority list
4. **Fallback Logic**: If no encoder found and software fallback enabled, try software encoders
5. **Fail Gracefully**: Return meaningful error if no suitable encoder available

```rust
let registry = gst::Registry::get();
if registry.find_feature(encoder_name, gst::ElementFactory::static_type()).is_some() {
    // Encoder available, use it
}
```

## Supported Encoders

### H.264 / AVC

**Priority Order:**

1. **NVIDIA NVENC** (GPU - best for NVIDIA hardware)
   - `nvautogpuh264enc` - Auto GPU select mode (recommended)
   - `nvh264enc` - CUDA mode
   - `nvd3d11h264enc` - Direct3D11 mode (Windows)

2. **Intel QSV** (GPU - best for Intel hardware)
   - `qsvh264enc` - Intel Quick Sync Video H.264 encoder

3. **VA-API** (GPU - works on Intel/AMD Linux)
   - `vah264enc` - VA-API H.264 encoder
   - `vah264lpenc` - VA-API H.264 Low Power encoder

4. **AMD AMF** (GPU - Windows only)
   - `amfh264enc` - AMD AMF H.264 encoder

5. **Software Fallback**
   - `x264enc` - x264 software encoder (widely available, excellent quality)

### H.265 / HEVC

**Priority Order:**

1. **NVIDIA NVENC**
   - `nvautogpuh265enc` - Auto GPU select mode
   - `nvh265enc` - CUDA mode
   - `nvd3d11h265enc` - Direct3D11 mode (Windows)

2. **Intel QSV**
   - `qsvh265enc` - Intel Quick Sync Video H.265 encoder

3. **VA-API**
   - `vah265enc` - VA-API H.265 encoder
   - `vah265lpenc` - VA-API H.265 Low Power encoder

4. **AMD AMF**
   - `amfh265enc` - AMD AMF H.265 encoder

5. **Software Fallback**
   - `x265enc` - x265 software encoder

### AV1

**Priority Order:**

1. **NVIDIA NVENC**
   - `nvautogpuav1enc` - Auto GPU select mode
   - `nvav1enc` - CUDA mode
   - `nvd3d11av1enc` - Direct3D11 mode (Windows)

2. **Intel QSV**
   - `qsvav1enc` - Intel Quick Sync Video AV1 encoder

3. **VA-API**
   - `vaav1enc` - VA-API AV1 encoder

4. **AMD AMF**
   - `amfav1enc` - AMD AMF AV1 encoder

5. **Software Fallback**
   - `svtav1enc` - SVT-AV1 encoder (high quality, good performance)
   - `av1enc` - libaom AV1 encoder (reference encoder, slower but highest quality)

### VP9

**Priority Order:**

1. **Intel QSV**
   - `qsvvp9enc` - Intel Quick Sync Video VP9 encoder

2. **VA-API**
   - `vavp9enc` - VA-API VP9 encoder

3. **Software Fallback**
   - `vp9enc` - libvpx VP9 encoder

### Note on H.266/VVC

H.266/VVC is very new and has limited GStreamer support. Not included in the initial implementation.

## Block Properties

### 1. `codec` (Enum, Required)

Select the video codec for encoding.

**Options:**
- `h264` - H.264 / AVC (most compatible, widely supported)
- `h265` - H.265 / HEVC (better compression than H.264)
- `av1` - AV1 (next-generation codec, best compression)
- `vp9` - VP9 (Google's codec, good for web)

**Default:** `h264`

### 2. `encoder_preference` (Enum, Optional)

Control hardware vs. software encoder selection.

**Options:**
- `auto` - Try hardware first, fall back to software (recommended)
- `hardware` - Only use hardware encoders, fail if unavailable
- `software` - Only use software encoders

**Default:** `auto`

### 3. `bitrate` (UInt, Optional)

Target bitrate in kilobits per second.

**Range:** 100 - 100,000 kbps
**Default:** 4000 kbps

**Guidelines:**
- 1080p30: 4000-8000 kbps (medium-high quality)
- 720p30: 2000-4000 kbps
- 480p30: 1000-2000 kbps
- 4K30: 15000-25000 kbps

### 4. `quality_preset` (Enum, Optional)

Encoding quality/speed tradeoff. Slower presets provide better quality at the same bitrate.

**Options:**
- `ultrafast` - Fastest encoding, lowest quality (recommended for live/real-time)
- `fast` - Fast encoding, good for live streaming
- `medium` - Balanced
- `slow` - Slower encoding, better quality
- `veryslow` - Slowest encoding, best quality

**Default:** `ultrafast`

**Note:** Presets are mapped to encoder-specific equivalents. Not all encoders support all presets.

### 5. `tune` (Enum, Optional)

Optimize encoder for specific use case. **Only applies to x264/x265 software encoders.**

**Options:**
- `zerolatency` - Zero latency mode for streaming/real-time (disables look-ahead, minimal delay)
- `film` - High quality for film content
- `animation` - Optimized for animation
- `grain` - Preserve film grain
- `stillimage` - Optimized for still images (slideshows)
- `fastdecode` - Fast decode (reduces decode complexity)

**Default:** `zerolatency`

**Note:** Hardware encoders (NVENC, QSV, VA-API, AMF) don't use this property - they have low latency built-in.

### 6. `rate_control` (Enum, Optional)

Rate control mode for encoding.

**Options:**
- `vbr` - Variable Bitrate (best quality, recommended)
- `cbr` - Constant Bitrate (consistent bandwidth, good for streaming)
- `cqp` - Constant Quality Parameter (quality-based encoding)

**Default:** `vbr`

### 7. `keyframe_interval` (UInt, Optional)

GOP (Group of Pictures) size - number of frames between keyframes.

**Range:** 0 - 600 frames (0 = automatic)
**Default:** 60 frames

**Guidelines:**
- 30fps video: 60 frames = 2 second GOP
- 60fps video: 120 frames = 2 second GOP
- Smaller values: More keyframes, better seek performance, larger file size
- Larger values: Fewer keyframes, better compression, worse seek performance

## Property Mapping

Different encoders use different property names and value ranges. The block automatically maps common properties to encoder-specific implementations:

### Bitrate Mapping

| Encoder Type | Property Name | Units |
|-------------|---------------|-------|
| x264/x265 | `bitrate` | kbps |
| NVENC | `bitrate` | kbps |
| Intel QSV | `bitrate` | kbps |
| VA-API | `bitrate` | kbps |
| AMD AMF | `bitrate` | kbps |
| SVT-AV1 | `target-bitrate` | kbps |
| libaom AV1 | `target-bitrate` | kbps |
| VP9 | `target-bitrate` | kbps |

### Quality Preset Mapping

| Block Preset | x264/x265 | NVENC | Intel QSV | AMD AMF | SVT-AV1 | libaom AV1 | VP9 |
|--------------|-----------|-------|-----------|---------|---------|------------|-----|
| ultrafast | ultrafast | hp | 7 | lowlatency | 12 | 8 | 5 |
| fast | fast | fast | 5 | lowlatency | 10 | 6 | 4 |
| medium | medium | medium | 4 | transcoding | 8 | 4 | 3 |
| slow | slow | hq | 2 | quality | 4 | 2 | 1 |
| veryslow | veryslow | hq | 1 | quality | 0 | 0 | 0 |

### Rate Control Mapping

| Block Mode | NVENC | x264/x265 | Notes |
|------------|-------|-----------|-------|
| VBR | vbr | Default | Variable bitrate |
| CBR | cbr | CBR mode | Constant bitrate |
| CQP | cqp | CRF mode | Constant quality |

### GOP Size / Keyframe Interval

Multiple property names are tried:
- `gop-size`
- `key-int-max`
- `keyint-max`

## Usage Examples

### Example 1: Auto H.264 Encoding (Recommended)

```json
{
  "codec": "h264",
  "encoder_preference": "auto",
  "bitrate": 5000,
  "quality_preset": "ultrafast",
  "tune": "zerolatency",
  "rate_control": "vbr",
  "keyframe_interval": 60
}
```

**Behavior:**
- Will use `nvh264enc` on systems with NVIDIA GPU
- Falls back to `x264enc` if no hardware encoder available
- 5 Mbps VBR encoding with ultra fast preset
- Zero latency tuning for minimal delay (when using software encoder)
- 60-frame GOP (2 seconds at 30fps)

### Example 2: Force Software H.265

```json
{
  "codec": "h265",
  "encoder_preference": "software",
  "bitrate": 8000,
  "quality_preset": "slow"
}
```

**Behavior:**
- Always uses `x265enc` (software encoder)
- Higher bitrate and slower preset for better quality
- Ideal for offline encoding where quality matters more than speed

### Example 3: Hardware-Only AV1

```json
{
  "codec": "av1",
  "encoder_preference": "hardware",
  "bitrate": 3000,
  "rate_control": "cbr"
}
```

**Behavior:**
- Only accepts hardware AV1 encoders (NVENC, QSV, VA-API, AMF)
- Fails if no hardware AV1 encoder available
- Uses constant bitrate for predictable bandwidth

### Example 4: Live Streaming H.264

```json
{
  "codec": "h264",
  "encoder_preference": "auto",
  "bitrate": 4000,
  "quality_preset": "ultrafast",
  "tune": "zerolatency",
  "rate_control": "cbr",
  "keyframe_interval": 60
}
```

**Behavior:**
- Ultra fast encoding preset for minimal latency
- Zero latency tuning (for software encoder)
- CBR for consistent network bandwidth
- 2-second GOP for good seek performance

## Implementation Details

### Encoder Detection

Uses GStreamer's Registry API to detect available encoders:

```rust
let registry = gst::Registry::get();
let feature = registry.find_feature(encoder_name, gst::ElementFactory::static_type());
```

### Caps Negotiation

Each codec outputs specific caps via the capsfilter:

- **H.264**: `video/x-h264,stream-format=byte-stream,alignment=au`
- **H.265**: `video/x-h265,stream-format=byte-stream,alignment=au`
- **AV1**: `video/x-av1`
- **VP9**: `video/x-vp9`

### Error Handling

The block provides meaningful error messages:

- "codec property is required" - No codec specified
- "Invalid codec: xyz" - Unsupported codec selected
- "No hardware encoder available for H264" - Hardware-only mode but no HW encoder found
- "No encoder available for AV1 (tried hardware and software)" - All encoder options exhausted

### Logging

The block logs encoder selection and configuration:

```
🎞️ Building VideoEncoder block instance: block_id
🎞️ Selected encoder 'nvh264enc' for codec H264 with preference Auto
🎞️ Set encoder properties: bitrate=4000 kbps, preset=medium, rate_control=Vbr, gop=60
🎞️ VideoEncoder block created (chain: videoconvert -> nvh264enc -> capsfilter [video/x-h264])
```

## Testing

### Available Encoders on Development System

```bash
gst-inspect-1.0 | grep -E "(x264enc|x265enc|nvh264enc|nvh265enc|nvav1enc)"
```

Example output:
```
nvcodec:  nvh264enc: NVENC H.264 Video Encoder
nvcodec:  nvh265enc: NVENC HEVC Video Encoder
x264:  x264enc: x264 H.264 Encoder
x265:  x265enc: x265enc
svtav1:  svtav1enc: SvtAv1Enc
aom:  av1enc: AV1 Encoder
vpx:  vp9enc: On2 VP9 Encoder
```

### Testing Encoder Selection

Create a test flow with:
1. `videotestsrc` (video source)
2. `builtin.videoenc` (encoder block)
3. `fakesink` (sink)

Test different configurations:
- Different codecs (h264, h265, av1, vp9)
- Different preferences (auto, hardware, software)
- With and without software fallback

### Validating Output

Use GStreamer tools to verify encoded output:

```bash
# Check pipeline topology
gst-launch-1.0 videotestsrc ! videoconvert ! nvh264enc ! fakesink

# Inspect encoded stream
gst-launch-1.0 videotestsrc ! videoconvert ! nvh264enc ! h264parse ! fakesink -v
```

## Performance Considerations

### Hardware vs. Software Encoding

**Hardware Encoding (NVENC, QSV, VA-API, AMF):**
- ✅ Much faster (10-50x)
- ✅ Lower CPU usage
- ✅ Ideal for live streaming and real-time encoding
- ⚠️ May have slightly lower quality at same bitrate
- ⚠️ Limited quality/speed presets

**Software Encoding (x264, x265, SVT-AV1, etc.):**
- ✅ Better quality at same bitrate
- ✅ More configuration options
- ✅ Works on any system
- ⚠️ Much slower
- ⚠️ High CPU usage

### Quality vs. Speed Tradeoffs

| Preset | Encoding Speed | Quality | Best Use Case |
|--------|---------------|---------|---------------|
| ultrafast | Very Fast | Lowest | Real-time streaming on limited hardware |
| fast | Fast | Good | Live streaming |
| medium | Moderate | Good | General purpose, balanced |
| slow | Slow | Better | High-quality recording |
| veryslow | Very Slow | Best | Archival, offline encoding |

### Codec Comparison

| Codec | Compression | Encoding Speed | Decoding Speed | Browser Support |
|-------|-------------|----------------|----------------|-----------------|
| H.264 | Baseline | Fast | Very Fast | Excellent |
| H.265 | 30-50% better | Slower | Fast | Good (modern) |
| AV1 | 50% better | Very Slow | Moderate | Growing |
| VP9 | 30% better | Slow | Moderate | Good (web) |

## Troubleshooting

### "No hardware encoder available"

**Cause:** Requested hardware encoder not found on system.

**Solutions:**
1. Check if GPU drivers are installed (`nvidia-smi`, `vainfo`, etc.)
2. Verify GStreamer plugins are installed:
   - NVIDIA: `gst-plugins-bad` with nvcodec
   - Intel: `gst-plugins-bad` with qsv
   - VA-API: `gstreamer1.0-vaapi`
   - AMD: `gst-plugins-bad` with amf
3. Use `encoder_preference: "auto"` to enable automatic fallback to software encoding
4. Use `encoder_preference: "software"` to force software encoding

### Encoder selection not working as expected

**Debug steps:**
1. Check Strom logs for encoder selection messages
2. Verify available encoders: `gst-inspect-1.0 | grep enc`
3. Test encoder manually: `gst-launch-1.0 videotestsrc ! nvh264enc ! fakesink`

### Poor quality output

**Solutions:**
1. Increase bitrate
2. Use slower quality preset
3. Try VBR or CQP rate control instead of CBR
4. Consider using software encoder for better quality

### High CPU usage

**Solutions:**
1. Enable hardware encoding (`encoder_preference: hardware`)
2. Use faster quality preset
3. Lower resolution or framerate before encoding
4. Reduce bitrate (less data to encode)

## Future Enhancements

### Potential Improvements

1. **Per-Encoder Advanced Properties**
   - Expose encoder-specific options (B-frames, reference frames, etc.)
   - Profile/level selection for H.264/H.265

2. **Multi-Pass Encoding**
   - Two-pass encoding for better quality
   - Requires buffering and coordination

3. **Adaptive Bitrate**
   - Dynamic bitrate adjustment based on content complexity
   - Network-aware bitrate scaling

4. **Codec Auto-Selection**
   - Choose codec based on resolution, framerate, and available encoders
   - Fallback chain: AV1 → H.265 → H.264

5. **Enhanced Presets**
   - Named presets for common use cases (streaming, recording, archival)
   - Optimize all parameters together

6. **Quality Metrics**
   - VMAF/PSNR/SSIM quality measurement
   - Automatic quality tuning

## Related Documentation

- [Blocks Implementation Guide](BLOCKS_IMPLEMENTATION.md)
- [GStreamer Elements](https://gstreamer.freedesktop.org/documentation/)
- [NVIDIA NVENC](https://developer.nvidia.com/nvidia-video-codec-sdk)
- [Intel Quick Sync Video](https://www.intel.com/content/www/us/en/architecture-and-technology/quick-sync-video/quick-sync-video-general.html)
- [VA-API](https://github.com/intel/libva)
- [x264 Encoder Guide](https://trac.ffmpeg.org/wiki/Encode/H.264)
- [SVT-AV1 Encoder](https://gitlab.com/AOMediaCodec/SVT-AV1)

## Changelog

### v0.2.4 (2025-11-28)

**Initial Implementation**
- ✅ Automatic hardware encoder selection
- ✅ Support for H.264, H.265, AV1, VP9 codecs
- ✅ Support for NVIDIA NVENC, Intel QSV, VA-API, AMD AMF
- ✅ Software encoder fallback (x264, x265, SVT-AV1, libaom, VP9)
- ✅ Configurable bitrate, quality presets, rate control, GOP size
- ✅ Property mapping for 10+ encoder types
- ✅ Comprehensive error handling and logging
