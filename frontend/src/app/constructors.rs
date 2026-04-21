use crate::api::ApiClient;
use crate::audioanalyzer::AudioAnalyzerDataStore;
use crate::graph::GraphEditor;
use crate::latency::LatencyDataStore;
use crate::loudness::LoudnessDataStore;
use crate::mediaplayer::{MediaPlayerDataStore, SeekThrottle};
use crate::meter::MeterDataStore;
use crate::palette::ElementPalette;
use crate::spectrum::SpectrumDataStore;
use crate::state::{AppStateChannels, ConnectionState};
use crate::system_monitor::SystemMonitorStore;
use crate::thread_monitor::ThreadMonitorStore;
use crate::webrtc_stats::WebRtcStatsStore;
use crate::ws::WebSocketClient;

use super::*;
use super::{FocusTarget, ImportFormat, ThemePreference, APP_SETTINGS_KEY};
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

                    // Port 8095: trunk serve mode - rewrite to backend port 8080
                    if protocol == "http:" && host.ends_with(":8095") {
                        let hostname = host.trim_end_matches(":8095");
                        let api_url = format!("http://{}:8080/api", hostname);
                        tracing::info!("REST API URL (trunk serve mode): {}", api_url);
                        return Self::new_internal(cc, api_url, None);
                    }

                    // Use current window location (works for Docker, production, etc.)
                    let api_url = format!("{}//{}/api", protocol, host);
                    tracing::info!("REST API URL: {}", api_url);
                    api_url
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
    pub fn new(cc: &eframe::CreationContext<'_>, port: u16, tls_enabled: bool) -> Self {
        let scheme = if tls_enabled { "https" } else { "http" };
        let api_base_url = format!("{}://localhost:{}/api", scheme, port);
        Self::new_internal(cc, api_base_url, None, port, tls_enabled, None)
    }

    /// Internal constructor shared by all creation methods (WASM version).
    #[cfg(target_arch = "wasm32")]
    pub(super) fn new_internal(
        cc: &eframe::CreationContext<'_>,
        api_base_url: String,
        _shutdown_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        // Install image loaders for egui (required for Image::from_bytes)
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // Load Phosphor icon fonts
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        cc.egui_ctx.set_fonts(fonts);

        let renderer_info = crate::info_page::detect_renderer(cc);

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
            pending_flow_selection: None,
            ws_client: None,
            connection_state: ConnectionState::Disconnected,
            channels,
            editing_properties_flow_id: None,
            properties_name_buffer: String::new(),
            properties_description_buffer: String::new(),
            properties_clock_type_buffer: strom_types::flow::GStreamerClockType::Monotonic,
            properties_ptp_domain_buffer: String::new(),
            properties_ntp_server_buffer: String::new(),
            properties_ntp_port_buffer: String::new(),
            properties_direct_media_timing_buffer: None,
            properties_thread_priority_buffer: strom_types::flow::ThreadPriority::High,
            properties_cpu_affinity_buffer: strom_types::flow::CpuAffinity::default(),
            properties_ephemeral_buffer: false,
            audioanalyzer_data: AudioAnalyzerDataStore::new(),
            meter_data: MeterDataStore::new(),
            spectrum_data: SpectrumDataStore::new(),
            loudness_data: LoudnessDataStore::new(),
            latency_data: LatencyDataStore::new(),
            mediaplayer_data: MediaPlayerDataStore::new(),
            seek_throttle: SeekThrottle::new(),
            webrtc_stats: WebRtcStatsStore::new(),
            system_monitor: SystemMonitorStore::new(),
            thread_monitor: ThreadMonitorStore::new(),
            ptp_stats: crate::ptp_monitor::PtpStatsStore::new(),
            qos_stats: crate::qos_monitor::QoSStore::new(),
            buffer_age_data: BufferAgeStore::new(),
            flow_start_times: std::collections::HashMap::new(),
            show_system_monitor: false,
            system_monitor_tab: crate::system_monitor::SystemMonitorTab::default(),
            thread_sort_column: crate::system_monitor::ThreadSortColumn::default(),
            thread_sort_direction: crate::system_monitor::SortDirection::default(),
            pending_thread_nav_action: None,
            last_webrtc_poll: instant::Instant::now(),
            settings: cc
                .storage
                .and_then(|s| eframe::get_value(s, APP_SETTINGS_KEY))
                .unwrap_or_default(),
            needs_initial_settings_apply: true,
            system_info: None,
            auth_status: None,
            checking_auth: false,
            show_import_dialog: false,
            import_format: ImportFormat::default(),
            import_json_buffer: String::new(),
            import_error: None,
            pending_gst_launch_export: None,
            latency_cache: std::collections::HashMap::new(),
            last_latency_fetch: instant::Instant::now(),
            rtp_stats_cache: std::collections::HashMap::new(),
            last_rtp_stats_fetch: instant::Instant::now(),
            system_clock_info: None,
            system_clock_unsupported: false,
            last_system_clock_fetch: instant::Instant::now() - std::time::Duration::from_secs(10),

            compositor_editor: None,
            mixer_editor: None,
            playlist_editor: None,
            routing_matrix_editor: None,
            network_interfaces: Vec::new(),
            network_interfaces_loaded: false,
            log_level_current: None,
            log_level_default: None,
            gst_log_level_current: None,
            gst_log_level_default: None,
            available_channels: Vec::new(),
            available_channels_loaded: false,
            last_inter_input_refresh: None,
            log_entries: Vec::new(),
            show_log_panel: false,
            show_flow_list_panel: true,
            show_palette_panel: true,
            max_log_entries: 100,
            current_page: AppPage::default(),
            app_mode: AppMode::default(),
            started_in_live_mode: false,
            discovery_page: crate::discovery::DiscoveryPage::new(),
            clocks_page: crate::clocks::ClocksPage::new(),
            media_page: crate::media::MediaPage::new(),
            info_page: crate::info_page::InfoPage::new(),
            renderer_info,
            links_page: crate::links::LinksPage::new(),
            flow_filter: String::new(),
            show_stream_picker_for_block: None,
            show_ndi_picker_for_block: None,
            ndi_search_filter: String::new(),
            focus_target: FocusTarget::None,
            focus_flow_filter_requested: false,
            native_pixels_per_point: cc.egui_ctx.pixels_per_point(),
            key_sequence_buffer: Vec::new(),
            interactive_overlay: None,
            block_thumbnails: std::collections::HashMap::new(),
            block_thumbnail_fetch_times: std::collections::HashMap::new(),
            block_thumbnail_loading: std::collections::HashSet::new(),
            qr_inline: None,
            qr_cache: crate::qr::QrCache::new(),
            recorder_filenames: std::collections::HashMap::new(),
            recorder_start_times: std::collections::HashMap::new(),
            live_property_debounce: std::collections::HashMap::new(),
        };

        // Note: Settings (theme, zoom) are applied in first update() frame for iOS compatibility

        // Load default elements temporarily (will be replaced by API data)
        app.palette.load_default_elements();

        // Check authentication status first
        app.check_auth_status(cc.egui_ctx.clone());

        app
    }

    /// Create a new application instance with a specific mode (WASM).
    #[cfg(target_arch = "wasm32")]
    pub fn new_with_mode(cc: &eframe::CreationContext<'_>, app_mode: AppMode) -> Self {
        let mut app = Self::new(cc);

        // Set the app mode
        let is_live = matches!(app_mode, AppMode::Live { .. });
        if let AppMode::Live { ref flow_id, .. } = app_mode {
            // In live mode, trigger navigation to the specific flow
            app.pending_flow_navigation = Some(*flow_id);
        }
        app.app_mode = app_mode;
        app.started_in_live_mode = is_live;

        app
    }

    /// Internal constructor shared by all creation methods (native version).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn new_internal(
        cc: &eframe::CreationContext<'_>,
        api_base_url: String,
        shutdown_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        port: u16,
        tls_enabled: bool,
        auth_token: Option<String>,
    ) -> Self {
        // Install image loaders for egui (required for Image::from_bytes)
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // Load Phosphor icon fonts
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        cc.egui_ctx.set_fonts(fonts);

        let renderer_info = crate::info_page::detect_renderer(cc);

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
            pending_flow_selection: None,
            ws_client: None,
            connection_state: ConnectionState::Disconnected,
            channels,
            editing_properties_flow_id: None,
            properties_name_buffer: String::new(),
            properties_description_buffer: String::new(),
            properties_clock_type_buffer: strom_types::flow::GStreamerClockType::Monotonic,
            properties_ptp_domain_buffer: String::new(),
            properties_ntp_server_buffer: String::new(),
            properties_ntp_port_buffer: String::new(),
            properties_direct_media_timing_buffer: None,
            properties_thread_priority_buffer: strom_types::flow::ThreadPriority::High,
            properties_cpu_affinity_buffer: strom_types::flow::CpuAffinity::default(),
            properties_ephemeral_buffer: false,
            shutdown_flag,
            port,
            tls_enabled,
            auth_token,
            audioanalyzer_data: AudioAnalyzerDataStore::new(),
            meter_data: MeterDataStore::new(),
            spectrum_data: SpectrumDataStore::new(),
            loudness_data: LoudnessDataStore::new(),
            latency_data: LatencyDataStore::new(),
            mediaplayer_data: MediaPlayerDataStore::new(),
            seek_throttle: SeekThrottle::new(),
            webrtc_stats: WebRtcStatsStore::new(),
            system_monitor: SystemMonitorStore::new(),
            thread_monitor: ThreadMonitorStore::new(),
            ptp_stats: crate::ptp_monitor::PtpStatsStore::new(),
            qos_stats: crate::qos_monitor::QoSStore::new(),
            buffer_age_data: BufferAgeStore::new(),
            flow_start_times: std::collections::HashMap::new(),
            show_system_monitor: false,
            system_monitor_tab: crate::system_monitor::SystemMonitorTab::default(),
            thread_sort_column: crate::system_monitor::ThreadSortColumn::default(),
            thread_sort_direction: crate::system_monitor::SortDirection::default(),
            pending_thread_nav_action: None,
            last_webrtc_poll: instant::Instant::now(),
            settings: cc
                .storage
                .and_then(|s| eframe::get_value(s, APP_SETTINGS_KEY))
                .unwrap_or_default(),
            needs_initial_settings_apply: true,
            system_info: None,
            auth_status: None,
            checking_auth: false,
            show_import_dialog: false,
            import_format: ImportFormat::default(),
            import_json_buffer: String::new(),
            import_error: None,
            pending_gst_launch_export: None,
            latency_cache: std::collections::HashMap::new(),
            last_latency_fetch: instant::Instant::now(),
            rtp_stats_cache: std::collections::HashMap::new(),
            last_rtp_stats_fetch: instant::Instant::now(),
            system_clock_info: None,
            system_clock_unsupported: false,
            last_system_clock_fetch: instant::Instant::now() - std::time::Duration::from_secs(10),

            compositor_editor: None,
            mixer_editor: None,
            playlist_editor: None,
            routing_matrix_editor: None,
            network_interfaces: Vec::new(),
            network_interfaces_loaded: false,
            log_level_current: None,
            log_level_default: None,
            gst_log_level_current: None,
            gst_log_level_default: None,
            available_channels: Vec::new(),
            available_channels_loaded: false,
            last_inter_input_refresh: None,
            log_entries: Vec::new(),
            show_log_panel: false,
            show_flow_list_panel: true,
            show_palette_panel: true,
            max_log_entries: 100,
            current_page: AppPage::default(),
            app_mode: AppMode::default(),
            started_in_live_mode: false,
            discovery_page: crate::discovery::DiscoveryPage::new(),
            clocks_page: crate::clocks::ClocksPage::new(),
            media_page: crate::media::MediaPage::new(),
            info_page: crate::info_page::InfoPage::new(),
            renderer_info,
            links_page: crate::links::LinksPage::new(),
            flow_filter: String::new(),
            show_stream_picker_for_block: None,
            show_ndi_picker_for_block: None,
            ndi_search_filter: String::new(),
            focus_target: FocusTarget::None,
            focus_flow_filter_requested: false,
            native_pixels_per_point: cc.egui_ctx.pixels_per_point(),
            key_sequence_buffer: Vec::new(),
            interactive_overlay: None,
            block_thumbnails: std::collections::HashMap::new(),
            block_thumbnail_fetch_times: std::collections::HashMap::new(),
            block_thumbnail_loading: std::collections::HashSet::new(),
            qr_inline: None,
            qr_cache: crate::qr::QrCache::new(),
            recorder_filenames: std::collections::HashMap::new(),
            recorder_start_times: std::collections::HashMap::new(),
            live_property_debounce: std::collections::HashMap::new(),
        };

        // Note: Settings (theme, zoom) are applied in first update() frame for iOS compatibility

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
        tls_enabled: bool,
        shutdown_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let scheme = if tls_enabled { "https" } else { "http" };
        let api_base_url = format!("{}://localhost:{}/api", scheme, port);
        Self::new_internal(
            cc,
            api_base_url,
            Some(shutdown_flag),
            port,
            tls_enabled,
            None,
        )
    }

    /// Create a new application instance with shutdown handler and auth token (native mode only).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new_with_shutdown_and_auth(
        cc: &eframe::CreationContext<'_>,
        port: u16,
        tls_enabled: bool,
        shutdown_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        auth_token: Option<String>,
    ) -> Self {
        let scheme = if tls_enabled { "https" } else { "http" };
        let api_base_url = format!("{}://localhost:{}/api", scheme, port);
        Self::new_internal(
            cc,
            api_base_url,
            Some(shutdown_flag),
            port,
            tls_enabled,
            auth_token,
        )
    }

    /// Apply the current theme preference to the UI context.
    pub(super) fn apply_theme(&self, ctx: egui::Context) {
        use crate::themes;

        tracing::debug!("Applying theme: {:?}", self.settings.theme);

        let visuals = match self.settings.theme {
            ThemePreference::EguiDark => egui::Visuals::dark(),
            ThemePreference::EguiLight => egui::Visuals::light(),
            ThemePreference::NordDark => themes::nord_dark(),
            ThemePreference::NordLight => themes::nord_light(),
            ThemePreference::TokyoNight => themes::tokyo_night(),
            ThemePreference::TokyoNightStorm => themes::tokyo_night_storm(),
            ThemePreference::TokyoNightLight => themes::tokyo_night_light(),
            ThemePreference::ClaudeDark => themes::claude_dark(),
            ThemePreference::ClaudeLight => themes::claude_light(),
        };
        ctx.set_visuals(visuals);
    }

    /// Set up WebSocket connection for real-time updates.
    pub(super) fn setup_websocket_connection(&mut self, ctx: egui::Context) {
        tracing::info!("Setting up WebSocket connection for real-time updates");

        // WebSocket URL - different logic for WASM vs native
        #[cfg(target_arch = "wasm32")]
        let ws_url = {
            if let Some(window) = web_sys::window() {
                if let Ok(host) = window.location().host() {
                    let protocol = window.location().protocol().ok();
                    let is_https = protocol.as_deref() == Some("https:");

                    // Port 8095: trunk serve mode - rewrite to backend port 8080
                    if !is_https && host.ends_with(":8095") {
                        let hostname = host.trim_end_matches(":8095");
                        let url = format!("ws://{}:8080/api/ws", hostname);
                        tracing::info!("WebSocket URL (trunk serve mode): {}", url);
                        url
                    } else {
                        let ws_protocol = if is_https { "wss" } else { "ws" };
                        let url = format!("{}://{}/api/ws", ws_protocol, host);
                        tracing::info!("WebSocket URL: {}", url);
                        url
                    }
                } else {
                    "/api/ws".to_string()
                }
            } else {
                "/api/ws".to_string()
            }
        };

        #[cfg(not(target_arch = "wasm32"))]
        let ws_url = {
            let scheme = if self.tls_enabled { "wss" } else { "ws" };
            format!("{}://localhost:{}/api/ws", scheme, self.port)
        };

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
}
