//! WHEP Player - serves web pages and static assets for playing WHEP streams.
//!
//! URL Structure:
//! - `/player/whep` - HTML page for playing a single WHEP stream
//! - `/player/whep-streams` - HTML page listing all active WHEP streams
//! - `/static/whep.css` - Shared CSS styles
//! - `/static/whep.js` - Shared JavaScript for WebRTC connections
//! - `/whep/{endpoint_id}` - Proxy to internal WHEP servers
//! - `/api/whep-streams` - JSON API listing all active WHEP endpoints

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use crate::assets::WhepAssets;
use crate::state::AppState;

// ============================================================================
// Player Pages (served from embedded HTML templates)
// ============================================================================

#[derive(Deserialize)]
pub struct WhepPlayerQuery {
    /// The WHEP endpoint URL to connect to (e.g., /whep/my-stream)
    endpoint: Option<String>,
}

/// Serve the WHEP player HTML page.
/// GET /player/whep?endpoint=/whep/my-stream
pub async fn whep_player(Query(params): Query<WhepPlayerQuery>) -> impl IntoResponse {
    let endpoint = params.endpoint.unwrap_or_default();

    match WhepAssets::get("player.html") {
        Some(content) => {
            // Convert to string and replace placeholder
            let html = String::from_utf8_lossy(&content.data);
            let html = html.replace("{{ENDPOINT}}", &endpoint);
            Html(html)
        }
        None => Html("<html><body>Player template not found</body></html>".to_string()),
    }
}

/// Serve the WHEP streams page HTML.
/// GET /player/whep-streams
pub async fn whep_streams_page() -> impl IntoResponse {
    match WhepAssets::get("streams.html") {
        Some(content) => {
            let html = String::from_utf8_lossy(&content.data);
            Html(html.to_string())
        }
        None => Html("<html><body>Streams template not found</body></html>".to_string()),
    }
}

// ============================================================================
// WHEP Proxy (endpoint_id-based routing via WhepRegistry)
// ============================================================================

/// Renumber any H.265 payload types that fall outside the dynamic RTP PT
/// range [96, 127] so that GStreamer's `rtph265pay` element accepts them.
///
/// Chrome offers H.265 on low PTs (49, 51 in current builds) which fall
/// outside `rtph265pay`'s src-pad template `payload=[96,127]`. webrtcsink's
/// discovery pipeline then fails to link `rtph265pay → payload-chain-output-caps`
/// (caps subset rejected on PT), `payloader-setup` never fires, the codec is
/// dropped from negotiation, and the m=video line ends up `a=inactive`.
///
/// Walk the SDP, build a map `old_pt -> new_pt` for H.265 payloads and their
/// RTX companions, and rewrite every reference (`m=video` PT list,
/// `a=rtpmap`, `a=fmtp` including `apt=` back-references, `a=rtcp-fb`).
fn renumber_low_h265_payload_types(sdp: &str, endpoint_id: &str) -> String {
    use std::collections::{HashMap, HashSet};

    // First pass: collect rtpmap entries and fmtp `apt=` mappings.
    let mut used_pts: HashSet<u8> = HashSet::new();
    let mut h265_pts: Vec<u8> = Vec::new(); // PTs whose rtpmap is H265
    let mut rtx_for: HashMap<u8, u8> = HashMap::new(); // rtx_pt -> source_pt
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some(sp) = rest.find(' ') {
                if let Ok(pt) = rest[..sp].parse::<u8>() {
                    used_pts.insert(pt);
                    let codec = rest[sp + 1..].split('/').next().unwrap_or("");
                    if codec.eq_ignore_ascii_case("H265") {
                        h265_pts.push(pt);
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            if let Some(sp) = rest.find(' ') {
                if let Ok(pt) = rest[..sp].parse::<u8>() {
                    if let Some(apt_idx) = rest[sp + 1..].find("apt=") {
                        let after = &rest[sp + 1 + apt_idx + "apt=".len()..];
                        let end = after
                            .find(|c: char| c == ';' || c.is_whitespace())
                            .unwrap_or(after.len());
                        if let Ok(source) = after[..end].parse::<u8>() {
                            rtx_for.insert(pt, source);
                        }
                    }
                }
            }
        }
    }

    // Identify the H.265 PTs that need renumbering (those below 96), plus
    // their RTX partners (which inherit the move).
    let mut to_renumber: Vec<u8> = Vec::new();
    for &pt in &h265_pts {
        if pt < 96 {
            to_renumber.push(pt);
        }
    }
    if to_renumber.is_empty() {
        return sdp.to_string();
    }
    let mut rtx_to_renumber: Vec<u8> = Vec::new();
    for (&rtx_pt, &source_pt) in &rtx_for {
        if to_renumber.contains(&source_pt) && rtx_pt < 96 {
            rtx_to_renumber.push(rtx_pt);
        }
    }

    // Allocate fresh PTs in [96, 127] avoiding all currently-used PTs.
    let mut pt_map: HashMap<u8, u8> = HashMap::new();
    let mut next_free = || -> Option<u8> {
        for candidate in 96..=127u8 {
            if !used_pts.contains(&candidate) {
                used_pts.insert(candidate);
                return Some(candidate);
            }
        }
        None
    };
    for &old in to_renumber.iter().chain(rtx_to_renumber.iter()) {
        if let Some(new_pt) = next_free() {
            pt_map.insert(old, new_pt);
        }
    }
    if pt_map.is_empty() {
        tracing::warn!(
            "WHEP offer for endpoint '{}': H.265 PTs {:?} need renumbering but no free slots in [96,127]",
            endpoint_id,
            to_renumber
        );
        return sdp.to_string();
    }

    // Second pass: rewrite every reference. Lines we touch:
    //   m=video <port> <proto> <pt1> <pt2> ...     → replace each pt
    //   a=rtpmap:<pt> ...                          → replace prefix pt
    //   a=fmtp:<pt> ... apt=<pt> ...               → replace prefix pt + apt= value
    //   a=rtcp-fb:<pt> ...                         → replace prefix pt
    let rewrite_pt = |s: &str| -> String {
        // Helper: parse leading u8 and remainder.
        if let Some(end) = s.find(|c: char| !c.is_ascii_digit()) {
            if let Ok(pt) = s[..end].parse::<u8>() {
                if let Some(&new_pt) = pt_map.get(&pt) {
                    return format!("{}{}", new_pt, &s[end..]);
                }
            }
        }
        s.to_string()
    };

    let mut out = String::with_capacity(sdp.len() + 32);
    for line in sdp.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("m=video ") {
            // Rewrite each PT token in the m= line PT list (skip first 2
            // tokens which are <port> and <proto>).
            let trailing = if rest.ends_with("\r\n") {
                "\r\n"
            } else if rest.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            let core = rest.trim_end_matches(['\r', '\n']);
            let parts: Vec<&str> = core.split(' ').collect();
            let mut rewritten: Vec<String> = Vec::with_capacity(parts.len());
            for (idx, p) in parts.iter().enumerate() {
                if idx < 2 {
                    rewritten.push((*p).to_string());
                    continue;
                }
                if let Ok(pt) = p.parse::<u8>() {
                    if let Some(&new_pt) = pt_map.get(&pt) {
                        rewritten.push(new_pt.to_string());
                        continue;
                    }
                }
                rewritten.push((*p).to_string());
            }
            out.push_str("m=video ");
            out.push_str(&rewritten.join(" "));
            out.push_str(trailing);
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            out.push_str("a=rtpmap:");
            out.push_str(&rewrite_pt(rest));
        } else if let Some(rest) = line.strip_prefix("a=rtcp-fb:") {
            out.push_str("a=rtcp-fb:");
            out.push_str(&rewrite_pt(rest));
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            // Rewrite prefix PT, and also any `apt=N` reference inside.
            let prefix_rewritten = rewrite_pt(rest);
            // Replace `apt=<n>` if <n> is in the map.
            let final_line = if let Some(idx) = prefix_rewritten.find("apt=") {
                let head = &prefix_rewritten[..idx + "apt=".len()];
                let tail = &prefix_rewritten[idx + "apt=".len()..];
                let end = tail
                    .find(|c: char| c == ';' || c.is_whitespace())
                    .unwrap_or(tail.len());
                let apt_str = &tail[..end];
                let apt_rewritten = if let Ok(apt) = apt_str.parse::<u8>() {
                    if let Some(&new_pt) = pt_map.get(&apt) {
                        new_pt.to_string()
                    } else {
                        apt_str.to_string()
                    }
                } else {
                    apt_str.to_string()
                };
                format!("{}{}{}", head, apt_rewritten, &tail[end..])
            } else {
                prefix_rewritten
            };
            out.push_str("a=fmtp:");
            out.push_str(&final_line);
        } else {
            out.push_str(line);
        }
    }

    let mut mapping: Vec<(u8, u8)> = pt_map.iter().map(|(a, b)| (*a, *b)).collect();
    mapping.sort();
    tracing::info!(
        "WHEP offer for endpoint '{}': renumbered low H.265 PTs (old->new): {:?}",
        endpoint_id,
        mapping
    );

    out
}

/// Remove `level-id=<n>;` (or `level-id=<n>` if trailing) from any H.265
/// `a=fmtp` line in an SDP. webrtcsink advertises level-id=180 (level 6.0)
/// regardless of the actual stream level; Chrome desktop's HW HEVC decoder
/// rejects level 6.0 because most platforms cap out at level 5.1 / 5.2. With
/// no level-id present, browsers default to a level compatible with the
/// actual encoded SPS.
fn strip_h265_fmtp_level_id(sdp: &str) -> String {
    // First pass: figure out which PTs are H.265 via rtpmap.
    let mut h265_pts: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some(sp) = rest.find(' ') {
                let pt = &rest[..sp];
                let codec = rest[sp + 1..].split('/').next().unwrap_or("");
                if codec.eq_ignore_ascii_case("H265") {
                    h265_pts.insert(pt.to_string());
                }
            }
        }
    }
    if h265_pts.is_empty() {
        return sdp.to_string();
    }

    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("a=fmtp:") {
            // Find the PT prefix of the fmtp line and check if it's H.265.
            if let Some(sp) = rest.find(' ') {
                let pt = &rest[..sp];
                if h265_pts.contains(pt) {
                    // Drop `level-id=<digits>` plus an optional trailing `;`.
                    let params = &rest[sp + 1..];
                    let cleaned = remove_fmtp_field(params, "level-id");
                    out.push_str("a=fmtp:");
                    out.push_str(pt);
                    out.push(' ');
                    out.push_str(&cleaned);
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    out
}

/// Remove a `key=value` (with optional trailing `;`) from an fmtp parameter
/// string. Preserves line endings.
fn remove_fmtp_field(params: &str, key: &str) -> String {
    let needle = format!("{}=", key);
    let Some(idx) = params.find(&needle) else {
        return params.to_string();
    };
    // Find end of value (next `;` or end of relevant portion).
    let after = &params[idx + needle.len()..];
    let end_rel = after.find(';').map(|i| i + 1).unwrap_or(after.len());
    let removed_total_len = needle.len() + end_rel;
    let mut out = String::with_capacity(params.len());
    out.push_str(&params[..idx]);
    out.push_str(&params[idx + removed_total_len..]);
    // Clean up double `;;` or leading `;` artifacts left after removal.
    out.replace(";;", ";").trim_start_matches(';').to_string()
}

/// Decide whether an H.264 profile-level-id (6 hex chars: profile_idc + iop +
/// level_idc) is one that gst-plugin-webrtc's `compat_profiles()` accepts.
/// That function panics outright on anything outside H264_PROFILES_COMPAT:
/// constrained-baseline, baseline, main, high, high-10, high-4:2:2,
/// high-4:4:4. So we have to filter at the SDP layer.
fn is_supported_h264_profile_level_id(value: &str) -> bool {
    if value.len() < 6 {
        return false;
    }
    let Ok(profile_idc) = u8::from_str_radix(&value[0..2], 16) else {
        return false;
    };
    let Ok(profile_iop) = u8::from_str_radix(&value[2..4], 16) else {
        return false;
    };
    // Bits in profile_iop:
    //   0x80 constraint_set0_flag
    //   0x40 constraint_set1_flag
    //   0x20 constraint_set2_flag
    //   0x10 constraint_set3_flag  -> "*-intra" variants (not supported)
    //   0x08 constraint_set4_flag  -> "progressive-high" / "constrained-high"
    //   0x04 constraint_set5_flag  -> "progressive-high" / "constrained-high"
    let bad_high_constraints = (profile_iop & 0x1C) != 0;
    match profile_idc {
        66 => true,                      // baseline / constrained-baseline
        77 => (profile_iop & 0x10) == 0, // main (not main-intra)
        88 => false,                     // extended — not in webrtcsink whitelist
        100 => !bad_high_constraints,    // high (block constrained/progressive/intra)
        110 => !bad_high_constraints,    // high-10
        122 => !bad_high_constraints,    // high-4:2:2
        244 => !bad_high_constraints,    // high-4:4:4
        _ => false,                      // scalable, multiview, stereo, etc.
    }
}

/// Strip `a=fmtp:N profile-level-id=...` lines whose profile webrtcsink would
/// panic on. Leaving the rtpmap intact lets webrtcsink still accept the
/// payload type — it just won't have a profile constraint, so it picks its
/// internal default (baseline) instead of crashing the whole process.
fn filter_unsupported_h264_profiles(sdp: &str, endpoint_id: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    let mut removed_pts: Vec<String> = Vec::new();
    let mut all_pts: Vec<(String, String)> = Vec::new();
    for line in sdp.split_inclusive('\n') {
        // Only inspect a=fmtp lines that look like H.264 profile-level-id
        if !line.starts_with("a=fmtp:") {
            out.push_str(line);
            continue;
        }
        let Some(rest) = line.strip_prefix("a=fmtp:") else {
            out.push_str(line);
            continue;
        };
        let Some(sp) = rest.find(' ') else {
            out.push_str(line);
            continue;
        };
        let (pt, params) = rest.split_at(sp);
        if let Some(idx) = params.find("profile-level-id=") {
            let after = &params[idx + "profile-level-id=".len()..];
            let end = after
                .find(|c: char| c == ';' || c.is_whitespace())
                .unwrap_or(after.len());
            let pli = &after[..end];
            all_pts.push((pt.to_string(), pli.to_string()));
            if !is_supported_h264_profile_level_id(pli) {
                removed_pts.push(format!("{}={}", pt, pli));
                continue; // drop this line
            }
        }
        out.push_str(line);
    }
    if !removed_pts.is_empty() {
        tracing::info!(
            "WHEP offer for endpoint '{}': stripped {} unsupported H264 fmtp(s) {:?}; all H264 PTs in offer: {:?}",
            endpoint_id,
            removed_pts.len(),
            removed_pts,
            all_pts
        );
    } else {
        tracing::debug!(
            "WHEP offer for endpoint '{}': all H264 profile-level-ids supported: {:?}",
            endpoint_id,
            all_pts
        );
    }

    // Diagnostic: list ALL video codecs the browser offered (from a=rtpmap
    // lines). Useful for verifying whether H265/HEVC, VP8, VP9, AV1 are
    // present in the offer — separate from H264-specific filtering above.
    {
        let codecs: Vec<String> = sdp
            .lines()
            .filter_map(|l| {
                l.strip_prefix("a=rtpmap:").and_then(|rest| {
                    rest.find(' ').map(|sp| {
                        let (pt, codec) = rest.split_at(sp);
                        let codec = codec.trim();
                        format!("{}={}", pt, codec)
                    })
                })
            })
            .collect();
        tracing::debug!(
            "WHEP offer for endpoint '{}': all rtpmap codecs: {:?}",
            endpoint_id,
            codecs
        );
    }

    out
}

/// Proxy POST requests to /whep/{endpoint_id}
/// Looks up the internal port from WhepRegistry and forwards to localhost:{port}/whep/endpoint
#[utoipa::path(
    post,
    path = "/whep/{endpoint_id}",
    tag = "whep",
    params(
        ("endpoint_id" = String, Path, description = "WHEP endpoint identifier")
    ),
    responses(
        (status = 201, description = "WHEP session created, SDP answer returned", content_type = "application/sdp"),
        (status = 404, description = "WHEP endpoint not found"),
        (status = 502, description = "Proxy error forwarding to internal WHEP server")
    )
)]
pub async fn whep_endpoint_proxy(
    State(state): State<AppState>,
    Path(endpoint_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Log the raw SDP offer the browser sent, then the SDP after our
    // profile-level-id filter, and finally the SDP answer from webrtcsink.
    // Lets us trace exactly what each side advertises.
    tracing::debug!(
        "WHEP SDP OFFER (raw) for '{}' [{} bytes]:\n{}",
        endpoint_id,
        body.len(),
        body
    );

    // Renumber low H.265 PTs (Chrome offers H265 on PT < 96, which
    // rtph265pay's src-pad template rejects → discovery link fails → codec
    // dropped → m=video goes inactive). Move them into [96, 127].
    let body = renumber_low_h265_payload_types(&body, &endpoint_id);

    // Filter out H264 fmtp lines with profile-level-ids that webrtcsink's
    // compat_profiles() panics on (constrained-high, progressive-high,
    // extended, scalable-*, etc — anything outside H264_PROFILES_COMPAT in
    // gst-plugin-webrtc/src/utils.rs). Without this, mobile browsers (notably
    // Safari/iOS) that advertise profile-level-id=640c1f abort the whole
    // strom process.
    let body = filter_unsupported_h264_profiles(&body, &endpoint_id);

    tracing::debug!(
        "WHEP SDP OFFER (after strip) for '{}' [{} bytes]:\n{}",
        endpoint_id,
        body.len(),
        body
    );

    // Look up internal port from registry
    let port = match state.whep_registry().get_port(&endpoint_id).await {
        Some(p) => p,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!(
                    "WHEP endpoint '{}' not found",
                    endpoint_id
                )))
                .unwrap();
        }
    };

    let target_url = format!("http://127.0.0.1:{}/whep/endpoint", port);
    let client = reqwest::Client::new();

    let mut request = client.post(&target_url);

    // Forward content-type header
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        if let Ok(ct) = content_type.to_str() {
            request = request.header(header::CONTENT_TYPE, ct);
        }
    }

    request = request.body(body);

    match request.send().await {
        Ok(response) => {
            let status = response.status();

            // Get Location header for resource URL and rewrite it
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            // Collect all Link headers (WHEP spec: ICE servers sent via Link headers)
            let link_headers: Vec<String> = response
                .headers()
                .get_all(header::LINK)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .collect();

            let body_bytes = response.bytes().await.unwrap_or_default();

            // DIAGNOSTIC: log the SDP answer webrtcsink returned (before our
            // post-processing).
            tracing::debug!(
                "WHEP SDP ANSWER (raw) for '{}' status={} [{} bytes]:\n{}",
                endpoint_id,
                status.as_u16(),
                body_bytes.len(),
                String::from_utf8_lossy(&body_bytes)
            );

            // Post-process the SDP answer: strip H.265 `level-id` from any
            // `a=fmtp` line. webrtcsink hardcodes level-id=180 (level 6.0)
            // which exceeds what most browser HW HEVC decoders support
            // (typically up to 5.1). With no level-id, browsers default to
            // a sane level that matches the actual stream the encoder emits.
            let body_bytes = if body_bytes.windows(5).any(|w| w == b"H265/") {
                let answer = String::from_utf8_lossy(&body_bytes).into_owned();
                let rewritten = strip_h265_fmtp_level_id(&answer);
                if rewritten != answer {
                    tracing::debug!(
                        "WHEP SDP ANSWER (final) for '{}' [{} bytes]:\n{}",
                        endpoint_id,
                        rewritten.len(),
                        rewritten
                    );
                }
                rewritten.into_bytes().into()
            } else {
                body_bytes
            };

            let mut builder = Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK))
                .header(header::CONTENT_TYPE, "application/sdp")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    "POST, DELETE, OPTIONS",
                )
                .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
                .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "Location, Link");

            if let Some(loc) = location {
                // Rewrite location from /whep/resource/{id} to /whep/{endpoint_id}/resource/{id}
                let proxy_location = if loc.starts_with("/whep/resource/") {
                    let resource_id = loc.trim_start_matches("/whep/resource/");
                    format!("/whep/{}/resource/{}", endpoint_id, resource_id)
                } else {
                    format!("/whep/{}{}", endpoint_id, loc)
                };
                builder = builder.header(header::LOCATION, proxy_location);
            }

            // Relay all Link headers (for ICE server configuration per WHEP spec)
            for link in link_headers {
                builder = builder.header(header::LINK, link);
            }

            builder.body(Body::from(body_bytes)).unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Proxy error: {}", e)))
            .unwrap(),
    }
}

/// Proxy DELETE requests to /whep/{endpoint_id}/resource/{resource_id}
#[utoipa::path(
    delete,
    path = "/whep/{endpoint_id}/resource/{resource_id}",
    tag = "whep",
    params(
        ("endpoint_id" = String, Path, description = "WHEP endpoint identifier"),
        ("resource_id" = String, Path, description = "WHEP resource/session identifier")
    ),
    responses(
        (status = 200, description = "WHEP session deleted"),
        (status = 404, description = "WHEP endpoint not found"),
        (status = 502, description = "Proxy error forwarding to internal WHEP server")
    )
)]
pub async fn whep_resource_proxy_delete(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
) -> Response {
    let port = match state.whep_registry().get_port(&endpoint_id).await {
        Some(p) => p,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!(
                    "WHEP endpoint '{}' not found",
                    endpoint_id
                )))
                .unwrap();
        }
    };

    let target_url = format!("http://127.0.0.1:{}/whep/resource/{}", port, resource_id);
    let client = reqwest::Client::new();

    match client.delete(&target_url).send().await {
        Ok(response) => {
            let status = response.status();
            Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK))
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Proxy error: {}", e)))
            .unwrap(),
    }
}

/// Handle OPTIONS preflight for /whep/{endpoint_id}
#[utoipa::path(
    options,
    path = "/whep/{endpoint_id}",
    tag = "whep",
    params(
        ("endpoint_id" = String, Path, description = "WHEP endpoint identifier")
    ),
    responses(
        (status = 204, description = "CORS preflight response")
    )
)]
pub async fn whep_endpoint_proxy_options() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .unwrap()
}

/// Proxy PATCH requests to /whep/{endpoint_id}/resource/{resource_id} for ICE candidates
#[utoipa::path(
    patch,
    path = "/whep/{endpoint_id}/resource/{resource_id}",
    tag = "whep",
    params(
        ("endpoint_id" = String, Path, description = "WHEP endpoint identifier"),
        ("resource_id" = String, Path, description = "WHEP resource/session identifier")
    ),
    responses(
        (status = 204, description = "ICE candidates accepted"),
        (status = 404, description = "WHEP endpoint not found"),
        (status = 502, description = "Proxy error forwarding to internal WHEP server")
    )
)]
pub async fn whep_resource_proxy_patch(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let port = match state.whep_registry().get_port(&endpoint_id).await {
        Some(p) => p,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!(
                    "WHEP endpoint '{}' not found",
                    endpoint_id
                )))
                .unwrap();
        }
    };

    let target_url = format!("http://127.0.0.1:{}/whep/resource/{}", port, resource_id);
    let client = reqwest::Client::new();

    let mut request = client.patch(&target_url);

    // Forward content-type header (typically application/trickle-ice-sdpfrag)
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        if let Ok(ct) = content_type.to_str() {
            request = request.header(header::CONTENT_TYPE, ct);
        }
    }

    request = request.body(body);

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::NO_CONTENT))
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Proxy error: {}", e)))
            .unwrap(),
    }
}

/// Handle OPTIONS preflight for /whep/{endpoint_id}/resource/{resource_id}
#[utoipa::path(
    options,
    path = "/whep/{endpoint_id}/resource/{resource_id}",
    tag = "whep",
    params(
        ("endpoint_id" = String, Path, description = "WHEP endpoint identifier"),
        ("resource_id" = String, Path, description = "WHEP resource/session identifier")
    ),
    responses(
        (status = 204, description = "CORS preflight response")
    )
)]
pub async fn whep_resource_proxy_options() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "PATCH, DELETE, OPTIONS",
        )
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .unwrap()
}

// ============================================================================
// WHEP Streams API (JSON)
// ============================================================================

pub use strom_types::whep::{IceServer, IceServersResponse, WhepStreamInfo, WhepStreamsResponse};

/// GET /api/whep-streams - List all active WHEP streams (JSON API).
#[utoipa::path(
    get,
    path = "/api/whep-streams",
    tag = "whep",
    responses(
        (status = 200, description = "List of active WHEP streams", body = WhepStreamsResponse)
    )
)]
pub async fn list_whep_streams(State(state): State<AppState>) -> axum::Json<WhepStreamsResponse> {
    let endpoints = state.whep_registry().list_all().await;

    let streams = endpoints
        .into_iter()
        .map(|(endpoint_id, entry)| WhepStreamInfo {
            endpoint_id,
            num_audio_tracks: entry.num_audio_tracks,
            num_video_tracks: entry.num_video_tracks,
        })
        .collect();

    axum::Json(WhepStreamsResponse { streams })
}

// ============================================================================
// ICE Servers API
// ============================================================================

/// Parse an ICE server URL into the browser-compatible format.
/// TURN URLs with embedded credentials (turn:user:pass@host:port) are parsed
/// into separate urls, username, and credential fields.
///
/// Handles both standard URI format (turn:user:pass@host) and
/// GStreamer format (turn://user:pass@host).
fn parse_ice_server(url: &str) -> IceServer {
    // Check if it's a TURN URL with credentials
    if url.starts_with("turn:") || url.starts_with("turns:") {
        // Determine scheme and strip optional // after scheme
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("turns://") {
            ("turns:", rest)
        } else if let Some(rest) = url.strip_prefix("turn://") {
            ("turn:", rest)
        } else if let Some(rest) = url.strip_prefix("turns:") {
            ("turns:", rest)
        } else if let Some(rest) = url.strip_prefix("turn:") {
            ("turn:", rest)
        } else {
            // Shouldn't happen given the outer if, but be safe
            return IceServer {
                urls: url.to_string(),
                username: None,
                credential: None,
            };
        };

        if let Some(at_pos) = rest.rfind('@') {
            // Has credentials: user:pass@host:port
            let credentials = &rest[..at_pos];
            let host_port = &rest[at_pos + 1..];

            // Split credentials on first ':' (username:password)
            if let Some(colon_pos) = credentials.find(':') {
                let username = &credentials[..colon_pos];
                let password = &credentials[colon_pos + 1..];

                return IceServer {
                    urls: format!("{}{}", scheme, host_port),
                    username: Some(username.to_string()),
                    credential: Some(password.to_string()),
                };
            }
        }
    }

    // STUN server or TURN without embedded credentials
    // Normalize stun:// to stun: for browser compatibility
    let normalized_url = if let Some(rest) = url.strip_prefix("stun://") {
        format!("stun:{}", rest)
    } else if let Some(rest) = url.strip_prefix("turn://") {
        if !url.contains('@') {
            format!("turn:{}", rest)
        } else {
            url.to_string()
        }
    } else if let Some(rest) = url.strip_prefix("turns://") {
        if !url.contains('@') {
            format!("turns:{}", rest)
        } else {
            url.to_string()
        }
    } else {
        url.to_string()
    };

    IceServer {
        urls: normalized_url,
        username: None,
        credential: None,
    }
}

/// GET /api/ice-servers - Get configured ICE servers for WebRTC connections.
#[utoipa::path(
    get,
    path = "/api/ice-servers",
    tag = "whep",
    responses(
        (status = 200, description = "List of configured ICE servers", body = IceServersResponse)
    )
)]
pub async fn get_ice_servers(State(state): State<AppState>) -> axum::Json<IceServersResponse> {
    let ice_servers = state
        .ice_servers()
        .iter()
        .map(|url| parse_ice_server(url))
        .collect();

    axum::Json(IceServersResponse {
        ice_servers,
        ice_transport_policy: state.ice_transport_policy().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stun_server() {
        let server = parse_ice_server("stun:stun.l.google.com:19302");
        assert_eq!(server.urls, "stun:stun.l.google.com:19302");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }

    #[test]
    fn test_parse_turn_server_with_credentials() {
        let server = parse_ice_server("turn:myuser:mypassword@turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert_eq!(server.username, Some("myuser".to_string()));
        assert_eq!(server.credential, Some("mypassword".to_string()));
    }

    #[test]
    fn test_parse_turns_server_with_credentials() {
        let server = parse_ice_server("turns:user:pass@turn.example.com:5349");
        assert_eq!(server.urls, "turns:turn.example.com:5349");
        assert_eq!(server.username, Some("user".to_string()));
        assert_eq!(server.credential, Some("pass".to_string()));
    }

    #[test]
    fn test_parse_turn_server_without_credentials() {
        let server = parse_ice_server("turn:turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }

    // Tests for GStreamer-style URLs with ://

    #[test]
    fn test_parse_stun_server_with_slashes() {
        let server = parse_ice_server("stun://stun.l.google.com:19302");
        assert_eq!(server.urls, "stun:stun.l.google.com:19302");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }

    #[test]
    fn test_parse_turn_server_with_slashes_and_credentials() {
        let server = parse_ice_server("turn://myuser:mypassword@turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert_eq!(server.username, Some("myuser".to_string()));
        assert_eq!(server.credential, Some("mypassword".to_string()));
    }

    #[test]
    fn test_parse_turns_server_with_slashes_and_credentials() {
        let server = parse_ice_server("turns://user:pass@turn.example.com:5349");
        assert_eq!(server.urls, "turns:turn.example.com:5349");
        assert_eq!(server.username, Some("user".to_string()));
        assert_eq!(server.credential, Some("pass".to_string()));
    }

    #[test]
    fn test_parse_turn_server_with_slashes_without_credentials() {
        let server = parse_ice_server("turn://turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }
}
