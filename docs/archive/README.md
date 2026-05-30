# Documentation Archive

This folder holds documents that are **no longer active references** but are kept
for transparency and residual value:

- **Postmortems of solved problems** — the bug is fixed in current Strom (and in our
  primary target, the Docker Linux image), but the root-cause analysis and workaround
  may still help anyone hitting the same issue on a non-standard setup.
- **Completed audits** — point-in-time reports whose findings have all shipped.
- **Design specs for features that are now built** — superseded by the living
  reference docs for the shipped feature.

Nothing here describes current behaviour you need to follow. For up-to-date docs, see
the parent [`docs/`](../) folder.

## Contents

| Document | What it is | Status |
|----------|------------|--------|
| [CEF_SIGILL_CRASH.md](CEF_SIGILL_CRASH.md) | gstcefsrc/Chromium `MemoryInfra` SIGILL postmortem | Solved — fixed in our `strom-full` image via the `mallinfo` LD_PRELOAD shim. Kept for others running gstcefsrc in containers. |
| [MPEGTSMUX_DEADLOCK_FIX.md](MPEGTSMUX_DEADLOCK_FIX.md) | `mpegtsmux` pipeline-construction deadlock postmortem | Solved — fix shipped. |
| [PAD_TEMPLATE_CRASH_FIX.md](PAD_TEMPLATE_CRASH_FIX.md) | SIGSEGV in pad-template access during multi-threaded construction | Solved — fix shipped. |
| [WHIP_ICE_DISCONNECT_INVESTIGATION.md](WHIP_ICE_DISCONNECT_INVESTIGATION.md) | WHIP/WHEP ICE disconnect investigation | Resolved — isolated pipeline per session + `drop-on-latency=true`. |
| [OPENAPI_AUDIT_2026-03-16.md](OPENAPI_AUDIT_2026-03-16.md) | OpenAPI contract coverage audit | Completed — all findings shipped; contract is now snapshot-tested in CI. |
| [MIXER_BLOCK_PLAN.md](MIXER_BLOCK_PLAN.md) | Original Audio Mixer design spec | Built — see [../MIXER_BLOCK.md](../MIXER_BLOCK.md) for the living reference. |
| [video-thumbnail-block.md](video-thumbnail-block.md) | Original thumbnail block design spec | Built — `builtin.thumbnail` shipped in v0.4.0. |
