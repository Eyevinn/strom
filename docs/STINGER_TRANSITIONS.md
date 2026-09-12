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

## Timing the clip

Three properties on the Media Player block say how the stinger behaves. They live on the
clip because they follow from the artwork: whoever cut the clip knows where it covers, and
an operator should not be retyping that under time pressure.

- **Stinger Cut Point** — how far into the clip the program source actually changes. Set it
  to the moment your clip fully covers the frame. Leave it at 0 and the halfway point is
  used.
- **Stinger Beneath** — what the program does at the cut point. A cut is what a fully
  covering clip wants. A clip that does not completely cover the frame is a good reason to
  put a wipe or a mix underneath instead.
- **Stinger Beneath Duration** — how long that transition beneath takes. It is *not* the
  length of the stinger; the clip's own length decides that. It is ignored for a cut.

If the cut point plus the duration would run past the end of the clip, the duration is
shortened so the transition finishes while the clip is still covering. You are told both
the duration declared and the one that was applied.

## Firing one

Trigger a transition of type `stinger` on the vision mixer, naming the media player block
that holds the clip. That is the whole request — the timing comes from the block. The
`duration_ms` field is ignored for a stinger.

A stinger owns the program bus until its clip ends. Firing another one on the same mixer
while one is running is refused rather than queued.

## Clip requirements

The clip needs a real alpha channel — a keyed graphic, not a graphic on a black background.

**Alpha must be straight, not premultiplied.** This is the one requirement that will bite
silently if you get it wrong, so set it deliberately at export time. A premultiplied clip
composited as straight comes out visibly dark around every soft edge,
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

## Current limitations

- **Straight alpha only.** Premultiplied clips are refused, not converted.
- **No fill and key pairs.** A stinger is one file with an alpha channel. Clips delivered
  as separate fill and key files, or with a luma matte alongside, are not supported yet.
- **No audio from the clip.** Any audio track on the stinger clip is ignored and the
  program audio continues uninterrupted.
- **A changed clip is not ready immediately.** A declared source is held ready between
  takes, so a stinger normally starts the moment you fire it. Change that player's clip and
  fire straight away and the start can slip by close to a frame at 4K.

## When a clip will not play

If the clip is missing, unreadable or fails to decode, the transition beneath still runs on
its own. You lose the branding for that take, but the program source still changes — a
broken file does not leave you stuck mid-transition. The failure is reported so you can see
which clip was at fault.
