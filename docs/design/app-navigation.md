# Application Navigation & Pages

## Current Implementation

The frontend now has a multi-page structure with top-level navigation:

```
┌─────────────────────────────────────────────────────────────────┐
│  [Strom Logo]  │ Flows │ Discovery │ Clocks │     [Status Bar] │
├────────────────┴────────────────────────────────────────────────┤
│                                                                 │
│                    Page Content Area                            │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Pages

### 1. Flows (default)

The main flow editor page with:
- Flow list in left sidebar (with filter/search)
- Graph editor in center
- Properties/palette panel on right
- Block palette for adding elements

### 2. Discovery

Browse and manage AES67 streams discovered via SAP:

**Features:**
- Split view: stream list + details panel
- Tabs for Discovered (RX) and Announced (TX) streams
- Filter/search by name, origin, multicast address
- View raw SDP content
- "Copy SDP" button
- "Create Flow" button - creates new flow with AES67 Input pre-configured
- Auto-refresh every 5 seconds

**Stream Details:**
- Name, source, multicast address, port
- Audio format (channels, sample rate, encoding)
- Origin host, first/last seen times, TTL

### 3. Clocks

PTP clock monitoring grouped by domain:

**Features:**
- Domain list with sync status indicators
- Detailed stats per domain:
  - Sync status (Synchronized / Not Synchronized)
  - Grandmaster and Master clock IDs
  - Clock offset (nanoseconds/microseconds)
  - Mean path delay
  - R² (clock estimation quality)
  - Clock rate ratio
- Historical graphs:
  - Clock offset over time
  - R² quality over time
  - Path delay over time
- List of flows using each domain

**Note:** PTP clocks are shared resources - one clock instance per domain regardless of how many flows use it.

---

## Implementation Details

### Navigation State

```rust
pub enum AppPage {
    Flows,
    Discovery,
    Clocks,
}

struct StromApp {
    current_page: AppPage,
    discovery_page: DiscoveryPage,
    clocks_page: ClocksPage,
    // ... existing fields
}
```

### File Structure

```
frontend/src/
├── app.rs              # Main app, page routing, navigation
├── discovery.rs        # Discovery page component
├── clocks.rs           # Clocks page component
├── list_navigator.rs   # Reusable list widget with keyboard nav
├── ptp_monitor.rs      # PTP stats data structures
└── ...
```

### Reusable Components

**ListNavigator** (`list_navigator.rs`):
- Consistent list styling across pages
- Keyboard navigation (arrow keys, Home, End)
- Support for tags, secondary text, status indicators
- Click and scroll-to-selection handling

Used by: Flow list, Discovery stream lists, Clocks domain list

---

## Future Pages

### Files (planned)
Media file management:
- Upload/download files
- Browse recordings, playout files, SDP files
- Preview thumbnails

### Settings (planned)
Application configuration:
- Server identity
- Network settings
- Storage paths
- Appearance (theme)

---

## Backend Support

### Discovery API
```
GET  /api/discovery/streams           # List discovered streams
GET  /api/discovery/streams/{id}      # Get stream details
GET  /api/discovery/streams/{id}/sdp  # Get raw SDP
GET  /api/discovery/announced         # List announced streams
```

### PTP Stats
PTP statistics are sent via WebSocket events:
```rust
StromEvent::PtpStats {
    flow_id: FlowId,
    stats: PtpStatsData,
}
```

Stats are collected by a centralized PTP monitor that registers a global GStreamer callback.
