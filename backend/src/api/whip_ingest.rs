//! WHIP ingest proxy and page handlers.
//!
//! Proxies WHIP POST/PATCH/DELETE requests from external clients to internal
//! whipserversrc instances, similar to how whep_player.rs proxies for WHEP.
//!
//! Also serves the WHIP ingest HTML page for browser-based camera/mic sending.

use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use tracing::{debug, error, info, warn};

/// Serve the WHIP ingest page.
pub async fn whip_ingest_page(State(state): State<AppState>) -> impl IntoResponse {
    let endpoints = state.whip_registry().list_all().await;
    let _ = endpoints; // Page fetches endpoints via JS

    match crate::assets::WhipAssets::get("ingest.html") {
        Some(content) => {
            let html = std::str::from_utf8(content.data.as_ref()).unwrap_or("");
            Html(html.to_string()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Ingest page not found").into_response(),
    }
}

/// List active WHIP endpoints (public API, no auth required).
pub async fn list_whip_endpoints(State(state): State<AppState>) -> impl IntoResponse {
    let endpoints = state.whip_registry().list_all().await;
    let list: Vec<serde_json::Value> = endpoints
        .into_iter()
        .map(|(id, entry)| {
            serde_json::json!({
                "endpoint_id": id,
                "mode": entry.mode.as_str(),
            })
        })
        .collect();
    axum::Json(list).into_response()
}

/// Handle WHIP POST request (SDP offer from client).
///
/// Proxies the SDP offer to the internal whipserversrc HTTP server and returns
/// the SDP answer, rewriting the Location header to use the proxy path.
pub async fn whip_post(
    State(state): State<AppState>,
    Path(endpoint_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    debug!("WHIP POST for endpoint: {}", endpoint_id);

    let port = match state.whip_registry().get_port(&endpoint_id).await {
        Some(port) => port,
        None => {
            warn!("WHIP endpoint not found: {}", endpoint_id);
            return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
        }
    };

    // Forward the request to the internal whipserversrc
    let internal_url = format!("http://127.0.0.1:{}/whip/endpoint", port);

    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    // Build the forwarded request
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return (StatusCode::BAD_REQUEST, "Failed to read body").into_response();
        }
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/sdp");

    // Strip RED/RTX/ULPFEC from SDP offer to prevent whipserversrc's internal
    // decodebin3 from failing on these meta-encodings it can't decode.
    let body_bytes = if content_type.contains("sdp") {
        if let Ok(sdp_str) = std::str::from_utf8(&body_bytes) {
            let cleaned = strip_redundancy_codecs(sdp_str);
            debug!("WHIP: SDP after stripping redundancy codecs:\n{}", cleaned);
            axum::body::Bytes::from(cleaned)
        } else {
            body_bytes
        }
    } else {
        body_bytes
    };

    let mut req = client
        .post(&internal_url)
        .header(header::CONTENT_TYPE, content_type)
        .body(body_bytes);

    // Forward Authorization header if present
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        req = req.header(header::AUTHORIZATION, auth.clone());
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to proxy WHIP POST to {}: {}", internal_url, e);
            return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response();
        }
    };

    let status = response.status();
    let resp_headers = response.headers().clone();
    let resp_body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read proxy response: {}", e);
            return (StatusCode::BAD_GATEWAY, "Failed to read response").into_response();
        }
    };

    // Patch the SDP answer for better Chrome bandwidth estimation:
    // 1. Add goog-remb as fallback bandwidth estimation
    // 2. Add x-google bitrate hints to the video fmtp line so Chrome
    //    starts at a reasonable bitrate (webrtcbin strips these from
    //    fmtp and puts them as standalone a=x-google-* attributes that
    //    Chrome ignores for bandwidth estimation)
    //
    // NOTE: We intentionally do NOT rewrite extmap IDs in the answer.
    // webrtcbin assigns its own extmap IDs internally.
    let resp_body = if let Ok(answer_str) = std::str::from_utf8(&resp_body) {
        let patched = add_goog_remb(answer_str);
        let patched = fix_video_bitrate_hints(&patched);
        debug!("WHIP: SDP answer:\n{}", patched);
        axum::body::Bytes::from(patched)
    } else {
        resp_body
    };

    // Build the response with rewritten headers
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));

    // Rewrite Location header: /whip/resource/{id} -> /whip/{endpoint_id}/resource/{id}
    if let Some(location) = resp_headers.get(header::LOCATION) {
        if let Ok(loc_str) = location.to_str() {
            let rewritten = if let Some(path_after_whip) = loc_str.strip_prefix("/whip/") {
                format!("/whip/{}/{}", endpoint_id, path_after_whip)
            } else {
                loc_str.to_string()
            };
            info!("WHIP: Rewriting Location: {} -> {}", loc_str, rewritten);
            builder = builder.header(header::LOCATION, &rewritten);
        }
    }

    // Forward relevant headers
    for (name, value) in &resp_headers {
        let name_str = name.as_str().to_lowercase();
        match name_str.as_str() {
            "content-type" | "link" | "accept-patch" | "etag" => {
                builder = builder.header(name, value);
            }
            _ => {}
        }
    }

    // Add CORS headers
    builder = builder
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Methods",
            "POST, PATCH, DELETE, OPTIONS",
        )
        .header(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, If-Match",
        )
        .header(
            "Access-Control-Expose-Headers",
            "Location, Link, Accept-Patch, ETag",
        );

    match builder.body(Body::from(resp_body)) {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            error!("Failed to build response: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Response build error").into_response()
        }
    }
}

/// Handle WHIP PATCH request (ICE trickle from client).
pub async fn whip_resource_patch(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    debug!(
        "WHIP PATCH for endpoint: {}, resource: {}",
        endpoint_id, resource_id
    );

    let port = match state.whip_registry().get_port(&endpoint_id).await {
        Some(port) => port,
        None => {
            return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
        }
    };

    let internal_url = format!("http://127.0.0.1:{}/whip/resource/{}", port, resource_id);

    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read PATCH body: {}", e);
            return (StatusCode::BAD_REQUEST, "Failed to read body").into_response();
        }
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/trickle-ice-sdpfrag");

    let response = match client
        .patch(&internal_url)
        .header(header::CONTENT_TYPE, content_type)
        .body(body_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to proxy WHIP PATCH: {}", e);
            return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response();
        }
    };

    let status = response.status();
    let resp_body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read PATCH proxy response: {}", e);
            axum::body::Bytes::new()
        }
    };

    let builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("Access-Control-Allow-Origin", "*");

    match builder.body(Body::from(resp_body)) {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            error!("Failed to build PATCH response: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Response error").into_response()
        }
    }
}

/// Handle WHIP DELETE request (client disconnect).
pub async fn whip_resource_delete(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
) -> impl IntoResponse {
    info!(
        "WHIP DELETE for endpoint: {}, resource: {}",
        endpoint_id, resource_id
    );

    let port = match state.whip_registry().get_port(&endpoint_id).await {
        Some(port) => port,
        None => {
            return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
        }
    };

    let internal_url = format!("http://127.0.0.1:{}/whip/resource/{}", port, resource_id);

    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    match client.delete(&internal_url).send().await {
        Ok(r) => {
            let status = r.status();
            Response::builder()
                .status(
                    StatusCode::from_u16(status.as_u16())
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                )
                .header("Access-Control-Allow-Origin", "*")
                .body(Body::empty())
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap()
                })
                .into_response()
        }
        Err(e) => {
            error!("Failed to proxy WHIP DELETE: {}", e);
            (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response()
        }
    }
}

/// Handle CORS preflight for WHIP endpoints.
pub async fn whip_options() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Methods",
            "POST, PATCH, DELETE, OPTIONS",
        )
        .header(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, If-Match",
        )
        .header(
            "Access-Control-Expose-Headers",
            "Location, Link, Accept-Patch, ETag",
        )
        .body(Body::empty())
        .unwrap()
}

/// Handle CORS preflight for WHIP resource endpoints.
pub async fn whip_resource_options() -> impl IntoResponse {
    whip_options().await
}

/// Strip RED, RTX and ULPFEC payload types from an SDP offer.
///
/// Browsers negotiate these meta-encodings for redundancy/retransmission,
/// but whipserversrc's internal decodebin3 can't decode them, causing
/// "No streams to output" errors. Removing them from the offer prevents
/// the browser from using them.
fn strip_redundancy_codecs(sdp: &str) -> String {
    // Collect payload type numbers for RED, RTX, and ULPFEC
    let mut blocked_pts: Vec<String> = Vec::new();

    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            // Format: "a=rtpmap:97 red/90000" or "a=rtpmap:98 rtx/90000"
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() == 2 {
                let encoding = parts[1].to_lowercase();
                if encoding.starts_with("red/")
                    || encoding.starts_with("rtx/")
                    || encoding.starts_with("ulpfec/")
                {
                    blocked_pts.push(parts[0].to_string());
                }
            }
        }
    }

    if blocked_pts.is_empty() {
        return sdp.to_string();
    }

    info!(
        "WHIP: Stripping redundancy payload types from SDP: {:?}",
        blocked_pts
    );

    let mut result = Vec::new();

    for line in sdp.lines() {
        // Remove a=rtpmap lines for blocked PTs
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let pt = rest.split(' ').next().unwrap_or("");
            if blocked_pts.contains(&pt.to_string()) {
                continue;
            }
        }

        // Remove a=fmtp lines for blocked PTs
        if let Some(rest) = line.strip_prefix("a=fmtp:") {
            let pt = rest.split(' ').next().unwrap_or("");
            if blocked_pts.contains(&pt.to_string()) {
                continue;
            }
        }

        // Remove a=rtcp-fb lines for blocked PTs
        if let Some(rest) = line.strip_prefix("a=rtcp-fb:") {
            let pt = rest.split(' ').next().unwrap_or("");
            if blocked_pts.contains(&pt.to_string()) {
                continue;
            }
        }

        // Remove blocked PTs from m= lines
        if line.starts_with("m=audio ") || line.starts_with("m=video ") {
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            if parts.len() == 4 {
                // parts[3] contains the space-separated payload type list
                let pts: Vec<&str> = parts[3]
                    .split_whitespace()
                    .filter(|pt| !blocked_pts.contains(&pt.to_string()))
                    .collect();
                result.push(format!(
                    "{} {} {} {}",
                    parts[0],
                    parts[1],
                    parts[2],
                    pts.join(" ")
                ));
                continue;
            }
        }

        result.push(line.to_string());
    }

    result.join("\r\n")
}

/// Add `goog-remb` RTCP feedback to video codecs in the SDP answer.
///
/// REMB (Receiver Estimated Maximum Bitrate) is a fallback bandwidth
/// estimation mechanism. The primary mechanism is transport-cc (TWCC),
/// but adding REMB gives Chrome an additional signal for bandwidth
/// estimation if TWCC feedback is delayed.
///
/// Uses targeted string insertion to preserve original SDP bytes and
/// line endings exactly as webrtcbin produced them.
fn add_goog_remb(sdp: &str) -> String {
    // Only operate within the video section
    let video_start = match sdp.find("m=video") {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    let video_section = &sdp[video_start..];

    // Find "transport-cc" in the video section (as part of a=rtcp-fb line)
    let tc_needle = " transport-cc";
    let tc_rel = match video_section.find(tc_needle) {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    // Walk backwards to find start of this a=rtcp-fb line
    let line_start_rel = video_section[..tc_rel]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_content = &video_section[line_start_rel..tc_rel + tc_needle.len()];

    // Extract PT number from the line
    let pt = match line_content.strip_prefix("a=rtcp-fb:") {
        Some(rest) => rest.split(' ').next().unwrap_or(""),
        None => return sdp.to_string(),
    };

    let remb_line = format!("a=rtcp-fb:{} goog-remb", pt);
    if sdp.contains(&remb_line) {
        return sdp.to_string();
    }

    // Find the end of the transport-cc line (the next \n)
    let abs_tc_end = video_start + tc_rel + tc_needle.len();
    let newline_pos = match sdp[abs_tc_end..].find('\n') {
        Some(pos) => abs_tc_end + pos + 1, // position after the \n
        None => return sdp.to_string(),
    };

    // Detect line ending style from the original SDP
    let line_ending = if newline_pos >= 2 && sdp.as_bytes().get(newline_pos - 2) == Some(&b'\r') {
        "\r\n"
    } else {
        "\n"
    };

    // Insert goog-remb line right after the transport-cc line
    let mut result = String::with_capacity(sdp.len() + remb_line.len() + 2);
    result.push_str(&sdp[..newline_pos]);
    result.push_str(&remb_line);
    result.push_str(line_ending);
    result.push_str(&sdp[newline_pos..]);

    info!("WHIP: Added {} to SDP answer", remb_line);
    result
}

/// Move x-google bitrate hints into the video codec's `a=fmtp:` line.
///
/// webrtcbin echoes x-google-{min,start,max}-bitrate from the offer but
/// places them as standalone `a=x-google-*:` SDP attributes instead of
/// keeping them in the `a=fmtp:` line. Chrome only processes these hints
/// when they appear inside the fmtp parameters, so without this fix the
/// browser starts at its default ~300 kbps instead of the intended 2 Mbps.
///
/// This function:
/// 1. Reads the values from standalone `a=x-google-*:` lines (or uses defaults)
/// 2. Appends them to the video codec fmtp line
/// 3. Removes the standalone lines
fn fix_video_bitrate_hints(sdp: &str) -> String {
    // Extract existing standalone x-google values
    let mut min_bitrate: Option<&str> = None;
    let mut start_bitrate: Option<&str> = None;
    let mut max_bitrate: Option<&str> = None;

    for line in sdp.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("a=x-google-min-bitrate:") {
            min_bitrate = Some(val.trim());
        } else if let Some(val) = trimmed.strip_prefix("a=x-google-start-bitrate:") {
            start_bitrate = Some(val.trim());
        } else if let Some(val) = trimmed.strip_prefix("a=x-google-max-bitrate:") {
            max_bitrate = Some(val.trim());
        }
    }

    // Use defaults if standalone lines weren't present
    let min_val = min_bitrate.unwrap_or("2000");
    let start_val = start_bitrate.unwrap_or("2000");
    let max_val = max_bitrate.unwrap_or("4000");

    let hints = format!(
        ";x-google-min-bitrate={};x-google-start-bitrate={};x-google-max-bitrate={}",
        min_val, start_val, max_val
    );

    // Find the video section and its fmtp line
    let video_start = match sdp.find("m=video") {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    // Find the fmtp line in the video section. We need the first a=fmtp:
    // line after m=video that isn't for a meta-codec (RTX/RED/ULPFEC).
    // Since we already stripped those from the offer, the answer should
    // only have the primary video codec's fmtp line.
    let video_section = &sdp[video_start..];
    let fmtp_needle = "a=fmtp:";
    let fmtp_rel = match video_section.find(fmtp_needle) {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    // Find the end of this fmtp line
    let fmtp_abs = video_start + fmtp_rel;
    let fmtp_line_end = sdp[fmtp_abs..]
        .find('\n')
        .map(|p| fmtp_abs + p)
        .unwrap_or(sdp.len());

    // Check if hints are already present
    let fmtp_line = &sdp[fmtp_abs..fmtp_line_end];
    if fmtp_line.contains("x-google-start-bitrate") {
        // Already has hints, just remove standalone lines
        return remove_standalone_x_google(sdp);
    }

    // Insert hints at the end of the fmtp line (before any \r\n)
    let insert_pos = if fmtp_line_end > 0 && sdp.as_bytes().get(fmtp_line_end - 1) == Some(&b'\r') {
        fmtp_line_end - 1 // insert before \r
    } else {
        fmtp_line_end // insert before \n
    };

    let mut result = String::with_capacity(sdp.len() + hints.len());
    result.push_str(&sdp[..insert_pos]);
    result.push_str(&hints);
    result.push_str(&sdp[insert_pos..]);

    info!(
        "WHIP: Added x-google bitrate hints to fmtp line (min={}, start={}, max={})",
        min_val, start_val, max_val
    );

    // Remove standalone a=x-google-* lines
    remove_standalone_x_google(&result)
}

/// Remove standalone `a=x-google-*:` lines from SDP.
fn remove_standalone_x_google(sdp: &str) -> String {
    // Use targeted removal to preserve original SDP bytes
    let mut result = String::with_capacity(sdp.len());
    let mut pos = 0;

    while pos < sdp.len() {
        let remaining = &sdp[pos..];
        let line_end = remaining
            .find('\n')
            .map(|p| p + 1)
            .unwrap_or(remaining.len());
        let line = &remaining[..line_end];
        let trimmed = line.trim();

        if trimmed.starts_with("a=x-google-min-bitrate:")
            || trimmed.starts_with("a=x-google-start-bitrate:")
            || trimmed.starts_with("a=x-google-max-bitrate:")
        {
            // Skip this line
            pos += line_end;
            continue;
        }

        result.push_str(line);
        pos += line_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // strip_redundancy_codecs tests
    // ========================================================================

    #[test]
    fn strip_redundancy_codecs_removes_red_rtx_ulpfec() {
        let sdp = "\
v=0\r\n\
o=- 0 0 IN IP4 127.0.0.1\r\n\
s=-\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111 63\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:63 red/48000/2\r\n\
a=fmtp:63 111/111\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=rtpmap:97 rtx/90000\r\n\
a=fmtp:97 apt=96\r\n\
a=rtpmap:98 red/90000\r\n\
a=rtpmap:99 ulpfec/90000\r\n\
a=rtcp-fb:97 nack\r\n";

        let result = strip_redundancy_codecs(sdp);

        // Primary codecs should remain
        assert!(result.contains("a=rtpmap:111 opus/48000/2"));
        assert!(result.contains("a=rtpmap:96 H264/90000"));
        assert!(result.contains("a=fmtp:96 level-asymmetry-allowed"));

        // Redundancy codecs should be removed
        assert!(!result.contains("a=rtpmap:63 red/"));
        assert!(!result.contains("a=fmtp:63"));
        assert!(!result.contains("a=rtpmap:97 rtx/"));
        assert!(!result.contains("a=fmtp:97"));
        assert!(!result.contains("a=rtpmap:98 red/"));
        assert!(!result.contains("a=rtpmap:99 ulpfec/"));
        assert!(!result.contains("a=rtcp-fb:97"));

        // m= lines should have PTs removed
        assert!(result.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111"));
        assert!(!result.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111 63"));
        assert!(result.contains("m=video 9 UDP/TLS/RTP/SAVPF 96"));
        assert!(!result.contains(" 97"));
    }

    #[test]
    fn strip_redundancy_codecs_noop_without_redundancy() {
        let sdp = "\
v=0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = strip_redundancy_codecs(sdp);
        assert_eq!(result, sdp);
    }

    #[test]
    fn strip_redundancy_codecs_preserves_primary_codecs() {
        let sdp = "\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n\
a=rtcp-fb:111 transport-cc\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtcp-fb:96 nack\r\n\
a=rtcp-fb:96 transport-cc\r\n";

        let result = strip_redundancy_codecs(sdp);
        assert_eq!(result, sdp);
    }

    // ========================================================================
    // add_goog_remb tests
    // ========================================================================

    #[test]
    fn add_goog_remb_inserts_after_transport_cc() {
        let sdp = "\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = add_goog_remb(sdp);

        assert!(result.contains("a=rtcp-fb:96 goog-remb\r\n"));
        // Should be right after transport-cc line
        let tc_pos = result.find("a=rtcp-fb:96 transport-cc").unwrap();
        let remb_pos = result.find("a=rtcp-fb:96 goog-remb").unwrap();
        let fmtp_pos = result.find("a=fmtp:96").unwrap();
        assert!(remb_pos > tc_pos);
        assert!(remb_pos < fmtp_pos);
    }

    #[test]
    fn add_goog_remb_noop_without_video() {
        let sdp = "\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtcp-fb:111 transport-cc\r\n";

        let result = add_goog_remb(sdp);
        assert_eq!(result, sdp);
    }

    #[test]
    fn add_goog_remb_noop_if_already_present() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=rtcp-fb:96 goog-remb\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = add_goog_remb(sdp);
        assert_eq!(result, sdp);
    }

    #[test]
    fn add_goog_remb_preserves_crlf() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=fmtp:96 foo\r\n";

        let result = add_goog_remb(sdp);
        // Every line should end with \r\n
        for line in result.split('\n') {
            if !line.is_empty() {
                assert!(line.ends_with('\r'), "Line missing \\r: {:?}", line);
            }
        }
    }

    // ========================================================================
    // fix_video_bitrate_hints tests
    // ========================================================================

    #[test]
    fn fix_video_bitrate_hints_moves_standalone_to_fmtp() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n\
a=x-google-min-bitrate:1500\r\n\
a=x-google-start-bitrate:1500\r\n\
a=x-google-max-bitrate:3000\r\n";

        let result = fix_video_bitrate_hints(sdp);

        // Standalone lines should be removed
        assert!(!result.contains("a=x-google-min-bitrate:"));
        assert!(!result.contains("a=x-google-start-bitrate:"));
        assert!(!result.contains("a=x-google-max-bitrate:"));

        // Values should be in the fmtp line
        assert!(result.contains("x-google-min-bitrate=1500"));
        assert!(result.contains("x-google-start-bitrate=1500"));
        assert!(result.contains("x-google-max-bitrate=3000"));
        // Original fmtp content preserved
        assert!(result.contains("level-asymmetry-allowed=1"));
    }

    #[test]
    fn fix_video_bitrate_hints_applies_defaults_without_standalone() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = fix_video_bitrate_hints(sdp);

        assert!(result.contains("x-google-min-bitrate=2000"));
        assert!(result.contains("x-google-start-bitrate=2000"));
        assert!(result.contains("x-google-max-bitrate=4000"));
    }

    #[test]
    fn fix_video_bitrate_hints_noop_if_already_in_fmtp() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;x-google-start-bitrate=2000;x-google-min-bitrate=2000;x-google-max-bitrate=4000\r\n";

        let result = fix_video_bitrate_hints(sdp);
        assert_eq!(result, sdp);
    }

    #[test]
    fn fix_video_bitrate_hints_removes_standalone_when_fmtp_has_hints() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=fmtp:96 level-asymmetry-allowed=1;x-google-start-bitrate=2000\r\n\
a=x-google-min-bitrate:1000\r\n";

        let result = fix_video_bitrate_hints(sdp);

        // Standalone line removed
        assert!(!result.contains("a=x-google-min-bitrate:"));
        // Existing fmtp hints preserved
        assert!(result.contains("x-google-start-bitrate=2000"));
    }

    #[test]
    fn fix_video_bitrate_hints_noop_without_video() {
        let sdp = "\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10\r\n";

        let result = fix_video_bitrate_hints(sdp);
        assert_eq!(result, sdp);
    }
}
