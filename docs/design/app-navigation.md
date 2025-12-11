# Application Navigation & Pages Design

## Current State

The frontend is currently flow-centric with no top-level navigation:
- Flow list in left sidebar
- Graph editor in center
- Properties panel on right
- All UI revolves around the selected flow

## Proposed Navigation Structure

Add a primary navigation bar to switch between major sections:

```
┌─────────────────────────────────────────────────────────────────┐
│  [Strom Logo]  │ Flows │ Discovery │ PTP │ Files │ Settings │   │
├────────────────┴────────────────────────────────────────────────┤
│                                                                 │
│                    Page Content Area                            │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Pages Overview

### 1. Flows (existing, default)
Current flow editor - no changes needed to core functionality.
- Flow list
- Graph editor
- Properties panel
- Palette

### 2. Discovery (SAP Browser)
Browse and manage discovered AES67 streams.

**Features:**
- List discovered streams with details (name, origin, channels, sample rate)
- Filter/search by name, encoding, source
- View raw SDP
- Create AES67 Input block from discovered stream
- Show our announced streams
- Real-time updates via WebSocket

**Settings within page:**
- SAP multicast address (default: 224.2.127.254, configurable for network-wide reach)
- SAP port (default: 9875)
- Enable/disable listener
- Enable/disable announcer
- Network interface selection

### 3. PTP (Clock Synchronization)
Dedicated PTP monitoring and configuration.

**Features:**
- PTP clock status (synced/not synced)
- Clock statistics:
  - Mean path delay
  - Clock offset
  - R-squared (estimation quality)
  - Clock rate ratio
- PTP domain selection
- Historical graphs (offset over time)
- Multi-domain support (show all active domains)

**Settings:**
- Default PTP domain
- PTP network interface

### 4. Files (Media Management)
Upload, browse, and manage media files.

**Features:**
- File browser (tree view or list)
- Upload files (drag & drop)
- Download files
- Organize into folders
- Preview (thumbnails for video/images)
- File metadata display

**Categories:**
- Playout files (video, audio, images)
- Recordings (captured streams)
- SDP files (manual imports)
- Logs (server logs download)

**Settings:**
- Storage paths
- Auto-cleanup policies
- Recording settings

### 5. Settings (Application Configuration)
Central configuration page.

**Sections:**

#### 5.1 Server Identity
- Hostname (display name for this Strom instance)
- Description/notes
- Location (for multi-server deployments)
- Admin contact

#### 5.2 Network Configuration
- Primary network interface
- SAP settings (also accessible from Discovery page)
- PTP settings (also accessible from PTP page)

#### 5.3 Authentication
- Enable/disable authentication
- User management (if multi-user in future)
- API key management

#### 5.4 Storage
- Media storage path
- Recording output path
- Log retention
- Auto-cleanup settings

#### 5.5 Appearance
- Theme (dark/light)
- UI density
- Default view on startup

#### 5.6 About
- Version info
- License
- System information

## Backend API Changes

### New Endpoints

```
# Discovery Settings
GET  /api/settings/discovery
PUT  /api/settings/discovery
  {
    sap_enabled: bool,
    sap_multicast_address: string,
    sap_port: u16,
    sap_interface: string | null,
    announcer_enabled: bool
  }

# PTP Settings
GET  /api/settings/ptp
PUT  /api/settings/ptp
  {
    default_domain: u8,
    interface: string | null
  }

# Server Identity
GET  /api/settings/identity
PUT  /api/settings/identity
  {
    hostname: string,
    description: string,
    location: string,
    admin_contact: string
  }

# Storage Settings
GET  /api/settings/storage
PUT  /api/settings/storage
  {
    media_path: string,
    recording_path: string,
    log_retention_days: u32
  }

# Files API
GET  /api/files                    # List files
GET  /api/files/{path}             # Get file info or contents
POST /api/files/{path}             # Upload file
DELETE /api/files/{path}           # Delete file
GET  /api/files/{path}/download    # Download file

# Logs
GET  /api/logs                     # List log files
GET  /api/logs/{name}              # Get log contents
GET  /api/logs/{name}/download     # Download log file
```

### Settings Storage

Options:
1. **Config file** (`strom.toml`) - Already exists for some settings
2. **Database** - If using SQLx already
3. **Separate settings file** - `settings.json` in config directory

Recommendation: Extend existing `strom.toml` / use figment for layered config.

## Frontend Implementation

### Navigation State

Add to `StromApp`:
```rust
enum AppPage {
    Flows,
    Discovery,
    Ptp,
    Files,
    Settings,
}

struct StromApp {
    current_page: AppPage,
    // ... existing fields
}
```

### Page Components

New files:
```
frontend/src/pages/
├── mod.rs
├── discovery.rs      # SAP browser
├── ptp.rs           # PTP statistics (extract from existing)
├── files.rs         # File management
└── settings.rs      # Settings page

frontend/src/components/
├── nav_bar.rs       # Top navigation bar
└── settings/
    ├── mod.rs
    ├── identity.rs
    ├── network.rs
    ├── storage.rs
    └── appearance.rs
```

### Render Flow

```rust
fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
    // Top navigation bar (always visible)
    self.render_nav_bar(ctx);

    // Page content
    match self.current_page {
        AppPage::Flows => self.render_flows_page(ctx),
        AppPage::Discovery => self.render_discovery_page(ctx),
        AppPage::Ptp => self.render_ptp_page(ctx),
        AppPage::Files => self.render_files_page(ctx),
        AppPage::Settings => self.render_settings_page(ctx),
    }

    // Status bar (always visible)
    self.render_status_bar(ctx);
}
```

## SAP Multicast Address Note

You're correct about the address scope:

- **224.2.127.254** (SAP standard) - Administratively scoped, typically stays within local network segment
- **239.x.x.x** range - Organization-local scope, can be routed within enterprise
- Some deployments use **239.255.255.255** or custom addresses for wider reach

The SAP address should be configurable because:
1. Default SAP address may not cross routers
2. Enterprise networks may have designated AES67 discovery addresses
3. Dante uses the standard address but some custom systems don't

## Implementation Phases

### Phase 1: Navigation Framework
1. Add navigation bar component
2. Add page routing enum
3. Restructure app.rs to support pages
4. Move existing flows UI into "Flows" page

### Phase 2: Discovery Page
1. Create discovery page component
2. Add stream list with real-time updates
3. Add "Create from stream" action
4. Add discovery settings section

### Phase 3: Settings Page
1. Create settings page structure
2. Add identity settings
3. Add appearance settings (theme)
4. Backend API for settings persistence

### Phase 4: PTP Page
1. Extract PTP monitor to dedicated page
2. Add historical graphs
3. Add configuration options

### Phase 5: Files Page
1. Backend file management API
2. Upload/download functionality
3. File browser UI
4. Integration with playout blocks

## Open Questions

1. **Settings persistence**: Config file vs database?
2. **Files storage**: Local filesystem? Object storage?
3. **Multi-instance**: Should settings sync across instances?
4. **Permissions**: Per-page access control in future?
