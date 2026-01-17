//! Main application structure.

use egui::{CentralPanel, Color32, Context, SidePanel, TopBottomPanel};
use strom_types::{Flow, PipelineState};

use crate::api::{ApiClient, AuthStatusResponse};
use crate::compositor_editor::CompositorEditor;
use crate::graph::GraphEditor;
use crate::info_page::{
    current_time_millis, format_datetime_local, format_uptime, parse_iso8601_to_millis,
};
use crate::login::LoginScreen;
use crate::mediaplayer::{MediaPlayerDataStore, PlaylistEditor};
use crate::meter::MeterDataStore;
use crate::palette::ElementPalette;
use crate::properties::PropertyInspector;
use crate::state::{AppMessage, AppStateChannels, ConnectionState};
use crate::system_monitor::SystemMonitorStore;
use crate::webrtc_stats::WebRtcStatsStore;
use crate::ws::WebSocketClient;

// Local storage helpers (WASM only)
#[cfg(target_arch = "wasm32")]
pub fn set_local_storage(key: &str, value: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(key, value);
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub fn get_local_storage(key: &str) -> Option<String> {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            return storage.get_item(key).ok().flatten();
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
pub fn remove_local_storage(key: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(key);
        }
    }
}

// Stubs for native mode (use in-memory HashMap)
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
static LOCAL_STORAGE: Mutex<Option<std::collections::HashMap<String, String>>> = Mutex::new(None);

#[cfg(not(target_arch = "wasm32"))]
pub fn set_local_storage(key: &str, value: &str) {
    let mut storage = LOCAL_STORAGE.lock().unwrap();
    if storage.is_none() {
        *storage = Some(std::collections::HashMap::new());
    }
    storage
        .as_mut()
        .unwrap()
        .insert(key.to_string(), value.to_string());
}

#[cfg(not(target_arch = "wasm32"))]
pub fn get_local_storage(key: &str) -> Option<String> {
    let storage = LOCAL_STORAGE.lock().unwrap();
    storage.as_ref()?.get(key).cloned()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn remove_local_storage(key: &str) {
    let mut storage = LOCAL_STORAGE.lock().unwrap();
    if let Some(ref mut map) = *storage {
        map.remove(key);
    }
}

/// Trigger a file download in the browser with the given content.
#[cfg(target_arch = "wasm32")]
pub fn download_file(filename: &str, content: &str, mime_type: &str) {
    use wasm_bindgen::JsCast;

    let window = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let document = match window.document() {
        Some(d) => d,
        None => return,
    };

    // Create a blob with the content
    let blob_parts = js_sys::Array::new();
    blob_parts.push(&wasm_bindgen::JsValue::from_str(content));

    let blob_options = web_sys::BlobPropertyBag::new();
    blob_options.set_type(mime_type);

    let blob = match web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_options) {
        Ok(b) => b,
        Err(_) => return,
    };

    // Create object URL
    let url = match web_sys::Url::create_object_url_with_blob(&blob) {
        Ok(u) => u,
        Err(_) => return,
    };

    // Create a temporary anchor element and click it
    let anchor = match document.create_element("a") {
        Ok(el) => el,
        Err(_) => return,
    };

    let _ = anchor.set_attribute("href", &url);
    let _ = anchor.set_attribute("download", filename);

    if let Some(html_anchor) = anchor.dyn_ref::<web_sys::HtmlElement>() {
        html_anchor.click();
    }

    // Clean up the object URL
    let _ = web_sys::Url::revoke_object_url(&url);
}

/// Native mode - save file to temp directory and open with default application.
#[cfg(not(target_arch = "wasm32"))]
pub fn download_file(filename: &str, content: &str, _mime_type: &str) {
    // Save to temp directory so it doesn't clutter working directory
    let path = std::env::temp_dir().join(filename);

    match std::fs::write(&path, content) {
        Ok(_) => {
            tracing::info!("Saved file to: {}", path.display());

            // Open the file with the default application (VLC for .xspf)
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                    tracing::error!("Failed to open file with xdg-open: {}", e);
                }
            }

            #[cfg(target_os = "macos")]
            {
                if let Err(e) = std::process::Command::new("open").arg(&path).spawn() {
                    tracing::error!("Failed to open file: {}", e);
                }
            }

            #[cfg(target_os = "windows")]
            {
                if let Err(e) = std::process::Command::new("cmd")
                    .args(["/C", "start", "", &path.to_string_lossy()])
                    .spawn()
                {
                    tracing::error!("Failed to open file: {}", e);
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to save file {}: {}", path.display(), e);
        }
    }
}

/// Generate XSPF playlist content for VLC to play an SRT stream.
///
/// If the block is in listener mode (e.g., `srt://:5000?mode=listener`), VLC needs to
/// connect as a caller. We transform the URI to use the server's hostname from the
/// current browser URL.
pub fn generate_vlc_playlist(srt_uri: &str, latency_ms: i32, stream_name: &str) -> String {
    // Transform URI if it's in listener mode - VLC needs to connect as caller
    let vlc_uri = transform_srt_uri_for_vlc(srt_uri);

    // Escape XML special characters in the URI
    let escaped_uri = vlc_uri
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");

    let escaped_name = stream_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist xmlns="http://xspf.org/ns/0/" xmlns:vlc="http://www.videolan.org/vlc/playlist/ns/0/" version="1">
  <title>Strom SRT Stream</title>
  <trackList>
    <track>
      <location>{}</location>
      <title>{}</title>
      <extension application="http://www.videolan.org/vlc/playlist/0">
        <vlc:option>network-caching={}</vlc:option>
      </extension>
    </track>
  </trackList>
</playlist>
"#,
        escaped_uri, escaped_name, latency_ms
    )
}

/// Transform SRT URI for VLC playback.
///
/// When the MPEG-TS/SRT block is in listener mode (server waiting for connections),
/// VLC needs to connect as a caller. This function:
/// 1. Detects listener mode URIs (e.g., `srt://:5000?mode=listener`)
/// 2. Replaces empty host with the Strom server's hostname
/// 3. Changes mode from listener to caller
fn transform_srt_uri_for_vlc(srt_uri: &str) -> String {
    // Check if this is a listener mode URI (empty host or mode=listener)
    let is_listener = srt_uri.contains("mode=listener");
    let has_empty_host = srt_uri.starts_with("srt://:") || srt_uri.starts_with("srt://:");

    if !is_listener && !has_empty_host {
        // Already in caller mode with a host, use as-is
        return srt_uri.to_string();
    }

    // Get the current hostname from the browser (WASM) or use localhost (native)
    let hostname = get_current_hostname();

    // Parse the URI to extract port and other parameters
    // URI format: srt://[host]:port[?params]
    let uri_without_scheme = srt_uri.strip_prefix("srt://").unwrap_or(srt_uri);

    // Find the port - it's between : and ? (or end of string)
    let (host_port, params) = if let Some(q_pos) = uri_without_scheme.find('?') {
        (
            &uri_without_scheme[..q_pos],
            Some(&uri_without_scheme[q_pos + 1..]),
        )
    } else {
        (uri_without_scheme, None)
    };

    // Extract just the port (after the last colon)
    let port = if let Some(colon_pos) = host_port.rfind(':') {
        &host_port[colon_pos + 1..]
    } else {
        host_port
    };

    // Build the new URI with caller mode
    let mut new_uri = format!("srt://{}:{}", hostname, port);

    // Add parameters, but change mode to caller
    if let Some(params) = params {
        let new_params: Vec<&str> = params
            .split('&')
            .filter(|p| !p.starts_with("mode="))
            .collect();

        if new_params.is_empty() {
            new_uri.push_str("?mode=caller");
        } else {
            new_uri.push('?');
            new_uri.push_str(&new_params.join("&"));
            new_uri.push_str("&mode=caller");
        }
    } else {
        new_uri.push_str("?mode=caller");
    }

    new_uri
}

/// Get the hostname of the current server.
/// Returns "127.0.0.1" instead of "localhost" because VLC doesn't work well with localhost.
#[cfg(target_arch = "wasm32")]
fn get_current_hostname() -> String {
    let hostname = web_sys::window()
        .and_then(|w| w.location().hostname().ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // VLC doesn't work well with "localhost", use 127.0.0.1 instead
    if hostname == "localhost" {
        "127.0.0.1".to_string()
    } else {
        hostname
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn get_current_hostname() -> String {
    // VLC doesn't work well with "localhost", use 127.0.0.1 instead
    "127.0.0.1".to_string()
}

/// Theme preference for the application
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemePreference {
    System,
    Light,
    Dark,
}

/// Import format for flow import
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ImportFormat {
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
    /// Quick links to streaming endpoints
    Links,
}

/// Focus target for Ctrl+F cycling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FocusTarget {
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

/// Log message severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Informational message
    Info,
    /// Warning message
    Warning,
    /// Error message
    Error,
}

/// A log entry for pipeline messages
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Timestamp when the message was received
    pub timestamp: instant::Instant,
    /// Severity level
    pub level: LogLevel,
    /// The message content
    pub message: String,
    /// Optional source element that generated the message
    pub source: Option<String>,
    /// Optional flow ID this message relates to
    pub flow_id: Option<strom_types::FlowId>,
}

impl LogEntry {
    /// Create a new log entry
    pub fn new(
        level: LogLevel,
        message: String,
        source: Option<String>,
        flow_id: Option<strom_types::FlowId>,
    ) -> Self {
        Self {
            timestamp: instant::Instant::now(),
            level,
            message,
            source,
            flow_id,
        }
    }

    /// Get the color for this log level
    pub fn color(&self) -> Color32 {
        match self.level {
            LogLevel::Info => Color32::from_rgb(100, 180, 255),
            LogLevel::Warning => Color32::from_rgb(255, 200, 50),
            LogLevel::Error => Color32::from_rgb(255, 80, 80),
        }
    }

    /// Get the icon/prefix for this log level
    pub fn prefix(&self) -> &'static str {
        match self.level {
            LogLevel::Info => "ℹ",
            LogLevel::Warning => "⚠",
            LogLevel::Error => "✖",
        }
    }
}

// Cross-platform task spawning
#[cfg(target_arch = "wasm32")]
pub fn spawn_task<F>(future: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_task<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(future);
}

/// The main Strom application.
pub struct StromApp {
    /// API client for backend communication
    api: ApiClient,
    /// List of all flows
    flows: Vec<Flow>,
    /// Currently selected flow ID (using ID instead of index for robustness)
    selected_flow_id: Option<strom_types::FlowId>,
    /// Graph editor for the current flow
    graph: GraphEditor,
    /// Element palette
    palette: ElementPalette,
    /// Status message
    status: String,
    /// Error message
    error: Option<String>,
    /// Loading state
    loading: bool,
    /// Whether flow list needs refresh
    needs_refresh: bool,
    /// New flow name input
    new_flow_name: String,
    /// Show new flow dialog
    show_new_flow_dialog: bool,
    /// Whether elements have been loaded
    elements_loaded: bool,
    /// Whether blocks have been loaded
    blocks_loaded: bool,
    /// Flow pending deletion (for confirmation dialog)
    flow_pending_deletion: Option<(strom_types::FlowId, String)>,
    /// Flow pending copy (to be processed after render)
    flow_pending_copy: Option<Flow>,
    /// Flow ID to navigate to after next refresh
    pending_flow_navigation: Option<strom_types::FlowId>,
    /// WebSocket client for real-time updates
    ws_client: Option<WebSocketClient>,
    /// Connection state
    connection_state: ConnectionState,
    /// Channel-based state management
    channels: AppStateChannels,
    /// Flow properties being edited (flow ID)
    editing_properties_flow_id: Option<strom_types::FlowId>,
    /// Temporary name buffer for properties dialog
    properties_name_buffer: String,
    /// Temporary description buffer for properties dialog
    properties_description_buffer: String,
    /// Temporary clock type for properties dialog
    properties_clock_type_buffer: strom_types::flow::GStreamerClockType,
    /// Temporary PTP domain buffer for properties dialog
    properties_ptp_domain_buffer: String,
    /// Temporary thread priority for properties dialog
    properties_thread_priority_buffer: strom_types::flow::ThreadPriority,
    /// Shutdown flag for Ctrl+C handling (native mode only)
    #[cfg(not(target_arch = "wasm32"))]
    shutdown_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Port number for backend connection (native mode only)
    #[cfg(not(target_arch = "wasm32"))]
    port: u16,
    /// Auth token for native GUI authentication
    #[cfg(not(target_arch = "wasm32"))]
    auth_token: Option<String>,
    /// Cached network interfaces (for network interface property dropdown)
    network_interfaces: Vec<strom_types::NetworkInterfaceInfo>,
    /// Whether network interfaces have been loaded
    network_interfaces_loaded: bool,
    /// Cached available inter channels (for InterInput channel dropdown)
    available_channels: Vec<strom_types::api::AvailableOutput>,
    /// Whether available channels have been loaded
    available_channels_loaded: bool,
    /// Last InterInput block ID we refreshed channels for (to avoid repeated refreshes)
    last_inter_input_refresh: Option<String>,
    /// Meter data storage for all audio level meters
    meter_data: MeterDataStore,
    /// Media player data storage for all media player blocks
    mediaplayer_data: MediaPlayerDataStore,
    /// WebRTC stats storage for all WebRTC connections
    webrtc_stats: WebRtcStatsStore,
    /// System monitoring statistics
    system_monitor: SystemMonitorStore,
    /// PTP clock statistics per flow
    ptp_stats: crate::ptp_monitor::PtpStatsStore,
    /// QoS (buffer drop) statistics per flow/element
    qos_stats: crate::qos_monitor::QoSStore,
    /// Track when flows started (for QoS grace period)
    flow_start_times: std::collections::HashMap<strom_types::FlowId, instant::Instant>,
    /// Whether to show the detailed system monitor window
    show_system_monitor: bool,
    /// Last time WebRTC stats were polled
    last_webrtc_poll: instant::Instant,
    /// Current theme preference
    theme_preference: ThemePreference,
    /// Version information from the backend
    version_info: Option<crate::api::VersionInfo>,
    /// Login screen
    login_screen: LoginScreen,
    /// Authentication status
    auth_status: Option<AuthStatusResponse>,
    /// Whether we're checking auth status
    checking_auth: bool,
    /// Show import flow dialog
    show_import_dialog: bool,
    /// Import format mode (JSON or gst-launch)
    import_format: ImportFormat,
    /// Buffer for import text (JSON or gst-launch pipeline)
    import_json_buffer: String,
    /// Error message for import dialog
    import_error: Option<String>,
    /// Pending gst-launch export (elements, links, flow_name) - for async processing
    pending_gst_launch_export: Option<(
        Vec<strom_types::Element>,
        Vec<strom_types::element::Link>,
        String,
    )>,
    /// Cached latency info for flows (flow_id -> LatencyInfo)
    latency_cache: std::collections::HashMap<String, crate::api::LatencyInfo>,
    /// Last time latency was fetched (for periodic refresh)
    last_latency_fetch: instant::Instant,
    /// Cached stats info for flows (flow_id -> FlowStatsInfo)
    stats_cache: std::collections::HashMap<String, crate::api::FlowStatsInfo>,
    /// Last time stats was fetched (for periodic refresh)
    last_stats_fetch: instant::Instant,
    /// Whether to show the stats panel
    show_stats_panel: bool,
    /// Compositor layout editor (if open)
    compositor_editor: Option<CompositorEditor>,
    /// Playlist editor (if open)
    playlist_editor: Option<PlaylistEditor>,
    /// Log entries for pipeline messages (errors, warnings, info)
    log_entries: Vec<LogEntry>,
    /// Whether to show the log panel
    show_log_panel: bool,
    /// Maximum number of log entries to keep
    max_log_entries: usize,
    /// Current application page
    current_page: AppPage,
    /// Discovery page state
    discovery_page: crate::discovery::DiscoveryPage,
    /// Clocks page state (PTP monitoring)
    clocks_page: crate::clocks::ClocksPage,
    /// Media file browser page state
    media_page: crate::media::MediaPage,
    /// Info page state
    info_page: crate::info_page::InfoPage,
    /// Links page state
    links_page: crate::links::LinksPage,
    /// Flow list filter text
    flow_filter: String,
    /// Show stream picker modal for this block ID (when browsing discovered streams for AES67 Input)
    show_stream_picker_for_block: Option<String>,
    /// Current focus target for Ctrl+F cycling
    focus_target: FocusTarget,
    /// Request to focus the flow filter on next frame
    focus_flow_filter_requested: bool,
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
            links_page: crate::links::LinksPage::new(),
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
            links_page: crate::links::LinksPage::new(),
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
    fn setup_websocket_connection(&mut self, ctx: egui::Context) {
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
    fn current_flow(&self) -> Option<&Flow> {
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
    fn clear_flow_selection(&mut self) {
        self.selected_flow_id = None;
        self.graph.load(vec![], vec![]);
        self.graph.load_blocks(vec![]);
    }

    /// Add a log entry, maintaining the maximum size limit.
    fn add_log_entry(&mut self, entry: LogEntry) {
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
    fn load_elements(&mut self, ctx: &Context) {
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
    fn load_blocks(&mut self, ctx: &Context) {
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
    fn load_version(&mut self, ctx: egui::Context) {
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
    fn load_network_interfaces(&mut self, ctx: egui::Context) {
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
    fn poll_webrtc_stats(&mut self, ctx: &Context) {
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
    fn check_auth_status(&mut self, ctx: egui::Context) {
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
    fn handle_login(&mut self, ctx: egui::Context) {
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
    fn load_flows(&mut self, ctx: &Context) {
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
    fn fetch_latency_for_running_flows(&self, ctx: &Context) {
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
    fn fetch_stats_for_running_flows(&self, ctx: &Context) {
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
    fn create_flow(&mut self, ctx: &Context) {
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
    fn create_flow_from_sdp(&mut self, sdp: String, ctx: &Context) {
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
    fn delete_flow(&mut self, flow_id: strom_types::FlowId, ctx: &Context) {
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
    fn handle_keyboard_shortcuts(&mut self, ctx: &Context) {
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
                AppPage::Links => {
                    // No search/filters on Links page
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
    fn render_toolbar(&mut self, ctx: &Context) {
        // First top bar: System-wide controls
        TopBottomPanel::top("system_bar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Strom logo and heading as clickable link to GitHub
                    if ui
                        .add(
                            egui::Image::from_bytes("bytes://strom-icon", include_bytes!("icon.png"))
                                .fit_to_exact_size(egui::vec2(24.0, 24.0))
                                .corner_radius(4.0),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Visit Strom on GitHub")
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Eyevinn/strom"));
                    }
                    if ui
                        .heading("Strom")
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Visit Strom on GitHub")
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Eyevinn/strom"));
                    }

                    // Open Web GUI button (native mode only)
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        if ui
                            .button("Open Web GUI")
                            .on_hover_text("Open the web interface in your browser")
                            .clicked()
                        {
                            let url = format!("http://localhost:{}", self.port);
                            ctx.open_url(egui::OpenUrl::new_tab(&url));
                        }
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
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Links,
                            egui::RichText::new("Links").size(16.0),
                        )
                        .on_hover_text("Quick links to streaming endpoints")
                        .clicked()
                    {
                        self.current_page = AppPage::Links;
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
            AppPage::Links => self.render_links_toolbar(ctx),
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

                    // Show flow uptime on the right side (only for running flows)
                    if let Some(flow) = self.flows.iter().find(|f| f.id == flow_id) {
                        if let Some(ref started_at) = flow.properties.started_at {
                            if let Some(started_millis) = parse_iso8601_to_millis(started_at) {
                                let uptime_millis = current_time_millis() - started_millis;

                                // Push to right side
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    // Build tooltip text
                                    let mut tooltip = format!("Started: {}", format_datetime_local(started_at));
                                    if let Some(ref modified) = flow.properties.last_modified {
                                        tooltip.push_str(&format!("\nLast modified: {}", format_datetime_local(modified)));
                                    }

                                    ui.label(
                                        egui::RichText::new(format!("Flow uptime: {}", format_uptime(uptime_millis)))
                                            .color(Color32::GREEN)
                                    ).on_hover_text(tooltip);
                                });
                            }
                        }
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

    /// Render the links page toolbar
    fn render_links_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Links").heading());
                });
            });
    }

    /// Render the flow list sidebar.
    fn render_flow_list(&mut self, ctx: &Context) {
        SidePanel::left("flow_list")
            .default_width(200.0)
            .resizable(true)
            .show(ctx, |ui| {
                // Filter input at top
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    let filter_id = egui::Id::new("flow_list_filter");
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.flow_filter).id(filter_id));
                    if self.focus_flow_filter_requested {
                        self.focus_flow_filter_requested = false;
                        response.request_focus();
                    }
                    if !self.flow_filter.is_empty() && ui.small_button("✕").clicked() {
                        self.flow_filter.clear();
                    }
                });
                ui.add_space(4.0);

                if self.flows.is_empty() {
                    ui.label("No flows yet");
                    ui.label("Click 'New Flow' to get started");
                } else {
                    // Create sorted and filtered list of flows (by name)
                    let filter_lower = self.flow_filter.to_lowercase();
                    let mut sorted_flows: Vec<&Flow> = self
                        .flows
                        .iter()
                        .filter(|f| {
                            filter_lower.is_empty() || f.name.to_lowercase().contains(&filter_lower)
                        })
                        .collect();
                    sorted_flows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

                    if sorted_flows.is_empty() {
                        ui.label("No matching flows");
                        return;
                    }

                    // Handle keyboard navigation
                    let list_id = ui.id().with("flow_list_nav");
                    let has_focus = ui.memory(|mem| mem.has_focus(list_id));

                    if has_focus {
                        let current_idx = self
                            .selected_flow_id
                            .and_then(|sel| sorted_flows.iter().position(|f| f.id == sel));

                        ui.input(|input| {
                            if input.key_pressed(egui::Key::ArrowDown) {
                                if let Some(idx) = current_idx {
                                    if idx + 1 < sorted_flows.len() {
                                        let flow = sorted_flows[idx + 1];
                                        self.selected_flow_id = Some(flow.id);
                                        self.graph.deselect_all();
                                        self.graph.clear_runtime_dynamic_pads();
                                        self.graph.load(flow.elements.clone(), flow.links.clone());
                                        self.graph.load_blocks(flow.blocks.clone());
                                    }
                                } else {
                                    let flow = sorted_flows[0];
                                    self.selected_flow_id = Some(flow.id);
                                    self.graph.deselect_all();
                                    self.graph.clear_runtime_dynamic_pads();
                                    self.graph.load(flow.elements.clone(), flow.links.clone());
                                    self.graph.load_blocks(flow.blocks.clone());
                                }
                            } else if input.key_pressed(egui::Key::ArrowUp) {
                                if let Some(idx) = current_idx {
                                    if idx > 0 {
                                        let flow = sorted_flows[idx - 1];
                                        self.selected_flow_id = Some(flow.id);
                                        self.graph.deselect_all();
                                        self.graph.clear_runtime_dynamic_pads();
                                        self.graph.load(flow.elements.clone(), flow.links.clone());
                                        self.graph.load_blocks(flow.blocks.clone());
                                    }
                                } else if !sorted_flows.is_empty() {
                                    let flow = sorted_flows[sorted_flows.len() - 1];
                                    self.selected_flow_id = Some(flow.id);
                                    self.graph.deselect_all();
                                    self.graph.clear_runtime_dynamic_pads();
                                    self.graph.load(flow.elements.clone(), flow.links.clone());
                                    self.graph.load_blocks(flow.blocks.clone());
                                }
                            }
                        });
                    }

                    for flow in sorted_flows {
                        let selected = self.selected_flow_id == Some(flow.id);

                        // Create full-width selectable area
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 20.0),
                            egui::Sense::click(),
                        );

                        if response.clicked() {
                            // Select the flow by ID
                            self.selected_flow_id = Some(flow.id);
                            // Clear graph selection when switching flows
                            self.graph.deselect_all();
                            // Clear runtime dynamic pads (will be re-fetched if flow is running)
                            self.graph.clear_runtime_dynamic_pads();
                            // Load flow into graph editor
                            self.graph.load(flow.elements.clone(), flow.links.clone());
                            self.graph.load_blocks(flow.blocks.clone());
                            // Request focus for keyboard navigation
                            ui.memory_mut(|mem| mem.request_focus(list_id));
                        }

                        // Check for QoS issues to tint the background
                        let qos_health = self.qos_stats.get_flow_health(&flow.id);
                        let has_qos_issues = qos_health
                            .map(|h| h != crate::qos_monitor::QoSHealth::Ok)
                            .unwrap_or(false);

                        // Draw background for selected/hovered item with QoS tint
                        if selected {
                            let mut bg_color = ui.visuals().selection.bg_fill;
                            if has_qos_issues {
                                // Blend selection color with warning/critical color
                                let qos_color = qos_health.unwrap().color();
                                bg_color = Color32::from_rgba_unmultiplied(
                                    ((bg_color.r() as u16 + qos_color.r() as u16) / 2) as u8,
                                    ((bg_color.g() as u16 + qos_color.g() as u16) / 2) as u8,
                                    ((bg_color.b() as u16 + qos_color.b() as u16) / 2) as u8,
                                    bg_color.a(),
                                );
                            }
                            ui.painter().rect_filled(rect, 2.0, bg_color);
                        } else if has_qos_issues {
                            // Draw QoS warning/critical background
                            let qos_color = qos_health.unwrap().color();
                            let bg_color = Color32::from_rgba_unmultiplied(
                                qos_color.r(),
                                qos_color.g(),
                                qos_color.b(),
                                40, // Semi-transparent
                            );
                            ui.painter().rect_filled(rect, 2.0, bg_color);
                            // Also draw a left border for emphasis
                            let border_rect =
                                egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
                            ui.painter().rect_filled(border_rect, 0.0, qos_color);
                        } else if response.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                2.0,
                                ui.visuals().widgets.hovered.bg_fill,
                            );
                        }

                        // Draw flow name and buttons
                        let mut child_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        child_ui.add_space(4.0);

                        let text_color = if selected {
                            ui.visuals().selection.stroke.color
                        } else {
                            ui.visuals().text_color()
                        };

                        // Show running state icon
                        let state_icon = match flow.state {
                            Some(PipelineState::Playing) => "▶",
                            Some(PipelineState::Paused) => "⏸",
                            Some(PipelineState::Ready) | Some(PipelineState::Null) | None => "⏹",
                        };
                        let state_color = match flow.state {
                            Some(PipelineState::Playing) => Color32::from_rgb(0, 200, 0),
                            Some(PipelineState::Paused) => Color32::from_rgb(255, 165, 0),
                            Some(PipelineState::Ready) | Some(PipelineState::Null) | None => {
                                Color32::GRAY
                            }
                        };
                        child_ui.colored_label(state_color, state_icon);

                        // Show QoS indicator if there are issues - make it clickable to open log
                        if let Some(qos_health) = self.qos_stats.get_flow_health(&flow.id) {
                            if qos_health != crate::qos_monitor::QoSHealth::Ok {
                                let qos_label = child_ui
                                    .colored_label(qos_health.color(), qos_health.icon())
                                    .interact(egui::Sense::click());

                                // Click to open log panel
                                if qos_label.clicked() {
                                    self.show_log_panel = true;
                                }

                                // Show tooltip with problem elements
                                let problem_elements =
                                    self.qos_stats.get_problem_elements(&flow.id);
                                if !problem_elements.is_empty() {
                                    qos_label.on_hover_ui(|ui| {
                                        ui.label(
                                            egui::RichText::new("QoS Issues (click to view log)")
                                                .strong(),
                                        );
                                        ui.separator();
                                        for (element_id, data) in &problem_elements {
                                            let health = data.health();
                                            ui.horizontal(|ui| {
                                                ui.colored_label(health.color(), health.icon());
                                                ui.label(format!(
                                                    "{}: {:.1}%",
                                                    element_id,
                                                    data.avg_proportion * 100.0
                                                ));
                                            });
                                        }
                                    });
                                }
                            }
                        }

                        child_ui.add_space(4.0);

                        // Show flow name with hover tooltip - make it clickable too
                        let name_label = child_ui
                            .colored_label(text_color, &flow.name)
                            .interact(egui::Sense::click());

                        // Handle click on the text itself (in addition to the background)
                        if name_label.clicked() {
                            self.selected_flow_id = Some(flow.id);
                            // Clear graph selection when switching flows
                            self.graph.deselect_all();
                            self.graph.load(flow.elements.clone(), flow.links.clone());
                            self.graph.load_blocks(flow.blocks.clone());
                        }

                        // Add hover tooltip with flow details
                        name_label.on_hover_ui(|ui| {
                            ui.label(egui::RichText::new(&flow.name).strong());
                            ui.separator();

                            if let Some(ref desc) = flow.properties.description {
                                if !desc.is_empty() {
                                    ui.label("Description:");
                                    ui.label(desc);
                                    ui.add_space(5.0);
                                }
                            }

                            ui.label(format!("Clock: {:?}", flow.properties.clock_type));

                            if let Some(domain) = flow.properties.ptp_domain {
                                ui.label(format!("PTP Domain: {}", domain));
                            }

                            if let Some(sync_status) = flow.properties.clock_sync_status {
                                use strom_types::flow::ClockSyncStatus;
                                let status_text = match sync_status {
                                    ClockSyncStatus::Synced => "Synced",
                                    ClockSyncStatus::NotSynced => "Not Synced",
                                    ClockSyncStatus::Unknown => "Unknown",
                                };
                                ui.label(format!("Sync Status: {}", status_text));
                            }

                            // Display PTP grandmaster info if available
                            if let Some(ref ptp_info) = flow.properties.ptp_info {
                                if let Some(ref gm) = ptp_info.grandmaster_clock_id {
                                    ui.label(format!("Grandmaster: {}", gm));
                                }
                            }

                            ui.add_space(5.0);
                            let state_text = match flow.state {
                                Some(PipelineState::Playing) => "Running",
                                Some(PipelineState::Paused) => "Paused",
                                Some(PipelineState::Ready) | Some(PipelineState::Null) | None => {
                                    "Stopped"
                                }
                            };
                            ui.label(format!("State: {}", state_text));

                            // Show timestamps
                            if flow.properties.started_at.is_some()
                                || flow.properties.last_modified.is_some()
                                || flow.properties.created_at.is_some()
                            {
                                ui.add_space(5.0);
                                ui.separator();

                                if let Some(ref started_at) = flow.properties.started_at {
                                    ui.label(format!(
                                        "Started: {}",
                                        format_datetime_local(started_at)
                                    ));
                                    if let Some(started_millis) =
                                        parse_iso8601_to_millis(started_at)
                                    {
                                        let uptime_millis = current_time_millis() - started_millis;
                                        ui.label(format!(
                                            "Uptime: {}",
                                            format_uptime(uptime_millis)
                                        ));
                                    }
                                }

                                if let Some(ref modified) = flow.properties.last_modified {
                                    ui.label(format!(
                                        "Last modified: {}",
                                        format_datetime_local(modified)
                                    ));
                                }

                                if let Some(ref created) = flow.properties.created_at {
                                    ui.label(format!(
                                        "Created: {}",
                                        format_datetime_local(created)
                                    ));
                                }
                            }
                        });

                        // Buttons on the right
                        child_ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add_space(4.0);

                                // Single menu button with dropdown
                                ui.menu_button("...", |ui| {
                                    ui.set_min_width(150.0);

                                    // Properties
                                    if ui.button("⚙  Properties").clicked() {
                                        self.editing_properties_flow_id = Some(flow.id);
                                        self.properties_name_buffer = flow.name.clone();
                                        self.properties_description_buffer =
                                            flow.properties.description.clone().unwrap_or_default();
                                        self.properties_clock_type_buffer =
                                            flow.properties.clock_type;
                                        self.properties_ptp_domain_buffer = flow
                                            .properties
                                            .ptp_domain
                                            .map(|d| d.to_string())
                                            .unwrap_or_else(|| "0".to_string());
                                        self.properties_thread_priority_buffer =
                                            flow.properties.thread_priority;
                                        ui.close();
                                    }

                                    ui.separator();

                                    // Export as JSON
                                    if ui.button("📤  Export as JSON").clicked() {
                                        match serde_json::to_string_pretty(flow) {
                                            Ok(json) => {
                                                ui.ctx().copy_text(json);
                                                self.status = format!(
                                                    "Flow '{}' exported to clipboard as JSON",
                                                    flow.name
                                                );
                                            }
                                            Err(e) => {
                                                self.error =
                                                    Some(format!("Failed to export flow: {}", e));
                                            }
                                        }
                                        ui.close();
                                    }

                                    // Export to gst-launch (only if flow has elements, not blocks)
                                    let has_only_elements =
                                        !flow.elements.is_empty() && flow.blocks.is_empty();
                                    let tooltip = if has_only_elements {
                                        "Export as gst-launch-1.0 pipeline"
                                    } else {
                                        "Only available for flows with elements, not blocks"
                                    };
                                    if ui
                                        .add_enabled(
                                            has_only_elements,
                                            egui::Button::new("🖥  Export as gst-launch"),
                                        )
                                        .on_hover_text(tooltip)
                                        .clicked()
                                        && has_only_elements
                                    {
                                        self.pending_gst_launch_export = Some((
                                            flow.elements.clone(),
                                            flow.links.clone(),
                                            flow.name.clone(),
                                        ));
                                        ui.close();
                                    }

                                    ui.separator();

                                    // Copy flow
                                    if ui.button("📋  Copy").clicked() {
                                        self.flow_pending_copy = Some(flow.clone());
                                        ui.close();
                                    }

                                    // Delete flow
                                    if ui.button("🗑  Delete").clicked() {
                                        self.flow_pending_deletion =
                                            Some((flow.id, flow.name.clone()));
                                        ui.close();
                                    }
                                });

                                // Show clock sync indicator for PTP/NTP (small colored dot)
                                use strom_types::flow::{ClockSyncStatus, GStreamerClockType};
                                if matches!(
                                    flow.properties.clock_type,
                                    GStreamerClockType::Ptp | GStreamerClockType::Ntp
                                ) {
                                    let (text_color, tooltip) = match flow
                                        .properties
                                        .clock_sync_status
                                    {
                                        Some(ClockSyncStatus::Synced) => (
                                            Color32::from_rgb(0, 200, 0),
                                            format!(
                                                "{:?} - Synchronized",
                                                flow.properties.clock_type
                                            ),
                                        ),
                                        Some(ClockSyncStatus::NotSynced) => (
                                            Color32::from_rgb(200, 0, 0),
                                            format!(
                                                "{:?} - Not Synchronized",
                                                flow.properties.clock_type
                                            ),
                                        ),
                                        Some(ClockSyncStatus::Unknown) | None => (
                                            Color32::GRAY,
                                            format!("{:?} - Unknown", flow.properties.clock_type),
                                        ),
                                    };

                                    // Small colored dot indicator
                                    ui.add_space(4.0);
                                    ui.add(egui::Label::new(
                                        egui::RichText::new("*").size(12.0).color(text_color),
                                    ))
                                    .on_hover_text(tooltip);
                                }

                                // Show thread priority warning indicator if priority not achieved
                                if let Some(ref status) = flow.properties.thread_priority_status {
                                    if !status.achieved && status.error.is_some() {
                                        let warning_color = Color32::from_rgb(255, 165, 0);
                                        let tooltip = status
                                            .error
                                            .as_ref()
                                            .map(|e| format!("Thread priority not set: {}", e))
                                            .unwrap_or_else(|| {
                                                "Thread priority warning".to_string()
                                            });

                                        ui.add_space(2.0);
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new("⚠")
                                                    .size(12.0)
                                                    .color(warning_color),
                                            )
                                            .sense(egui::Sense::hover()),
                                        )
                                        .on_hover_text(tooltip);
                                    }
                                }
                            },
                        );
                    }
                }
            });
    }

    /// Render the element palette sidebar.
    fn render_palette(&mut self, ctx: &Context) {
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

                        // Handle WHEP player request (for WHEP Output)
                        if let Some(endpoint_id) = result.whep_player_url {
                            let player_url = self.api.get_whep_player_url(&endpoint_id);
                            ctx.open_url(egui::OpenUrl::new_tab(&player_url));
                        }

                        // Handle copy WHEP URL to clipboard
                        if let Some(endpoint_id) = result.copy_whep_url_requested {
                            let player_url = self.api.get_whep_player_url(&endpoint_id);
                            ctx.copy_text(player_url);
                            self.status = "Player URL copied to clipboard".to_string();
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
    fn render_canvas(&mut self, ctx: &Context) {
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
    fn render_status_bar(&mut self, ctx: &Context) {
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
    fn render_log_panel(&mut self, ctx: &Context) {
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

    /// Render the new flow dialog.
    fn render_new_flow_dialog(&mut self, ctx: &Context) {
        if !self.show_new_flow_dialog {
            return;
        }

        egui::Window::new("New Flow")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut self.new_flow_name);
                });

                // Check for Enter key to create flow
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.new_flow_name.is_empty() {
                    self.create_flow(ctx);
                }

                ui.horizontal(|ui| {
                    if ui.button("Create").clicked() {
                        self.create_flow(ctx);
                    }

                    if ui.button("Cancel").clicked() {
                        self.show_new_flow_dialog = false;
                        self.new_flow_name.clear();
                    }
                });
            });
    }

    /// Render the delete confirmation dialog.
    fn render_delete_confirmation_dialog(&mut self, ctx: &Context) {
        if self.flow_pending_deletion.is_none() {
            return;
        }

        let (flow_id, flow_name) = self.flow_pending_deletion.as_ref().unwrap().clone();

        egui::Window::new("Delete Flow")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Are you sure you want to delete this flow?");
                ui.add_space(5.0);
                ui.colored_label(Color32::YELLOW, format!("Flow: {}", flow_name));
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if ui.button("❌ Delete").clicked() {
                        self.delete_flow(flow_id, ctx);
                        self.flow_pending_deletion = None;
                    }

                    if ui.button("Cancel").clicked() {
                        self.flow_pending_deletion = None;
                    }
                });
            });
    }

    /// Render the system monitor window.
    fn render_system_monitor_window(&mut self, ctx: &Context) {
        if !self.show_system_monitor {
            return;
        }

        egui::Window::new("System Monitoring")
            .collapsible(true)
            .resizable(true)
            .default_width(700.0)
            .default_height(500.0)
            .open(&mut self.show_system_monitor)
            .show(ctx, |ui| {
                crate::system_monitor::DetailedSystemMonitor::new(&self.system_monitor).show(ui);
            });
    }

    /// Render the flow properties dialog.
    fn render_flow_properties_dialog(&mut self, ctx: &Context) {
        let flow_id = match self.editing_properties_flow_id {
            Some(id) => id,
            None => return,
        };

        let flow = match self.flows.iter().find(|f| f.id == flow_id) {
            Some(f) => f,
            None => {
                self.editing_properties_flow_id = None;
                return;
            }
        };

        let flow_name = flow.name.clone();

        egui::Window::new(format!("⚙ {} - Properties", flow_name))
            .collapsible(false)
            .resizable(true)
            .default_width(400.0)
            .default_height(500.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 50.0) // Leave room for buttons
                    .show(ui, |ui| {
                ui.heading("Flow Properties");
                ui.add_space(5.0);

                // Name
                ui.label("Name:");
                ui.text_edit_singleline(&mut self.properties_name_buffer);
                ui.add_space(10.0);

                // Description
                ui.label("Description:");
                ui.add(
                    egui::TextEdit::multiline(&mut self.properties_description_buffer)
                        .desired_width(f32::INFINITY)
                        .desired_rows(5)
                        .hint_text("Optional description for this flow..."),
                );

                ui.add_space(10.0);

                // Clock Type
                ui.label("Clock Type:");
                ui.horizontal(|ui| {
                    use strom_types::flow::GStreamerClockType;

                    egui::ComboBox::from_id_salt("clock_type_selector")
                        .selected_text(self.properties_clock_type_buffer.label())
                        .show_ui(ui, |ui| {
                            for clock_type in GStreamerClockType::all() {
                                let label = if *clock_type == GStreamerClockType::Monotonic {
                                    format!("{} (recommended)", clock_type.label())
                                } else {
                                    clock_type.label().to_string()
                                };
                                ui.selectable_value(
                                    &mut self.properties_clock_type_buffer,
                                    *clock_type,
                                    label,
                                );
                            }
                        });
                });

                // Show description of selected clock type
                ui.label(self.properties_clock_type_buffer.description());

                // Show PTP domain field only when PTP is selected
                if matches!(
                    self.properties_clock_type_buffer,
                    strom_types::flow::GStreamerClockType::Ptp
                ) {
                    ui.add_space(10.0);
                    ui.label("PTP Domain (0-255):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.properties_ptp_domain_buffer)
                            .desired_width(100.0)
                            .hint_text("0"),
                    );
                    ui.label("The PTP domain for clock synchronization");
                }

                // Show clock sync status for PTP/NTP clocks
                if matches!(
                    self.properties_clock_type_buffer,
                    strom_types::flow::GStreamerClockType::Ptp
                        | strom_types::flow::GStreamerClockType::Ntp
                ) {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.label("Clock Status:");
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                            if let Some(sync_status) = flow.properties.clock_sync_status {
                                use strom_types::flow::ClockSyncStatus;
                                match sync_status {
                                    ClockSyncStatus::Synced => {
                                        ui.colored_label(Color32::from_rgb(0, 200, 0), "[OK] Synced");
                                    }
                                    ClockSyncStatus::NotSynced => {
                                        ui.colored_label(
                                            Color32::from_rgb(200, 0, 0),
                                            "[!] Not Synced",
                                        );
                                    }
                                    ClockSyncStatus::Unknown => {
                                        ui.colored_label(Color32::GRAY, "[-] Unknown");
                                    }
                                }
                            } else {
                                ui.colored_label(Color32::GRAY, "[-] Unknown");
                            }
                        }
                    });

                    // Show PTP-specific options and link to Clocks page
                    if matches!(
                        self.properties_clock_type_buffer,
                        strom_types::flow::GStreamerClockType::Ptp
                    ) {
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                            ui.add_space(5.0);

                            // Show warning if restart needed - compare buffer with running domain
                            if let Some(ref ptp_info) = flow.properties.ptp_info {
                                let buffer_domain: u8 = self
                                    .properties_ptp_domain_buffer
                                    .parse()
                                    .unwrap_or(0);
                                let domain_changed = buffer_domain != ptp_info.domain;
                                if domain_changed {
                                    ui.colored_label(
                                        Color32::from_rgb(255, 165, 0),
                                        "! Restart needed - domain changed",
                                    );
                                }
                            }

                            // Button to open Clocks page for detailed stats
                            ui.add_space(5.0);
                            if ui
                                .button("View PTP Statistics")
                                .on_hover_text("Open Clocks page for detailed PTP statistics")
                                .clicked()
                            {
                                self.current_page = AppPage::Clocks;
                            }
                        }
                    }
                }

                ui.add_space(15.0);
                ui.separator();
                ui.add_space(10.0);

                // Thread Priority
                ui.label("Thread Priority:");
                ui.horizontal(|ui| {
                    use strom_types::flow::ThreadPriority;

                    egui::ComboBox::from_id_salt("thread_priority_selector")
                        .selected_text(format!("{:?}", self.properties_thread_priority_buffer))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::Normal,
                                "Normal",
                            );
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::High,
                                "High (recommended)",
                            );
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::Realtime,
                                "Realtime (requires privileges)",
                            );
                        });
                });

                // Show description of selected thread priority
                ui.label(self.properties_thread_priority_buffer.description());

                // Show thread priority status for running pipelines
                if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                    if let Some(ref status) = flow.properties.thread_priority_status {
                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.label("Status:");
                            if status.achieved {
                                ui.colored_label(
                                    Color32::from_rgb(0, 200, 0),
                                    format!("[OK] Achieved ({} threads)", status.threads_configured),
                                );
                            } else if let Some(ref err) = status.error {
                                ui.colored_label(Color32::from_rgb(255, 165, 0), "[!] Warning");
                                ui.label(format!("- {}", err));
                            } else {
                                ui.colored_label(Color32::GRAY, "[-] Not set");
                            }
                        });
                    }
                }

                // Show timestamps section
                if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                    let has_timestamps = flow.properties.created_at.is_some()
                        || flow.properties.last_modified.is_some()
                        || flow.properties.started_at.is_some();

                    if has_timestamps {
                        ui.add_space(15.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Timestamps").strong());

                        egui::Grid::new("timestamps_grid")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                if let Some(ref created) = flow.properties.created_at {
                                    ui.label("Created:");
                                    ui.label(format_datetime_local(created));
                                    ui.end_row();
                                }

                                if let Some(ref modified) = flow.properties.last_modified {
                                    ui.label("Last modified:");
                                    ui.label(format_datetime_local(modified));
                                    ui.end_row();
                                }

                                if let Some(ref started) = flow.properties.started_at {
                                    ui.label("Started:");
                                    ui.label(format_datetime_local(started));
                                    ui.end_row();

                                    // Show uptime
                                    if let Some(started_millis) = parse_iso8601_to_millis(started) {
                                        let uptime_millis = current_time_millis() - started_millis;
                                        ui.label("Uptime:");
                                        ui.label(format_uptime(uptime_millis));
                                        ui.end_row();
                                    }
                                }
                            });
                    }
                }

                }); // End ScrollArea

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(5.0);

                // Buttons (outside scroll area)
                ui.horizontal(|ui| {
                    if ui.button("💾 Save").clicked() {
                        // Update flow properties
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter_mut().find(|f| f.id == id)) {
                            // Update flow name
                            flow.name = self.properties_name_buffer.clone();

                            flow.properties.description =
                                if self.properties_description_buffer.is_empty() {
                                    None
                                } else {
                                    Some(self.properties_description_buffer.clone())
                                };
                            flow.properties.clock_type = self.properties_clock_type_buffer;

                            // Parse and set PTP domain if PTP clock is selected
                            flow.properties.ptp_domain = if matches!(
                                self.properties_clock_type_buffer,
                                strom_types::flow::GStreamerClockType::Ptp
                            ) {
                                self.properties_ptp_domain_buffer.parse::<u8>().ok()
                            } else {
                                None
                            };

                            // Set thread priority
                            flow.properties.thread_priority =
                                self.properties_thread_priority_buffer;

                            let flow_clone = flow.clone();
                            let api = self.api.clone();
                            let ctx_clone = ctx.clone();

                            spawn_task(async move {
                                match api.update_flow(&flow_clone).await {
                                    Ok(_) => {
                                        tracing::info!("Flow properties updated successfully - WebSocket event will trigger refresh");
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to update flow properties: {}", e);
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        self.editing_properties_flow_id = None;
                    }

                    if ui.button("Cancel").clicked() {
                        self.editing_properties_flow_id = None;
                    }
                });
            });
    }

    /// Render the stream picker modal for selecting discovered streams.
    fn render_stream_picker_modal(&mut self, ctx: &Context) {
        let Some(block_id) = self.show_stream_picker_for_block.clone() else {
            return;
        };

        let mut close_modal = false;
        let mut selected_sdp: Option<String> = None;

        egui::Window::new("Select Discovered Stream")
            .collapsible(false)
            .resizable(true)
            .default_width(500.0)
            .default_height(400.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Select a stream to use its SDP:");
                ui.add_space(8.0);

                let streams = &self.discovery_page.discovered_streams;
                let is_loading = self.discovery_page.loading;

                if is_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading discovered streams...");
                    });
                } else if streams.is_empty() {
                    ui.label("No discovered streams available.");
                    ui.label("Make sure SAP discovery is running and streams are being announced on the network.");
                    ui.add_space(8.0);
                    if ui.button("🔄 Refresh").clicked() {
                        self.discovery_page.refresh(&self.api, ctx, &self.channels.tx);
                    }
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for stream in streams {
                                let text = format!(
                                    "{} - {}:{} ({}ch {}Hz)",
                                    stream.name,
                                    stream.multicast_address,
                                    stream.port,
                                    stream.channels,
                                    stream.sample_rate
                                );

                                if ui.selectable_label(false, &text).clicked() {
                                    // Fetch SDP for this stream
                                    // For now, we'll construct it from the stream info
                                    // In a real implementation, we'd fetch the actual SDP
                                    selected_sdp = Some(stream.id.clone());
                                }
                            }
                        });
                }

                ui.add_space(8.0);
                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close_modal = true;
                    }
                });
            });

        if close_modal {
            self.show_stream_picker_for_block = None;
        }

        // If a stream was selected, fetch its SDP and update the block
        if let Some(stream_id) = selected_sdp {
            self.show_stream_picker_for_block = None;

            // Fetch the SDP and update the block
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            spawn_task(async move {
                match api.get_stream_sdp(&stream_id).await {
                    Ok(sdp) => {
                        tracing::info!(
                            "Fetched SDP for stream {}, sending to block {}",
                            stream_id,
                            block_id
                        );
                        let _ = tx.send(AppMessage::StreamPickerSdpLoaded { block_id, sdp });
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch stream SDP for {}: {}", stream_id, e);
                        let _ = tx.send(AppMessage::FlowOperationError(format!(
                            "Failed to fetch stream SDP: {}",
                            e
                        )));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Render the import flow dialog.
    fn render_import_dialog(&mut self, ctx: &Context) {
        if !self.show_import_dialog {
            return;
        }

        egui::Window::new("Import Flow")
            .collapsible(false)
            .resizable(true)
            .default_width(550.0)
            .default_height(450.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                // Format selection tabs
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    ui.add_space(10.0);
                    if ui
                        .selectable_label(self.import_format == ImportFormat::Json, "JSON")
                        .clicked()
                    {
                        self.import_format = ImportFormat::Json;
                        self.import_error = None;
                    }
                    if ui
                        .selectable_label(self.import_format == ImportFormat::GstLaunch, "gst-launch")
                        .clicked()
                    {
                        self.import_format = ImportFormat::GstLaunch;
                        self.import_error = None;
                    }
                });

                ui.add_space(5.0);
                ui.separator();
                ui.add_space(5.0);

                // Format-specific instructions
                match self.import_format {
                    ImportFormat::Json => {
                        ui.label("Paste flow JSON below:");
                    }
                    ImportFormat::GstLaunch => {
                        ui.label("Paste gst-launch-1.0 pipeline below, or click an example:");
                        ui.add_space(5.0);

                        // Example pipelines in a collapsible section
                        egui::CollapsingHeader::new("Examples")
                            .default_open(true)
                            .show(ui, |ui| {
                                let examples = [
                                    ("Test Video", "videotestsrc pattern=ball is-live=true ! videoconvert ! autovideosink"),
                                    ("Test Audio", "audiotestsrc wave=sine freq=440 is-live=true ! audioconvert ! autoaudiosink"),
                                    ("Video + Overlay", "videotestsrc is-live=true ! clockoverlay ! videoconvert ! autovideosink"),
                                    ("Record Video", "videotestsrc num-buffers=300 is-live=true ! x264enc ! mp4mux ! filesink location=test.mp4"),
                                    ("RTP Stream Send", "videotestsrc is-live=true ! x264enc tune=zerolatency bitrate=500 ! rtph264pay ! udpsink port=5000 host=127.0.0.1"),
                                    ("RTP Stream Receive", "udpsrc ! application/x-rtp,payload=96 ! rtph264depay ! avdec_h264 ! videoconvert ! autovideosink"),
                                    ("Record + Display", "videotestsrc is-live=true ! tee name=t t. ! queue ! x264enc ! mp4mux ! filesink location=output.mp4 t. ! queue ! autovideosink"),
                                    ("AV Mux", "videotestsrc is-live=true ! x264enc ! mp4mux name=mux ! filesink location=av.mp4 audiotestsrc is-live=true ! lamemp3enc ! mux."),
                                    ("File Playback", "filesrc location=video.mp4 ! decodebin ! videoconvert ! autovideosink"),
                                    ("Camera", "v4l2src ! videoconvert ! autovideosink"),
                                ];

                                ui.horizontal_wrapped(|ui| {
                                    for (name, pipeline) in examples {
                                        if ui.small_button(name).on_hover_text(pipeline).clicked() {
                                            self.import_json_buffer = pipeline.to_string();
                                        }
                                    }
                                });
                            });
                    }
                }
                ui.add_space(5.0);

                // Large text area for input
                let hint_text = match self.import_format {
                    ImportFormat::Json => "Paste flow JSON here...",
                    ImportFormat::GstLaunch => "videotestsrc ! videoconvert ! autovideosink",
                };

                egui::ScrollArea::vertical()
                    .max_height(280.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.import_json_buffer)
                                .desired_width(f32::INFINITY)
                                .desired_rows(12)
                                .font(egui::TextStyle::Monospace)
                                .hint_text(hint_text),
                        );
                    });

                // Show error if any
                if let Some(ref error) = self.import_error {
                    ui.add_space(5.0);
                    ui.colored_label(Color32::RED, error);
                }

                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if ui.button("📥 Import").clicked() {
                        match self.import_format {
                            ImportFormat::Json => self.import_flow_from_json(ctx),
                            ImportFormat::GstLaunch => self.import_flow_from_gst_launch(ctx),
                        }
                    }

                    if ui.button("Cancel").clicked() {
                        self.show_import_dialog = false;
                        self.import_json_buffer.clear();
                        self.import_error = None;
                    }
                });
            });
    }

    /// Import a flow from the JSON buffer.
    /// Note: The backend's create_flow only takes a name, so we create first then update.
    fn import_flow_from_json(&mut self, ctx: &Context) {
        if self.import_json_buffer.trim().is_empty() {
            self.import_error = Some("Please paste flow JSON first".to_string());
            return;
        }

        // Try to parse the JSON as a Flow
        match serde_json::from_str::<Flow>(&self.import_json_buffer) {
            Ok(flow) => {
                // Regenerate all IDs to avoid conflicts
                let flow = Self::regenerate_flow_ids(flow);

                let api = self.api.clone();
                let tx = self.channels.sender();
                let ctx = ctx.clone();
                let flow_name = flow.name.clone();

                self.status = format!("Importing flow '{}'...", flow_name);
                self.show_import_dialog = false;
                self.import_json_buffer.clear();
                self.import_error = None;

                spawn_task(async move {
                    // Step 1: Create an empty flow with the name
                    match api.create_flow(&flow).await {
                        Ok(created_flow) => {
                            tracing::info!(
                                "Empty flow created: {} ({}), now updating with content...",
                                created_flow.name,
                                created_flow.id
                            );

                            // Step 2: Update the created flow with the full content
                            let mut full_flow = flow.clone();
                            full_flow.id = created_flow.id;
                            let flow_id = created_flow.id;

                            match api.update_flow(&full_flow).await {
                                Ok(_) => {
                                    tracing::info!(
                                        "Flow imported successfully: {} - WebSocket event will trigger refresh",
                                        flow_name
                                    );
                                    let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                        "Flow '{}' imported",
                                        flow_name
                                    )));
                                    // Navigate to imported flow
                                    let _ = tx.send(AppMessage::FlowCreated(flow_id));
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to update imported flow with content: {}",
                                        e
                                    );
                                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                                        "Failed to import flow: {}",
                                        e
                                    )));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to create flow for import: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to import flow: {}",
                                e
                            )));
                        }
                    }
                    ctx.request_repaint();
                });
            }
            Err(e) => {
                self.import_error = Some(format!("Invalid JSON: {}", e));
            }
        }
    }

    /// Import a flow from gst-launch-1.0 syntax.
    /// Parses the pipeline using the backend's GStreamer parser and creates a new flow.
    fn import_flow_from_gst_launch(&mut self, ctx: &Context) {
        let pipeline = self.import_json_buffer.trim();
        if pipeline.is_empty() {
            self.import_error = Some("Please enter a gst-launch pipeline".to_string());
            return;
        }

        // Strip leading "gst-launch-1.0 " if present
        let pipeline = pipeline
            .strip_prefix("gst-launch-1.0 ")
            .or_else(|| pipeline.strip_prefix("gst-launch "))
            .unwrap_or(pipeline)
            .to_string();

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();

        self.status = "Parsing gst-launch pipeline...".to_string();
        self.show_import_dialog = false;
        self.import_json_buffer.clear();
        self.import_error = None;

        spawn_task(async move {
            // Step 1: Parse the pipeline using the backend
            match api.parse_gst_launch(&pipeline).await {
                Ok(parsed) => {
                    if parsed.elements.is_empty() {
                        let _ = tx.send(AppMessage::FlowOperationError(
                            "No elements found in pipeline".to_string(),
                        ));
                        ctx.request_repaint();
                        return;
                    }

                    // Step 2: Create a new flow with a name based on first element
                    // Add random suffix to make each import unique
                    let unique_id = &uuid::Uuid::new_v4().to_string()[..8];
                    let flow_name = format!(
                        "Imported: {} ({})",
                        parsed
                            .elements
                            .first()
                            .map(|e| e.element_type.as_str())
                            .unwrap_or("pipeline"),
                        unique_id
                    );

                    let mut new_flow = Flow::new(&flow_name);
                    new_flow.elements = parsed.elements;
                    new_flow.links = parsed.links;

                    // Save the original gst-launch syntax in the description
                    new_flow.properties.description = Some(format!(
                        "Imported from gst-launch-1.0:\n\n```\n{}\n```",
                        pipeline
                    ));

                    // Step 3: Create the flow via API
                    match api.create_flow(&new_flow).await {
                        Ok(created_flow) => {
                            tracing::info!(
                                "Flow created from gst-launch: {} ({})",
                                created_flow.name,
                                created_flow.id
                            );

                            // Step 4: Update with the parsed content
                            let mut full_flow = new_flow.clone();
                            full_flow.id = created_flow.id;
                            let flow_id = created_flow.id;

                            match api.update_flow(&full_flow).await {
                                Ok(_) => {
                                    tracing::info!(
                                        "Flow imported from gst-launch successfully: {}",
                                        flow_name
                                    );
                                    let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                        "Flow '{}' imported from gst-launch",
                                        flow_name
                                    )));
                                    let _ = tx.send(AppMessage::FlowCreated(flow_id));
                                }
                                Err(e) => {
                                    tracing::error!("Failed to update imported flow: {}", e);
                                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                                        "Failed to import flow: {}",
                                        e
                                    )));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to create flow from gst-launch: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to create flow: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to parse gst-launch pipeline: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to parse pipeline: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Regenerate all IDs in a flow (flow ID, element IDs, block IDs) and update links.
    /// This is used for both import and copy operations to avoid ID conflicts.
    fn regenerate_flow_ids(mut flow: Flow) -> Flow {
        use std::collections::HashMap;

        // Generate new flow ID
        flow.id = uuid::Uuid::new_v4();

        // Reset state to Null
        flow.state = Some(PipelineState::Null);

        // Clear auto_restart flag
        flow.properties.auto_restart = false;

        // Clear runtime data (e.g., SDP for AES67 blocks)
        for block in &mut flow.blocks {
            block.runtime_data = None;
        }

        // Build mapping of old IDs to new IDs for elements
        let mut element_id_map: HashMap<String, String> = HashMap::new();
        for element in &mut flow.elements {
            let old_id = element.id.clone();
            let new_id = format!("e{}", uuid::Uuid::new_v4().simple());
            element_id_map.insert(old_id, new_id.clone());
            element.id = new_id;
        }

        // Build mapping of old IDs to new IDs for blocks
        let mut block_id_map: HashMap<String, String> = HashMap::new();
        for block in &mut flow.blocks {
            let old_id = block.id.clone();
            let new_id = format!("b{}", uuid::Uuid::new_v4().simple());
            block_id_map.insert(old_id, new_id.clone());
            block.id = new_id;
        }

        // Update links to use new IDs
        for link in &mut flow.links {
            // Update 'from' reference (format: "element_id:pad_name")
            if let Some((old_id, pad_name)) = link.from.split_once(':') {
                if let Some(new_id) = element_id_map.get(old_id) {
                    link.from = format!("{}:{}", new_id, pad_name);
                } else if let Some(new_id) = block_id_map.get(old_id) {
                    link.from = format!("{}:{}", new_id, pad_name);
                }
            }

            // Update 'to' reference (format: "element_id:pad_name")
            if let Some((old_id, pad_name)) = link.to.split_once(':') {
                if let Some(new_id) = element_id_map.get(old_id) {
                    link.to = format!("{}:{}", new_id, pad_name);
                } else if let Some(new_id) = block_id_map.get(old_id) {
                    link.to = format!("{}:{}", new_id, pad_name);
                }
            }
        }

        flow
    }

    /// Copy a flow with regenerated IDs and create it on the backend.
    /// Note: The backend's create_flow only takes a name, so we create first then update.
    fn copy_flow(&mut self, flow: &Flow, ctx: &Context) {
        let mut flow_copy = flow.clone();

        // Add " (copy)" suffix to the name
        flow_copy.name = format!("{} (copy)", flow.name);

        // Regenerate all IDs
        let flow_copy = Self::regenerate_flow_ids(flow_copy);

        let api = self.api.clone();
        let tx = self.channels.sender();
        let ctx = ctx.clone();
        let flow_name = flow_copy.name.clone();

        self.status = format!("Copying flow '{}'...", flow.name);

        spawn_task(async move {
            // Step 1: Create an empty flow with the name
            match api.create_flow(&flow_copy).await {
                Ok(created_flow) => {
                    tracing::info!(
                        "Empty flow created: {} ({}), now updating with content...",
                        created_flow.name,
                        created_flow.id
                    );

                    // Step 2: Update the created flow with the full content
                    // Use the ID from the created flow
                    let mut full_flow = flow_copy.clone();
                    full_flow.id = created_flow.id;
                    let flow_id = created_flow.id;

                    match api.update_flow(&full_flow).await {
                        Ok(_) => {
                            tracing::info!(
                                "Flow copied successfully: {} - WebSocket event will trigger refresh",
                                flow_name
                            );
                            let _ = tx.send(AppMessage::FlowOperationSuccess(format!(
                                "Flow '{}' created",
                                flow_name
                            )));
                            // Navigate to copied flow
                            let _ = tx.send(AppMessage::FlowCreated(flow_id));
                        }
                        Err(e) => {
                            tracing::error!("Failed to update copied flow with content: {}", e);
                            let _ = tx.send(AppMessage::FlowOperationError(format!(
                                "Failed to copy flow: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create flow for copy: {}", e);
                    let _ = tx.send(AppMessage::FlowOperationError(format!(
                        "Failed to copy flow: {}",
                        e
                    )));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Render the full-screen disconnect overlay when WebSocket is not connected.
    fn render_disconnect_overlay(&mut self, ctx: &Context) {
        CentralPanel::default().show(ctx, |ui| {
            // Center everything vertically and horizontally
            ui.vertical_centered(|ui| {
                // Add vertical spacing to center content
                let available_height = ui.available_height();
                ui.add_space(available_height * 0.35);

                // Show large icon and status based on connection state
                match self.connection_state {
                    ConnectionState::Disconnected => {
                        ui.heading(
                            egui::RichText::new("⚠")
                                .size(80.0)
                                .color(Color32::from_rgb(255, 165, 0))
                        );
                        ui.add_space(20.0);
                        ui.heading(
                            egui::RichText::new("Disconnected from Backend")
                                .size(32.0)
                                .color(Color32::from_rgb(200, 200, 200))
                        );
                    }
                    ConnectionState::Reconnecting { attempt } => {
                        // Animated spinner
                        ui.add(egui::Spinner::new().size(80.0));
                        ui.add_space(20.0);
                        ui.heading(
                            egui::RichText::new(format!("Reconnecting (Attempt {})", attempt))
                                .size(32.0)
                                .color(Color32::from_rgb(200, 200, 200))
                        );
                    }
                    ConnectionState::Connected => {
                        // Should not reach here, but just in case
                        ui.heading(
                            egui::RichText::new("✓")
                                .size(80.0)
                                .color(Color32::from_rgb(0, 200, 0))
                        );
                        ui.add_space(20.0);
                        ui.heading(
                            egui::RichText::new("Connected")
                                .size(32.0)
                                .color(Color32::from_rgb(200, 200, 200))
                        );
                    }
                }

                ui.add_space(15.0);
                ui.label(
                    egui::RichText::new("Please wait while we attempt to reconnect to the Strom backend...")
                        .size(16.0)
                        .color(Color32::from_rgb(150, 150, 150))
                );

                ui.add_space(30.0);
                ui.separator();
                ui.add_space(10.0);

                // Show connection details
                ui.label(
                    egui::RichText::new("The application will automatically reconnect when the backend is available.")
                        .size(14.0)
                        .color(Color32::from_rgb(120, 120, 120))
                );
            });
        });
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

        // Process all pending channel messages
        while let Ok(msg) = self.channels.rx.try_recv() {
            match msg {
                AppMessage::FlowsLoaded(flows) => {
                    tracing::info!("Received FlowsLoaded: {} flows", flows.len());

                    // Remember the previously selected flow ID (using ID, not index!)
                    let previously_selected_id = self.selected_flow_id;

                    self.flows = flows;
                    self.status = format!("Loaded {} flows", self.flows.len());
                    self.loading = false;

                    // Check if there's a pending flow navigation (takes priority)
                    if let Some(pending_flow_id) = self.pending_flow_navigation.take() {
                        tracing::info!(
                            "Processing pending navigation to flow ID: {}",
                            pending_flow_id
                        );
                        if let Some(flow) = self.flows.iter().find(|f| f.id == pending_flow_id) {
                            self.selected_flow_id = Some(pending_flow_id);
                            // Clear graph selection and load the new flow
                            self.graph.deselect_all();
                            self.graph.load(flow.elements.clone(), flow.links.clone());
                            self.graph.load_blocks(flow.blocks.clone());
                            tracing::info!("Navigated to flow: {}", flow.name);
                        } else {
                            tracing::warn!(
                                "Pending flow ID {} not found in refreshed flow list",
                                pending_flow_id
                            );
                        }
                    } else if let Some(prev_id) = previously_selected_id {
                        // No pending navigation - check if previously selected flow still exists
                        if !self.flows.iter().any(|f| f.id == prev_id) {
                            // Flow was deleted - clear selection and graph
                            tracing::info!(
                                "Previously selected flow {} was deleted, clearing selection",
                                prev_id
                            );
                            self.clear_flow_selection();
                        }
                        // If flow still exists, selection is automatically valid (ID-based!)
                    }
                }
                AppMessage::FlowsError(error) => {
                    tracing::error!("Received FlowsError: {}", error);
                    self.error = Some(format!("Flows: {}", error));
                    self.loading = false;
                    self.status = "Error loading flows".to_string();
                }
                AppMessage::ElementsLoaded(elements) => {
                    let count = elements.len();
                    tracing::info!("Received ElementsLoaded: {} elements", count);
                    self.palette.load_elements(elements.clone());
                    self.graph.set_all_element_info(elements);
                    self.status = format!("Loaded {} elements", count);
                }
                AppMessage::ElementsError(error) => {
                    tracing::error!("Received ElementsError: {}", error);
                    self.error = Some(format!("Elements: {}", error));
                }
                AppMessage::BlocksLoaded(blocks) => {
                    let count = blocks.len();
                    tracing::info!("Received BlocksLoaded: {} blocks", count);
                    self.palette.load_blocks(blocks.clone());
                    self.graph.set_all_block_definitions(blocks);
                    self.status = format!("Loaded {} blocks", count);
                }
                AppMessage::BlocksError(error) => {
                    tracing::error!("Received BlocksError: {}", error);
                    self.error = Some(format!("Blocks: {}", error));
                }
                AppMessage::ElementPropertiesLoaded(info) => {
                    tracing::info!(
                        "Received ElementPropertiesLoaded: {} ({} properties)",
                        info.name,
                        info.properties.len()
                    );
                    self.palette.cache_element_properties(info);
                }
                AppMessage::ElementPropertiesError(error) => {
                    tracing::error!("Received ElementPropertiesError: {}", error);
                    self.error = Some(format!("Element properties: {}", error));
                }
                AppMessage::ElementPadPropertiesLoaded(info) => {
                    tracing::info!(
                        "Received ElementPadPropertiesLoaded: {} (sink: {} pads, src: {} pads)",
                        info.name,
                        info.sink_pads.len(),
                        info.src_pads.len()
                    );
                    // Update graph's element info map so pads render correctly
                    self.graph.set_element_info(info.name.clone(), info.clone());
                    self.palette.cache_element_pad_properties(info);
                }
                AppMessage::ElementPadPropertiesError(error) => {
                    tracing::error!("Received ElementPadPropertiesError: {}", error);
                    self.error = Some(format!("Pad properties: {}", error));
                }
                AppMessage::Event(event) => {
                    tracing::trace!("Received WebSocket event: {}", event.description());
                    // Handle flow state changes
                    use strom_types::StromEvent;
                    match event {
                        StromEvent::FlowCreated { .. } => {
                            tracing::info!("Flow created, triggering full refresh");
                            self.needs_refresh = true;
                        }
                        StromEvent::FlowDeleted { flow_id } => {
                            tracing::info!("Flow deleted, triggering full refresh");
                            // Clear QoS stats and start time for deleted flow
                            self.qos_stats.clear_flow(&flow_id);
                            self.flow_start_times.remove(&flow_id);
                            self.needs_refresh = true;
                        }
                        StromEvent::FlowStopped { flow_id } => {
                            tracing::info!("Flow {} stopped, clearing QoS stats", flow_id);
                            // Clear QoS stats and start time when flow is stopped
                            self.qos_stats.clear_flow(&flow_id);
                            // Refresh available channels (channels may have been removed)
                            self.refresh_available_channels();
                            self.flow_start_times.remove(&flow_id);

                            // Fetch updated flow state
                            let api = self.api.clone();
                            let tx = self.channels.sender();
                            let ctx = ctx.clone();

                            spawn_task(async move {
                                match api.get_flow(flow_id).await {
                                    Ok(flow) => {
                                        tracing::info!("Fetched updated flow: {}", flow.name);
                                        let _ = tx.send(AppMessage::FlowFetched(Box::new(flow)));
                                        ctx.request_repaint();
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to fetch updated flow: {}", e);
                                        let _ = tx.send(AppMessage::RefreshNeeded);
                                        ctx.request_repaint();
                                    }
                                }
                            });
                        }
                        StromEvent::FlowStarted { flow_id } => {
                            // Record when the flow started (for QoS grace period)
                            self.flow_start_times
                                .insert(flow_id, instant::Instant::now());
                            // Refresh available channels (new channels may be available)
                            self.refresh_available_channels();

                            // Fetch the updated flow state
                            tracing::info!("Flow {} started, fetching updated flow", flow_id);
                            let api = self.api.clone();
                            let tx = self.channels.sender();
                            let ctx = ctx.clone();

                            spawn_task(async move {
                                match api.get_flow(flow_id).await {
                                    Ok(flow) => {
                                        tracing::info!("Fetched started flow: {}", flow.name);
                                        let _ = tx.send(AppMessage::FlowFetched(Box::new(flow)));
                                        ctx.request_repaint();
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to fetch started flow: {}", e);
                                        let _ = tx.send(AppMessage::RefreshNeeded);
                                        ctx.request_repaint();
                                    }
                                }
                            });
                        }
                        StromEvent::FlowUpdated { flow_id } => {
                            // For updates, fetch the specific flow to update it in-place
                            tracing::info!("Flow {} updated, fetching updated flow", flow_id);
                            // Refresh available channels (flow name may have changed)
                            self.refresh_available_channels();
                            let api = self.api.clone();
                            let tx = self.channels.sender();
                            let ctx = ctx.clone();

                            spawn_task(async move {
                                match api.get_flow(flow_id).await {
                                    Ok(flow) => {
                                        tracing::info!("Fetched updated flow: {}", flow.name);
                                        let _ = tx.send(AppMessage::FlowFetched(Box::new(flow)));
                                        ctx.request_repaint();
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to fetch updated flow: {}", e);
                                        // Fall back to full refresh
                                        let _ = tx.send(AppMessage::RefreshNeeded);
                                        ctx.request_repaint();
                                    }
                                }
                            });
                        }
                        StromEvent::PipelineError {
                            flow_id,
                            error,
                            source,
                        } => {
                            tracing::error!(
                                "Pipeline error in flow {}: {} (source: {:?})",
                                flow_id,
                                error,
                                source
                            );
                            // Add to log entries
                            self.add_log_entry(LogEntry::new(
                                LogLevel::Error,
                                error.clone(),
                                source.clone(),
                                Some(flow_id),
                            ));
                            // Also set the legacy error field for status bar
                            let error_msg = if let Some(ref src) = source {
                                format!("{}: {}", src, error)
                            } else {
                                error
                            };
                            self.error = Some(error_msg);
                            // Auto-show log panel on errors
                            self.show_log_panel = true;
                        }
                        StromEvent::PipelineWarning {
                            flow_id,
                            warning,
                            source,
                        } => {
                            tracing::warn!(
                                "Pipeline warning in flow {}: {} (source: {:?})",
                                flow_id,
                                warning,
                                source
                            );
                            self.add_log_entry(LogEntry::new(
                                LogLevel::Warning,
                                warning,
                                source,
                                Some(flow_id),
                            ));
                        }
                        StromEvent::PipelineInfo {
                            flow_id,
                            message,
                            source,
                        } => {
                            tracing::info!(
                                "Pipeline info in flow {}: {} (source: {:?})",
                                flow_id,
                                message,
                                source
                            );
                            self.add_log_entry(LogEntry::new(
                                LogLevel::Info,
                                message,
                                source,
                                Some(flow_id),
                            ));
                        }
                        StromEvent::MeterData {
                            flow_id,
                            element_id,
                            rms,
                            peak,
                            decay,
                        } => {
                            tracing::trace!(
                                "📊 METER DATA RECEIVED: flow={}, element={}, channels={}, rms={:?}, peak={:?}",
                                flow_id,
                                element_id,
                                rms.len(),
                                rms,
                                peak
                            );
                            // Store meter data for visualization
                            self.meter_data.update(
                                flow_id,
                                element_id.clone(),
                                crate::meter::MeterData { rms, peak, decay },
                            );
                            tracing::trace!("📊 Meter data stored for element {}", element_id);
                        }
                        StromEvent::MediaPlayerPosition {
                            flow_id,
                            block_id,
                            position_ns,
                            duration_ns,
                            current_file_index,
                            total_files,
                        } => {
                            tracing::trace!(
                                "Media player position: flow={}, block={}, pos={}ns, dur={}ns",
                                flow_id,
                                block_id,
                                position_ns,
                                duration_ns
                            );
                            self.mediaplayer_data.update_position(
                                flow_id,
                                block_id,
                                position_ns,
                                duration_ns,
                                current_file_index,
                                total_files,
                            );
                        }
                        StromEvent::MediaPlayerStateChanged {
                            flow_id,
                            block_id,
                            state,
                            current_file,
                        } => {
                            tracing::debug!(
                                "Media player state changed: flow={}, block={}, state={}",
                                flow_id,
                                block_id,
                                state
                            );
                            self.mediaplayer_data.update_state(
                                flow_id,
                                block_id,
                                state,
                                current_file,
                            );
                        }
                        StromEvent::SystemStats(stats) => {
                            self.system_monitor.update(stats);
                        }
                        StromEvent::PtpStats {
                            flow_id,
                            domain,
                            synced,
                            mean_path_delay_ns,
                            clock_offset_ns,
                            r_squared,
                            clock_rate,
                            grandmaster_id,
                            master_id,
                        } => {
                            // Update PTP stats in the corresponding flow for real-time display
                            if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
                                // Update clock_sync_status (used by the UI for status display)
                                flow.properties.clock_sync_status = Some(if synced {
                                    strom_types::flow::ClockSyncStatus::Synced
                                } else {
                                    strom_types::flow::ClockSyncStatus::NotSynced
                                });

                                // Ensure ptp_info exists
                                if flow.properties.ptp_info.is_none() {
                                    flow.properties.ptp_info =
                                        Some(strom_types::flow::PtpInfo::default());
                                }
                                if let Some(ref mut ptp_info) = flow.properties.ptp_info {
                                    ptp_info.domain = domain;
                                    ptp_info.synced = synced;
                                    // Update stats
                                    let stats = strom_types::flow::PtpStats {
                                        mean_path_delay_ns,
                                        clock_offset_ns,
                                        r_squared,
                                        clock_rate,
                                        last_update: None,
                                    };
                                    ptp_info.stats = Some(stats);
                                }
                            }

                            // Also update the PTP stats store for history tracking
                            self.ptp_stats.update(
                                flow_id,
                                crate::ptp_monitor::PtpStatsData {
                                    domain,
                                    synced,
                                    mean_path_delay_ns,
                                    clock_offset_ns,
                                    r_squared,
                                    clock_rate,
                                    grandmaster_id,
                                    master_id,
                                },
                            );
                        }
                        StromEvent::QoSStats {
                            flow_id,
                            block_id,
                            element_id,
                            element_name,
                            internal_element_type,
                            event_count,
                            avg_proportion,
                            min_proportion,
                            max_proportion,
                            avg_jitter,
                            total_processed,
                            is_falling_behind,
                        } => {
                            // Grace period: ignore QoS events in first 3 seconds after flow start
                            // (transient issues during startup are common and not indicative of real problems)
                            const QOS_GRACE_PERIOD_SECS: u64 = 3;
                            let in_grace_period = self
                                .flow_start_times
                                .get(&flow_id)
                                .map(|start| {
                                    start.elapsed()
                                        < std::time::Duration::from_secs(QOS_GRACE_PERIOD_SECS)
                                })
                                .unwrap_or(false);

                            if in_grace_period {
                                // Skip QoS processing during grace period
                                continue;
                            }

                            // Update QoS store
                            self.qos_stats.update(
                                flow_id,
                                crate::qos_monitor::QoSElementData {
                                    element_id: element_id.clone(),
                                    block_id: block_id.clone(),
                                    element_name: element_name.clone(),
                                    internal_element_type: internal_element_type.clone(),
                                    avg_proportion,
                                    min_proportion,
                                    max_proportion,
                                    avg_jitter_ns: avg_jitter,
                                    event_count,
                                    total_processed,
                                    is_falling_behind,
                                    last_update: instant::Instant::now(),
                                },
                            );

                            // Log QoS issues (only when falling behind or recovering)
                            if is_falling_behind {
                                let display_name = if let Some(ref internal) = internal_element_type
                                {
                                    format!("{} ({})", element_name, internal)
                                } else {
                                    element_name.clone()
                                };
                                let message = format!(
                                    "QoS: {} falling behind ({:.1}%, {} events)",
                                    display_name,
                                    avg_proportion * 100.0,
                                    event_count
                                );
                                self.add_log_entry(LogEntry::new(
                                    if avg_proportion < 0.8 {
                                        LogLevel::Error
                                    } else {
                                        LogLevel::Warning
                                    },
                                    message,
                                    Some(element_id.clone()),
                                    Some(flow_id),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                AppMessage::ConnectionStateChanged(state) => {
                    tracing::info!("Connection state changed: {:?}", state);

                    // If we're transitioning to Connected state, invalidate all cached data
                    let was_disconnected = !self.connection_state.is_connected();
                    let now_connected = state.is_connected();

                    if was_disconnected && now_connected {
                        tracing::info!("Reconnected to backend - invalidating all cached state");
                        // Trigger reload of all data from backend
                        self.needs_refresh = true;
                        self.elements_loaded = false;
                        self.blocks_loaded = false;

                        // Check if backend has been rebuilt - this will trigger a reload if build_id changed
                        self.load_version(ctx.clone());
                    }

                    self.connection_state = state;
                }
                AppMessage::FlowFetched(flow) => {
                    let flow = *flow; // Unbox
                    tracing::info!("Received updated flow: {} (id={})", flow.name, flow.id);

                    // Check if this is the currently selected flow BEFORE updating
                    let current_flow_id = self.current_flow().map(|f| f.id);
                    let is_selected_flow = current_flow_id == Some(flow.id);

                    tracing::info!(
                        "Current selected flow: {:?}, Fetched flow: {}, Is selected: {}",
                        current_flow_id,
                        flow.id,
                        is_selected_flow
                    );

                    // Log runtime_data for AES67 blocks
                    for block in &flow.blocks {
                        if block.block_definition_id == "builtin.aes67_output" {
                            let has_sdp = block
                                .runtime_data
                                .as_ref()
                                .and_then(|data| data.get("sdp"))
                                .is_some();
                            tracing::info!("AES67 block {} has SDP: {}", block.id, has_sdp);
                        }
                    }

                    // Update the specific flow in-place
                    if let Some(existing_flow) = self.flows.iter_mut().find(|f| f.id == flow.id) {
                        *existing_flow = flow.clone();
                        tracing::info!("Updated flow in self.flows");

                        // If this is the currently selected flow, update the graph editor in-place
                        if is_selected_flow {
                            tracing::info!("This is the selected flow - updating graph editor");

                            // Selectively update graph editor data without overwriting positions
                            // This ensures property inspector sees latest runtime_data while preserving
                            // local position changes that may have occurred after save

                            // Update element properties (but preserve positions)
                            for updated_elem in &flow.elements {
                                if let Some(local_elem) = self
                                    .graph
                                    .elements
                                    .iter_mut()
                                    .find(|e| e.id == updated_elem.id)
                                {
                                    // Preserve local position
                                    let saved_position = local_elem.position;
                                    // Update properties from backend
                                    local_elem.properties = updated_elem.properties.clone();
                                    local_elem.pad_properties = updated_elem.pad_properties.clone();
                                    // Restore local position
                                    local_elem.position = saved_position;
                                }
                            }

                            // Update block runtime_data and properties (but preserve positions)
                            for updated_block in &flow.blocks {
                                if let Some(local_block) = self
                                    .graph
                                    .blocks
                                    .iter_mut()
                                    .find(|b| b.id == updated_block.id)
                                {
                                    // Preserve local position
                                    let saved_position = local_block.position;
                                    // Update runtime_data, properties, and computed_external_pads from backend
                                    local_block.runtime_data = updated_block.runtime_data.clone();
                                    local_block.properties = updated_block.properties.clone();
                                    local_block.computed_external_pads =
                                        updated_block.computed_external_pads.clone();
                                    // Restore local position
                                    local_block.position = saved_position;
                                }
                            }

                            // Update links (links don't have positions)
                            self.graph.links = flow.links.clone();

                            tracing::info!(
                                "Graph editor updated with {} blocks",
                                flow.blocks.len()
                            );
                        } else {
                            tracing::info!("Not the selected flow - skipping graph editor update");
                        }
                    } else {
                        tracing::warn!("Flow not found in list, adding it");
                        self.flows.push(flow);
                    }
                }
                AppMessage::RefreshNeeded => {
                    tracing::info!("Refresh requested due to flow fetch failure");
                    self.needs_refresh = true;
                }
                AppMessage::VersionLoaded(version_info) => {
                    tracing::info!(
                        "Version info loaded: v{} ({}) build_id={}",
                        version_info.version,
                        version_info.git_hash,
                        version_info.build_id
                    );

                    // Check if backend build_id differs from the one we got on initial load
                    // If so, the backend has been rebuilt and we need to reload the frontend
                    if let Some(ref existing_info) = self.version_info {
                        if !version_info.build_id.is_empty()
                            && !existing_info.build_id.is_empty()
                            && version_info.build_id != existing_info.build_id
                        {
                            tracing::warn!(
                                "Build ID mismatch! Previous: {}, Current: {} - reloading frontend",
                                existing_info.build_id,
                                version_info.build_id
                            );

                            // Force a hard reload to get the new frontend from the backend
                            #[cfg(target_arch = "wasm32")]
                            {
                                if let Some(window) = web_sys::window() {
                                    if let Err(e) = window.location().reload() {
                                        tracing::error!("Failed to reload page: {:?}", e);
                                    }
                                }
                            }
                            return;
                        }
                    }

                    self.version_info = Some(version_info);
                }
                AppMessage::AuthStatusLoaded(status) => {
                    tracing::info!(
                        "Auth status loaded: required={}, authenticated={}",
                        status.auth_required,
                        status.authenticated
                    );
                    self.auth_status = Some(status.clone());
                    self.checking_auth = false;

                    // If authenticated or auth not required, set up connections
                    if !status.auth_required || status.authenticated {
                        self.setup_websocket_connection(ctx.clone());
                        self.load_version(ctx.clone());
                    }
                }
                AppMessage::LoginResult(response) => {
                    tracing::info!("Login result: success={}", response.success);
                    self.login_screen.set_logging_in(false);

                    if response.success {
                        // Clear login form
                        self.login_screen.username.clear();
                        self.login_screen.password.clear();
                        self.login_screen.clear_error();

                        // Recheck auth status to update UI
                        self.check_auth_status(ctx.clone());
                    } else {
                        self.login_screen.set_error(response.message);
                    }
                }
                AppMessage::LogoutComplete => {
                    tracing::info!("Logout complete, reloading page to show login form");

                    // Reload the page so the HTML login form can re-initialize
                    // The session cookie has been cleared by the logout API call
                    #[cfg(target_arch = "wasm32")]
                    {
                        if let Some(window) = web_sys::window() {
                            if let Err(e) = window.location().reload() {
                                tracing::error!("Failed to reload page: {:?}", e);
                            }
                        }
                    }

                    // For native mode, just reset state and recheck auth
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        self.flows.clear();
                        self.ws_client = None;
                        self.connection_state = ConnectionState::Disconnected;
                        self.check_auth_status(ctx.clone());
                    }
                }
                AppMessage::WebRtcStatsLoaded { flow_id, stats } => {
                    tracing::debug!(
                        "WebRTC stats loaded for flow {}: {} connections",
                        flow_id,
                        stats.connections.len()
                    );
                    self.webrtc_stats.update(flow_id, stats);
                }
                AppMessage::FlowOperationSuccess(message) => {
                    tracing::info!("Flow operation succeeded: {}", message);
                    self.status = message;
                    self.error = None;
                }
                AppMessage::FlowOperationError(message) => {
                    tracing::error!("Flow operation failed: {}", message);
                    self.status = "Ready".to_string();
                    self.error = Some(message.clone());
                    // Add to log entries
                    let flow_id = self.current_flow().map(|f| f.id);
                    self.add_log_entry(LogEntry::new(LogLevel::Error, message, None, flow_id));
                    // Auto-show log panel on errors
                    self.show_log_panel = true;
                }
                AppMessage::FlowCreated(flow_id) => {
                    tracing::info!(
                        "Flow created, will navigate to flow ID after next refresh: {}",
                        flow_id
                    );
                    // Store the flow ID to navigate to after the next refresh
                    self.pending_flow_navigation = Some(flow_id);
                }
                AppMessage::LatencyLoaded { flow_id, latency } => {
                    tracing::debug!(
                        "Latency loaded for flow {}: {}",
                        flow_id,
                        latency.min_latency_formatted
                    );
                    self.latency_cache.insert(flow_id, latency);
                }
                AppMessage::LatencyNotAvailable(flow_id) => {
                    tracing::debug!("Latency not available for flow {}", flow_id);
                    self.latency_cache.remove(&flow_id);
                }
                AppMessage::WebRtcStatsError(error) => {
                    tracing::trace!("WebRTC stats error: {}", error);
                }
                AppMessage::StatsLoaded { flow_id, stats } => {
                    tracing::debug!(
                        "Stats loaded for flow {}: {} blocks",
                        flow_id,
                        stats.blocks.len()
                    );
                    self.stats_cache.insert(flow_id, stats);
                }
                AppMessage::StatsNotAvailable(flow_id) => {
                    tracing::debug!("Stats not available for flow {}", flow_id);
                    self.stats_cache.remove(&flow_id);
                }
                AppMessage::DynamicPadsLoaded { flow_id, pads } => {
                    tracing::debug!(
                        "Dynamic pads loaded for flow {}: {} elements",
                        flow_id,
                        pads.len()
                    );
                    // Update graph editor if this is the currently selected flow
                    if let Some(current_flow) = self.current_flow() {
                        if current_flow.id.to_string() == flow_id {
                            self.graph.set_runtime_dynamic_pads(pads);
                        }
                    }
                }
                AppMessage::GstLaunchExported {
                    pipeline,
                    flow_name,
                } => {
                    ctx.copy_text(pipeline);
                    self.status =
                        format!("Flow '{}' exported to clipboard as gst-launch", flow_name);
                }
                AppMessage::GstLaunchExportError(e) => {
                    self.error = Some(format!("Failed to export as gst-launch: {}", e));
                }
                AppMessage::NetworkInterfacesLoaded(interfaces) => {
                    tracing::info!("Network interfaces loaded: {} interfaces", interfaces.len());
                    self.network_interfaces = interfaces;
                }
                AppMessage::AvailableChannelsLoaded(mut channels) => {
                    // Sort by flow name, then by description/name
                    channels.sort_by(|a, b| {
                        let flow_cmp = a.flow_name.cmp(&b.flow_name);
                        if flow_cmp != std::cmp::Ordering::Equal {
                            return flow_cmp;
                        }
                        // Then by description or block name
                        let a_label = a.description.as_ref().unwrap_or(&a.name);
                        let b_label = b.description.as_ref().unwrap_or(&b.name);
                        a_label.cmp(b_label)
                    });
                    tracing::info!("Available channels loaded: {} channels", channels.len());
                    self.available_channels = channels;
                }
                AppMessage::DiscoveredStreamsLoaded(streams) => {
                    tracing::debug!("Discovered streams loaded: {} streams", streams.len());
                    self.discovery_page.set_discovered_streams(streams);
                }
                AppMessage::AnnouncedStreamsLoaded(streams) => {
                    tracing::debug!("Announced streams loaded: {} streams", streams.len());
                    self.discovery_page.set_announced_streams(streams);
                }
                AppMessage::StreamSdpLoaded { stream_id, sdp } => {
                    tracing::info!("Stream SDP loaded for: {}", stream_id);
                    self.discovery_page.set_stream_sdp(stream_id, sdp);
                }
                AppMessage::StreamPickerSdpLoaded { block_id, sdp } => {
                    tracing::info!(
                        "Stream picker SDP loaded for block: {}, SDP length: {}",
                        block_id,
                        sdp.len()
                    );
                    // Find the block and update its SDP property
                    if let Some(block) = self.graph.get_block_by_id_mut(&block_id) {
                        block
                            .properties
                            .insert("SDP".to_string(), strom_types::PropertyValue::String(sdp));
                        self.status = "SDP applied to block".to_string();
                        tracing::info!("SDP property updated for block {}", block_id);
                    } else {
                        tracing::warn!("Block {} not found in graph when applying SDP", block_id);
                        self.error = Some(format!("Block not found: {}", block_id));
                    }
                }
                AppMessage::MediaListLoaded(response) => {
                    tracing::debug!(
                        "Media list loaded: {} entries in {}",
                        response.entries.len(),
                        response.current_path
                    );
                    self.media_page.set_entries(response);
                }
                AppMessage::MediaSuccess(message) => {
                    tracing::info!("Media operation success: {}", message);
                    self.status = message;
                }
                AppMessage::MediaError(message) => {
                    tracing::error!("Media operation error: {}", message);
                    self.error = Some(message);
                }
                AppMessage::MediaRefresh => {
                    tracing::debug!("Media refresh requested");
                    self.media_page
                        .refresh(&self.api, ctx, &self.channels.sender());
                }
                // SDP messages are handled elsewhere
                AppMessage::SdpLoaded { .. } | AppMessage::SdpError(_) => {}
            }
        }

        // Process pending gst-launch export
        if let Some((elements, links, flow_name)) = self.pending_gst_launch_export.take() {
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ctx.clone();

            spawn_task(async move {
                match api.export_gst_launch(&elements, &links).await {
                    Ok(pipeline) => {
                        let _ = tx.send(AppMessage::GstLaunchExported {
                            pipeline,
                            flow_name,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(AppMessage::GstLaunchExportError(e.to_string()));
                    }
                }
                ctx.request_repaint();
            });
        }

        // Check authentication - if required and not authenticated, don't render
        // The HTML login form (in index.html) handles authentication
        // WASM should just stay quiet until authentication is complete
        if let Some(ref status) = self.auth_status {
            if status.auth_required && !status.authenticated {
                // Don't render anything - HTML login form is handling auth
                return;
            }
        }

        // Check if we're disconnected - if so, show blocking overlay and don't render normal UI
        if !self.connection_state.is_connected() {
            self.render_disconnect_overlay(ctx);
            return;
        }

        // Load elements on first frame
        if !self.elements_loaded {
            self.load_elements(ctx);
            self.elements_loaded = true;
        }

        // Load blocks on first frame
        if !self.blocks_loaded {
            self.load_blocks(ctx);
            self.blocks_loaded = true;
        }

        // Load flows on first frame or when refresh is needed
        if self.needs_refresh {
            self.load_flows(ctx);
            self.needs_refresh = false;
        }

        // Poll WebRTC stats every second for running flows
        {
            let poll_interval = std::time::Duration::from_secs(1);
            if self.last_webrtc_poll.elapsed() >= poll_interval {
                self.poll_webrtc_stats(ctx);
                self.last_webrtc_poll = instant::Instant::now();
            }
        }

        // Periodically fetch latency for running flows (every 2 seconds)
        if self.last_latency_fetch.elapsed() > std::time::Duration::from_secs(2) {
            self.last_latency_fetch = instant::Instant::now();
            self.fetch_latency_for_running_flows(ctx);
        }

        // Periodically fetch stats for running flows (every 2 seconds)
        if self.last_stats_fetch.elapsed() > std::time::Duration::from_secs(2) {
            self.last_stats_fetch = instant::Instant::now();
            self.fetch_stats_for_running_flows(ctx);
        }

        // Handle keyboard shortcuts
        self.handle_keyboard_shortcuts(ctx);

        // Check for compositor editor open signal
        if let Some(block_id) = get_local_storage("open_compositor_editor") {
            remove_local_storage("open_compositor_editor");

            // Get current flow
            if let Some(flow) = self.current_flow() {
                // Find the block
                if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Extract resolution from output_resolution property
                    // Default to 1920x1080 (Full HD) if not set or can't be parsed
                    let (output_width, output_height) = block
                        .properties
                        .get("output_resolution")
                        .and_then(|v| match v {
                            strom_types::PropertyValue::String(s) if !s.is_empty() => {
                                strom_types::parse_resolution_string(s)
                            }
                            _ => None,
                        })
                        .unwrap_or((1920, 1080));

                    let num_inputs = block
                        .properties
                        .get("num_inputs")
                        .and_then(|v| match v {
                            strom_types::PropertyValue::UInt(u) => Some(*u as usize),
                            strom_types::PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
                            _ => None,
                        })
                        .unwrap_or(2);

                    // Create editor
                    let mut editor = CompositorEditor::new(
                        flow.id,
                        block_id.clone(),
                        output_width,
                        output_height,
                        num_inputs,
                        self.api.clone(),
                    );

                    // Load current properties from backend
                    editor.load_properties(ctx);

                    self.compositor_editor = Some(editor);
                }
            }
        }

        // Show compositor editor if open (as a window, doesn't block main UI)
        if let Some(ref mut editor) = self.compositor_editor {
            let is_open = editor.show(ctx);
            if !is_open {
                self.compositor_editor = None;
            }
        }

        // Check for playlist editor open signal
        if let Some(block_id) = get_local_storage("open_playlist_editor") {
            remove_local_storage("open_playlist_editor");

            // Get current flow
            if let Some(flow) = self.current_flow() {
                // Find the block
                if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                    // Create playlist editor
                    let mut editor = PlaylistEditor::new(flow.id, block_id.clone());

                    // Load current playlist from block properties
                    if let Some(strom_types::PropertyValue::String(playlist_json)) =
                        block.properties.get("playlist")
                    {
                        if let Ok(playlist) = serde_json::from_str::<Vec<String>>(playlist_json) {
                            editor.set_playlist(playlist);
                        }
                    }

                    self.playlist_editor = Some(editor);
                }
            }
        }

        // Show playlist editor if open (as a window, doesn't block main UI)
        if let Some(ref mut editor) = self.playlist_editor {
            // Check if browser needs to load files
            if let Some(path) = editor.get_browser_path_to_load() {
                let api = self.api.clone();
                // Use local storage to pass results back
                #[cfg(target_arch = "wasm32")]
                {
                    wasm_bindgen_futures::spawn_local(async move {
                        match api.list_media(&path).await {
                            Ok(result) => {
                                // Serialize result to local storage
                                if let Ok(json) = serde_json::to_string(&result) {
                                    set_local_storage("media_browser_result", &json);
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to list media files: {}", e);
                                set_local_storage("media_browser_result", "error");
                            }
                        }
                    });
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let rt = tokio::runtime::Handle::try_current();
                    if let Ok(handle) = rt {
                        handle.spawn(async move {
                            match api.list_media(&path).await {
                                Ok(result) => {
                                    if let Ok(json) = serde_json::to_string(&result) {
                                        set_local_storage("media_browser_result", &json);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to list media files: {}", e);
                                    set_local_storage("media_browser_result", "error");
                                }
                            }
                        });
                    }
                }
            }

            // Check for media browser results
            if let Some(result_json) = get_local_storage("media_browser_result") {
                remove_local_storage("media_browser_result");
                if result_json != "error" {
                    if let Ok(result) =
                        serde_json::from_str::<strom_types::api::ListMediaResponse>(&result_json)
                    {
                        let entries: Vec<crate::mediaplayer::MediaEntry> = result
                            .entries
                            .into_iter()
                            .map(|e| crate::mediaplayer::MediaEntry {
                                name: e.name,
                                path: e.path,
                                is_dir: e.is_directory,
                                size: e.size,
                            })
                            .collect();
                        editor.set_browser_entries(
                            result.current_path,
                            result.parent_path,
                            entries,
                        );
                    }
                } else {
                    // Clear loading state on error
                    editor.browser_loading = false;
                }
            }

            // Update current playing index from player data
            if let Some(player_data) = self.mediaplayer_data.get(&editor.flow_id, &editor.block_id)
            {
                editor.current_playing_index = Some(player_data.current_file_index);
            }

            if let Some(playlist) = editor.show(ctx) {
                // User clicked Save - send playlist to API
                let flow_id = editor.flow_id;
                let block_id = editor.block_id.clone();
                let api = self.api.clone();

                #[cfg(target_arch = "wasm32")]
                {
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(e) = api.set_player_playlist(flow_id, &block_id, playlist).await
                        {
                            tracing::error!("Failed to set playlist: {}", e);
                        }
                    });
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let rt = tokio::runtime::Handle::try_current();
                    if let Ok(handle) = rt {
                        handle.spawn(async move {
                            if let Err(e) =
                                api.set_player_playlist(flow_id, &block_id, playlist).await
                            {
                                tracing::error!("Failed to set playlist: {}", e);
                            }
                        });
                    }
                }
            }

            if !editor.open {
                self.playlist_editor = None;
            }
        }

        // Check for player action signals (from compact UI controls)
        if let Some(action_data) = get_local_storage("player_action") {
            remove_local_storage("player_action");
            tracing::info!("Received player action: {}", action_data);

            // Parse action data: "block_id:action" or "block_id:action:position"
            let parts: Vec<&str> = action_data.split(':').collect();
            if parts.len() >= 2 {
                let block_id = parts[0].to_string();
                let action = parts[1];
                tracing::info!("Parsed action: block={}, action={}", block_id, action);

                if let Some(flow) = self.current_flow() {
                    let flow_id = flow.id;
                    let api = self.api.clone();
                    tracing::info!("Sending action to flow {}", flow_id);

                    match action {
                        "play" | "pause" | "next" | "previous" => {
                            let action_str = action.to_string();
                            #[cfg(target_arch = "wasm32")]
                            {
                                wasm_bindgen_futures::spawn_local(async move {
                                    if let Err(e) =
                                        api.control_player(flow_id, &block_id, &action_str).await
                                    {
                                        tracing::error!("Failed to control player: {}", e);
                                    }
                                });
                            }

                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                let rt = tokio::runtime::Handle::try_current();
                                if let Ok(handle) = rt {
                                    handle.spawn(async move {
                                        if let Err(e) = api
                                            .control_player(flow_id, &block_id, &action_str)
                                            .await
                                        {
                                            tracing::error!("Failed to control player: {}", e);
                                        }
                                    });
                                }
                            }
                        }
                        "seek" if parts.len() >= 3 => {
                            if let Ok(position_ns) = parts[2].parse::<u64>() {
                                #[cfg(target_arch = "wasm32")]
                                {
                                    wasm_bindgen_futures::spawn_local(async move {
                                        if let Err(e) =
                                            api.seek_player(flow_id, &block_id, position_ns).await
                                        {
                                            tracing::error!("Failed to seek player: {}", e);
                                        }
                                    });
                                }

                                #[cfg(not(target_arch = "wasm32"))]
                                {
                                    let rt = tokio::runtime::Handle::try_current();
                                    if let Ok(handle) = rt {
                                        handle.spawn(async move {
                                            if let Err(e) = api
                                                .seek_player(flow_id, &block_id, position_ns)
                                                .await
                                            {
                                                tracing::error!("Failed to seek player: {}", e);
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        "playlist" => {
                            // Open playlist editor for this block
                            let mut editor = PlaylistEditor::new(flow_id, block_id.clone());

                            // Load current playlist from block properties
                            if let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) {
                                if let Some(strom_types::PropertyValue::String(playlist_json)) =
                                    block.properties.get("playlist")
                                {
                                    if let Ok(playlist) =
                                        serde_json::from_str::<Vec<String>>(playlist_json)
                                    {
                                        editor.set_playlist(playlist);
                                    }
                                }
                            }

                            self.playlist_editor = Some(editor);
                        }
                        _ => {
                            tracing::warn!("Unknown player action: {}", action);
                        }
                    }
                }
            }
        }

        self.render_toolbar(ctx);

        // Render page-specific content
        match self.current_page {
            AppPage::Flows => {
                self.render_flow_list(ctx);

                // Always show palette, even if no flow selected
                if self.current_flow().is_some() {
                    self.render_palette(ctx);
                } else {
                    // Show simplified palette when no flow is selected
                    SidePanel::right("palette")
                        .default_width(250.0)
                        .resizable(true)
                        .show(ctx, |ui| {
                            ui.heading("Elements");
                            ui.separator();
                            ui.label("Select or create a flow to see the element palette");
                        });
                }

                self.render_canvas(ctx);
                self.render_log_panel(ctx);
                self.render_new_flow_dialog(ctx);
                self.render_delete_confirmation_dialog(ctx);
                self.render_flow_properties_dialog(ctx);
                self.render_import_dialog(ctx);
                self.render_stream_picker_modal(ctx);
            }
            AppPage::Discovery => {
                CentralPanel::default().show(ctx, |ui| {
                    self.discovery_page
                        .render(ui, &self.api, ctx, &self.channels.tx);
                });

                // Handle pending create flow from discovery
                if let Some(sdp) = self.discovery_page.take_pending_create_flow_sdp() {
                    self.create_flow_from_sdp(sdp, ctx);
                }
            }
            AppPage::Clocks => {
                CentralPanel::default().show(ctx, |ui| {
                    self.clocks_page.render(ui, &self.ptp_stats, &self.flows);
                });
            }
            AppPage::Media => {
                CentralPanel::default().show(ctx, |ui| {
                    self.media_page
                        .render(ui, &self.api, ctx, &self.channels.sender());
                });
            }
            AppPage::Info => {
                // Auto-load network interfaces when Info page is shown
                if self.info_page.should_load_network() {
                    self.network_interfaces_loaded = false;
                    self.load_network_interfaces(ctx.clone());
                }

                CentralPanel::default().show(ctx, |ui| {
                    self.info_page.render(
                        ui,
                        self.version_info.as_ref(),
                        &self.system_monitor,
                        &self.network_interfaces,
                        &self.flows,
                    );
                });
            }
            AppPage::Links => {
                CentralPanel::default().show(ctx, |ui| {
                    self.links_page.render(ui, &self.api, ctx);
                });
            }
        }

        self.render_status_bar(ctx);
        self.render_system_monitor_window(ctx);

        // Process pending flow copy (after render to avoid borrow checker issues)
        if let Some(flow) = self.flow_pending_copy.take() {
            self.copy_flow(&flow, ctx);
        }
    }
}
