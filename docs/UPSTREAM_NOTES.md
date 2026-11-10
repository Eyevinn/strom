# Upstream Notes

Bugs and odd behaviour in GStreamer (and other dependencies) that we have run into while
building Strom. **This is data collection, not a to-do list.** We do not plan to fix these
upstream ourselves. They are written down as input for anyone who wants to report or fix
them upstream.

Each entry has the same fields, filled in as far as we know them:

- **Seen on:** versions (GStreamer, plugin or library) and platform.
- **Symptom:** what happens, with numbers where we measured them.
- **Repro:** the simplest way to show it, ideally one `gst-launch-1.0` line. Say if it is
  untested.
- **Hint:** our best guess at the cause, with source file or function names if we read them.
- **Tests:** the Strom test that hits it, if any.
- **Upstream:** the upstream issue or merge request, the version that fixes it, or "not
  searched" / "not reported".
- **Found in:** the Strom issue or PR.

Each entry says what is measured and what is a guess. "Not checked" and "not recorded" mean
exactly that. Strom may work around an entry; the workaround lives in the code. Code is the
source of truth — a workaround may have changed since; read the code.

When an entry is fixed upstream, keep it and fill in the version that fixes it, so the
workaround can go once we move past that version.

---

## Video

### `videoscale`: borders are pink in YUV formats deeper than 8 bits
- **Seen on:** GStreamer 1.28.8 (Windows MSI). Not seen on 1.26.10, but the trigger path
  was different there. 1.22 not checked.
- **Symptom:** with `add-borders=true` (the default), the borders `videoscale` adds when it
  letterboxes or pillarboxes are RGB (255, 125, 255) instead of black in every YUV format
  deeper than 8 bits we tried: `P010_10LE`, `v210`, `I420_10LE`, `A444_16LE`. 8-bit YUV and
  RGB get black borders.
- **Repro:** a 4:3 source in a >8-bit YUV format scaled to 16:9. Untested sketch of a
  `gst-launch` line:
  `videotestsrc ! video/x-raw,format=A444_16LE,width=320,height=240 ! videoscale ! video/x-raw,width=1280,height=720 ! videoconvert ! autovideosink`.
- **Hint:** probably the border fill colour for high-bit-depth YUV (black computed for 8 bits
  and not scaled to the format's depth, or chroma left at 0 instead of mid-scale), not the
  scaler itself.
- **Upstream:** a quick search found no report. Not reported.
- **Found in:** [#1156](https://github.com/Eyevinn/strom/issues/1156)

### `autovideoconvert` behind `gldownload` crashes or hangs
- **Seen on:** GStreamer 1.24.2 (Ubuntu 24.04, surfaceless EGL). Later versions not checked.
- **Symptom:** 9 of 20 runs crashed or hung with
  `gst_pad_set_chain_function_full: assertion 'GST_IS_PAD (pad)' failed`. The same pipeline
  with `videoconvert` passed 20 of 20.
- **Hint:** the assertion suggests `autovideoconvert` sets up pad functions on a child that is
  already gone or not yet created, i.e. a race in how it (re)builds its internal chain.
- **Upstream:** not searched.
- **Found in:** the memory-format work behind `gst::video_adapt::decide()`
  ([#682](https://github.com/Eyevinn/strom/issues/682)).

### `autovideoconvert` cannot cover every memory transition
- **Seen on:** GStreamer 1.24–1.26 (Ubuntu, the Strom container image, the Windows MSI).
- **Symptom:** odd behaviour rather than a bug. Selection is by fixed rank with no
  passthrough, so it inserts work when nothing needs converting, and CUDA → CUDA fails.
  `cudaconvert`/`cudascale` exist only in builds with NVIDIA Performance Primitives, and
  were missing on every NVIDIA host we measured; a template whose elements are missing is
  dropped silently. D3D11 → system memory has no path. CUDA is routed through GL, so it
  needs an EGL display.
- **Upstream:** not reported.
- **Found in:** [#682](https://github.com/Eyevinn/strom/issues/682)

### `whepserversink` (webrtcsink) accepts GL memory it cannot encode
- **Seen on:** gst-plugins-rs, versions not recorded. Seen on Linux and macOS.
- **Symptom:** the video request pads advertise `video/x-raw(memory:GLMemory)`, so GL frames
  negotiate all the way to the sink. Encoder discovery then finds no encoder that takes them,
  and video goes missing while audio keeps working. On macOS this is the normal case, since
  `vtdec_hw` outputs GL memory. D3D11 memory is also accepted, but the sink picks the
  highest-ranked encoder for the codec, which on an NVIDIA host does not take D3D11 memory.
- **Hint:** the sink's pad template should only advertise memory features that at least one
  available encoder chain can consume, or discovery should insert a download.
- **Upstream:** not searched.
- **Found in:** the WHEP Output input bridge (`gst::video_input_bridge`).

### `nvcodec` has no NVDEC 4:2:2 decode
- **Seen on:** GStreamer 1.24, 1.26, 1.28 and main (sources read).
- **Symptom:** H.264 4:2:2 10-bit falls back to CPU decode, even on Blackwell GPUs whose NVDEC
  supports it (Video Codec SDK 13.0).
- **Hint:** `gstnvdecoder.cpp` keeps `cudaVideoChromaFormat_422` inside `#if 0`; the chroma
  and surface format mappers have no 4:2:2 branch; the vendored `cuviddec.h` predates SDK 13;
  `nvh264dec`'s sink caps list no `high-4:2:2` or `high-10` profile, so decodebin skips it.
  10-bit 4:2:2 also needs a raw format gst-plugins-base does not have (P210/P216). 8-bit
  4:2:2 could map to the existing NV16.
- **Upstream:** [gstreamer#4940](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/4940)
  (open, framed around the encoder).
- **Found in:** [#711](https://github.com/Eyevinn/strom/issues/711)

### `decodebin3` picks `vtdec_hw` for ProRes where VideoToolbox has no ProRes decoder
- **Seen on:** Homebrew GStreamer 1.28.6 on the GitHub macOS runner (virtual Apple M1).
  A real Mac decodes ProRes fine.
- **Symptom:** `vtdec_hw` (rank primary+1) is picked, `VTDecompressionSessionCreate` returns
  -12906, then every frame fails with -12902. There is no fallback to `avdec_prores`.
- **Hint:** `vtdec_hw` could probe whether a session can be created for the codec before
  accepting caps, so autoplugging moves on to the next decoder.
- **Upstream:** not searched.

### `vtenc`: ABR rate control ignores the bitrate target
- **Seen on:** GStreamer 1.28 on Apple Silicon (720p30, 5 Mbit/s target).
- **Symptom:** in ABR mode the encoder ran at about 95 Mbit/s against a 5 Mbit/s target.
- **Hint:** `vtenc` always sets `kVTCompressionPropertyKey_Quality` (property default 0.5)
  after `AverageBitRate`, and VideoToolbox then encodes at constant quality. A
  `VTCompressionSession` with the same settings minus `Quality` follows the target. CBR uses
  `ConstantBitRate` and is honoured.
- **Upstream:** not searched.

### `avfvideosrc`: reports one frame of latency, frames arrive 52–89 ms after capture
- **Seen on:** Homebrew GStreamer 1.28.6, MacBook Pro (Apple Silicon) built-in camera,
  1280x720 at 30 fps.
- **Symptom:** the source answers the latency query with one frame (33.3 ms) as both minimum
  and maximum. Its own `AVF clock diff` debug line (capture timestamp to delegate callback)
  measured 52 ms at best and up to 89 ms. The capture timestamps were evenly spaced (33.3 ms);
  the delivery to the callback jittered (21–45 ms between callbacks). A syncing sink downstream
  saw the frames late and QoS dropped up to ~5 frames a second upstream. How large the delay
  was changed from run to run and minute to minute.
- **Repro:** `GST_DEBUG=avfvideosrc:5 gst-launch-1.0 avfvideosrc ! video/x-raw,width=1280,height=720,framerate=30/1 ! queue ! videoconvert ! x264enc tune=zerolatency speed-preset=ultrafast ! mpegtsmux ! srtsink uri=srt://127.0.0.1:5099 sync=true qos=true`
  with an SRT listener on port 5099; count `qos` bus messages from `videoconvert` (`-m`).
  The drops come and go with the camera's delivery delay.
- **Hint:** `avfvideosrc.m` sets `latency` to one frame duration in `setCaps` and reports it
  as min and max. `getSampleBuffer` already computes the delivery delay per frame
  (`input_clock_diff`), so it could report that instead, or post a latency message when it
  grows.
- **Tests:** `avf_capture_latency_raises_live_minimum` (the Strom workaround, not the
  upstream element).
- **Upstream:** not reported. Searched 2026-11-10: no issue or MR about this. The one-frame
  report dates from 2014. The capture-time timestamps came in 2015 ("subtract time spent in AVF
  queues"), and the latency report did not change with them. Main is unchanged. Related:
  [gst-plugins-bad#517](https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/517)
  (no latency message after renegotiation),
  [gstreamer#103](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/103) (queue
  design of OS-thread sources). `mfvideosrc` on Windows does report a latency from its
  measured capture delay.
- **Found in:** #1183.

## HTTP

### libsoup HTTP/2: a paused download reads the whole file into memory
- **Seen on:** libsoup 3.6.6 with GStreamer 1.28.6 (macOS). Linux not checked; the cause is
  in libsoup's own code.
- **Symptom:** `souphttpsrc` in PAUSED on a file from an HTTP/2 server keeps downloading.
  A 344 MB file was received in full within 45 s of pausing, and memory grew with it. The
  same over HTTP/1.1 (`SOUP_FORCE_HTTP1=1`) stopped at about 1 MB.
- **Repro:** `souphttpsrc location=<large file on an HTTP/2 server> ! fakesink`, set to
  PAUSED, watch bytes received.
- **Hint:** `soup-client-message-io-http2.c` adds every DATA frame to an unbounded queue
  (`soup_body_input_stream_http2_add_data`), creates the nghttp2 session without
  `NGHTTP2_OPT_NO_AUTO_WINDOW_UPDATE` and never calls `nghttp2_session_consume()`, so
  window credit goes back to the server on receipt, not when the data is read.
  `souphttpsrc` also has no property for the HTTP version, although libsoup has a
  per-message `soup_message_set_force_http1`. Strom sets `SOUP_FORCE_HTTP1` for the whole
  process.
- **Upstream:** not reported.
- **Found in:** [#1225](https://github.com/Eyevinn/strom/pull/1225)

### `reqwesthttpsrc`: turns non-seekable on a `206` without `Accept-Ranges`
- **Seen on:** gst-plugins-rs with GStreamer 1.28.6 (macOS).
- **Symptom:** its HTTP/2 flow control is correct (2 MB received while paused), but playback
  of a `.mov` never starts. After `qtdemux` seeks to `moov` at the end of the file, the
  source reports itself non-seekable.
- **Hint:** it recomputes `seekable` on every response and requires `Accept-Ranges: bytes`.
  The server sent that header on `200` but not on `206`. A `206 Partial Content` answer is
  itself proof that ranges work.
- **Upstream:** not reported.
- **Found in:** [#1225](https://github.com/Eyevinn/strom/pull/1225)

### `curlhttpsrc`: buffers the whole file while paused
- **Seen on:** GStreamer 1.28.6 (macOS).
- **Symptom:** like libsoup over HTTP/2, a paused `curlhttpsrc` downloads the whole file,
  over both HTTP/1.1 and HTTP/2.
- **Upstream:** not searched.
- **Found in:** [#1225](https://github.com/Eyevinn/strom/pull/1225)

## Transport

### `tsdemux`: PTS wrap puts timestamps 26.5 h ahead in `ignore-pcr` mode
- **Seen on:** GStreamer 1.28.2 (Ubuntu 26.04) and earlier.
- **Symptom:** after the 33-bit PTS wraps, every timestamp is a whole 26.5 h epoch ahead.
  A live MPEG-TS HLS source (`hlsdemux2` runs `tsdemux` with `ignore-pcr`) freezes once
  every 26.5 h, with no error.
- **Upstream:** fixed in a972093b, in 1.28.5 and later. Our Docker image carries a patched
  `libgstmpegtsdemux.so` until the distro ships it.
- **Found in:** [#1101](https://github.com/Eyevinn/strom/issues/1101),
  [#1132](https://github.com/Eyevinn/strom/pull/1132)

### `rtpjitterbuffer`: packets held for the length of a mute gap
- **Seen on:** versions not recorded.
- **Symptom:** after a gap with no RTP packets (a muted sender), the jitterbuffer holds
  packets for about as long as the gap lasted instead of outputting them.
- **Hint:** `calculate_packet_spacing` reads the RTP timestamp jump across the gap as a huge
  packet spacing, which corrupts the lost-timer scheduling.
- **Upstream:** [gst-plugins-good!951](https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/951).
  Strom sets `drop-on-latency` on `rtpbin` to break the stall.

### `GstRTPBaseDepayload`: abort when a fragmentation unit is interrupted
- **Seen on:** GStreamer 1.24 and later (header-extension aggregation does not exist before
  1.24).
- **Symptom:** the process aborts with
  `gstrtpbasedepayload.c:942: 'gst_buffer_list_length (priv->hdrext_buffers) == 0' should be TRUE`.
  It is a `g_assert_true`, so it takes down every pipeline in the process.
- **Hint:** `rtph264depay` calls `gst_rtp_base_depayload_delayed()` on an interrupted FU and
  then `finish_fragmentation_unit()`, which in access-unit mode can absorb the truncated NAL
  without pushing. The header-extension cache is still populated when the base class asserts
  it is empty. Strom turns aggregation off on every depayloader.
- **Upstream:** [gstreamer#5057](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/5057)
  (open).

### Live `appsrc` reports a maximum latency of 0
- **Seen on:** all versions we have used.
- **Symptom:** odd default rather than a bug. A live `appsrc` with `min-latency`/`max-latency`
  unset answers a LATENCY query with max 0, even though it can queue seconds of data. A
  downstream aggregator with another input whose minimum is larger then fails with
  "Impossible to configure latency" on every aggregation cycle, which burns CPU.
- **Hint:** a live `appsrc` could derive its maximum from `max-time`/`max-bytes` when
  `max-latency` is unset.
- **Upstream:** not reported. Strom builds its bridge `appsrc`s with explicit latencies.
- **Found in:** [#1072](https://github.com/Eyevinn/strom/pull/1072)
