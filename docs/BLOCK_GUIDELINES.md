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

## Reported Latency
- A source reports the latency it can actually hold: its minimum is how late its data is, its maximum how long it can buffer before data is lost. A mixer takes the largest minimum and the smallest maximum among its inputs, so one input that understates its maximum fails the whole mixer as soon as another input has more latency. GStreamer then posts "Impossible to configure latency" on every aggregation cycle.
- A block that hands media into a flow through an `appsrc` (a pipeline of its own, a callback, a network session) builds it with `blocks::live_bridge_appsrc`. A live `appsrc` at its defaults reports a maximum of 0, even though it queues seconds of data.
- When a mixer fails its latency configuration, fix the input that reports the wrong latency. Do not raise queue sizes in front of the mixer to hide it.

## Sinks That Block
- **A sink that can block on the network (a caller that connects or reconnects inside `render`) must not stall the flow.** Strom links one block output to several blocks through a `tee` without queues, so a sink that blocks holds up everything upstream: a live mixer stalls, the other outputs stop, and the backlog bursts out when the sink unblocks. Give such a sink a thread of its own and let it drop only its own data: a `queue` with `leaky=downstream` in front of it. That is a documented requirement in the sense of the queue rule in `CLAUDE.md`; justify each limit you change in the code. The SRT outputs in caller mode do this (#1259).
- **That queue must never drop while the sink is connected and keeping up.** A syncing sink holds each buffer for the pipeline latency, which any other branch of the flow can raise, so the queue holds that much media in normal operation. Bound it by time only, derived from the pipeline latency (the SRT outputs use `max(latency + 2 s, 3 s)`, updated from the LATENCY event), with `max-size-bytes=0` and `max-size-buffers=0`: a byte or buffer count stands for a different time at every bitrate. Fixed limits (1 s, 10 MB) dropped media continuously from a connected output once the flow had more than 1 s of latency or ran above ~80 Mbit/s. Make sure every buffer reaching the queue has a timestamp, or the time bound does not hold. Count what the queue drops and log it per output: a drop while connected is a warning.
- Guard tests: one builds the real block with nobody to connect to, tees a live source into it and into a counting sink, and fails if the counting sink stops receiving at the source rate. Another connects the block to a real receiver with a high pipeline latency and a high bitrate, and fails on any queue overrun.

## Thread Priority
- **A block that runs a pipeline of its own gives its streaming threads the flow's priority.** The flow's thread priority, CPU affinity and thread registry follow the flow's bus. An internal pipeline has its own bus, and gets none of them unless the block hands them over. Hand them over through `SessionThreadConfig` on every pipeline the block creates: `install_on_internal_pipeline` where the block owns the bus, `install_on_webrtcsink` where webrtcsink owns it. A pipeline Strom runs outside any flow calls `thread_priority::reset_streaming_threads`, so a pooled thread a WebRTC session left elevated gets its own scheduling back.
- A guard test builds the real block with a thread registry, and fails if no streaming thread of the internal pipeline is registered under the block.

## GStreamer Memory Formats
- A block emits the memory type it naturally produces (system, GL, CUDA, ...). The **consuming** block adapts its own input. A producer does not know its consumer, so any producer-side download is wrong for half the graph and costs a GPU round trip per frame in the other half.
- Adapt at build time where the input is known (`glupload` on a GL consumer's inputs). Where it depends on what `decodebin` autoplugged upstream, decide from the negotiated caps — `gst::video_input_bridge` does this for WHEP Output's video input.
- Beware sinks that advertise GPU memory features they cannot actually process: `whepserversink` accepts `video/x-raw(memory:GLMemory)` and then fails encoder discovery. A successful link is not proof the consumer can use the frames.
- **A block that decodes in a pipeline of its own (an isolation pipeline behind appsink/appsrc) shares the flow's GL context with it.** GL memory is only usable in the GL context that made it, and two pipelines share no context unless the block hands it over. Install `gst::context_forward` on the internal pipeline, with the block's element in the flow as the target. It forwards GL context types only: CUDA and D3D memory cross without a shared context, and their contexts are tied to a device. A guard test builds the real block, pushes GL memory from its internal pipeline into a GL converter in the flow, and fails if the block stops forwarding: a frame from another context fails there with "input memory OpenGL context is different". Frames that cross between two flows (Inter) cannot share a context, because each flow starts and stops on its own. The consumer downloads them instead.
- Avoid `autovideoconvert` (and other auto-pluggers that choose elements by rank) in new code. It picks by fixed rank with no passthrough, so it adds work nobody asked for. Which elements it can pick depends on how the local GStreamer was built. It has also crashed and hung: behind `gldownload` on GStreamer 1.24.2 it failed 9 of 20 runs. Pick the elements explicitly from the negotiated caps instead, through helpers whose behaviour we control and test (`gst::video_adapt::decide()`). If no helper covers the case yet, extend `decide()` rather than reach for an auto-plugger.

## Output Block Inputs
- This applies to output blocks that carry encoded media (SRT, RTMP, recorder, TAMS, ...). Outputs whose target takes raw media (NDI, DeckLink, AES67) take raw only. WHEP Output is exempt from the video rule below: each viewer negotiates its own codec, and the sink encodes for it. WHIP Output is not: it publishes one session to one server, so it takes encoded video like any other output.
- **Video must arrive encoded.** Refuse raw video and name `builtin.videoenc` in the message. Never encode video inside an output block: codec, profile and bitrate are the operator's choice, made in one explicit block.
- **Audio may arrive either way.** Pass encoded audio through as it is. Encode raw audio inside the block, with defaults that suit the target.
- Check what the target actually accepts from the negotiated caps, not only the caps name. That means the codec, and also `profile` where the target restricts it.
- **A refusal fails the flow, with a message the operator can act on.** Post an element error from the block (`gst::element_error!`) saying what arrived, what the target needs, and which block property fixes it. Do not only log the refusal and leave the pad unlinked: the flow then shows only `Internal data stream error`, or a track goes missing without a word.

## Property Descriptions
- A property's `description` is UI text: it says what the property does, plus the unit, range or special values (`0 = off`) the operator needs to set it. Aim for under 150 characters, and never go over 200.
- Why the property exists, its trade-offs, and how it works go in the code and the PR, not in the description.
- When you find a description over 200 characters, in code you change or review, propose a shorter replacement with the new text: fix it in the PR you are working on, or report it as a review finding.
