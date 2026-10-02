# Block Guidelines

The rules every Strom block follows: what it emits, what it accepts, and how it adapts. They apply to blocks written by people and by AI agents alike. `CLAUDE.md` imports this file, so agents read the same text.

These are rules, not a description of the code. The code is the source of truth for how each block implements them.

## Block Input Contract
- A block emits what it naturally produces. Each **consuming** block takes what arrives and adapts its own input to what it can process. A producer does not know its consumer, so it never converts on a consumer's behalf, and the operator never has to insert a converter just to make two blocks link.
- This applies to every property of what flows between blocks:
  - **Encoded or raw**: see "Output Block Inputs".
  - **Memory type** (system, GL, CUDA, ...): see "GStreamer Memory Formats".
  - **Pixel format**: a consumer that needs one format (a mixer that composites BGRA, say) converts on its own input instead of refusing what arrives.
  - **Resolution and framerate**: the consumer scales or adapts, and logs that it did so.
  - **Audio sample rate, format and channels**: a consumer that combines inputs (a mixer, a router) resamples and converts each input to its own working rate and format. That rate is a block property with a sensible default (48 kHz), not a hardcoded value.
- When the input is known at build time, adapt at build time. When it depends on what upstream negotiates (an autoplugged decoder, a caps-less `appsrc`), decide from the negotiated caps at runtime.
- A consumer that cannot adapt fails the flow with an element error that says what arrived and what it needs. It never leaves a silent `not-negotiated` or `Internal data stream error`.
- A guard test feeds the consumer something other than its preferred input, and fails if the adaptation is removed.

## GStreamer Memory Formats
- A block emits the memory type it naturally produces (system, GL, CUDA, ...). The **consuming** block adapts its own input. A producer does not know its consumer, so any producer-side download is wrong for half the graph and costs a GPU round trip per frame in the other half.
- Adapt at build time where the input is known (`glupload` on a GL consumer's inputs). Where it depends on what `decodebin` autoplugged upstream, decide from the negotiated caps — `gst::video_input_bridge` does this for WHEP Output's video input.
- Beware sinks that advertise GPU memory features they cannot actually process: `whepserversink` accepts `video/x-raw(memory:GLMemory)` and then fails encoder discovery. A successful link is not proof the consumer can use the frames.
- Avoid `autovideoconvert` (and other auto-pluggers that choose elements by rank) in new code. It picks by fixed rank with no passthrough, so it adds work nobody asked for. Which elements it can pick depends on how the local GStreamer was built. It has also crashed and hung: behind `gldownload` on GStreamer 1.24.2 it failed 9 of 20 runs. Pick the elements explicitly from the negotiated caps instead, through helpers whose behaviour we control and test (`gst::video_adapt::decide()`). If no helper covers the case yet, extend `decide()` rather than reach for an auto-plugger.

## Output Block Inputs
- This applies to output blocks that carry encoded media (SRT, RTMP, recorder, TAMS, ...). Outputs whose target takes raw media (NDI, DeckLink, AES67) take raw only. WebRTC outputs (WHEP, WHIP) are exempt from the video rule below: codec negotiation is part of the protocol, and the sink encodes.
- **Video must arrive encoded.** Refuse raw video and name `builtin.videoenc` in the message. Never encode video inside an output block: codec, profile and bitrate are the operator's choice, made in one explicit block.
- **Audio may arrive either way.** Pass encoded audio through as it is. Encode raw audio inside the block, with defaults that suit the target.
- Check what the target actually accepts from the negotiated caps, not only the caps name. That means the codec, and also `profile` where the target restricts it.
- **A refusal fails the flow, with a message the operator can act on.** Post an element error from the block (`gst::element_error!`) saying what arrived, what the target needs, and which block property fixes it. Do not only log the refusal and leave the pad unlinked: the flow then shows only `Internal data stream error`, or a track goes missing without a word.
