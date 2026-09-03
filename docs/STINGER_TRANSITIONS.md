# Stinger Transitions

> **Code is the source of truth.** This guide describes intended behaviour and may have
> drifted; read the code for the current implementation.

A stinger transition plays a piece of branded motion graphics over the program bus while
the program source changes underneath it. The clip covers the frame, the sources swap
behind it, and the clip plays out — so the audience sees the graphic, not the cut.

## Setting one up

1. Add a **Media Player** block and put your stinger clip in its playlist.
2. Turn on **Stinger Clip Source** on that block.
3. Wire its video output into one of the **Vision Mixer**'s DSK (keyed) inputs.

Step 2 is not optional and is not implied by step 3. Declaring a player as a stinger source
changes how it behaves: its clip is held on the first frame ready to fire, and looping is
switched off so it plays once per trigger. A media player wired to a keyed input *without*
that switch is left completely alone, which is what lets you keep a looping graphic on a
keyed input alongside a stinger.

## Firing one

Trigger a transition of type `stinger` on the vision mixer, naming the media player block
that holds the clip. Three things shape it:

- **Cut point** — how far into the clip the program source actually changes. Set it to the
  moment your clip fully covers the frame. If you leave it out, the halfway point is used.
- **Transition beneath** — what the program does at the cut point. Defaults to a cut, which
  is what a fully covering clip wants, but it can be any transition the mixer supports. A
  clip that does not completely cover the frame is a good reason to put a wipe or a fade
  underneath instead.
- **Duration** — how long that transition beneath takes. It is *not* the length of the
  stinger; the clip's own length decides that.

If the cut point plus the duration would run past the end of the clip, the duration is
shortened so the transition finishes while the clip is still covering. You are told both
the duration you asked for and the one that was applied.

A stinger owns the program bus until its clip ends. Firing another one on the same mixer
while one is running is refused rather than queued.

## Clip requirements

The clip needs a real alpha channel — a keyed graphic, not a graphic on a black background.

**Alpha must be straight, not premultiplied.** This is the one requirement that will bite
silently if you get it wrong, so it is worth being deliberate about at export time. A
premultiplied clip composited as straight comes out visibly dark around every soft edge,
and nothing in the compositing path corrects it. Premultiplication also cannot be detected
from the file, so Strom cannot warn you automatically — if you know a clip is
premultiplied, declare it on the source block and the binding is refused outright rather
than quietly putting a dark-fringed graphic on air.

Formats that can carry alpha and are known to work:

| Format | Notes |
| --- | --- |
| FFV1 in Matroska | Lossless, intra-only, seeks cheaply. The safest choice. |
| VP8 or VP9 with alpha in WebM | The common web delivery format. |
| HEVC with alpha | macOS only. |

H.264 cannot carry an alpha channel at all, so an H.264 clip will not work as a stinger no
matter how it was exported.

## Timing

A declared stinger source is held on its first frame, which is what makes a stinger start
immediately when you fire it — the frame is already decoded, so starting playback is only a
state change. Strom re-establishes that state after every fire and whenever the clip
changes.

The one case that costs you is firing immediately after switching to a different clip,
before it has been brought back to its first frame. At high resolutions that can push the
start of the stinger out by close to a frame.

## Current limitations

- **Straight alpha only.** Premultiplied clips are refused, not converted.
- **No fill and key pairs.** A stinger is one file with an alpha channel. Clips delivered
  as separate fill and key files, or with a luma matte alongside, are not supported yet.
- **No audio from the clip.** Any audio track on the stinger clip is ignored and the
  program audio continues uninterrupted.

## When a clip will not play

If the clip is missing, unreadable or fails to decode, the transition beneath still runs on
its own. You lose the branding for that take, but the program source still changes — a
broken file does not leave you stuck mid-transition. The failure is reported so you can see
which clip was at fault.
