# Feature Ideas

An unordered list of ideas that could enhance Strom. **This is not a roadmap or a
commitment** — nothing here is planned, scheduled, or guaranteed. For what Strom can do
today, see the [README](../README.md) and [CHANGELOG.md](CHANGELOG.md). If an idea
interests you, open a GitHub issue or discussion.

---

## Monitoring & diagnostics

- **Pipeline health dashboard** — live per-pipeline metrics (bitrate, frame drops, buffer
  levels, latency) with historical graphs and configurable alerting.
- **Stream health analytics** — RTSP/RTMP/SRT connection stability, jitter, glass-to-glass
  latency, and encoded-quality metrics (PSNR/SSIM).
- **Cross-flow multi-view** — a grid that monitors several independent pipelines at once
  (the Vision Mixer multiview is per-mix, not server-wide).

## Workflow & automation

- **Flow templates / preset library** — ready-made flows for common tasks (RTSP recorder,
  HLS server, transcoder, etc.) and savable element presets.
- **Scheduling & automation** — cron-style start/stop, webhook triggers, recording windows,
  and retention policies.
- **Batch processing** — file queue / folder-watch for bulk transcoding with concurrency
  limits and progress tracking.
- **Undo/redo** and **drag-drop flow import** in the editor.
- **Connection validation hints** — pre-flight checks for element compatibility and required
  properties before starting.

## Encoding & integration

- **Codec/quality assistant** — bitrate/resolution recommendations, file-size estimates, and
  A/B comparison of encoder settings.
- **Cloud storage integration** — S3-compatible sinks, CDN push for HLS/DASH, webhook
  notifications on pipeline events.
- **Plugin manager** — discover installed GStreamer plugins, show element capabilities, and
  suggest which plugin to install for a missing element.

## Intelligence & collaboration

- **AI troubleshooting (enhanced MCP)** — natural-language pipeline creation, error diagnosis
  from QoS data, and best-practice/anti-pattern detection on top of the existing MCP server.
- **Flow version control** — git-like history with visual diff and one-click rollback.
- **Multi-user collaboration** — real-time co-editing, presence, role-based permissions, and
  audit logging.

## Platform & reach

- **Publish straight to the big live platforms** — take Strom's RTMP output to YouTube Live
  and Twitch and write down what an operator has to set. The transport side is there: the
  sink speaks `rtmp` and `rtmps`, and it keeps the query string Twitch's
  `?bandwidthtest=true` rides on, so a test broadcast can be verified in Twitch Inspector
  without going live. The encoder side is where the work is — the Video Encoder block's
  default profile lets the encoder negotiate freely, and against `x264enc` that lands on
  High 4:4:4 Predictive 10-bit, which neither platform accepts. Pinning `profile=high` gives
  8-bit 4:2:0. Both platforms also want an audio track and roughly 2-second keyframes.
- **Kubernetes operator** — deploy flows as pods with resource limits and auto-scaling.
- **Block marketplace** — browse and install community-contributed blocks.
- **Mobile companion** — monitor status, start/stop flows, and receive alerts from a phone.

---

*Ideas welcome — open a GitHub issue or discussion to propose or champion one.*
