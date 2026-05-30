# Documentation Archive

This folder holds documents that are **no longer active references** but are kept
for transparency and residual value:

- **Postmortems of solved problems** — the bug is fixed in current Strom (and in our
  primary target, the Docker Linux image), but the root-cause analysis and workaround
  may still help anyone hitting the same issue on a non-standard setup.
- **Completed audits** — point-in-time reports whose findings have all shipped.
- **Original design / implementation writeups** — documents that describe how a feature
  was once designed or built. **The code is the source of truth**; these have very likely
  drifted from it. They are kept as high-level pointers to original intent, not as specs.

Nothing here describes current behaviour you need to follow. For up-to-date docs, see
the parent [`docs/`](../) folder; for how a feature actually works, read the code.

## Contents

| Document | What it is | Status |
|----------|------------|--------|
| [CEF_SIGILL_CRASH.md](CEF_SIGILL_CRASH.md) | gstcefsrc/Chromium `MemoryInfra` SIGILL postmortem | Solved — fixed in our `strom-full` image via the `mallinfo` LD_PRELOAD shim. Kept for others running gstcefsrc in containers. |
| [MPEGTSMUX_DEADLOCK_FIX.md](MPEGTSMUX_DEADLOCK_FIX.md) | `mpegtsmux` pipeline-construction deadlock postmortem | Solved — fix shipped. |
| [PAD_TEMPLATE_CRASH_FIX.md](PAD_TEMPLATE_CRASH_FIX.md) | SIGSEGV in pad-template access during multi-threaded construction | Solved — fix shipped. |
| [WHIP_ICE_DISCONNECT_INVESTIGATION.md](WHIP_ICE_DISCONNECT_INVESTIGATION.md) | WHIP/WHEP ICE disconnect investigation | Resolved — isolated pipeline per session + `drop-on-latency=true`. |
| [OPENAPI_AUDIT_2026-03-16.md](OPENAPI_AUDIT_2026-03-16.md) | OpenAPI contract coverage audit | Completed — all findings shipped; contract is now snapshot-tested in CI. |
| [BLOCKS_IMPLEMENTATION.md](BLOCKS_IMPLEMENTATION.md) | Block system architecture & how-to-add-a-block writeup | Design/impl — likely drifted; read the code. |
| [MIXER_BLOCK.md](MIXER_BLOCK.md) | Audio Mixer block design/implementation reference | Design/impl — likely drifted. See [../AUDIO_MIXER_OPERATOR_GUIDE.md](../AUDIO_MIXER_OPERATOR_GUIDE.md) for usage. |
| [VIDEO_ENCODER_BLOCK.md](VIDEO_ENCODER_BLOCK.md) | Video Encoder block design/implementation reference | Design/impl — likely drifted; read the code. |
| [COMPOSITOR_EDITOR.md](COMPOSITOR_EDITOR.md) | First-generation compositor layout editor writeup | Legacy + design/impl — prefer the [Vision Mixer](../VISION_MIXER_OPERATOR_GUIDE.md). |
| [AES67_DISCOVERY.md](AES67_DISCOVERY.md) | AES67/SAP discovery design | Design/impl — likely drifted; read the code. |
| [APP_NAVIGATION.md](APP_NAVIGATION.md) | Frontend page/navigation architecture | Design/impl — likely drifted; read the code. |
| [MIXER_BLOCK_PLAN.md](MIXER_BLOCK_PLAN.md) | Original Audio Mixer planning spec | Built — predates [MIXER_BLOCK.md](MIXER_BLOCK.md). |
| [VIDEO_THUMBNAIL_BLOCK.md](VIDEO_THUMBNAIL_BLOCK.md) | Original thumbnail block design spec | Built — `builtin.thumbnail` shipped in v0.4.0. |
| [WHEP_OUTPUT_BLOCK.md](WHEP_OUTPUT_BLOCK.md) | Original WHEP Output block design/implementation writeup | Outdated — the block evolved (multi-track since v0.5.0); kept for background. |
