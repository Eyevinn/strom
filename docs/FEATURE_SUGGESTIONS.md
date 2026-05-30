# Feature Suggestions for Strom

This document outlines potential new features that could enhance Strom beyond the current roadmap. These suggestions are organized by priority and complexity to help guide future development.

## High Priority Features

### 1. Real-time Monitoring Dashboard
**Complexity: Medium**

Provide comprehensive monitoring and observability for running pipelines:

- **Live pipeline metrics**: CPU usage, memory consumption, bitrate, frame rate, buffer levels
- **Performance graphs**: Historical data visualization with time-series charts
- **Alert system**: Configurable thresholds for errors, dropped frames, buffer underruns
- **Resource usage tracking**: Per-pipeline and system-wide resource monitoring

**Why valuable**: Critical for production deployments where pipeline health monitoring is essential for maintaining service quality and detecting issues early.

**Implementation considerations**:
- Integrate GStreamer's tracer subsystem for detailed metrics
- Use Server-Sent Events (SSE) for real-time metric streaming
- Store time-series data (consider InfluxDB or embedded solution)
- Add egui charts for visualization

---

### 2. Scheduling & Automation System
**Complexity: Medium**

Enable time-based and event-driven pipeline automation:

- **Cron-like scheduling**: Start/stop pipelines at specific times
- **Event-triggered actions**: Webhook-based pipeline control
- **Recording windows**: Time-based recording schedules (e.g., record daily 9am-5pm)
- **Retention policies**: Auto-delete old recordings based on age or storage limits

**Why valuable**: Enables unattended operation for broadcast, surveillance, and scheduled recording use cases.

**Implementation considerations**:
- Use `tokio-cron-scheduler` for time-based execution
- Add webhook endpoint for external triggers
- Implement storage quota management
- Add calendar view UI for schedule visualization

---

### 3. Live Video/Audio Preview
**Complexity: High** | **Status: Partially implemented**

Provide in-browser preview of pipeline output:

- ✅ **WebRTC-based preview**: WHEP Output block serves streams with built-in browser player pages
- ✅ **Audio monitoring**: Audio Meter block with RMS and peak level monitoring per channel
- ✅ **Thumbnail generation**: `builtin.thumbnail` block / thumbnail tap captures frames from video sources (v0.4.0)
- ✅ **Multi-view (within a mix)**: Vision Mixer multiview output shows all its sources in one frame
- **Cross-flow multi-view**: Monitor multiple independent pipelines simultaneously in a grid layout

**Remaining work**: A cross-flow multi-view dashboard (the Vision Mixer multiview is per-mix, not server-wide).

---

## Medium Priority Features

### 4. Pipeline Templates & Preset Library
**Complexity: Low**

Accelerate workflow creation with pre-built configurations:

- **Community templates**: Pre-built pipelines for common tasks:
  - RTSP camera recorder
  - HLS live streaming server
  - File transcoding
  - Multi-bitrate adaptive streaming
  - Audio podcasting pipeline
  - Screen recording
- **Element presets**: Save frequently used configurations (e.g., "High Quality H264", "Low Latency RTMP")
- **Import/export**: Share flows with team or community as JSON/YAML
- **Template marketplace**: Browse and download community-contributed templates

**Why valuable**: Accelerates workflow creation, knowledge sharing, and makes GStreamer accessible to beginners.

**Implementation considerations**:
- Store templates in dedicated directory
- Add template category filtering in UI
- Include template metadata (author, description, use case)
- Add "Save as Template" button to current flows

---

### 5. Codec Optimizer & Quality Assistant
**Complexity: Medium** | **Status: Partially implemented**

Intelligent encoding recommendations and optimization:

- **Smart encoding suggestions**: Recommend bitrate/resolution based on use case (streaming, archival, etc.)
- **Quality calculator**: Estimate file size for given settings
- ✅ **Hardware acceleration detector**: Video Encoder block auto-detects and selects best available encoder (NVENC, QSV, VA-API, AMF, software)
- **A/B comparison**: Test different encoding settings side-by-side
- **Preset wizard**: Step-by-step guide for selecting optimal encoder settings

**Remaining work**: Smart encoding suggestions, quality calculator, A/B comparison, preset wizard.

---

### 6. Multi-Pipeline Orchestration
**Complexity: High** | **Status: Partially implemented**

Coordinate multiple pipelines for complex workflows:

- **Pipeline groups**: Organize related pipelines (e.g., "Camera System A", "Live Event Production")
- ✅ **Cascading pipelines**: Inter Input/Output blocks enable inter-pipeline routing (publish/subscribe streams between flows)
- **Synchronized operations**: Start/stop multiple pipelines atomically
- **Dependency management**: Pipeline B waits for Pipeline A to be ready
- **Shared resources**: Manage resource allocation across pipelines

**Remaining work**: Pipeline groups, synchronized operations, dependency management, shared resources.

---

### 7. Cloud Storage Integration
**Complexity: Medium**

Integrate with cloud services for modern deployments:

- **S3-compatible sinks**: Direct upload to AWS S3, MinIO, Backblaze B2, Google Cloud Storage
- **Cloud transcoding**: Offload heavy encoding to cloud services (AWS MediaConvert, etc.)
- **CDN integration**: Push HLS/DASH streams to CDN
- **Webhook notifications**: Trigger cloud functions on pipeline events
- **Cloud backup**: Auto-backup flow configurations

**Why valuable**: Modern deployments require cloud integration for scalability and cost optimization.

**Implementation considerations**:
- Create custom GStreamer sink or use s3sink plugin
- Add cloud credential management
- Implement retry logic for reliability
- Add progress tracking for uploads

---

### 8. Stream Health Analytics
**Complexity: Medium**

Comprehensive diagnostics for live streaming:

- **RTSP/RTMP diagnostics**: Connection stability, packet loss, jitter measurements
- **Buffer analysis**: Underrun/overrun detection with root cause hints
- **Latency measurement**: Glass-to-glass latency tracking
- **Quality metrics**: PSNR, SSIM for encoded video quality assessment
- **Network statistics**: Bandwidth usage, retransmissions, etc.

**Why valuable**: Essential for live streaming and broadcast applications where reliability is critical.

**Implementation considerations**:
- Integrate GStreamer's QoS (Quality of Service) system
- Add network probe elements
- Implement latency measurement pipeline
- Store analytics history for trend analysis

---

### 9. AI-Powered Troubleshooting Assistant
**Complexity: Medium** | **Status: Partially implemented**

Intelligent assistance for debugging and optimization:

- ✅ **Natural language queries**: MCP integration enables AI assistants (Claude, etc.) to create and manage pipelines
- **Error diagnosis**: Analyze GStreamer errors and suggest fixes with context
- **Compatibility checker**: Validate element combinations before runtime
- **Pipeline recommendations**: "Users who built X also used Y" suggestions
- **Best practices advisor**: Detect anti-patterns and suggest improvements

**Remaining work**: Error diagnosis, compatibility checker, pipeline recommendations, best practices advisor.

---

## Low Priority Features

### 10. Plugin Manager
**Complexity: Low**

Manage GStreamer plugins and extensions:

- **Plugin discovery**: Scan system for installed GStreamer plugins
- **Plugin metadata**: Show version, license, capabilities, element list
- **Missing element helper**: Suggest which plugin to install for unavailable elements
- **Installation assistant**: Guide users through plugin installation
- **Custom plugin wizard**: Simplified development helper for custom elements

**Why valuable**: Makes the GStreamer ecosystem more accessible and helps users discover available functionality.

**Implementation considerations**:
- Use GStreamer registry API
- Create plugin database with installation instructions
- Add UI panel for plugin management
- Include links to plugin documentation

---

### 11. Batch Processing Mode
**Complexity: Medium**

Process multiple files or streams efficiently:

- **File queue**: Process multiple files with same pipeline configuration
- **Folder watch**: Auto-process new files in watched directories
- **Parallel processing**: Run multiple pipelines concurrently with resource management
- **Progress tracking**: Overall batch progress visualization
- **Job scheduling**: Queue management with priority levels

**Why valuable**: Useful for transcoding services, archival workflows, and bulk processing tasks.

**Implementation considerations**:
- Implement job queue system
- Add file watcher using `notify` crate
- Resource pool for concurrent pipelines
- Progress aggregation across jobs

---

### 12. Configuration Profiles
**Complexity: Low**

Flexible configuration management:

- **Environment configs**: dev/staging/production settings
- **Secret management**: Secure credential storage (passwords, API keys, tokens)
- **Variable substitution**: Dynamic properties (e.g., `${RTSP_URL}`, `${OUTPUT_PATH}`)
- **Config import/export**: Backup and restore configurations
- **Config validation**: Ensure required variables are set

**Why valuable**: Simplifies deployment across environments and improves security.

**Implementation considerations**:
- Use environment variable substitution
- Integrate with system keyring for secrets
- Add config validation at startup
- Support .env files

---

### 13. Collaboration Features
**Complexity: High**

Enable team workflows:

- **Real-time collaborative editing**: Multiple users editing same flow (CRDT-based)
- **Version control**: Git-like history for flows with diff visualization
- **Comments & annotations**: Add notes to pipeline elements
- **Audit log**: Track who changed what and when
- **User management**: Roles and permissions (admin, editor, viewer)
- **Flow sharing**: Share flows with specific users or teams

**Why valuable**: Critical for team environments and production operations.

**Implementation considerations**:
- Implement WebSocket-based real-time sync
- Add user authentication system
- Use CRDT (Conflict-free Replicated Data Types) for concurrent editing
- Add database backend for user/permission storage

---

### 14. Mobile Companion App
**Complexity: High**

Remote management from mobile devices:

- **Monitor status**: View pipeline states on mobile
- **Remote control**: Start/stop flows
- **Push notifications**: Alert on errors/events
- **Basic editing**: Simple property changes
- **Live preview**: View pipeline output on mobile

**Why valuable**: Enables on-the-go management for operators and monitoring staff.

**Implementation considerations**:
- Build React Native or Flutter app
- Reuse existing REST API
- Implement push notification service
- Add mobile-optimized UI

---

## Quick Wins (Low Effort, High Value)

These features can be implemented quickly while providing significant value:

1. **Preset Library**: Package 10-15 common pipelines as JSON templates
   - RTSP recorder, file transcoder, HLS server, etc.
   - Estimated effort: 4-6 hours

2. **Plugin Scanner**: Simple GStreamer plugin discovery tool
   - List installed plugins and their elements
   - Estimated effort: 2-3 hours

3. **Configuration Profiles**: Environment-based settings
   - Support .env files and variable substitution
   - Estimated effort: 3-4 hours

4. ~~**Thumbnail Generation**: Screenshot capability for video pipelines~~ ✅ Done
   - `builtin.thumbnail` block + frame-capture endpoint shipped in v0.4.0

5. **Pipeline Validation**: Pre-flight checks before starting
   - Validate element compatibility and required properties
   - Estimated effort: 4-6 hours

---

## Recommended Implementation Roadmap

### Phase 1: MVP+ (Core Enhancements)
**Goal**: Production-ready with essential monitoring

1. Real-time monitoring dashboard
2. Pipeline templates library
3. Configuration profiles
4. Plugin manager
5. Pipeline validation

**Estimated timeline**: 4-6 weeks

---

### Phase 2: Production Ready (Reliability & Usability)
**Goal**: Enterprise-grade stability and user experience

1. Scheduling & automation system
2. Codec optimizer & quality assistant
3. Stream health analytics
4. Batch processing mode
5. AI-powered troubleshooting

**Estimated timeline**: 6-8 weeks

---

### Phase 3: Advanced Features (Scale & Integration)
**Goal**: Cloud-native and team-oriented

1. Cloud storage integration
2. Live video/audio preview
3. Multi-pipeline orchestration
4. Collaboration features
5. Mobile companion app

**Estimated timeline**: 8-12 weeks

---

## Community Input

We welcome community feedback on these suggestions! Please:

- Open GitHub issues to discuss specific features
- Vote on features you'd like to see prioritized
- Contribute implementations via pull requests
- Share your use cases to help shape development

---

## Contributing

Interested in implementing any of these features? See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines, and open an issue to discuss your approach before starting significant work.

---

*Last updated: 2026-05-30*
*Status: Community feedback welcome*
