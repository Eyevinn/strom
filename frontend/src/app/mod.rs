//! Main application structure.

mod block_thumbnails;
mod constructors;
mod data_loading;
mod dialogs;
mod flow_ops;
mod flow_selection;
mod import_export;
mod keyboard;
mod live_mode;
mod probe_ui;
mod rendering;
mod update;

use egui::Color32;
use strom_types::Flow;

use crate::api::{ApiClient, AuthStatusResponse};
use crate::audioanalyzer::AudioAnalyzerDataStore;
use crate::audiorouter::RoutingMatrixEditor;
use crate::buffer_age::BufferAgeStore;
use crate::compositor_editor::CompositorEditor;
use crate::graph::GraphEditor;
use crate::latency::LatencyDataStore;
use crate::loudness::LoudnessDataStore;
use crate::mediaplayer::{MediaPlayerDataStore, PlaylistEditor, SeekThrottle};
use crate::meter::MeterDataStore;
use crate::mixer::MixerEditor;
use crate::palette::ElementPalette;
use crate::spectrum::SpectrumDataStore;
use crate::srt_stats::SrtStatsStore;
use crate::state::{AppStateChannels, ConnectionState};
use crate::system_monitor::SystemMonitorStore;
use crate::thread_monitor::ThreadMonitorStore;
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

// Stubs for native mode (in-memory only, used for transient state)
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

/// Escape XML special characters in a string.
pub(crate) fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Generate XSPF playlist content for VLC to play an SRT stream.
///
/// If the block is in listener mode (e.g., `srt://:5000?mode=listener`), VLC needs to
/// connect as a caller. We transform the URI to use the server's hostname from the
/// current browser URL.
pub fn generate_vlc_playlist(srt_uri: &str, latency_ms: i32, stream_name: &str) -> String {
    // Transform URI if it's in listener mode - VLC needs to connect as caller
    let vlc_uri = transform_srt_uri_for_vlc(srt_uri);

    let escaped_uri = escape_xml(&vlc_uri);

    // Include SRT URL in the track title for easy identification
    let title_with_url = format!("{} ({})", stream_name, vlc_uri);
    let escaped_title = escape_xml(&title_with_url);

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
        escaped_uri, escaped_title, latency_ms
    )
}

/// Transform SRT URI for VLC playback.
///
/// When the MPEG-TS/SRT block is in listener mode (server waiting for connections),
/// VLC needs to connect as a caller. This function:
/// 1. Detects listener mode URIs (e.g., `srt://:5000?mode=listener`)
/// 2. Replaces empty host with the Strom server's hostname
/// 3. Changes mode from listener to caller
pub fn transform_srt_uri_for_vlc(srt_uri: &str) -> String {
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
///
/// - WASM: reads from browser `window.location.hostname` (already the external address).
/// - Native: uses the OS hostname via `gethostname`.
///
/// Falls back to `"127.0.0.1"` if detection fails.
#[cfg(target_arch = "wasm32")]
pub(crate) fn get_current_hostname() -> String {
    let hostname = web_sys::window()
        .and_then(|w| w.location().hostname().ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    if hostname == "localhost" {
        "127.0.0.1".to_string()
    } else {
        hostname
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn get_current_hostname() -> String {
    // In native mode, the GUI runs on the same machine as the server.
    // Using the OS hostname can resolve to IPv6 (e.g. ::1 or fe80::...)
    // which won't work with SRT listeners bound to 0.0.0.0 (IPv4 only).
    // Always use 127.0.0.1 for reliable local connectivity.
    "127.0.0.1".to_string()
}

/// Rewrite a URL so that `localhost` / `127.0.0.1` is replaced with the
/// machine's actual hostname.
///
/// This is needed whenever a URL will be consumed by an external device
/// (e.g. a phone scanning a QR code, or VLC connecting to an SRT stream).
/// Returns the URL unchanged if the host is already external.
///
/// When `server_hostname` is provided (from backend `SystemInfo`), it takes
/// precedence over local detection. This ensures correct results even in
/// WASM mode where `window.location.hostname` may return "localhost".
pub(crate) fn make_external_url(url: &str, server_hostname: Option<&str>) -> String {
    // Quick check before doing any work
    let is_local = url.contains("://localhost") || url.contains("://127.0.0.1");
    if !is_local {
        return url.to_string();
    }

    // Prefer the backend-provided hostname, fall back to local detection
    let hostname = match server_hostname {
        Some(h) if !h.is_empty() && h != "localhost" && h != "127.0.0.1" => h.to_string(),
        _ => {
            let h = get_current_hostname();
            if h == "127.0.0.1" || h == "localhost" {
                return url.to_string();
            }
            h
        }
    };

    url.replace("://localhost", &format!("://{}", hostname))
        .replace("://127.0.0.1", &format!("://{}", hostname))
}

/// Theme preference for the application
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
enum ThemePreference {
    /// Standard egui dark theme
    #[default]
    EguiDark,
    /// Standard egui light theme
    EguiLight,
    /// Nord Dark theme (arctic-inspired)
    NordDark,
    /// Nord Light theme (arctic-inspired)
    NordLight,
    /// Tokyo Night theme (VSCode-inspired)
    TokyoNight,
    /// Tokyo Night Storm variant (lighter dark)
    TokyoNightStorm,
    /// Tokyo Night Light theme
    TokyoNightLight,
    /// Claude Dark theme (warm brown)
    ClaudeDark,
    /// Claude Light theme (warm cream)
    ClaudeLight,
}

/// Persisted application settings (theme, zoom, etc.)
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct AppSettings {
    /// Theme preference
    theme: ThemePreference,
    /// Zoom level (None = system default)
    zoom: Option<f32>,
}

const APP_SETTINGS_KEY: &str = "app_settings";

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

/// Application mode - determines which UI is rendered
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AppMode {
    /// Full admin interface with all features
    #[default]
    Admin,
    /// Live view - just the compositor editor for a specific block
    Live {
        flow_id: strom_types::FlowId,
        block_id: String,
    },
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
    #[allow(dead_code)]
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
    /// Flow ID to select on next frame (deferred to avoid accesskit focus issues)
    pending_flow_selection: Option<strom_types::FlowId>,
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
    /// Temporary NTP server buffer for properties dialog
    properties_ntp_server_buffer: String,
    /// Temporary NTP port buffer for properties dialog
    properties_ntp_port_buffer: String,
    /// Temporary direct media timing selection for properties dialog.
    /// None = inherit from clock_type default; Some = explicit override.
    properties_direct_media_timing_buffer: Option<bool>,
    /// Temporary thread priority for properties dialog
    properties_thread_priority_buffer: strom_types::flow::ThreadPriority,
    /// Temporary CPU affinity for properties dialog
    properties_cpu_affinity_buffer: strom_types::flow::CpuAffinity,
    /// Temporary ephemeral flag for properties dialog
    properties_ephemeral_buffer: bool,
    /// Shutdown flag for Ctrl+C handling (native mode only)
    #[cfg(not(target_arch = "wasm32"))]
    shutdown_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Port number for backend connection (native mode only)
    #[cfg(not(target_arch = "wasm32"))]
    port: u16,
    /// Whether the backend is using TLS (native mode only)
    #[cfg(not(target_arch = "wasm32"))]
    tls_enabled: bool,
    /// Auth token for native GUI authentication
    #[cfg(not(target_arch = "wasm32"))]
    auth_token: Option<String>,
    /// Cached network interfaces (for network interface property dropdown)
    network_interfaces: Vec<strom_types::NetworkInterfaceInfo>,
    /// Whether network interfaces have been loaded
    network_interfaces_loaded: bool,
    /// Cached local video capture devices (Local Input block picker)
    video_devices: Vec<strom_types::discovery::DeviceResponse>,
    /// Cached local audio capture devices (Local Input block picker)
    audio_devices: Vec<strom_types::discovery::DeviceResponse>,
    /// In-flight flag for video device fetch
    video_devices_loading: bool,
    /// In-flight flag for audio device fetch
    audio_devices_loading: bool,
    /// Last time the device lists were fetched (used as a TTL to avoid spamming the API)
    devices_last_loaded: Option<instant::Instant>,
    /// Current log filter string from backend
    log_level_current: Option<String>,
    /// Default log filter string the server started with
    log_level_default: Option<String>,
    /// Current GStreamer debug filter string from backend
    gst_log_level_current: Option<String>,
    /// Default GStreamer debug filter string the server started with
    gst_log_level_default: Option<String>,
    /// Cached available inter channels (for InterInput channel dropdown)
    available_channels: Vec<strom_types::api::AvailableOutput>,
    /// Whether available channels have been loaded
    available_channels_loaded: bool,
    /// Last InterInput block ID we refreshed channels for (to avoid repeated refreshes)
    last_inter_input_refresh: Option<String>,
    /// Audio analyzer data storage for all audio analyzer blocks
    audioanalyzer_data: AudioAnalyzerDataStore,
    /// Meter data storage for all audio level meters
    meter_data: MeterDataStore,
    /// Spectrum data storage for all spectrum analyzer blocks
    spectrum_data: SpectrumDataStore,
    /// Loudness data storage for all EBU R128 loudness meters
    loudness_data: LoudnessDataStore,
    /// Latency data storage for all audio latency measurements
    latency_data: LatencyDataStore,
    /// Media player data storage for all media player blocks
    mediaplayer_data: MediaPlayerDataStore,
    /// Seek throttle to avoid flooding the API during drags
    seek_throttle: SeekThrottle,
    /// WebRTC stats storage for all WebRTC connections
    webrtc_stats: WebRtcStatsStore,
    /// SRT stats storage for all SRT inputs/outputs
    srt_stats: SrtStatsStore,
    /// System monitoring statistics
    system_monitor: SystemMonitorStore,
    /// Thread CPU monitoring statistics
    thread_monitor: ThreadMonitorStore,
    /// PTP clock statistics per flow
    ptp_stats: crate::ptp_monitor::PtpStatsStore,
    /// QoS (buffer drop) statistics per flow/element
    qos_stats: crate::qos_monitor::QoSStore,
    /// Buffer age warnings and probe data
    buffer_age_data: BufferAgeStore,
    /// Track when flows started (for QoS grace period)
    flow_start_times: std::collections::HashMap<strom_types::FlowId, instant::Instant>,
    /// Whether to show the detailed system monitor window
    show_system_monitor: bool,
    /// Selected tab in the system monitor window
    system_monitor_tab: crate::system_monitor::SystemMonitorTab,
    /// Thread sort column
    thread_sort_column: crate::system_monitor::ThreadSortColumn,
    /// Thread sort direction
    thread_sort_direction: crate::system_monitor::SortDirection,
    /// Pending navigation action from thread monitor (deferred to avoid borrow conflicts)
    pending_thread_nav_action: Option<crate::system_monitor::ThreadNavigationAction>,
    /// Last time WebRTC stats were polled
    last_webrtc_poll: instant::Instant,
    /// Last time SRT stats were polled
    last_srt_poll: instant::Instant,
    /// Persisted settings (theme, zoom, etc.)
    settings: AppSettings,
    /// Whether we need to apply settings in the first update frame (workaround for iOS)
    needs_initial_settings_apply: bool,
    /// System information from the backend (version, host details, runtime environment)
    system_info: Option<crate::api::SystemInfo>,
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
    /// Cached latency info for flows (flow_id -> LatencyResponse)
    latency_cache: std::collections::HashMap<String, crate::api::LatencyResponse>,
    /// Last time latency was fetched (for periodic refresh)
    last_latency_fetch: instant::Instant,
    /// Cached RTP stats info for flows (flow_id -> FlowStatsResponse)
    rtp_stats_cache: std::collections::HashMap<String, strom_types::api::FlowStatsResponse>,
    /// Last time stats was fetched (for periodic refresh)
    last_rtp_stats_fetch: instant::Instant,
    /// Cached system clock state from kernel adjtimex
    system_clock_info: Option<crate::api::SystemClockInfo>,
    /// True if the backend reported the system clock endpoint is unsupported on
    /// this platform (non-Linux). Drives the "not available" message.
    system_clock_unsupported: bool,
    /// Last time system clock was fetched (for periodic refresh while Clocks page is open).
    /// `None` means never fetched — triggers an immediate fetch on first Clocks page visit.
    last_system_clock_fetch: Option<instant::Instant>,
    /// Compositor layout editor (if open)
    compositor_editor: Option<CompositorEditor>,
    /// Mixer editor (if open)
    mixer_editor: Option<MixerEditor>,
    /// Playlist editor (if open)
    playlist_editor: Option<PlaylistEditor>,
    /// Routing matrix editor for Audio Router blocks (if open)
    routing_matrix_editor: Option<RoutingMatrixEditor>,
    /// Log entries for pipeline messages (errors, warnings, info)
    log_entries: Vec<LogEntry>,
    /// Whether to show the log panel
    show_log_panel: bool,
    /// Whether to show the left flow list panel
    show_flow_list_panel: bool,
    /// Whether to show the right palette panel
    show_palette_panel: bool,
    /// Maximum number of log entries to keep
    max_log_entries: usize,
    /// Current application page
    current_page: AppPage,
    /// Application mode (Admin or Live)
    app_mode: AppMode,
    /// Whether the app started in live mode (via URL) - hides back button
    started_in_live_mode: bool,
    /// Discovery page state
    discovery_page: crate::discovery::DiscoveryPage,
    /// Clocks page state (PTP monitoring)
    clocks_page: crate::clocks::ClocksPage,
    /// Media file browser page state
    media_page: crate::media::MediaPage,
    /// Info page state
    info_page: crate::info_page::InfoPage,
    /// Rendering backend info detected at startup
    renderer_info: crate::info_page::RendererInfo,
    /// Links page state
    links_page: crate::links::LinksPage,
    /// Flow list filter text
    flow_filter: String,
    /// Show stream picker modal for this block ID (when browsing discovered streams for AES67 Input)
    show_stream_picker_for_block: Option<String>,
    /// Show NDI picker modal for this block ID (when browsing NDI sources for NDI Input)
    show_ndi_picker_for_block: Option<String>,
    /// Search filter for NDI picker modal
    ndi_search_filter: String,
    /// Current focus target for Ctrl+F cycling
    focus_target: FocusTarget,
    /// Request to focus the flow filter on next frame
    focus_flow_filter_requested: bool,
    /// Native pixels per point (device pixel ratio at startup)
    native_pixels_per_point: f32,
    /// Key sequence buffer for activation detection
    key_sequence_buffer: Vec<egui::Key>,
    /// Interactive overlay state (activated by key sequence)
    interactive_overlay: Option<crate::interactive_overlay::OverlayState>,
    /// Thumbnail textures for thumbnail blocks (flow_id, block_id) -> texture
    block_thumbnails: std::collections::HashMap<(strom_types::FlowId, String), egui::TextureHandle>,
    /// Last thumbnail fetch time per block
    block_thumbnail_fetch_times:
        std::collections::HashMap<(strom_types::FlowId, String), instant::Instant>,
    /// Blocks currently loading thumbnails
    block_thumbnail_loading: std::collections::HashSet<(strom_types::FlowId, String)>,
    /// Block ID and URL to show as inline QR code in the properties panel
    qr_inline: Option<(String, String)>,
    /// QR code texture cache (for properties popup)
    qr_cache: crate::qr::QrCache,
    /// Current recording filename per recorder block (flow_id, block_id) -> filename
    recorder_filenames: std::collections::HashMap<(strom_types::FlowId, String), String>,
    /// Recording start time per recorder block (flow_id, block_id) -> Instant
    recorder_start_times:
        std::collections::HashMap<(strom_types::FlowId, String), instant::Instant>,
    /// Debounce state for live property updates: tracks last-sent time and any pending update
    /// Key: (element_id, property_name)
    live_property_debounce:
        std::collections::HashMap<(String, String), crate::properties::LivePropertyDebounce>,
}
