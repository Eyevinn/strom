# Strom Feature Roadmap

A prioritized list of killer features to enhance Strom's capabilities as a visual GStreamer pipeline editor.

---

## Tier 1: High Impact / High Feasibility

### 1. Flow Templates & Quick Start Gallery

Pre-built templates that dramatically lower the barrier to entry and showcase Strom's capabilities.

**Examples:**
- Live Stream to YouTube/Twitch
- RTSP to SRT Relay
- Multi-camera Compositor
- AES67 Studio Intercom
- Webcam to WebRTC
- File Transcoding Pipeline
- NDI to SDI Bridge

**Implementation:**
- One-click deployment with parameter wizards
- Categorized gallery view in UI
- User can save their own flows as templates

---

### 2. Live Parameter Tweaking with Visual Preview

Real-time adjustments for running pipelines with immediate visual feedback.

**Current status:** ✅ Basic live parameter editing works - properties can be changed on running pipelines. Audio level meter block (`builtin.meter`) available.

**Remaining features:**
- Real-time thumbnail previews embedded in graph nodes
- Audio/video scopes: waveform, vectorscope, histogram overlays
- Parameter interpolation for smooth transitions

**Why it matters:** Broadcasting professionals expect this level of control. No open-source competitor offers this integrated experience.

---

### 3. Multi-Flow Orchestration & Routing Matrix

Enable complex broadcast workflows by routing outputs between flows dynamically.

**Current status:** ✅ Vision Mixer block with broadcast-style PVW/PGM switching, CUT/AUTO transitions, DSK overlays, fade-to-black, PiP, and multiview output. ✅ Compositor block with layout templates and visual editor (first-generation; superseded by Vision Mixer). ✅ Inter Input/Output blocks for cross-flow routing. ✅ Audio Router block with multi-input channel routing matrix. ✅ Audio Mixer block with up to 32 channels, aux sends, groups, PFL/AFL, and a monitor bus.

**Remaining features:**
- Global audio/video buses across flows

**Use cases:**
- Live production switching
- Multi-channel encoding from single source
- Redundant path failover

---

### 4. Pipeline Health Dashboard

Comprehensive monitoring for production deployments.

**Metrics:**
- Real-time graphs: bitrate, frame drops, buffer levels, latency
- Per-element QoS visualization
- CPU/GPU/memory utilization per pipeline
- Network I/O statistics

**Alerting:**
- Configurable thresholds
- Notifications via webhook, email, Slack, PagerDuty
- Alert history and acknowledgment

**Historical:**
- Time-range selection for metrics
- Export to Prometheus/Grafana

---

## Tier 2: Strategic Differentiators

### 5. AI Pipeline Assistant (Enhanced MCP)

Leverage the existing MCP infrastructure to provide intelligent assistance.

**Current status:** ✅ MCP server with Streamable HTTP (`/api/mcp`) and stdio transports. Supports flow management, element discovery, and pipeline control. SSE events for real-time updates.

**Future capabilities:**
- Natural language pipeline creation: *"Create a pipeline that takes RTSP, adds a logo overlay, and outputs to SRT"*
- Error diagnosis: *"My pipeline is dropping frames"* → AI analyzes QoS data and suggests fixes
- Auto-optimization based on hardware detection
- Element recommendation based on use case

**Implementation:**
- Enhance MCP server with diagnostic tools
- Add system introspection capabilities
- Train/prompt for GStreamer domain knowledge

---

### 6. Flow Version Control & Rollback

Git-like history for pipeline configurations.

**Features:**
- Automatic versioning on every save
- Compare versions side-by-side (visual diff)
- One-click rollback to previous configuration
- Branch flows for A/B testing
- Commit messages and annotations

**Why it matters:** Critical for production safety. No competitor offers this for media pipelines.

---

### 7. Scheduled Pipeline Operations

Enable unattended operation and broadcast automation.

**Features:**
- Cron-style scheduling: *"Start at 08:00, stop at 18:00"*
- Calendar integration (iCal, Google Calendar)
- Event-based triggers (file arrival, API webhook)
- Failover rules: *"If source fails, switch to backup flow"*

**Use cases:**
- Scheduled broadcasts
- Automated recording windows
- Time-based source switching

---

### 8. Remote Source Preview & Confidence Monitoring

Multi-viewer style monitoring for all running pipelines.

**Current status:** ✅ Vision Mixer multiview output with per-input labels and VU meters. ✅ Audio Meter block with RMS and peak level monitoring per channel. ✅ WHEP Output block with built-in browser player pages for live preview. ✅ Links page for quick access to WHEP player pages and stream URLs.

**Remaining features:**
- Thumbnail grid of all active pipelines (cross-flow, not just within a Vision Mixer)
- Full-screen preview on click

**Why it matters:** Standard requirement for broadcast control rooms.

---

## Tier 3: Platform Play

### 9. Kubernetes Operator & Auto-Scaling

Enterprise-grade deployment capabilities.

**Features:**
- Custom Kubernetes operator for Strom flows
- Deploy flows as K8s pods with resource limits
- Auto-scale based on viewer count, CPU, or custom metrics
- Geographic distribution for CDN-like deployments
- Helm charts for easy installation

**Benefits:**
- Horizontal scaling for large events
- High availability with pod redundancy
- Integration with cloud-native tooling

---

### 10. Plugin/Block Marketplace

Community-driven extensibility.

**Features:**
- Browse and install community-contributed blocks
- Rating and review system
- Documentation and usage examples
- Semantic versioning for blocks
- One-click install from UI

**Categories:**
- Video effects and filters
- Audio processing
- Protocol adapters
- Hardware integrations
- AI/ML inference blocks

---

### 11. Multi-User Collaboration

Team workflows and enterprise access control.

**Features:**
- Real-time collaborative editing (Figma-style)
- Cursor presence and user indicators
- Role-based permissions (viewer/editor/admin)
- Per-flow access control
- Comprehensive audit logging

**Implementation:**
- WebSocket-based operational transforms or CRDTs
- User management API
- Integration with LDAP/OIDC

---

### 12. Recording & Replay System

Essential for live production workflows.

**Current status:** ✅ Recorder block (`builtin.recorder`, shipped v0.3.25) — writes any audio/video stream to file with configurable segmentation, a recording duration counter, and auto-stop.

**Remaining features:**
- Timeline-based clip marking during recording
- Instant replay capability with variable speed
- Integration with remote storage backends (S3, NFS)

**Use cases:**
- Sports instant replay
- Event archiving
- Compliance recording

---

## Quick Wins

Low effort improvements with noticeable impact.

| Feature | Effort | Impact | Status |
|---------|--------|--------|--------|
| ~~Dark/light theme toggle~~ | 1 day | UX polish | ✅ Done |
| ~~Keyboard shortcuts~~ | 2 days | Power users | ✅ Done (Ctrl+F, Delete, copy/paste) |
| ~~Copy/paste elements~~ | 1 day | Workflow speed | ✅ Done |
| ~~Element search~~ | 1 day | Navigation | ✅ Done (Ctrl+F cycles filters) |
| ~~Zoom to fit~~ | 0.5 day | UX | ✅ Done |
| Undo/redo | 3 days | Essential UX | Not started |
| Export flow as Docker Compose | 2 days | Deployment | Not started |
| Drag-drop flow import | 1 day | Onboarding | Not started |
| Connection validation hints | 2 days | Error prevention | Not started |

---

## Recommended Roadmap

### Phase 1: Foundation (Next Release)
- [ ] Templates Gallery
- [ ] Undo/Redo system
- [ ] Basic Health Dashboard
- [x] Keyboard shortcuts (Ctrl+F, Delete, copy/paste)

### Phase 2: Professional Tools
- [x] Live Parameter Tweaking (basic - properties editable on running pipelines)
- [x] Compositor with layout templates (first-generation)
- [x] Vision Mixer with PVW/PGM switching, transitions, DSK, FTB, PiP, multiview
- [x] Inter Input/Output blocks for cross-flow routing
- [x] Audio Mixer (up to 32 channels, aux sends, groups, PFL/AFL, monitor bus)
- [x] Audio Router (multi-input channel routing matrix)
- [x] WHEP Output with built-in player pages
- [x] Recorder block (segmentation, auto-stop)
- [ ] Multi-Flow Routing Matrix (visual patchbay)
- [ ] Scheduled Operations
- [ ] Cross-flow Source Preview Grid

### Phase 3: Intelligence & Safety
- [ ] Flow Version Control
- [x] MCP server with Streamable HTTP and stdio transports
- [ ] Enhanced AI Assistant (diagnostics, auto-optimization)
- [ ] Advanced Alerting

### Phase 4: Scale & Community
- [ ] Kubernetes Operator
- [ ] Block Marketplace
- [ ] Multi-User Collaboration
- [x] Recording System (Recorder block; replay/clip-marking still open)

---

## Technical Considerations

### Architecture Impact

| Feature | Backend Changes | Frontend Changes | New Dependencies |
|---------|-----------------|------------------|------------------|
| Templates | Template storage, API | Gallery UI, wizard | None |
| Live Tweaking | Property streaming | Real-time controls | None |
| Multi-Flow Routing | Inter-pipeline routing | Matrix UI | None |
| Health Dashboard | Metrics collection | Chart components | Time-series storage |
| Version Control | Git-like storage | Diff viewer | None (or libgit2) |
| Scheduling | Scheduler service | Calendar UI | cron parser |
| K8s Operator | Operator binary | - | kube-rs |
| Collaboration | OT/CRDT engine | Presence UI | None |

### Performance Considerations

- Live preview thumbnails: Consider WebRTC for low-latency previews
- Metrics storage: Time-series database (InfluxDB/TimescaleDB) for historical data
- Multi-user sync: WebSocket with efficient delta updates
- Recording: Separate recording process to avoid pipeline interference

---

## Competitive Analysis

| Feature | Strom | OBS | vMix | Wirecast |
|---------|-------|-----|------|----------|
| Visual Pipeline Editor | ✅ | ❌ | ❌ | ❌ |
| GStreamer Backend | ✅ | ❌ | ❌ | ❌ |
| Web-Based UI | ✅ | ❌ | ❌ | ❌ |
| API-First | ✅ | ⚠️ | ⚠️ | ❌ |
| AI Integration | ✅ | ❌ | ❌ | ❌ |
| Open Source | ✅ | ✅ | ❌ | ❌ |
| AES67/ST2110 | ✅ | ❌ | ⚠️ | ❌ |
| Linux Native | ✅ | ✅ | ❌ | ❌ |

Strom's unique position: **Professional broadcast capabilities with open-source flexibility and modern architecture.**

---

*Last updated: 2026-05-30*
