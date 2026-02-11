//! Embedded static assets for the frontend and WHEP player.

use axum::{
    body::Body,
    http::{header, Response, StatusCode, Uri},
    response::IntoResponse,
};
use rust_embed::RustEmbed;

/// Embedded frontend assets (WASM app)
#[derive(RustEmbed)]
#[folder = "dist/"]
pub struct Assets;

/// Embedded WHEP player assets (CSS, JS, HTML templates)
#[derive(RustEmbed)]
#[folder = "static/whep/"]
pub struct WhepAssets;

/// Embedded WHIP ingest assets (CSS, JS, HTML templates)
#[derive(RustEmbed)]
#[folder = "static/whip/"]
pub struct WhipAssets;

/// Shared WebRTC page assets (CSS, JS shared across WHIP/WHEP pages)
#[derive(RustEmbed)]
#[folder = "static/webrtc/"]
pub struct WebrtcAssets;

/// Embedded icon assets (favicons, app icons, etc.)
#[derive(RustEmbed)]
#[folder = "../assets/"]
pub struct IconAssets;

/// Serve embedded static files
pub async fn serve_static(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    // Serve icon assets from /assets/ path
    if let Some(asset_path) = path.strip_prefix("assets/") {
        if let Some(content) = IconAssets::get(asset_path) {
            let mime = mime_guess::from_path(asset_path).first_or_octet_stream();
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, "public, max-age=31536000") // Cache for 1 year
                .body(Body::from(content.data))
                .unwrap();
        }
    }

    // If path is empty, serve index.html
    let path = if path.is_empty() || path == "index.html" {
        "index.html"
    } else {
        path
    };

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from(content.data))
                .unwrap()
        }
        None => {
            // If file not found, try to serve index.html for SPA routing
            if let Some(index) = Assets::get("index.html") {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/html")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .body(Body::from(index.data))
                    .unwrap()
            } else {
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("404 Not Found"))
                    .unwrap()
            }
        }
    }
}
