//! Strom backend library.
//!
//! This module exposes the application builder for use in tests.

use axum::extract::DefaultBodyLimit;
use axum::http::HeaderValue;
use axum::http::{header, HeaderName, Method};
use axum::{
    middleware,
    routing::{delete, get, patch, post, put},
    Extension, Router,
};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_sessions::{cookie::time::Duration, Expiry, MemoryStore, SessionManagerLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

pub mod api;
pub mod assets;
pub mod auth;
pub mod blocks;
pub mod config;
pub mod discovery;
pub mod events;
pub mod gpu;
pub mod gst;
pub mod gui;
pub mod layout;
pub mod mcp;
pub mod network;
pub mod openapi;
pub mod paths;
pub mod ptp_monitor;
pub mod rtsp_server;
pub mod sharing;
pub mod state;
pub mod stats;
pub mod storage;
pub mod system_monitor;
pub mod thread_registry;
pub mod tls;
pub mod version;
pub mod whep_registry;
pub mod whip_registry;

use state::AppState;

/// Create the Axum application router.
///
/// This function is used both by the main server binary and by integration tests.
pub async fn create_app() -> Router {
    create_app_with_state(AppState::default()).await
}

/// Create the Axum application router with a given state.
pub async fn create_app_with_state(state: AppState) -> Router {
    create_app_with_state_and_auth(state, auth::AuthConfig::from_env()).await
}

/// Create the Axum application router with a given state and auth configuration.
pub async fn create_app_with_state_and_auth(
    state: AppState,
    auth_config: auth::AuthConfig,
) -> Router {
    create_app_with_config(state, auth_config, Vec::new()).await
}

/// Create the Axum application router with a given state, auth configuration, and CORS origins.
///
/// If `cors_allowed_origins` is empty, any origin is allowed.
/// Otherwise, only the specified origins are allowed.
pub async fn create_app_with_config(
    state: AppState,
    auth_config: auth::AuthConfig,
    cors_allowed_origins: Vec<String>,
) -> Router {
    // Note: GStreamer is already initialized in main.rs before this is called.
    // DO NOT call gst::init() here - it can corrupt internal state if pipelines
    // are already running (e.g., during auto-restart at startup).

    let auth_config = Arc::new(auth_config);

    if auth_config.enabled {
        tracing::info!("Authentication enabled");
        if auth_config.has_session_auth() {
            tracing::info!("  - Session authentication configured");
        }
        if auth_config.has_api_key_auth() {
            tracing::info!("  - API key authentication configured");
        }
    } else {
        tracing::warn!("Authentication disabled - all endpoints are public!");
    }

    // Create session store (in-memory, sessions lost on restart)
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_expiry(Expiry::OnInactivity(Duration::hours(24)))
        .with_secure(false);

    // Build protected API router (requires authentication)
    let protected_api_router = Router::new()
        .route("/flows", get(api::flows::list_flows))
        .route("/flows", post(api::flows::create_flow))
        .route("/flows/{id}", get(api::flows::get_flow))
        .route("/flows/{id}", post(api::flows::update_flow))
        .route("/flows/{id}", put(api::flows::update_flow_put))
        .route("/flows/{id}", delete(api::flows::delete_flow))
        .route("/flows/{id}/start", post(api::flows::start_flow))
        .route("/flows/{id}/stop", post(api::flows::stop_flow))
        .route("/flows/{id}/latency", get(api::flows::get_flow_latency))
        .route("/flows/{id}/rtp-stats", get(api::flows::get_flow_rtp_stats))
        .route("/flows/{id}/debug", get(api::flows::get_flow_debug_info))
        .route("/flows/{id}/debug-graph", get(api::flows::debug_graph))
        .route(
            "/flows/{id}/dynamic-pads",
            get(api::flows::get_dynamic_pads),
        )
        .route(
            "/flows/{id}/properties",
            patch(api::flows::update_flow_properties),
        )
        .route(
            "/flows/{id}/webrtc-stats",
            get(api::flows::get_webrtc_stats),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/sdp",
            get(api::flows::get_block_sdp),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/properties",
            get(api::flows::get_element_properties),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/properties",
            patch(api::flows::update_element_property),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/pads/{pad_name}/properties",
            get(api::flows::get_pad_properties),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/pads/{pad_name}/properties",
            patch(api::flows::update_pad_property),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/transition",
            post(api::flows::trigger_transition),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/animate",
            post(api::flows::animate_input),
        )
        .route(
            "/flows/{id}/compositor/{block_id}/thumbnail/{input_idx}",
            get(api::flows::get_compositor_thumbnail),
        )
        .route("/elements", get(api::elements::list_elements))
        .route("/elements/{name}", get(api::elements::get_element_info))
        .route(
            "/elements/{name}/pads",
            get(api::elements::get_element_pad_properties),
        )
        .route("/blocks", get(api::blocks::list_blocks))
        .route("/blocks", post(api::blocks::create_block))
        .route("/blocks/categories", get(api::blocks::get_categories))
        .route("/blocks/{id}", get(api::blocks::get_block))
        .route(
            "/blocks/{id}",
            axum::routing::put(api::blocks::update_block),
        )
        .route(
            "/blocks/{id}",
            axum::routing::delete(api::blocks::delete_block),
        )
        .route("/version", get(api::version::get_version))
        .route("/ws", get(api::websocket::websocket_handler))
        // gst-launch-1.0 import/export
        .route("/gst-launch/parse", post(api::gst_launch::parse_gst_launch))
        .route(
            "/gst-launch/export",
            post(api::gst_launch::export_gst_launch),
        )
        // Network interfaces
        .route("/network/interfaces", get(api::network::list_interfaces))
        // Sources (for inter-pipeline sharing)
        .route("/sources", get(api::flows::get_available_sources))
        // Discovery (SAP/AES67)
        .route("/discovery/streams", get(api::discovery::list_streams))
        .route("/discovery/streams/{id}", get(api::discovery::get_stream))
        .route(
            "/discovery/streams/{id}/sdp",
            get(api::discovery::get_stream_sdp),
        )
        .route("/discovery/announced", get(api::discovery::list_announced))
        // Device Discovery (generic - audio, video, NDI, etc.)
        .route(
            "/discovery/devices/status",
            get(api::discovery::device_status),
        )
        .route("/discovery/devices", get(api::discovery::list_devices))
        .route("/discovery/devices/{id}", get(api::discovery::get_device))
        .route(
            "/discovery/devices/refresh",
            post(api::discovery::refresh_devices),
        )
        // NDI Discovery (backward compatibility)
        .route("/discovery/ndi/status", get(api::discovery::ndi_status))
        .route(
            "/discovery/ndi/sources",
            get(api::discovery::list_ndi_sources),
        )
        .route(
            "/discovery/ndi/refresh",
            post(api::discovery::refresh_ndi_sources),
        )
        // Media file management
        .route("/media", get(api::media::list_media))
        .route("/media/file/{*path}", get(api::media::download_file))
        .route(
            "/media/upload",
            post(api::media::upload_files).layer(DefaultBodyLimit::max(500 * 1024 * 1024)), // 500MB limit
        )
        .route("/media/rename", post(api::media::rename_media))
        .route("/media/file/{*path}", delete(api::media::delete_file))
        .route("/media/directory", post(api::media::create_directory))
        .route(
            "/media/directory/{*path}",
            delete(api::media::delete_directory),
        )
        // Media player controls
        .route(
            "/flows/{flow_id}/blocks/{block_id}/player/state",
            get(api::mediaplayer::get_player_state),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/player/playlist",
            post(api::mediaplayer::set_playlist),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/player/control",
            post(api::mediaplayer::control_player),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/player/seek",
            post(api::mediaplayer::seek_player),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/player/goto",
            post(api::mediaplayer::goto_file),
        )
        // Apply authentication middleware to all protected routes
        .layer(middleware::from_fn(auth::auth_middleware));

    // Build public API router (no authentication required)
    let public_api_router = Router::new()
        .route("/login", post(auth::login_handler))
        .route("/logout", post(auth::logout_handler))
        .route("/auth/status", get(auth::auth_status_handler))
        // WHEP streams list API (JSON)
        .route("/whep-streams", get(api::whep_player::list_whep_streams))
        // WHIP endpoints list API (JSON)
        .route(
            "/whip-endpoints",
            get(api::whip_ingest::list_whip_endpoints),
        )
        // Client-side log relay (WHIP/WHEP browser pages)
        .route("/client-log", post(api::whip_ingest::client_log))
        // ICE servers for WebRTC connections
        .route("/ice-servers", get(api::whep_player::get_ice_servers))
        // MCP Streamable HTTP endpoint (has its own session management)
        .route("/mcp", post(api::mcp::mcp_post))
        .route("/mcp", get(api::mcp::mcp_get))
        .route("/mcp", delete(api::mcp::mcp_delete));

    // Player/ingest pages (HTML) - outside /api
    let player_router = Router::new()
        .route("/whep", get(api::whep_player::whep_player))
        .route("/whep-streams", get(api::whep_player::whep_streams_page))
        .route("/whip-ingest", get(api::whip_ingest::whip_ingest_page))
        .with_state(state.clone());

    // WHEP proxy routes - outside /api (acts as WHEP server endpoint)
    let whep_router = Router::new()
        .route(
            "/{endpoint_id}",
            post(api::whep_player::whep_endpoint_proxy),
        )
        .route(
            "/{endpoint_id}",
            axum::routing::options(api::whep_player::whep_endpoint_proxy_options),
        )
        .route(
            "/{endpoint_id}/resource/{resource_id}",
            delete(api::whep_player::whep_resource_proxy_delete),
        )
        .route(
            "/{endpoint_id}/resource/{resource_id}",
            patch(api::whep_player::whep_resource_proxy_patch),
        )
        .route(
            "/{endpoint_id}/resource/{resource_id}",
            axum::routing::options(api::whep_player::whep_resource_proxy_options),
        )
        .with_state(state.clone());

    // WHIP proxy routes - outside /api (acts as WHIP server endpoint)
    let whip_router = Router::new()
        .route("/{endpoint_id}", post(api::whip_ingest::whip_post))
        .route(
            "/{endpoint_id}",
            axum::routing::options(api::whip_ingest::whip_options),
        )
        .route(
            "/{endpoint_id}/resource/{resource_id}",
            delete(api::whip_ingest::whip_resource_delete),
        )
        .route(
            "/{endpoint_id}/resource/{resource_id}",
            patch(api::whip_ingest::whip_resource_patch),
        )
        .route(
            "/{endpoint_id}/resource/{resource_id}",
            axum::routing::options(api::whip_ingest::whip_resource_options),
        )
        .with_state(state.clone());

    // Static assets for WHEP player and WHIP ingest
    let static_router = Router::new()
        .route(
            "/webrtc.css",
            get(|| async {
                serve_embedded_asset::<assets::WebrtcAssets>("webrtc.css", "text/css")
            }),
        )
        .route(
            "/webrtc.js",
            get(|| async {
                serve_embedded_asset::<assets::WebrtcAssets>("webrtc.js", "application/javascript")
            }),
        )
        .route(
            "/devices.js",
            get(|| async {
                serve_embedded_asset::<assets::WebrtcAssets>("devices.js", "application/javascript")
            }),
        )
        .route(
            "/whep.css",
            get(|| async { serve_embedded_asset::<assets::WhepAssets>("whep.css", "text/css") }),
        )
        .route(
            "/whep.js",
            get(|| async {
                serve_embedded_asset::<assets::WhepAssets>("whep.js", "application/javascript")
            }),
        )
        .route(
            "/whip.js",
            get(|| async {
                serve_embedded_asset::<assets::WhipAssets>("whip.js", "application/javascript")
            }),
        )
        .route(
            "/whip.css",
            get(|| async { serve_embedded_asset::<assets::WhipAssets>("whip.css", "text/css") }),
        );

    // Create MCP session manager
    let mcp_sessions = mcp::McpSessionManager::new();

    // Combine routers with auth config and MCP session manager extensions
    let api_router = Router::new()
        .merge(public_api_router)
        .merge(protected_api_router)
        .layer(Extension(auth_config.clone()))
        .layer(Extension(mcp_sessions));

    // Build Swagger UI router behind authentication
    let swagger_router = Router::new()
        .merge(
            SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi::ApiDoc::openapi()),
        )
        .layer(middleware::from_fn(auth::auth_middleware))
        .layer(Extension(auth_config));

    // Build main router
    Router::new()
        .route("/health", get(health))
        .merge(swagger_router)
        .nest("/api", api_router)
        .nest("/player", player_router)
        .nest("/whep", whep_router)
        .nest("/whip", whip_router)
        .nest("/static", static_router)
        .layer(session_layer)
        .layer({
            let cors = CorsLayer::new()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::AUTHORIZATION,
                    header::ACCEPT,
                    header::COOKIE,
                    HeaderName::from_static("mcp-session-id"),
                ])
                .expose_headers([HeaderName::from_static("mcp-session-id")]);

            // If no origins specified, allow any origin
            // Otherwise, restrict to the specified origins
            if cors_allowed_origins.is_empty() {
                cors.allow_origin(Any)
            } else {
                let origins: Vec<HeaderValue> = cors_allowed_origins
                    .iter()
                    .filter_map(|o| o.parse::<HeaderValue>().ok())
                    .collect();
                cors.allow_origin(origins).allow_credentials(true)
            }
        })
        .with_state(state)
        // Serve embedded frontend for all other routes
        .fallback(assets::serve_static)
}

/// Health check endpoint.
async fn health() -> &'static str {
    "OK"
}

/// Serve an embedded static asset with the given content type.
fn serve_embedded_asset<T: rust_embed::RustEmbed>(
    file: &str,
    content_type: &str,
) -> axum::response::Response {
    match T::get(file) {
        Some(content) => axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, content_type)
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(axum::body::Body::from(content.data))
            .unwrap(),
        None => axum::response::Response::builder()
            .status(axum::http::StatusCode::NOT_FOUND)
            .body(axum::body::Body::from("Not found"))
            .unwrap(),
    }
}
