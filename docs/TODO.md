# Strom Development Roadmap

## Phase 1: Project Foundation ✅ COMPLETE

### 1.1 Project Structure ✅
- [x] Initialize project architecture plan
- [x] Create Cargo workspace with three packages (types, backend, frontend)
- [x] Set up `strom-types` library crate
- [x] Set up `strom` binary crate
- [x] Set up `strom-frontend` crate with WASM support
- [x] Configure workspace dependencies and shared versions
- [x] Add .gitignore and basic git configuration

### 1.2 Backend Foundation ✅
- [x] Set up axum web server with basic routes
- [x] Implement health check endpoint
- [x] Configure CORS for local development
- [x] Set up static file serving for frontend
- [x] Add structured logging (tracing/tracing-subscriber)
- [x] Create basic configuration system (config file + env vars)

### 1.3 Frontend Foundation ✅
- [x] Initialize egui/eframe web application
- [x] Set up Trunk build configuration
- [x] Create basic HTML template
- [x] Implement main application structure
- [x] Add basic routing/navigation if needed

### 1.4 Development Tooling ✅
- [x] Create development scripts (run backend, run frontend, etc.)
- [x] Set up hot-reload for development
- [x] Add basic README instructions for running locally
- [ ] Configure CI/CD basics (optional at this stage)

## Phase 2: Data Models & Persistence ✅ COMPLETE

### 2.1 Shared Types Library (`strom-types`) ✅
- [x] Define Flow struct (id, name, auto_start, elements, links) in `flow.rs`
- [x] Define Element struct (id, type, properties) in `element.rs`
- [x] Define Link struct (from_pad, to_pad) in `element.rs`
- [x] Define PipelineState enum (NULL, READY, PAUSED, PLAYING) in `state.rs`
- [x] Define API request/response types in `api.rs`:
  - [x] CreateFlowRequest, UpdateFlowRequest
  - [x] FlowResponse, FlowListResponse
  - [x] ElementInfoResponse, ElementListResponse
  - [x] WebSocketMessage types (StateChange, Error, etc.)
- [x] Add serde serialization for all types
- [x] Add validation helpers (optional)
- [x] Add OpenAPI schema support with utoipa

### 2.2 Persistence Layer ✅
- [x] Create Storage trait abstraction
- [x] Implement JsonFileStorage (read/write flows.json)
- [x] Add error handling for file operations
- [x] Create example flows.json for testing
- [x] Add configuration for storage path

### 2.3 State Management ✅
- [x] Implement shared application state (Arc<RwLock<AppState>>)
- [x] Create FlowManager to hold runtime pipeline state
- [x] Add methods for CRUD operations on flows
- [x] Implement state synchronization between storage and memory

## Phase 3: REST API ✅ COMPLETE

### 3.1 Flow Endpoints ✅
- [x] `GET /api/flows` - List all flows
- [x] `GET /api/flows/:id` - Get flow details
- [x] `POST /api/flows` - Create new flow
- [x] `POST /api/flows/:id` - Update flow
- [x] `DELETE /api/flows/:id` - Delete flow
- [x] Add request validation
- [x] Add proper error responses
- [x] Add OpenAPI documentation with utoipa

### 3.2 Flow Control Endpoints ✅
- [x] `POST /api/flows/:id/start` - Start pipeline
- [x] `POST /api/flows/:id/stop` - Stop pipeline
- [x] `GET /api/flows/:id/state` - Get current pipeline state (via flow object)
- [x] Add proper state transition validation
- [x] `GET /api/flows/:id/debug-graph` - Generate pipeline debug visualization

### 3.3 GStreamer Element Discovery ✅
- [x] `GET /api/elements` - List available GStreamer elements
- [x] `GET /api/elements/:name` - Get element details (pads, properties)
- [x] Cache element information for performance
- [x] Add filtering/search capabilities

## Phase 4: WebSocket API

### 4.1 WebSocket Infrastructure
- [ ] Set up WebSocket endpoint (/ws)
- [ ] Implement connection handling
- [ ] Create message protocol (JSON-based)
- [ ] Add client connection management
- [ ] Implement broadcast mechanism for state updates

### 4.2 Real-time Updates
- [ ] Send pipeline state changes to connected clients
- [ ] Send element property updates
- [ ] Send error/warning messages
- [ ] Implement heartbeat/ping-pong for connection health
- [ ] Add reconnection logic on frontend

## Phase 5: GStreamer Integration ✅ COMPLETE

### 5.1 Basic Pipeline Management ✅
- [x] Initialize GStreamer (gst::init)
- [x] Create pipeline from Flow definition
- [x] Implement element creation from type string
- [x] Set element properties from configuration
- [x] Link elements based on Link definitions

### 5.2 Pipeline Lifecycle ✅
- [x] Start pipeline (set to PLAYING state)
- [x] Stop pipeline (set to NULL state)
- [x] Pause/resume pipeline
- [x] Handle pipeline state transitions
- [x] Clean up resources on pipeline destruction

### 5.3 Element Introspection ✅
- [x] Query available GStreamer elements using ElementFactory
- [x] Get element pads (src/sink) and capabilities
- [x] Get element properties with types and defaults
- [x] Validate pad compatibility for linking
- [x] Generate element metadata for frontend

### 5.4 Error Handling & Bus Messages 🔄 IN PROGRESS
- [ ] Set up GStreamer bus message handling
- [ ] Handle EOS (End of Stream) messages
- [ ] Handle error messages with details
- [ ] Handle warning messages
- [ ] Handle state-change messages
- [ ] Forward relevant messages via WebSocket

### 5.5 Auto-start on Server Boot ✅
- [x] Load flows from storage on startup
- [x] Reconstruct pipelines for all flows
- [x] Auto-start flows with auto_start=true
- [x] Log startup progress and errors

### 5.6 Pipeline Debugging ✅ NEW
- [x] Generate DOT graph representation of pipeline
- [x] Convert DOT to SVG using Graphviz
- [x] Serve SVG via HTTP endpoint
- [x] Frontend integration for viewing debug graphs

## Phase 6: Frontend - API Client ✅ COMPLETE

### 6.1 HTTP Client ✅
- [x] Create REST API client module
- [x] Implement flow CRUD operations
- [x] Implement flow control operations (start/stop)
- [x] Fetch element catalog
- [x] Add error handling and retries
- [x] Debug graph URL generation

### 6.2 WebSocket Client 🔄 DEFERRED
- [ ] Connect to WebSocket on app initialization
- [ ] Parse incoming messages
- [ ] Update local state based on messages
- [ ] Implement reconnection logic
- [ ] Handle connection errors gracefully
Note: WebSocket support deferred to Phase 4 implementation

## Phase 7: Frontend - Flow Editor UI ✅ COMPLETE

### 7.1 Basic Layout ✅
- [x] Create main application layout (sidebar + canvas)
- [x] Add top toolbar with actions (new flow, save, etc.)
- [x] Implement flow list panel
- [x] Add flow selection and navigation
- [x] Create status bar with connection indicator

### 7.2 Node Graph Editor ✅
- [x] Integrate or implement node-based graph editor (custom implementation)
- [x] Render elements as nodes with input/output pads
- [x] Implement drag-and-drop for nodes
- [x] Visual linking between pads (drag from src to sink)
- [x] Delete nodes and connections (Delete key)
- [x] Pan and zoom canvas (scroll wheel + drag)

### 7.3 Element Palette ✅
- [x] Display available GStreamer elements in sidebar
- [x] Categorize elements (sources, filters, sinks, etc.)
- [x] Implement search/filter for elements
- [x] Drag elements from palette onto canvas
- [x] Show element descriptions/tooltips
- [x] Pre-loaded with common GStreamer elements

### 7.4 Property Inspector ✅
- [x] Show selected element properties
- [x] Render appropriate input widgets (text, number, bool, enum)
- [x] Update properties in real-time
- [x] Validate property values
- [x] Show property descriptions and defaults
- [x] Support for custom properties

### 7.5 Flow Controls ✅
- [x] Add start/stop buttons for current flow
- [x] Show current pipeline state (NULL, READY, PAUSED, PLAYING)
- [x] Display visual state indicator (color-coded)
- [x] Show auto-start toggle
- [x] Add flow name editor
- [x] Debug graph button to visualize pipeline

### 7.6 Flow Management ✅
- [x] Create new flow dialog
- [x] Delete flow with confirmation
- [ ] Duplicate flow
- [x] Save flow (persist to backend)
- [ ] Show unsaved changes indicator

## Phase 8: Validation & Error Handling

### 8.1 Flow Validation
- [ ] Validate element types exist in GStreamer
- [ ] Check pad compatibility before linking
- [ ] Ensure no disconnected elements (warn user)
- [ ] Validate property values before setting
- [ ] Show validation errors in UI

### 8.2 Error Display
- [ ] Show GStreamer errors in UI with context
- [ ] Display API errors (network, server errors)
- [ ] Add error notification system (toasts/snackbars)
- [ ] Log errors to console for debugging
- [ ] Provide helpful error messages

### 8.3 Robust State Handling
- [ ] Handle backend disconnection gracefully
- [ ] Prevent invalid state transitions
- [ ] Sync frontend state with backend on reconnect
- [ ] Handle concurrent modifications (if multi-user)

## Phase 9: Polish & UX

### 9.1 Visual Design
- [ ] Choose color scheme and theme
- [ ] Design element node appearance
- [ ] Style property inspector
- [ ] Add icons for elements and actions
- [ ] Ensure responsive layout

### 9.2 User Experience
- [ ] Add keyboard shortcuts (save, delete, copy, paste)
- [ ] Implement undo/redo for graph edits
- [ ] Add helpful empty states (no flows, no elements)
- [ ] Show loading indicators for async operations
- [ ] Add confirmation dialogs for destructive actions

### 9.3 Documentation
- [ ] Write API documentation
- [ ] Add inline code documentation
- [ ] Create user guide for UI
- [ ] Add example flows with explanations
- [ ] Document common GStreamer patterns

## Phase 10: Advanced Features (Future)

### 10.1 Flow Templates
- [ ] Create template system for common patterns
- [ ] Add built-in templates (RTSP recorder, HLS streamer, etc.)
- [ ] Allow saving custom templates
- [ ] Template import/export

### 10.2 Monitoring & Statistics
- [ ] Display pipeline performance metrics
- [ ] Show element statistics (buffers, bytes, fps)
- [ ] Add live preview for video/audio (if feasible)
- [ ] Create dashboard view with multiple flow statuses

### 10.3 Database Backend
- [ ] Define database schema
- [ ] Implement SqliteStorage backend
- [ ] Add migration system
- [ ] Implement PostgresStorage (optional)
- [ ] Add configuration for database connection

### 10.4 Multi-user Support
- [ ] Add authentication system
- [ ] Implement user sessions
- [ ] Add authorization (flow ownership)
- [ ] Handle concurrent editing

### 10.5 Advanced GStreamer Features
- [ ] Support for dynamic pads
- [ ] Handle complex pad negotiation
- [ ] Add support for bins and sub-pipelines
- [ ] Implement pipeline debugging output
- [ ] Add GStreamer plugin path configuration

### 10.6 Deployment
- [ ] Create Docker container
- [ ] Add production configuration
- [ ] Set up systemd service file
- [ ] Document deployment process
- [ ] Add backup/restore functionality

## Known Issues & Technical Debt

### High CPU Usage from Tokio Worker Threads
**Issue**: Backend shows ~40-100% CPU usage on tokio-runtime-w threads even when idle
- Observed with 3 tokio worker threads constantly active
- Not caused by bus watch (already optimized to only run when pipeline is active)
- Not caused by SSE keep-alive (15 second interval)
- Likely related to GStreamer's GLib main loop integration with Tokio
- **Impact**: High baseline CPU usage even with no pipelines running
- **Possible Solutions**:
  - Investigate GStreamer + Tokio integration settings
  - Consider moving GStreamer operations to separate thread pool
  - Profile to identify exact polling source
  - May need to accept as GStreamer overhead

## Immediate Next Steps (Priority Order)

1. ✅ Create Cargo workspace structure with three crates
2. ✅ Set up `strom-types` library with core domain models
3. ✅ Set up backend with axum and basic endpoints (depends on types)
4. ✅ Set up frontend with egui and basic UI (depends on types)
5. ✅ Implement JSON persistence layer in backend
6. ✅ Create basic REST API for flows
7. ✅ Initialize GStreamer and test pipeline creation
8. ✅ Build minimal flow editor with manual JSON input
9. ✅ Integrate GStreamer pipeline execution
10. ✅ Remove auto_start behavior, persist running state instead

**Key architectural principle**: The `strom-types` crate is the foundation - both frontend and backend depend on it for shared types and API contracts.

---

## Development Workflow

1. Start with backend infrastructure (server, API, storage)
2. Add GStreamer integration (element discovery, pipeline management)
3. Build frontend UI incrementally (start with forms, then graph editor)
4. Connect frontend to backend via API
5. Test with real GStreamer pipelines
6. Polish UX and add validation
7. Add advanced features iteratively

## Testing Strategy

- **Unit tests**: Core logic (flow validation, element linking)
- **Integration tests**: API endpoints, storage layer
- **Manual testing**: GStreamer pipelines, UI interactions
- **Example flows**: Test with real-world GStreamer use cases
