//! Main application structure.

use egui::{CentralPanel, Color32, Context, SidePanel, TopBottomPanel};
use strom_types::{Flow, PipelineState};

use crate::api::{ApiClient, AuthStatusResponse};
use crate::compositor_editor::CompositorEditor;
use crate::graph::GraphEditor;
use crate::logging::{LogEntry, LogLevel};
use crate::login::LoginScreen;
use crate::mediaplayer::{MediaPlayerDataStore, PlaylistEditor};
use crate::meter::MeterDataStore;
use crate::palette::ElementPalette;
use crate::properties::PropertyInspector;
use crate::state::{AppMessage, AppStateChannels, ConnectionState};
use crate::system_monitor::SystemMonitorStore;
use crate::utils::spawn_task;
use crate::utils::{download_file, generate_vlc_playlist, set_local_storage};
use crate::webrtc_stats::WebRtcStatsStore;
use crate::ws::WebSocketClient;

/// Theme preference for the application
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemePreference {
    System,
    Light,
    Dark,
}

/// Import format for flow import
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ImportFormat {
    /// JSON format (full flow definition)
    #[default]
    Json,
    /// gst-launch-1.0 pipeline syntax
    GstLaunch,
}

/// Application page/section
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppPage {
    /// Flow editor (default view)
    #[default]
    Flows,
    /// SAP/AES67 stream discovery
    Discovery,
    /// PTP clock monitoring
    Clocks,
    /// Media file browser
    Media,
    /// System and version information
    Info,
}

/// Focus target for Ctrl+F cycling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FocusTarget {
    /// No specific focus target
    #[default]
    None,
    /// Flow list filter (Flows page)
    FlowFilter,
    /// Elements palette search (Flows page)
    PaletteElements,
    /// Blocks palette search (Flows page)
    PaletteBlocks,
    /// Discovery search filter (Discovery page)
    DiscoveryFilter,
    /// Media search filter (Media page)
    MediaFilter,
}

/// The main Strom application.
pub struct StromApp {
    /// API client for backend communication
    pub(crate) api: ApiClient,
    /// List of all flows
    pub(crate) flows: Vec<Flow>,
    /// Currently selected flow ID (using ID instead of index for robustness)
    pub(crate) selected_flow_id: Option<strom_types::FlowId>,
    /// Graph editor for the current flow
    pub(crate) graph: GraphEditor,
    /// Element palette
    pub(crate) palette: ElementPalette,
    /// Status message
    pub(crate) status: String,
    /// Error message
    pub(crate) error: Option<String>,
    /// Loading state
    pub(crate) loading: bool,
    /// Whether flow list needs refresh
    pub(crate) needs_refresh: bool,
    /// New flow name input
    pub(crate) new_flow_name: String,
    /// Show new flow dialog
    pub(crate) show_new_flow_dialog: bool,
    /// Whether elements have been loaded
    pub(crate) elements_loaded: bool,
    /// Whether blocks have been loaded
    pub(crate) blocks_loaded: bool,
    /// Flow pending deletion (for confirmation dialog)
    pub(crate) flow_pending_deletion: Option<(strom_types::FlowId, String)>,
    /// Flow pending copy (to be processed after render)
    pub(crate) flow_pending_copy: Option<Flow>,
    /// Flow ID to navigate to after next refresh
    pub(crate) pending_flow_navigation: Option<strom_types::FlowId>,
    /// WebSocket client for real-time updates
    pub(crate) ws_client: Option<WebSocketClient>,
    /// Connection state
    pub(crate) connection_state: ConnectionState,
    /// Channel-based state management
    pub(crate) channels: AppStateChannels,
    /// Flow properties being edited (flow ID)
    pub(crate) editing_properties_flow_id: Option<strom_types::FlowId>,
    /// Temporary name buffer for properties dialog
    pub(crate) properties_name_buffer: String,
    /// Temporary description buffer for properties dialog
    pub(crate) properties_description_buffer: String,
    /// Temporary clock type for properties dialog
    pub(crate) properties_clock_type_buffer: strom_types::flow::GStreamerClockType,
    /// Temporary PTP domain buffer for properties dialog
    pub(crate) properties_ptp_domain_buffer: String,
    /// Temporary thread priority for properties dialog
    pub(crate) properties_thread_priority_buffer: strom_types::flow::ThreadPriority,
    /// Shutdown flag for Ctrl+C handling (native mode only)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) shutdown_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Port number for backend connection (native mode only)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) port: u16,
    /// Auth token for native GUI authentication
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) auth_token: Option<String>,
    /// Cached network interfaces (for network interface property dropdown)
    pub(crate) network_interfaces: Vec<strom_types::NetworkInterfaceInfo>,
    /// Whether network interfaces have been loaded
    pub(crate) network_interfaces_loaded: bool,
    /// Cached available inter channels (for InterInput channel dropdown)
    pub(crate) available_channels: Vec<strom_types::api::AvailableOutput>,
    /// Whether available channels have been loaded
    pub(crate) available_channels_loaded: bool,
    /// Last InterInput block ID we refreshed channels for (to avoid repeated refreshes)
    pub(crate) last_inter_input_refresh: Option<String>,
    /// Meter data storage for all audio level meters
    pub(crate) meter_data: MeterDataStore,
    /// Media player data storage for all media player blocks
    pub(crate) mediaplayer_data: MediaPlayerDataStore,
    /// WebRTC stats storage for all WebRTC connections
    pub(crate) webrtc_stats: WebRtcStatsStore,
    /// System monitoring statistics
    pub(crate) system_monitor: SystemMonitorStore,
    /// PTP clock statistics per flow
    pub(crate) ptp_stats: crate::ptp_monitor::PtpStatsStore,
    /// QoS (buffer drop) statistics per flow/element
    pub(crate) qos_stats: crate::qos_monitor::QoSStore,
    /// Track when flows started (for QoS grace period)
    pub(crate) flow_start_times: std::collections::HashMap<strom_types::FlowId, instant::Instant>,
    /// Whether to show the detailed system monitor window
    pub(crate) show_system_monitor: bool,
    /// Last time WebRTC stats were polled
    pub(crate) last_webrtc_poll: instant::Instant,
    /// Current theme preference
    pub(crate) theme_preference: ThemePreference,
    /// Version information from the backend
    pub(crate) version_info: Option<crate::api::VersionInfo>,
    /// Login screen
    pub(crate) login_screen: LoginScreen,
    /// Authentication status
    pub(crate) auth_status: Option<AuthStatusResponse>,
    /// Whether we're checking auth status
    pub(crate) checking_auth: bool,
    /// Show import flow dialog
    pub(crate) show_import_dialog: bool,
    /// Import format mode (JSON or gst-launch)
    pub(crate) import_format: ImportFormat,
    /// Buffer for import text (JSON or gst-launch pipeline)
    pub(crate) import_json_buffer: String,
    /// Error message for import dialog
    pub(crate) import_error: Option<String>,
    /// Pending gst-launch export (elements, links, flow_name) - for async processing
    pub(crate) pending_gst_launch_export: Option<(
        Vec<strom_types::Element>,
        Vec<strom_types::element::Link>,
        String,
    )>,
    /// Cached latency info for flows (flow_id -> LatencyInfo)
    pub(crate) latency_cache: std::collections::HashMap<String, crate::api::LatencyInfo>,
    /// Last time latency was fetched (for periodic refresh)
    pub(crate) last_latency_fetch: instant::Instant,
    /// Cached stats info for flows (flow_id -> FlowStatsInfo)
    pub(crate) stats_cache: std::collections::HashMap<String, crate::api::FlowStatsInfo>,
    /// Last time stats was fetched (for periodic refresh)
    pub(crate) last_stats_fetch: instant::Instant,
    /// Whether to show the stats panel
    pub(crate) show_stats_panel: bool,
    /// Compositor layout editor (if open)
    pub(crate) compositor_editor: Option<CompositorEditor>,
    /// Playlist editor (if open)
    pub(crate) playlist_editor: Option<PlaylistEditor>,
    /// Log entries for pipeline messages (errors, warnings, info)
    pub(crate) log_entries: Vec<LogEntry>,
    /// Whether to show the log panel
    pub(crate) show_log_panel: bool,
    /// Maximum number of log entries to keep
    pub(crate) max_log_entries: usize,
    /// Current application page
    pub(crate) current_page: AppPage,
    /// Discovery page state
    pub(crate) discovery_page: crate::discovery::DiscoveryPage,
    /// Clocks page state (PTP monitoring)
    pub(crate) clocks_page: crate::clocks::ClocksPage,
    /// Media file browser page state
    pub(crate) media_page: crate::media::MediaPage,
    /// Info page state
    pub(crate) info_page: crate::info_page::InfoPage,
    /// Flow list filter text
    pub(crate) flow_filter: String,
    /// Show stream picker modal for this block ID (when browsing discovered streams for AES67 Input)
    pub(crate) show_stream_picker_for_block: Option<String>,
    /// Current focus target for Ctrl+F cycling
    pub(crate) focus_target: FocusTarget,
    /// Request to focus the flow filter on next frame
    pub(crate) focus_flow_filter_requested: bool,
}

impl StromApp {
    /// Create a new application instance.
    /// For WASM, the port parameter is ignored (URL is detected from browser location).
    #[cfg(target_arch = "wasm32")]
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Note: Dark theme is set in main.rs before creating the app

        // Detect API base URL from browser location
        let api_base_url = {
            if let Some(window) = web_sys::window() {
                if let Ok(host) = window.location().host() {
                    let protocol = window
                        .location()
                        .protocol()
                        .unwrap_or_else(|_| "http:".to_string());

                    // Exception: trunk serve runs on :8095, backend on :8080
                    if host == "localhost:8095" || host == "127.0.0.1:8095" {
                        "http://localhost:8080/api".to_string()
                    } else {
                        // Use current window location (works for Docker, production, etc.)
                        format!("{}//{}/api", protocol, host)
                    }
                } else {
                    "http://localhost:8080/api".to_string()
                }
            } else {
                "http://localhost:8080/api".to_string()
            }
        };

        Self::new_internal(cc, api_base_url, None)
    }

    /// Create a new application instance for native mode.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(cc: &eframe::CreationContext<'_>, port: u16) -> Self {
        let api_base_url = format!("http://localhost:{}/api", port);
        Self::new_internal(cc, api_base_url, None, port, None)
    }

    /// Internal constructor shared by all creation methods (WASM version).
    #[cfg(target_arch = "wasm32")]
    fn new_internal(
        cc: &eframe::CreationContext<'_>,
        api_base_url: String,
        _shutdown_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        // Create channels for async communication
        let channels = AppStateChannels::new();

        let mut app = Self {
            api: ApiClient::new(&api_base_url),
            flows: Vec::new(),
            selected_flow_id: None,
            graph: GraphEditor::new(),
            palette: ElementPalette::new(),
            status: "Ready".to_string(),
            error: None,
            loading: false,
            needs_refresh: true,
            new_flow_name: String::new(),
            show_new_flow_dialog: false,
            elements_loaded: false,
            blocks_loaded: false,
            flow_pending_deletion: None,
            flow_pending_copy: None,
            pending_flow_navigation: None,
            ws_client: None,
            connection_state: ConnectionState::Disconnected,
            channels,
            editing_properties_flow_id: None,
            properties_name_buffer: String::new(),
            properties_description_buffer: String::new(),
            properties_clock_type_buffer: strom_types::flow::GStreamerClockType::Monotonic,
            properties_ptp_domain_buffer: String::new(),
            properties_thread_priority_buffer: strom_types::flow::ThreadPriority::High,
            meter_data: MeterDataStore::new(),
            mediaplayer_data: MediaPlayerDataStore::new(),
            webrtc_stats: WebRtcStatsStore::new(),
            system_monitor: SystemMonitorStore::new(),
            ptp_stats: crate::ptp_monitor::PtpStatsStore::new(),
            qos_stats: crate::qos_monitor::QoSStore::new(),
            flow_start_times: std::collections::HashMap::new(),
            show_system_monitor: false,
            last_webrtc_poll: instant::Instant::now(),
            theme_preference: ThemePreference::Dark,
            version_info: None,
            login_screen: LoginScreen::default(),
            auth_status: None,
            checking_auth: false,
            show_import_dialog: false,
            import_format: ImportFormat::default(),
            import_json_buffer: String::new(),
            import_error: None,
            pending_gst_launch_export: None,
            latency_cache: std::collections::HashMap::new(),
            last_latency_fetch: instant::Instant::now(),
            stats_cache: std::collections::HashMap::new(),
            last_stats_fetch: instant::Instant::now(),
            show_stats_panel: false,
            compositor_editor: None,
            playlist_editor: None,
            network_interfaces: Vec::new(),
            network_interfaces_loaded: false,
            available_channels: Vec::new(),
            available_channels_loaded: false,
            last_inter_input_refresh: None,
            log_entries: Vec::new(),
            show_log_panel: false,
            max_log_entries: 100,
            current_page: AppPage::default(),
            discovery_page: crate::discovery::DiscoveryPage::new(),
            clocks_page: crate::clocks::ClocksPage::new(),
            media_page: crate::media::MediaPage::new(),
            info_page: crate::info_page::InfoPage::new(),
            flow_filter: String::new(),
            show_stream_picker_for_block: None,
            focus_target: FocusTarget::None,
            focus_flow_filter_requested: false,
        };

        // Apply initial theme based on system preference
        app.apply_theme(cc.egui_ctx.clone());

        // Load default elements temporarily (will be replaced by API data)
        app.palette.load_default_elements();

        // Check authentication status first
        app.check_auth_status(cc.egui_ctx.clone());

        app
    }

    /// Internal constructor shared by all creation methods (native version).
    #[cfg(not(target_arch = "wasm32"))]
    fn new_internal(
        cc: &eframe::CreationContext<'_>,
        api_base_url: String,
        shutdown_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        port: u16,
        auth_token: Option<String>,
    ) -> Self {
        // Create channels for async communication
        let channels = AppStateChannels::new();

        let mut app = Self {
            api: ApiClient::new_with_auth(&api_base_url, auth_token.clone()),
            flows: Vec::new(),
            selected_flow_id: None,
            graph: GraphEditor::new(),
            palette: ElementPalette::new(),
            status: "Ready".to_string(),
            error: None,
            loading: false,
            needs_refresh: true,
            new_flow_name: String::new(),
            show_new_flow_dialog: false,
            elements_loaded: false,
            blocks_loaded: false,
            flow_pending_deletion: None,
            flow_pending_copy: None,
            pending_flow_navigation: None,
            ws_client: None,
            connection_state: ConnectionState::Disconnected,
            channels,
            editing_properties_flow_id: None,
            properties_name_buffer: String::new(),
            properties_description_buffer: String::new(),
            properties_clock_type_buffer: strom_types::flow::GStreamerClockType::Monotonic,
            properties_ptp_domain_buffer: String::new(),
            properties_thread_priority_buffer: strom_types::flow::ThreadPriority::High,
            shutdown_flag,
            port,
            auth_token,
            meter_data: MeterDataStore::new(),
            mediaplayer_data: MediaPlayerDataStore::new(),
            webrtc_stats: WebRtcStatsStore::new(),
            system_monitor: SystemMonitorStore::new(),
            ptp_stats: crate::ptp_monitor::PtpStatsStore::new(),
            qos_stats: crate::qos_monitor::QoSStore::new(),
            flow_start_times: std::collections::HashMap::new(),
            show_system_monitor: false,
            last_webrtc_poll: instant::Instant::now(),
            theme_preference: ThemePreference::Dark,
            version_info: None,
            login_screen: LoginScreen::default(),
            auth_status: None,
            checking_auth: false,
            show_import_dialog: false,
            import_format: ImportFormat::default(),
            import_json_buffer: String::new(),
            import_error: None,
            pending_gst_launch_export: None,
            latency_cache: std::collections::HashMap::new(),
            last_latency_fetch: instant::Instant::now(),
            stats_cache: std::collections::HashMap::new(),
            last_stats_fetch: instant::Instant::now(),
            show_stats_panel: false,
            compositor_editor: None,
            playlist_editor: None,
            network_interfaces: Vec::new(),
            network_interfaces_loaded: false,
            available_channels: Vec::new(),
            available_channels_loaded: false,
            last_inter_input_refresh: None,
            log_entries: Vec::new(),
            show_log_panel: false,
            max_log_entries: 100,
            current_page: AppPage::default(),
            discovery_page: crate::discovery::DiscoveryPage::new(),
            clocks_page: crate::clocks::ClocksPage::new(),
            media_page: crate::media::MediaPage::new(),
            info_page: crate::info_page::InfoPage::new(),
            flow_filter: String::new(),
            show_stream_picker_for_block: None,
            focus_target: FocusTarget::None,
            focus_flow_filter_requested: false,
        };

        // Apply initial theme based on system preference
        app.apply_theme(cc.egui_ctx.clone());

        // Load default elements temporarily (will be replaced by API data)
        app.palette.load_default_elements();

        // Set up WebSocket connection for real-time updates
        app.setup_websocket_connection(cc.egui_ctx.clone());

        // Load version info
        app.load_version(cc.egui_ctx.clone());

        app
    }

    /// Create a new application instance with shutdown handler (native mode only).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new_with_shutdown(
        cc: &eframe::CreationContext<'_>,
        port: u16,
        shutdown_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let api_base_url = format!("http://localhost:{}/api", port);
        Self::new_internal(cc, api_base_url, Some(shutdown_flag), port, None)
    }

    /// Create a new application instance with shutdown handler and auth token (native mode only).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new_with_shutdown_and_auth(
        cc: &eframe::CreationContext<'_>,
        port: u16,
        shutdown_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        auth_token: Option<String>,
    ) -> Self {
        let api_base_url = format!("http://localhost:{}/api", port);
        Self::new_internal(cc, api_base_url, Some(shutdown_flag), port, auth_token)
    }

    /// Apply the current theme preference to the UI context.
    fn apply_theme(&self, ctx: egui::Context) {
        let visuals = match self.theme_preference {
            ThemePreference::System => {
                // Detect system theme preference
                #[cfg(target_arch = "wasm32")]
                {
                    // In WASM, check browser's preferred color scheme
                    if let Some(window) = web_sys::window() {
                        if let Ok(Some(mql)) = window.match_media("(prefers-color-scheme: dark)") {
                            if mql.matches() {
                                egui::Visuals::dark()
                            } else {
                                egui::Visuals::light()
                            }
                        } else {
                            egui::Visuals::dark() // Default to dark if detection fails
                        }
                    } else {
                        egui::Visuals::dark() // Default to dark if no window
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    // In native mode, default to dark theme (could be enhanced to detect OS theme)
                    egui::Visuals::dark()
                }
            }
            ThemePreference::Light => egui::Visuals::light(),
            ThemePreference::Dark => egui::Visuals::dark(),
        };
        ctx.set_visuals(visuals);
    }

    /// Set up WebSocket connection for real-time updates.
    pub(crate) fn setup_websocket_connection(&mut self, ctx: egui::Context) {
        tracing::info!("Setting up WebSocket connection for real-time updates");

        // WebSocket URL - different logic for WASM vs native
        #[cfg(target_arch = "wasm32")]
        let ws_url = {
            if let Some(window) = web_sys::window() {
                if let Ok(host) = window.location().host() {
                    // Exception: trunk serve runs on :8095, backend on :8080
                    if host == "localhost:8095" || host == "127.0.0.1:8095" {
                        "ws://localhost:8080/api/ws".to_string()
                    } else {
                        // Use current window location - ws:// or wss:// based on protocol
                        let ws_protocol =
                            if window.location().protocol().ok().as_deref() == Some("https:") {
                                "wss"
                            } else {
                                "ws"
                            };
                        format!("{}://{}/api/ws", ws_protocol, host)
                    }
                } else {
                    "/api/ws".to_string()
                }
            } else {
                "/api/ws".to_string()
            }
        };

        #[cfg(not(target_arch = "wasm32"))]
        let ws_url = format!("ws://localhost:{}/api/ws", self.port);

        tracing::info!("Connecting WebSocket to: {}", ws_url);

        // Create WebSocket client with auth token if available
        #[cfg(not(target_arch = "wasm32"))]
        let mut ws_client = WebSocketClient::new_with_auth(ws_url, self.auth_token.clone());

        #[cfg(target_arch = "wasm32")]
        let mut ws_client = WebSocketClient::new(ws_url);

        // Connect the WebSocket with the channel sender
        ws_client.connect(self.channels.sender(), ctx);

        // Store the WebSocket client to keep the connection alive
        self.ws_client = Some(ws_client);
    }

    /// Get the currently selected flow.
    pub(crate) fn current_flow(&self) -> Option<&Flow> {
        self.selected_flow_id
            .and_then(|id| self.flows.iter().find(|f| f.id == id))
    }

    /// Get the currently selected flow mutably.
    fn current_flow_mut(&mut self) -> Option<&mut Flow> {
        self.selected_flow_id
            .and_then(|id| self.flows.iter_mut().find(|f| f.id == id))
    }

    /// Get the index of the currently selected flow (for UI rendering).
    fn selected_flow_index(&self) -> Option<usize> {
        self.selected_flow_id
            .and_then(|id| self.flows.iter().position(|f| f.id == id))
    }

    /// Select a flow by ID.
    fn select_flow(&mut self, flow_id: strom_types::FlowId) {
        if let Some(flow) = self.flows.iter().find(|f| f.id == flow_id) {
            self.selected_flow_id = Some(flow_id);
            self.graph.deselect_all();
            self.graph.load(flow.elements.clone(), flow.links.clone());
            self.graph.load_blocks(flow.blocks.clone());
            tracing::info!("Selected flow: {} ({})", flow.name, flow_id);
        } else {
            tracing::warn!("Cannot select flow {}: not found", flow_id);
        }
    }

    /// Clear the current flow selection.
    pub(crate) fn clear_flow_selection(&mut self) {
        self.selected_flow_id = None;
        self.graph.load(vec![], vec![]);
        self.graph.load_blocks(vec![]);
    }

    /// Add a log entry, maintaining the maximum size limit.
    pub(crate) fn add_log_entry(&mut self, entry: LogEntry) {
        self.log_entries.push(entry);
        // Trim to max size
        while self.log_entries.len() > self.max_log_entries {
            self.log_entries.remove(0);
        }
    }

    /// Clear all log entries.
    fn clear_log_entries(&mut self) {
        self.log_entries.clear();
        self.error = None;
    }

    /// Get log entry counts by level.
    fn log_counts(&self) -> (usize, usize, usize) {
        let errors = self
            .log_entries
            .iter()
            .filter(|e| e.level == LogLevel::Error)
            .count();
        let warnings = self
            .log_entries
            .iter()
            .filter(|e| e.level == LogLevel::Warning)
            .count();
        let infos = self
            .log_entries
            .iter()
            .filter(|e| e.level == LogLevel::Info)
            .count();
        (errors, warnings, infos)
    }

    /// Load GStreamer elements from the backend.
    pub(crate) fn load_elements(&mut self, ctx: &Context) {
        tracing::info!("Starting to load GStreamer elements...");
        self.status = "Loading elements...".to_string();

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.list_elements().await {
                Ok(elements) => {
                    tracing::info!("Successfully fetched {} elements", elements.len());
                    let _ = tx.send(AppMessage::ElementsLoaded(elements));
                }
                Err(e) => {
                    tracing::error!("Failed to load elements: {}", e);
                    let _ = tx.send(AppMessage::ElementsError(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load blocks from the backend.
    pub(crate) fn load_blocks(&mut self, ctx: &Context) {
        tracing::info!("Starting to load blocks...");
        self.status = "Loading blocks...".to_string();

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.list_blocks().await {
                Ok(blocks) => {
                    tracing::info!("Successfully fetched {} blocks", blocks.len());
                    let _ = tx.send(AppMessage::BlocksLoaded(blocks));
                }
                Err(e) => {
                    tracing::error!("Failed to load blocks: {}", e);
                    let _ = tx.send(AppMessage::BlocksError(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load version information from the backend.
    pub(crate) fn load_version(&mut self, ctx: egui::Context) {
        tracing::info!("Loading version information from backend...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_version().await {
                Ok(version_info) => {
                    tracing::info!(
                        "Successfully loaded version: v{} ({})",
                        version_info.version,
                        version_info.git_hash
                    );
                    let _ = tx.send(AppMessage::VersionLoaded(version_info));
                }
                Err(e) => {
                    tracing::warn!("Failed to load version info: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load network interfaces from the backend (for network interface property dropdown).
    pub(crate) fn load_network_interfaces(&mut self, ctx: egui::Context) {
        if self.network_interfaces_loaded {
            return;
        }
        self.network_interfaces_loaded = true; // Prevent multiple concurrent requests
        tracing::info!("Loading network interfaces from backend...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.list_network_interfaces().await {
                Ok(response) => {
                    tracing::info!(
                        "Successfully loaded {} network interfaces",
                        response.interfaces.len()
                    );
                    let _ = tx.send(AppMessage::NetworkInterfacesLoaded(response.interfaces));
                }
                Err(e) => {
                    tracing::warn!("Failed to load network interfaces: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Get cached network interfaces (for property inspector).
    pub fn network_interfaces(&self) -> &[strom_types::NetworkInterfaceInfo] {
        &self.network_interfaces
    }

    /// Load available inter channels from the backend (for InterInput channel dropdown).
    fn load_available_channels(&mut self, ctx: egui::Context) {
        if self.available_channels_loaded {
            return;
        }
        self.available_channels_loaded = true; // Prevent multiple concurrent requests
        tracing::info!("Loading available inter channels from backend...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_available_sources().await {
                Ok(response) => {
                    // Flatten all outputs from all source flows
                    let all_channels: Vec<_> = response
                        .sources
                        .into_iter()
                        .flat_map(|source| source.outputs)
                        .collect();
                    tracing::info!(
                        "Successfully loaded {} available inter channels",
                        all_channels.len()
                    );
                    let _ = tx.send(AppMessage::AvailableChannelsLoaded(all_channels));
                }
                Err(e) => {
                    tracing::warn!("Failed to load available channels: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Refresh available channels (called when flow state changes).
    pub fn refresh_available_channels(&mut self) {
        self.available_channels_loaded = false;
    }

    /// Get cached available channels (for property inspector).
    pub fn available_channels(&self) -> &[strom_types::api::AvailableOutput] {
        &self.available_channels
    }

    /// Poll WebRTC stats for running flows that have WebRTC elements.
    /// Called periodically (every second).
    pub(crate) fn poll_webrtc_stats(&mut self, ctx: &Context) {
        // Find running flows
        let running_flows: Vec<_> = self
            .flows
            .iter()
            .filter(|f| matches!(f.state, Some(PipelineState::Playing)))
            .map(|f| f.id)
            .collect();

        for flow_id in running_flows {
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            spawn_task(async move {
                match api.get_webrtc_stats(flow_id).await {
                    Ok(stats) => {
                        if !stats.connections.is_empty() {
                            tracing::debug!(
                                "Fetched WebRTC stats for flow {}: {} connections",
                                flow_id,
                                stats.connections.len()
                            );
                            let _ = tx.send(AppMessage::WebRtcStatsLoaded { flow_id, stats });
                        }
                    }
                    Err(e) => {
                        // Don't log errors for flows without WebRTC elements
                        tracing::trace!("No WebRTC stats for flow {}: {}", flow_id, e);
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Check authentication status
    pub(crate) fn check_auth_status(&mut self, ctx: egui::Context) {
        if self.checking_auth {
            return;
        }

        self.checking_auth = true;
        tracing::info!("Checking authentication status...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.get_auth_status().await {
                Ok(status) => {
                    tracing::info!(
                        "Auth status: required={}, authenticated={}",
                        status.auth_required,
                        status.authenticated
                    );
                    let _ = tx.send(AppMessage::AuthStatusLoaded(status));
                }
                Err(e) => {
                    tracing::warn!("Failed to check auth status: {}", e);
                    // Assume auth is not required if check fails
                    let _ = tx.send(AppMessage::AuthStatusLoaded(AuthStatusResponse {
                        authenticated: true,
                        auth_required: false,
                        methods: vec![],
                    }));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Handle login attempt
    pub(crate) fn handle_login(&mut self, ctx: egui::Context) {
        let username = self.login_screen.username.clone();
        let password = self.login_screen.password.clone();

        if username.is_empty() || password.is_empty() {
            self.login_screen
                .set_error("Username and password are required".to_string());
            return;
        }

        self.login_screen.set_logging_in(true);
        tracing::info!("Attempting login for user: {}", username);

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.login(username, password).await {
                Ok(response) => {
                    tracing::info!("Login response: success={}", response.success);
                    let _ = tx.send(AppMessage::LoginResult(response));
                }
                Err(e) => {
                    tracing::error!("Login failed: {}", e);
                    let _ = tx.send(AppMessage::LoginResult(crate::api::LoginResponse {
                        success: false,
                        message: format!("Login failed: {}", e),
                    }));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Handle logout
    fn handle_logout(&mut self, ctx: egui::Context) {
        tracing::info!("Logging out...");

        let api = self.api.clone();
        let tx = self.channels.sender();

        spawn_task(async move {
            match api.logout().await {
                Ok(_) => {
                    tracing::info!("Logged out successfully");
                    let _ = tx.send(AppMessage::LogoutComplete);
                }
                Err(e) => {
                    tracing::error!("Logout failed: {}", e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load element properties from the backend (lazy loading).
    /// Properties are cached after first load.
    fn load_element_properties(&mut self, element_type: String, ctx: &Context) {
        tracing::info!("Starting to load properties for element: {}", element_type);

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.get_element_info(&element_type).await {
                Ok(element_info) => {
                    tracing::info!(
                        "Successfully fetched properties for '{}' ({} properties)",
                        element_info.name,
                        element_info.properties.len()
                    );
                    let _ = tx.send(AppMessage::ElementPropertiesLoaded(element_info));
                }
                Err(e) => {
                    tracing::error!("Failed to load element properties: {}", e);
                    let _ = tx.send(AppMessage::ElementPropertiesError(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load pad properties from the backend (on-demand lazy loading).
    /// Pad properties are cached separately after first load.
    fn load_element_pad_properties(&mut self, element_type: String, ctx: &Context) {
        tracing::info!(
            "Starting to load pad properties for element: {}",
            element_type
        );

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.get_element_pad_properties(&element_type).await {
                Ok(element_info) => {
                    tracing::info!(
                        "Successfully fetched pad properties for '{}' (sink_pads: {}, src_pads: {})",
                        element_info.name,
                        element_info.sink_pads.iter().map(|p| p.properties.len()).sum::<usize>(),
                        element_info.src_pads.iter().map(|p| p.properties.len()).sum::<usize>()
                    );
                    let _ = tx.send(AppMessage::ElementPadPropertiesLoaded(element_info));
                }
                Err(e) => {
                    tracing::error!("Failed to load pad properties: {}", e);
                    let _ = tx.send(AppMessage::ElementPadPropertiesError(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Load flows from the backend.
    pub(crate) fn load_flows(&mut self, ctx: &Context) {
        if self.loading {
            return;
        }

        tracing::info!("Starting to load flows...");
        self.loading = true;
        self.status = "Loading flows...".to_string();
        self.error = None;

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        spawn_task(async move {
            match api.list_flows().await {
                Ok(flows) => {
                    tracing::info!("Successfully fetched {} flows", flows.len());
                    let _ = tx.send(AppMessage::FlowsLoaded(flows));
                }
                Err(e) => {
                    tracing::error!("Failed to load flows: {}", e);
                    let _ = tx.send(AppMessage::FlowsError(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Fetch latency for all running flows.
    pub(crate) fn fetch_latency_for_running_flows(&self, ctx: &Context) {
        use strom_types::PipelineState;

        // Find all flows that are currently playing
        let running_flows: Vec<_> = self
            .flows
            .iter()
            .filter(|f| f.state == Some(PipelineState::Playing))
            .map(|f| f.id)
            .collect();

        if running_flows.is_empty() {
            return;
        }

        // Fetch latency for each running flow
        for flow_id in running_flows {
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();
            let flow_id_str = flow_id.to_string();

            spawn_task(async move {
                match api.get_flow_latency(flow_id).await {
                    Ok(latency) => {
                        let _ = tx.send(AppMessage::LatencyLoaded {
                            flow_id: flow_id_str,
                            latency,
                        });
                    }
                    Err(_) => {
                        // Flow not running or latency not available - silently ignore
                        let _ = tx.send(AppMessage::LatencyNotAvailable(flow_id_str));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Fetch statistics for all running flows.
    pub(crate) fn fetch_stats_for_running_flows(&self, ctx: &Context) {
        use strom_types::PipelineState;

        // Find all flows that are currently playing
        let running_flows: Vec<_> = self
            .flows
            .iter()
            .filter(|f| f.state == Some(PipelineState::Playing))
            .map(|f| f.id)
            .collect();

        if running_flows.is_empty() {
            return;
        }

        // Get the currently selected flow ID for dynamic pads fetching
        let selected_flow_id = self.current_flow().map(|f| f.id);

        // Fetch stats for each running flow
        for flow_id in running_flows {
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();
            let flow_id_str = flow_id.to_string();
            let fetch_dynamic_pads = selected_flow_id == Some(flow_id);

            spawn_task(async move {
                match api.get_flow_stats(flow_id).await {
                    Ok(stats) => {
                        let _ = tx.send(AppMessage::StatsLoaded {
                            flow_id: flow_id_str.clone(),
                            stats,
                        });
                    }
                    Err(_) => {
                        // Flow not running or stats not available - silently ignore
                        let _ = tx.send(AppMessage::StatsNotAvailable(flow_id_str.clone()));
                    }
                }

                // Also fetch dynamic pads for the selected flow
                if fetch_dynamic_pads {
                    if let Ok(pads) = api.get_dynamic_pads(flow_id).await {
                        let _ = tx.send(AppMessage::DynamicPadsLoaded {
                            flow_id: flow_id_str,
                            pads,
                        });
                    }
                }

                ctx.request_repaint();
            });
        }
    }

    /// Save the current flow to the backend.
    fn save_current_flow(&mut self, ctx: &Context) {
        tracing::info!(
            "save_current_flow called, selected_flow_id: {:?}",
            self.selected_flow_id
        );

        if let Some(flow_id) = self.selected_flow_id {
            // Update flow with current graph state
            if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
                flow.elements = self.graph.elements.clone();
                flow.blocks = self.graph.blocks.clone();
                flow.links = self.graph.links.clone();

                tracing::info!(
                    "Preparing to save flow: id={}, name='{}', elements={}, links={}",
                    flow.id,
                    flow.name,
                    flow.elements.len(),
                    flow.links.len()
                );

                let flow_clone = flow.clone();
                let api = self.api.clone();
                let tx = self.channels.sender();
                let ctx = ctx.clone();

                self.status = "Saving flow...".to_string();

                spawn_task(async move {
                    tracing::info!("Starting async save operation for flow {}", flow_clone.id);
                    match api.update_flow(&flow_clone).await {
                        Ok(_) => {
                            tracing::info!(
                                "Flow saved successfully - WebSocket event will trigger refresh"
                            );
                            let _ =
                                tx.send(AppMessage::FlowOperationSuccess("Flow saved".to_string()));
                        }
                        Err(e) => {
                            tracing::error!("Failed to save flow: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to save flow: {}",
                                e
                            )));
                        }
                    }
                    ctx.request_repaint();
                });
            } else {
                tracing::warn!("save_current_flow: No flow found with id {}", flow_id);
            }
        } else {
            tracing::warn!("save_current_flow: No flow selected");
        }
    }

    /// Create a new flow.
    pub(crate) fn create_flow(&mut self, ctx: &Context) {
        if self.new_flow_name.is_empty() {
            self.error = Some("Flow name cannot be empty".to_string());
            return;
        }

        let new_flow = Flow::new(self.new_flow_name.clone());
        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Creating flow...".to_string();
        self.show_new_flow_dialog = false;
        self.new_flow_name.clear();

        spawn_task(async move {
            match api.create_flow(&new_flow).await {
                Ok(created_flow) => {
                    tracing::info!(
                        "Flow created successfully: {} - WebSocket event will trigger refresh",
                        created_flow.name
                    );
                    let flow_id = created_flow.id;
                    let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                        "Flow '{}' created",
                        created_flow.name
                    )));
                    // Send flow ID so we can navigate to it after refresh
                    let _ = tx.send(AppMessage::FlowCreated(flow_id));
                }
                Err(e) => {
                    tracing::error!("Failed to create flow: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to create flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Create a new flow from an SDP (from discovered stream).
    pub(crate) fn create_flow_from_sdp(&mut self, sdp: String, ctx: &Context) {
        use strom_types::{block::Position, BlockInstance, PropertyValue};

        // Parse stream name from SDP
        let stream_name = sdp
            .lines()
            .find(|l| l.starts_with("s="))
            .map(|l| l.trim_start_matches("s=").trim())
            .unwrap_or("Discovered Stream");

        let flow_name = format!("AES67 - {}", stream_name);

        // Create flow with AES67 Input block
        let mut new_flow = Flow::new(flow_name.clone());

        // Create AES67 Input block instance
        let block = BlockInstance {
            id: uuid::Uuid::new_v4().to_string(),
            block_definition_id: "builtin.aes67_input".to_string(),
            name: Some(stream_name.to_string()),
            properties: std::collections::HashMap::from([(
                "SDP".to_string(),
                PropertyValue::String(sdp),
            )]),
            position: Position { x: 100.0, y: 100.0 },
            runtime_data: None,
            computed_external_pads: None,
        };

        new_flow.blocks.push(block);

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Creating flow from SDP...".to_string();
        // Switch to Flows page
        self.current_page = AppPage::Flows;

        spawn_task(async move {
            // First create the empty flow to get an ID
            match api.create_flow(&new_flow).await {
                Ok(created_flow) => {
                    tracing::info!("Flow created from SDP: {}", created_flow.name);
                    let flow_id = created_flow.id;
                    let flow_name = created_flow.name.clone();

                    // Now update the flow with the blocks
                    let mut full_flow = new_flow;
                    full_flow.id = flow_id;

                    match api.update_flow(&full_flow).await {
                        Ok(_) => {
                            tracing::info!("Flow updated with AES67 Input block: {}", flow_name);
                            let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                "Flow '{}' created from discovered stream",
                                flow_name
                            )));
                            let _ = tx.send(AppMessage::FlowCreated(flow_id));
                        }
                        Err(e) => {
                            tracing::error!("Failed to update flow with block: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to add block to flow: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create flow from SDP: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to create flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Start the current flow.
    fn start_flow(&mut self, ctx: &Context) {
        if let Some(flow) = self.current_flow() {
            let flow_id = flow.id;
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            self.status = "Starting flow...".to_string();

            spawn_task(async move {
                match api.start_flow(flow_id).await {
                    Ok(_) => {
                        tracing::info!(
                            "Flow started successfully - WebSocket event will trigger refresh"
                        );
                        let _ =
                            tx.send(AppMessage::FlowOperationSuccess("Flow started".to_string()));
                    }
                    Err(e) => {
                        tracing::error!("Failed to start flow: {}", e);
                        let _ = tx.send(AppMessage::FlowOperationError(format!(
                            "Failed to start flow: {}",
                            e
                        )));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Stop the current flow.
    fn stop_flow(&mut self, ctx: &Context) {
        if let Some(flow) = self.current_flow() {
            let flow_id = flow.id;
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            self.status = "Stopping flow...".to_string();

            spawn_task(async move {
                match api.stop_flow(flow_id).await {
                    Ok(_) => {
                        tracing::info!(
                            "Flow stopped successfully - WebSocket event will trigger refresh"
                        );
                        let _ =
                            tx.send(AppMessage::FlowOperationSuccess("Flow stopped".to_string()));
                    }
                    Err(e) => {
                        tracing::error!("Failed to stop flow: {}", e);
                        let _ = tx.send(AppMessage::FlowOperationError(format!(
                            "Failed to stop flow: {}",
                            e
                        )));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Delete a flow.
    pub(crate) fn delete_flow(&mut self, flow_id: strom_types::FlowId, ctx: &Context) {
        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Deleting flow...".to_string();

        spawn_task(async move {
            match api.delete_flow(flow_id).await {
                Ok(_) => {
                    tracing::info!(
                        "Flow deleted successfully - WebSocket event will trigger refresh"
                    );
                    let _ = tx.send(AppMessage::FlowOperationSuccess("Flow deleted".to_string()));
                }
                Err(e) => {
                    tracing::error!("Failed to delete flow: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to delete flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Format keyboard shortcut for display (adapts to platform).
    fn format_shortcut(shortcut: &str) -> String {
        #[cfg(target_os = "macos")]
        {
            shortcut.replace("Ctrl", "⌘")
        }
        #[cfg(not(target_os = "macos"))]
        {
            shortcut.to_string()
        }
    }

    /// Navigate to the previous flow in the sorted flow list.
    fn navigate_flow_list_up(&mut self) {
        if self.flows.is_empty() {
            return;
        }

        // Create sorted list to match the display order (by name)
        let mut sorted_flows: Vec<&Flow> = self.flows.iter().collect();
        sorted_flows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        if let Some(current_id) = self.selected_flow_id {
            // Find position of current selection in sorted list
            if let Some(pos) = sorted_flows.iter().position(|f| f.id == current_id) {
                if pos > 0 {
                    // Move to previous flow
                    let flow = sorted_flows[pos - 1];
                    self.selected_flow_id = Some(flow.id);
                    // Clear graph selection when switching flows
                    self.graph.deselect_all();
                    self.graph.clear_runtime_dynamic_pads();
                    self.graph.load(flow.elements.clone(), flow.links.clone());
                    self.graph.load_blocks(flow.blocks.clone());
                }
            }
        } else if !sorted_flows.is_empty() {
            // No selection, select first flow
            let flow = sorted_flows[0];
            self.selected_flow_id = Some(flow.id);
            // Clear graph selection when switching flows
            self.graph.deselect_all();
            self.graph.clear_runtime_dynamic_pads();
            self.graph.load(flow.elements.clone(), flow.links.clone());
            self.graph.load_blocks(flow.blocks.clone());
        }
    }

    /// Navigate to the next flow in the sorted flow list.
    fn navigate_flow_list_down(&mut self) {
        if self.flows.is_empty() {
            return;
        }

        // Create sorted list to match the display order (by name)
        let mut sorted_flows: Vec<&Flow> = self.flows.iter().collect();
        sorted_flows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        if let Some(current_id) = self.selected_flow_id {
            // Find position of current selection in sorted list
            if let Some(pos) = sorted_flows.iter().position(|f| f.id == current_id) {
                if pos < sorted_flows.len() - 1 {
                    // Move to next flow
                    let flow = sorted_flows[pos + 1];
                    self.selected_flow_id = Some(flow.id);
                    // Clear graph selection when switching flows
                    self.graph.deselect_all();
                    self.graph.clear_runtime_dynamic_pads();
                    self.graph.load(flow.elements.clone(), flow.links.clone());
                    self.graph.load_blocks(flow.blocks.clone());
                }
            }
        } else if !sorted_flows.is_empty() {
            // No selection, select first flow
            let flow = sorted_flows[0];
            self.selected_flow_id = Some(flow.id);
            // Clear graph selection when switching flows
            self.graph.deselect_all();
            self.graph.clear_runtime_dynamic_pads();
            self.graph.load(flow.elements.clone(), flow.links.clone());
            self.graph.load_blocks(flow.blocks.clone());
        }
    }

    /// Handle global keyboard shortcuts.
    pub(crate) fn handle_keyboard_shortcuts(&mut self, ctx: &Context) {
        // Don't process shortcuts if a text input has focus (except ESC)
        let wants_keyboard = ctx.wants_keyboard_input();

        // ESC key - highest priority, works even in text inputs
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            // Priority 1: Close dialogs and windows
            if self.show_new_flow_dialog {
                self.show_new_flow_dialog = false;
            } else if self.show_import_dialog {
                self.show_import_dialog = false;
            } else if self.flow_pending_deletion.is_some() {
                self.flow_pending_deletion = None;
            } else if self.editing_properties_flow_id.is_some() {
                self.editing_properties_flow_id = None;
            } else if !wants_keyboard {
                // Priority 2: Deselect in graph editor
                self.graph.deselect_all();
            }
        }

        // Ctrl+S - Save (works even in text inputs)
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save_current_flow(ctx);
        }

        // F5 or Ctrl+R - Refresh (works even in text inputs)
        if ctx.input(|i| {
            i.key_pressed(egui::Key::F5) || (i.modifiers.command && i.key_pressed(egui::Key::R))
        }) {
            self.needs_refresh = true;
        }

        // Ctrl+D - Debug Graph (works even in text inputs)
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::D)) {
            if let Some(flow) = self.current_flow() {
                let url = self.api.get_debug_graph_url(flow.id);
                ctx.open_url(egui::OpenUrl::new_tab(&url));
            }
        }

        // Shift+F9 - Stop Flow (works even in text inputs, must be checked before plain F9)
        if ctx.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::F9)) {
            self.stop_flow(ctx);
        }
        // F9 - Start/Restart Flow (works even in text inputs)
        else if ctx.input(|i| !i.modifiers.shift && i.key_pressed(egui::Key::F9)) {
            if let Some(flow) = self.current_flow() {
                let state = flow.state.unwrap_or(PipelineState::Null);
                let is_running = matches!(state, PipelineState::Playing);

                if is_running {
                    // Restart
                    let api = self.api.clone();
                    let tx = self.channels.sender();
                    let flow_id = flow.id;
                    let ctx_clone = ctx.clone();

                    self.status = "Restarting flow...".to_string();

                    spawn_task(async move {
                        match api.stop_flow(flow_id).await {
                            Ok(_) => match api.start_flow(flow_id).await {
                                Ok(_) => {
                                    let _ = tx.send(AppMessage::FlowOperationSuccess(
                                        "Flow restarted".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                                        "Failed to restart flow: {}",
                                        e
                                    )));
                                }
                            },
                            Err(e) => {
                                let _ = tx.send(AppMessage::FlowOperationError(format!(
                                    "Failed to restart flow: {}",
                                    e
                                )));
                            }
                        }
                        ctx_clone.request_repaint();
                    });
                } else {
                    self.start_flow(ctx);
                }
            }
        }

        // Ctrl+F - Find: cycle through filter boxes (works even in text inputs)
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            // Deselect any selected element/block
            self.graph.deselect_all();

            // Cycle to next focus target based on current page
            match self.current_page {
                AppPage::Flows => {
                    self.focus_target = match self.focus_target {
                        FocusTarget::None | FocusTarget::PaletteBlocks => {
                            self.focus_flow_filter_requested = true;
                            FocusTarget::FlowFilter
                        }
                        FocusTarget::FlowFilter => {
                            self.palette.switch_to_elements();
                            self.palette.focus_search();
                            FocusTarget::PaletteElements
                        }
                        FocusTarget::PaletteElements => {
                            self.palette.switch_to_blocks();
                            self.palette.focus_search();
                            FocusTarget::PaletteBlocks
                        }
                        _ => {
                            self.focus_flow_filter_requested = true;
                            FocusTarget::FlowFilter
                        }
                    };
                }
                AppPage::Discovery => {
                    self.discovery_page.focus_search();
                    self.focus_target = FocusTarget::DiscoveryFilter;
                }
                AppPage::Clocks => {
                    // No filters on Clocks page
                }
                AppPage::Media => {
                    self.media_page.focus_search();
                    self.focus_target = FocusTarget::MediaFilter;
                }
                AppPage::Info => {
                    // No search/filters on Info page
                }
            }
        }

        // Don't process other shortcuts if text input has focus
        if wants_keyboard {
            return;
        }

        // Up/Down arrow keys - Navigate flow list
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            self.navigate_flow_list_up();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            self.navigate_flow_list_down();
        }

        // Delete key - Delete selected flow (only if nothing is selected in graph editor)
        if ctx.input(|i| i.key_pressed(egui::Key::Delete)) && !self.graph.has_selection() {
            if let Some(flow) = self.current_flow() {
                self.flow_pending_deletion = Some((flow.id, flow.name.clone()));
            }
        }

        // Ctrl+N - New Flow
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::N)) {
            self.show_new_flow_dialog = true;
        }

        // Ctrl+O - Import
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.show_import_dialog = true;
            self.import_json_buffer.clear();
            self.import_error = None;
        }

        // F1 - Help (GitHub)
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Eyevinn/strom"));
        }

        // Ctrl+C - Copy selected element/block in graph
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) {
            self.graph.copy_selected();
        }

        // Ctrl+V - Paste element/block in graph
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::V)) {
            self.graph.paste_clipboard();
        }
    }

    /// Render the top toolbar.
    pub(crate) fn render_toolbar(&mut self, ctx: &Context) {
        // First top bar: System-wide controls
        TopBottomPanel::top("system_bar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Strom heading as clickable link to GitHub
                    if ui
                        .heading("⚡ Strom")
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Visit Strom on GitHub")
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Eyevinn/strom"));
                    }
                    ui.separator();

                    // Navigation tabs (bigger text)
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Flows,
                            egui::RichText::new("Flows").size(16.0),
                        )
                        .clicked()
                    {
                        self.current_page = AppPage::Flows;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Discovery,
                            egui::RichText::new("Discovery").size(16.0),
                        )
                        .on_hover_text("Browse SAP/AES67 streams")
                        .clicked()
                    {
                        self.current_page = AppPage::Discovery;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Clocks,
                            egui::RichText::new("Clocks").size(16.0),
                        )
                        .on_hover_text("PTP clock synchronization")
                        .clicked()
                    {
                        self.current_page = AppPage::Clocks;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Media,
                            egui::RichText::new("Media").size(16.0),
                        )
                        .on_hover_text("Media file browser")
                        .clicked()
                    {
                        self.current_page = AppPage::Media;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Info,
                            egui::RichText::new("Info").size(16.0),
                        )
                        .on_hover_text("System and version information")
                        .clicked()
                    {
                        self.current_page = AppPage::Info;
                        self.focus_target = FocusTarget::None;
                    }

                    // Right-aligned system controls
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // System monitoring widget (rightmost)
                        let has_gpu = self
                            .system_monitor
                            .latest()
                            .map(|s| !s.gpu_stats.is_empty())
                            .unwrap_or(false);
                        let monitor_height = if has_gpu { 30.0 } else { 24.0 };

                        let monitor_response = ui.add(
                            crate::system_monitor::CompactSystemMonitor::new(&self.system_monitor)
                                .width(180.0)
                                .height(monitor_height),
                        );
                        if monitor_response.clicked() {
                            self.show_system_monitor = !self.show_system_monitor;
                        }
                        monitor_response.on_hover_text("Click to show detailed system monitoring");

                        ui.separator();

                        // Logout button (only show if auth is enabled and user is authenticated)
                        if let Some(ref status) = self.auth_status {
                            if status.auth_required
                                && status.authenticated
                                && ui.button("🚪").on_hover_text("Logout").clicked()
                            {
                                self.handle_logout(ctx.clone());
                            }
                        }

                        // Theme switch button (leftmost)
                        let theme_icon = match self.theme_preference {
                            ThemePreference::System => "🖥",
                            ThemePreference::Light => "☀",
                            ThemePreference::Dark => "🌙",
                        };

                        if ui
                            .button(theme_icon)
                            .on_hover_text("Change theme")
                            .clicked()
                        {
                            let new_theme = match self.theme_preference {
                                ThemePreference::System => ThemePreference::Light,
                                ThemePreference::Light => ThemePreference::Dark,
                                ThemePreference::Dark => ThemePreference::System,
                            };
                            self.theme_preference = new_theme;
                            self.apply_theme(ctx.clone());
                        }
                    });
                });
            });

        // Second top bar: Page-specific controls
        self.render_page_toolbar(ctx);
    }

    /// Render the page-specific toolbar (second row)
    fn render_page_toolbar(&mut self, ctx: &Context) {
        match self.current_page {
            AppPage::Flows => self.render_flows_toolbar(ctx),
            AppPage::Discovery => self.render_discovery_toolbar(ctx),
            AppPage::Clocks => self.render_clocks_toolbar(ctx),
            AppPage::Media => self.render_media_toolbar(ctx),
            AppPage::Info => self.render_info_toolbar(ctx),
        }
    }

    /// Render the flows page toolbar
    fn render_flows_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(8, 4)))
            .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(egui::RichText::new("Flows").heading());
                ui.separator();

                if ui
                    .button("New Flow")
                    .on_hover_text(format!("Create a new flow ({})", Self::format_shortcut("Ctrl+N")))
                    .clicked()
                {
                    self.show_new_flow_dialog = true;
                }

                if ui
                    .button("Import")
                    .on_hover_text(format!("Import flow from JSON ({})", Self::format_shortcut("Ctrl+O")))
                    .clicked()
                {
                    self.show_import_dialog = true;
                    self.import_json_buffer.clear();
                    self.import_error = None;
                }

                if ui
                    .button("Refresh")
                    .on_hover_text("Reload flows from server (F5 or Ctrl+R)")
                    .clicked()
                {
                    self.needs_refresh = true;
                }

                if ui
                    .button("Save")
                    .on_hover_text(format!("Save current flow ({})", Self::format_shortcut("Ctrl+S")))
                    .clicked()
                {
                    self.save_current_flow(ctx);
                }

                // Flow controls - only show when a flow is selected
                let flow_info = self.current_flow().map(|f| (f.id, f.state));

                if let Some((flow_id, state)) = flow_info {
                    ui.separator();

                    let state = state.unwrap_or(PipelineState::Null);

                    // Map internal states to user-friendly names
                    let (state_text, state_color) = match state {
                        PipelineState::Null | PipelineState::Ready => ("Stopped", Color32::GRAY),
                        PipelineState::Paused => ("Paused", Color32::from_rgb(255, 165, 0)),
                        PipelineState::Playing => ("Started", Color32::GREEN),
                    };

                    ui.colored_label(state_color, format!("State: {}", state_text));

                    // Show latency for running flows
                    let is_running = matches!(state, PipelineState::Playing);
                    if is_running {
                        if let Some(latency) = self.latency_cache.get(&flow_id.to_string()) {
                            ui.label(format!("Latency: {}", latency.min_latency_formatted));
                        }
                    }

                    ui.separator();

                    // Show Start or Restart button depending on state
                    let button_text = if is_running {
                        "🔄 Restart"
                    } else {
                        "▶ Start"
                    };

                    if ui
                        .button(button_text)
                        .on_hover_text(if is_running {
                            "Restart pipeline (F9)"
                        } else {
                            "Start pipeline (F9)"
                        })
                        .clicked()
                    {
                        if is_running {
                            // For restart: stop first, then start
                            let api = self.api.clone();
                            let tx = self.channels.sender();
                            let ctx_clone = ctx.clone();

                            self.status = "Restarting flow...".to_string();

                            spawn_task(async move {
                                // First stop the flow
                                match api.stop_flow(flow_id).await {
                                    Ok(_) => {
                                        tracing::info!("Flow stopped, now starting...");
                                        // Then start it again
                                        match api.start_flow(flow_id).await {
                                            Ok(_) => {
                                                tracing::info!("Flow restarted successfully - WebSocket events will trigger refresh");
                                                let _ = tx.send(AppMessage::FlowOperationSuccess("Flow restarted".to_string()));
                                            }
                                            Err(e) => {
                                                tracing::error!(
                                                    "Failed to start flow after stop: {}",
                                                    e
                                                );
                                                let _ = tx.send(AppMessage::FlowOperationError(format!("Failed to restart flow: {}", e)));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to stop flow for restart: {}", e);
                                        let _ = tx.send(AppMessage::FlowOperationError(format!("Failed to restart flow: {}", e)));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        } else {
                            self.start_flow(ctx);
                        }
                    }

                    if ui
                        .button("⏹ Stop")
                        .on_hover_text("Stop pipeline (Shift+F9)")
                        .clicked()
                    {
                        self.stop_flow(ctx);
                    }

                    if ui
                        .button("🔍 Debug Graph")
                        .on_hover_text(format!(
                            "View pipeline debug graph ({})",
                            Self::format_shortcut("Ctrl+D")
                        ))
                        .clicked()
                    {
                        let url = self.api.get_debug_graph_url(flow_id);
                        ctx.open_url(egui::OpenUrl::new_tab(&url));
                    }
                }

            });
        });
    }

    /// Render the discovery page toolbar
    fn render_discovery_toolbar(&mut self, ctx: &Context) {
        let is_loading = self.discovery_page.loading;

        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Discovery").heading());
                    ui.separator();

                    if ui.button("Refresh").clicked() {
                        self.discovery_page
                            .refresh(&self.api, ctx, &self.channels.sender());
                    }
                    if is_loading {
                        ui.spinner();
                    }
                });
            });
    }

    /// Render the clocks page toolbar
    fn render_clocks_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Clocks").heading());
                    ui.separator();
                    ui.label("PTP clocks are shared per domain");
                });
            });
    }

    /// Render the media page toolbar
    fn render_media_toolbar(&mut self, ctx: &Context) {
        let is_loading = self.media_page.loading;

        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Media Files").heading());
                    ui.separator();

                    if ui.button("Refresh").clicked() {
                        self.media_page
                            .refresh(&self.api, ctx, &self.channels.sender());
                    }
                    if is_loading {
                        ui.spinner();
                    }
                });
            });
    }

    /// Render the info page toolbar
    fn render_info_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("System Information").heading());
                    ui.separator();

                    if ui.button("Refresh").clicked() {
                        self.load_version(ctx.clone());
                        // Force reload of network interfaces
                        self.network_interfaces_loaded = false;
                        self.load_network_interfaces(ctx.clone());
                    }
                });
            });
    }

    /// Render the element palette sidebar.
    pub(crate) fn render_palette(&mut self, ctx: &Context) {
        SidePanel::right("palette")
            .default_width(250.0)
            .resizable(true)
            .show(ctx, |ui| {
                // Check if an element is selected and trigger property loading if needed
                // Do this BEFORE getting mutable reference to avoid borrow checker issues
                if let Some((selected_element_type, active_tab)) = self
                    .graph
                    .get_selected_element()
                    .map(|e| (e.element_type.clone(), self.graph.active_property_tab))
                {
                    // Trigger lazy loading if properties not cached
                    if !self.palette.has_properties_cached(&selected_element_type) {
                        tracing::info!(
                            "Element '{}' selected but properties not cached, triggering lazy load",
                            selected_element_type
                        );
                        self.load_element_properties(selected_element_type.clone(), ctx);
                    }

                    // Trigger pad properties loading if on Input/Output Pads tabs
                    use crate::graph::PropertyTab;
                    if matches!(active_tab, PropertyTab::InputPads | PropertyTab::OutputPads)
                        && !self.palette.has_pad_properties_cached(&selected_element_type)
                    {
                        tracing::info!(
                            "Element '{}' showing pad tab but pad properties not cached, triggering lazy load",
                            selected_element_type
                        );
                        self.load_element_pad_properties(selected_element_type.clone(), ctx);
                    }
                }

                // Show either the palette or the property inspector, not both
                // Collect data BEFORE getting mutable reference to avoid borrow checker issues
                let selected_element_data = self.graph.get_selected_element().map(|element| {
                    let active_tab = self.graph.active_property_tab;

                    // Use pad properties if showing pad tabs, otherwise regular properties
                    use crate::graph::PropertyTab;
                    let element_info = if matches!(active_tab, PropertyTab::InputPads | PropertyTab::OutputPads) {
                        self.palette.get_element_info_with_pads(&element.element_type)
                    } else {
                        self.palette.get_element_info(&element.element_type)
                    };

                    let element_id = element.id.clone();
                    let focused_pad = self.graph.focused_pad.clone();
                    let input_pads = self.graph.get_actual_input_pads(&element_id);
                    let output_pads = self.graph.get_actual_output_pads(&element_id);
                    (element_info, active_tab, focused_pad, input_pads, output_pads)
                });

                if let Some((element_info, active_tab, focused_pad, input_pads, output_pads)) = selected_element_data {
                    // Element selected: show ONLY property inspector
                    ui.heading("Properties");
                    ui.separator();

                    // Split borrow: get mutable access to graph fields separately
                    let graph = &mut self.graph;
                    if let Some(element) = graph.get_selected_element_mut() {
                        let (new_tab, delete_requested) = PropertyInspector::show(
                            ui,
                            element,
                            element_info,
                            active_tab,
                            focused_pad,
                            input_pads,
                            output_pads,
                        );
                        graph.active_property_tab = new_tab;

                        // Handle deletion request
                        if delete_requested {
                            graph.remove_selected();
                        }
                    }
                } else if let Some(block_def_id) = self
                    .graph
                    .get_selected_block()
                    .map(|b| b.block_definition_id.clone())
                {
                    // Block selected: show block property inspector
                    ui.heading("Block Properties");
                    ui.separator();

                    // Clone definition to avoid borrow checker issues
                    let definition_opt = self
                        .graph
                        .get_block_definition_by_id(&block_def_id)
                        .cloned();
                    let flow_id = self.current_flow().map(|f| f.id);

                    // Load network interfaces if block has NetworkInterface properties
                    if let Some(ref def) = definition_opt {
                        let has_network_prop = def.exposed_properties.iter().any(|prop| {
                            matches!(
                                prop.property_type,
                                strom_types::block::PropertyType::NetworkInterface
                            )
                        });
                        if has_network_prop {
                            self.load_network_interfaces(ctx.clone());
                        }

                        // Load available channels if this is an InterInput block
                        // Only refresh once when selection changes to this block
                        if def.id == "builtin.inter_input" {
                            if let Some(block) = self.graph.get_selected_block() {
                                let block_id = block.id.clone();
                                if self.last_inter_input_refresh.as_ref() != Some(&block_id) {
                                    self.last_inter_input_refresh = Some(block_id);
                                    self.refresh_available_channels();
                                }
                            }
                            self.load_available_channels(ctx.clone());
                        }
                    }

                    // Get stats for this flow if available
                    let stats = flow_id
                        .map(|fid| fid.to_string())
                        .and_then(|fid| self.stats_cache.get(&fid));

                    // Then get mutable reference to block
                    if let (Some(block), Some(def)) =
                        (self.graph.get_selected_block_mut(), definition_opt)
                    {
                        let block_id = block.id.clone();
                        let result = PropertyInspector::show_block(
                            ui,
                            block,
                            &def,
                            flow_id,
                            &self.meter_data,
                            &self.webrtc_stats,
                            stats,
                            &self.network_interfaces,
                            &self.available_channels,
                        );

                        // Handle deletion request
                        if result.delete_requested {
                            self.graph.remove_selected();
                        }

                        // Handle browse streams request (for AES67 Input)
                        if result.browse_streams_requested {
                            self.show_stream_picker_for_block = Some(block_id.clone());
                            // Refresh discovered streams for the picker
                            self.discovery_page.refresh(&self.api, ctx, &self.channels.tx);
                        }

                        // Handle VLC playlist download request (for MPEG-TS/SRT Output)
                        if let Some((srt_uri, latency_ms)) = result.vlc_playlist_requested {
                            // Get flow name for the stream title
                            let stream_name = self
                                .current_flow()
                                .map(|f| f.name.clone())
                                .unwrap_or_else(|| "SRT Stream".to_string());

                            let playlist_content =
                                generate_vlc_playlist(&srt_uri, latency_ms, &stream_name);

                            // Generate filename based on flow name
                            let safe_name: String = stream_name
                                .chars()
                                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                                .collect();
                            let filename = format!("{}.xspf", safe_name);

                            download_file(&filename, &playlist_content, "application/xspf+xml");
                        }
                    } else {
                        ui.label("Block definition not found");
                    }
                } else {
                    // No element or block selected: show ONLY the palette
                    self.palette.show(ui);
                }
            });
    }

    /// Render the main canvas area.
    pub(crate) fn render_canvas(&mut self, ctx: &Context) {
        CentralPanel::default().show(ctx, |ui| {
            if self.current_flow().is_some() {
                // Show compact instructions banner at the top
                let legend_bg = if ui.visuals().dark_mode {
                    Color32::from_rgb(40, 40, 50) // Dark theme: dark background
                } else {
                    Color32::from_rgb(230, 230, 240) // Light theme: light background
                };

                let legend_text_color = if ui.visuals().dark_mode {
                    Color32::from_rgb(200, 200, 200) // Dark theme: lighter text
                } else {
                    Color32::from_rgb(60, 60, 70) // Light theme: dark text
                };

                egui::Frame::new()
                    .fill(legend_bg)
                    .inner_margin(4.0)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("💡");
                            ui.small(
                                egui::RichText::new("Search & click +Add to add elements/blocks")
                                    .color(legend_text_color),
                            );
                            ui.separator();
                            ui.small(
                                egui::RichText::new("Drag output→input ports to link")
                                    .color(legend_text_color),
                            );
                            ui.separator();
                            ui.small(
                                egui::RichText::new(
                                    "Drag nodes (snaps to grid) | Scroll=pan | Ctrl+Scroll=zoom | Del=delete",
                                )
                                .color(legend_text_color),
                            );
                        });
                    });

                ui.add_space(2.0);

                // Setup dynamic content for meter blocks before rendering
                self.graph.clear_block_content();
                if let Some(flow_id) = self.current_flow().map(|f| f.id) {
                    // Clone block IDs to avoid borrowing issues
                    let meter_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.meter")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in meter_blocks {
                        if let Some(meter_data) = self.meter_data.get(&flow_id, &block_id) {
                            let height =
                                crate::meter::calculate_compact_height(meter_data.rms.len());
                            let meter_data_clone = meter_data.clone();

                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height: height + 10.0,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        crate::meter::show_compact(ui, &meter_data_clone);
                                    })),
                                },
                            );
                        }
                    }

                    // Setup dynamic content for WHIP/WHEP blocks
                    let webrtc_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| {
                            b.block_definition_id == "builtin.whep_input"
                                || b.block_definition_id == "builtin.whip_output"
                        })
                        .map(|b| b.id.clone())
                        .collect();

                    if let Some(stats) = self.webrtc_stats.get(&flow_id) {
                        let stats_clone = stats.clone();
                        for block_id in webrtc_blocks {
                            let stats_for_block = stats_clone.clone();
                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height: 25.0,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        crate::webrtc_stats::show_compact(ui, &stats_for_block);
                                    })),
                                },
                            );
                        }
                    }

                    // Setup dynamic content for Media Player blocks
                    let player_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.media_player")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in player_blocks {
                        // Get player data or use default
                        let player_data = self
                            .mediaplayer_data
                            .get(&flow_id, &block_id)
                            .cloned()
                            .unwrap_or_default();

                        let height = crate::mediaplayer::calculate_compact_height();
                        let player_data_clone = player_data.clone();
                        let block_id_for_action = block_id.clone();

                        self.graph.set_block_content(
                            block_id,
                            crate::graph::BlockContentInfo {
                                additional_height: height + 10.0,
                                render_callback: Some(Box::new(move |ui, _rect| {
                                    if let Some((action, seek_pos)) =
                                        crate::mediaplayer::show_compact(ui, &player_data_clone)
                                    {
                                        // Use local storage to signal actions
                                        let action_data = if let Some(pos) = seek_pos {
                                            format!("{}:{}:{}", block_id_for_action, action, pos)
                                        } else {
                                            format!("{}:{}", block_id_for_action, action)
                                        };
                                        tracing::debug!("Setting player_action: {}", action_data);
                                        set_local_storage("player_action", &action_data);
                                    }
                                })),
                            },
                        );
                    }
                }

                // Update QoS health map for the current flow before rendering
                if let Some(flow_id) = self.selected_flow_id {
                    let qos_health_map = self.qos_stats.get_element_health_map(&flow_id);
                    self.graph.set_qos_health_map(qos_health_map);
                }

                // Show graph editor
                let response = self.graph.show(ui);

                // Check if a QoS marker in the graph was clicked - open log panel
                if self.graph.was_qos_marker_clicked() {
                    self.show_log_panel = true;
                }

                // Handle adding elements from palette
                if let Some(element_type) = self.palette.take_dragging_element() {
                    // Add element at center of visible area
                    let center = response.rect.center();
                    let world_pos = ((center - response.rect.min - self.graph.pan_offset)
                        / self.graph.zoom)
                        .to_pos2();
                    self.graph.add_element(element_type.clone(), world_pos);

                    // Trigger pad info loading if not already cached
                    if !self.palette.has_pad_properties_cached(&element_type) {
                        self.load_element_pad_properties(element_type, ctx);
                    }
                }

                // Handle adding blocks from palette
                if let Some(block_id) = self.palette.take_dragging_block() {
                    // Add block at center of visible area
                    let center = response.rect.center();
                    let world_pos = ((center - response.rect.min - self.graph.pan_offset)
                        / self.graph.zoom)
                        .to_pos2();

                    // Set default description for InterOutput blocks
                    if block_id == "builtin.inter_output" {
                        // Count existing inter_output blocks to get next number
                        let counter = self
                            .graph
                            .blocks
                            .iter()
                            .filter(|b| b.block_definition_id == "builtin.inter_output")
                            .count()
                            + 1;
                        let mut props = std::collections::HashMap::new();
                        props.insert(
                            "description".to_string(),
                            strom_types::PropertyValue::String(format!("stream_{}", counter)),
                        );
                        self.graph.add_block_with_props(block_id, world_pos, props);
                    } else {
                        self.graph.add_block(block_id, world_pos);
                    }
                }

                // Handle delete key for elements and links
                // Only process delete if no text edit widget has focus
                if ui.input(|i| i.key_pressed(egui::Key::Delete))
                    && !ui.ctx().wants_keyboard_input()
                {
                    self.graph.remove_selected(); // Remove selected element (if any)
                    self.graph.remove_selected_link(); // Remove selected link (if any)
                }

            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.heading("Welcome to Strom");
                    ui.label("Select a flow from the sidebar or create a new one");
                });
            }
        });
    }

    /// Render the status bar.
    pub(crate) fn render_status_bar(&mut self, ctx: &Context) {
        TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.separator();
                ui.label(format!("Flows: {}", self.flows.len()));

                // Log message counts with toggle button
                let (errors, warnings, _infos) = self.log_counts();
                if errors > 0 || warnings > 0 {
                    ui.separator();
                    let toggle_text = if self.show_log_panel {
                        format!("Messages: {} errors, {} warnings [hide]", errors, warnings)
                    } else {
                        format!("Messages: {} errors, {} warnings [show]", errors, warnings)
                    };
                    let color = if errors > 0 {
                        Color32::from_rgb(255, 80, 80)
                    } else {
                        Color32::from_rgb(255, 200, 50)
                    };
                    if ui
                        .add(
                            egui::Label::new(egui::RichText::new(&toggle_text).color(color))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text("Click to toggle message panel")
                        .clicked()
                    {
                        self.show_log_panel = !self.show_log_panel;
                    }
                }

                // Version info on the right side
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ref version_info) = self.version_info {
                        let version_text = if !version_info.git_tag.is_empty() {
                            // On a tagged release
                            version_info.git_tag.to_string()
                        } else {
                            // Development version
                            format!("v{}-{}", version_info.version, version_info.git_hash)
                        };

                        let color = if version_info.git_dirty {
                            Color32::from_rgb(255, 165, 0) // Orange for dirty
                        } else if !version_info.git_tag.is_empty() {
                            Color32::from_rgb(0, 200, 0) // Green for release
                        } else {
                            Color32::GRAY // Gray for dev
                        };

                        let full_version_text = if version_info.git_dirty {
                            format!("{} (modified)", version_text)
                        } else {
                            version_text
                        };

                        ui.colored_label(color, full_version_text)
                            .on_hover_ui(|ui| {
                                ui.label(format!("Version: v{}", version_info.version));
                                ui.label(format!("Git: {}", version_info.git_hash));
                                if !version_info.git_tag.is_empty() {
                                    ui.label(format!("Tag: {}", version_info.git_tag));
                                }
                                ui.label(format!("Branch: {}", version_info.git_branch));
                                ui.label(format!("Built: {}", version_info.build_timestamp));
                                if !version_info.gstreamer_version.is_empty() {
                                    ui.label(format!(
                                        "GStreamer: {}",
                                        version_info.gstreamer_version
                                    ));
                                }
                                if !version_info.os_info.is_empty() {
                                    let os_text = if version_info.in_docker {
                                        format!("{} (Docker)", version_info.os_info)
                                    } else {
                                        version_info.os_info.clone()
                                    };
                                    ui.label(format!("OS: {}", os_text));
                                }
                                if version_info.git_dirty {
                                    ui.colored_label(
                                        Color32::YELLOW,
                                        "Working directory had uncommitted changes",
                                    );
                                }
                            });
                    }
                });
            });
        });
    }

    /// Render the log panel showing errors, warnings, and info messages.
    pub(crate) fn render_log_panel(&mut self, ctx: &Context) {
        if !self.show_log_panel || self.log_entries.is_empty() {
            return;
        }

        // Calculate dynamic height based on number of entries (min 80px, max 200px)
        let panel_height = (self.log_entries.len() as f32 * 20.0).clamp(80.0, 200.0);

        // Collect actions to perform after rendering (to avoid borrow issues)
        let mut entry_to_remove: Option<usize> = None;
        let mut navigate_to: Option<(strom_types::FlowId, Option<String>)> = None;

        TopBottomPanel::bottom("log_panel")
            .resizable(true)
            .min_height(80.0)
            .max_height(400.0)
            .default_height(panel_height)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Pipeline Messages");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Clear All").clicked() {
                            self.clear_log_entries();
                            // Also clear all QoS stats since we're clearing the log
                            self.qos_stats = crate::qos_monitor::QoSStore::new();
                        }
                        if ui.button("Hide").clicked() {
                            self.show_log_panel = false;
                        }
                    });
                });

                ui.separator();

                // Scrollable area for log entries
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        // Show entries in reverse chronological order (newest first)
                        // Use enumerate to track indices for removal
                        let entries_len = self.log_entries.len();
                        for (rev_idx, entry) in self.log_entries.iter().rev().enumerate() {
                            let actual_idx = entries_len - 1 - rev_idx;

                            ui.horizontal(|ui| {
                                // Dismiss button (X) - small and subtle
                                let dismiss_btn = ui.add(
                                    egui::Button::new(
                                        egui::RichText::new("×").size(14.0).color(Color32::GRAY),
                                    )
                                    .frame(false)
                                    .min_size(egui::vec2(16.0, 16.0)),
                                );
                                if dismiss_btn.clicked() {
                                    entry_to_remove = Some(actual_idx);
                                }
                                dismiss_btn.on_hover_text("Dismiss this entry");

                                // Level indicator
                                ui.colored_label(entry.color(), entry.prefix());

                                // Source element if available - make it clickable
                                if let Some(ref source) = entry.source {
                                    let source_label = ui
                                        .colored_label(
                                            Color32::from_rgb(150, 150, 255),
                                            format!("[{}]", source),
                                        )
                                        .interact(egui::Sense::click());

                                    if source_label.clicked() {
                                        if let Some(flow_id) = entry.flow_id {
                                            navigate_to = Some((flow_id, Some(source.clone())));
                                        }
                                    }
                                    source_label.on_hover_text("Click to navigate to this element");
                                }

                                // Flow ID if available - make it clickable
                                if let Some(flow_id) = entry.flow_id {
                                    let flow_name = self
                                        .flows
                                        .iter()
                                        .find(|f| f.id == flow_id)
                                        .map(|f| f.name.clone())
                                        .unwrap_or_else(|| "unknown".to_string());

                                    let flow_label = ui
                                        .colored_label(Color32::GRAY, format!("({})", flow_name))
                                        .interact(egui::Sense::click());

                                    if flow_label.clicked() {
                                        navigate_to = Some((flow_id, entry.source.clone()));
                                    }
                                    flow_label.on_hover_text("Click to navigate to this flow");
                                }

                                // Message - use selectable label so user can copy text
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&entry.message).color(entry.color()),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                                );
                            });
                        }
                    });
            });

        // Process deferred actions
        if let Some(idx) = entry_to_remove {
            // Check if this is a QoS entry - if so, clear from QoS store
            if idx < self.log_entries.len() {
                let entry = &self.log_entries[idx];
                if entry.message.starts_with("QoS:") {
                    if let (Some(flow_id), Some(ref element_id)) = (entry.flow_id, &entry.source) {
                        self.qos_stats.clear_element(&flow_id, element_id);
                    }
                }
                self.log_entries.remove(idx);
            }
        }

        if let Some((flow_id, element_id)) = navigate_to {
            // Navigate to the flow
            self.selected_flow_id = Some(flow_id);

            // Find and load the flow
            if let Some(flow) = self.flows.iter().find(|f| f.id == flow_id).cloned() {
                self.graph.deselect_all();
                self.graph.load(flow.elements.clone(), flow.links.clone());
                self.graph.load_blocks(flow.blocks.clone());

                // If we have an element ID, try to select it in the graph
                if let Some(ref elem_id) = element_id {
                    // ElementId is a String, so we can use it directly
                    // It will match either an element or a block
                    self.graph.select_node(elem_id.clone());
                    // Center the view on the selected element
                    self.graph.center_on_selected();
                }
            }
        }
    }
}

impl eframe::App for StromApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Check shutdown flag (Ctrl+C handler for native mode)
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ref flag) = self.shutdown_flag {
            use std::sync::atomic::Ordering;
            if flag.load(Ordering::SeqCst) {
                tracing::info!("Shutdown flag set, closing GUI...");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        // Delegate to update_loop module
        self.run_update_loop(ctx);
    }
}
