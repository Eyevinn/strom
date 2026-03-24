//! Flow API handlers.

use crate::json_rejection::{JsonBody, ValidatedJson};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Local;
use serde::Deserialize;
use std::process::{Command, Stdio};
use strom_types::{
    api::{
        AnimateInputRequest, AvailableOutput, AvailableSourcesResponse, CreateFlowRequest,
        DynamicPadsResponse, ElementPropertiesResponse, ErrorResponse, FlowDebugInfo,
        FlowListResponse, FlowResponse, FlowStatsResponse, LatencyResponse, PadPropertiesResponse,
        SourceFlowInfo, TransitionResponse, TriggerTransitionRequest, UpdateFlowPropertiesRequest,
        UpdatePadPropertyRequest, UpdatePropertyRequest, WebRtcStatsResponse,
    },
    Flow, FlowId,
};
use tracing::{debug, error, info, trace, warn};

use crate::layout;
use crate::state::AppState;

/// Check if a pad reference is valid (exists on an element or block).
///
/// For elements, we just check if the element exists.
/// For blocks with computed pads, we strictly validate against the valid_block_pads set.
/// For blocks without computed pads, we trust the static pad definition and just check block existence.
fn is_pad_valid(
    pad_ref: &str,
    valid_block_pads: &std::collections::HashSet<String>,
    element_ids: &std::collections::HashSet<String>,
    block_ids: &std::collections::HashSet<String>,
    blocks_with_computed_pads: &std::collections::HashSet<String>,
) -> bool {
    // Parse the pad reference (format: "element_id:pad_name" or "block_id:pad_name")
    let parts: Vec<&str> = pad_ref.split(':').collect();
    if parts.len() < 2 {
        return false;
    }

    let node_id = parts[0];

    // Check if it's an element by looking it up in element_ids
    // (Don't rely on ID prefix - gst-launch imports use element_type as ID prefix like "videotestsrc_0")
    if element_ids.contains(node_id) {
        // For elements, we just check if the element exists
        // The actual pad validation happens at pipeline build time
        return true;
    }

    // Check if it's a block by looking it up in block_ids
    // (Don't rely on ID prefix - could change in the future)
    if block_ids.contains(node_id) {
        // Only strictly validate blocks that have computed pads
        if blocks_with_computed_pads.contains(node_id) {
            // This block has dynamic pads - validate against computed external pads
            return valid_block_pads.contains(pad_ref);
        }
        // For blocks without computed pads, assume valid (uses static pad definition from block definition)
        // The actual pad existence will be validated at pipeline build time
        return true;
    }

    // Unknown node type
    false
}

/// List all flows.
#[utoipa::path(
    get,
    path = "/api/flows",
    tag = "flows",
    responses(
        (status = 200, description = "List all flows", body = FlowListResponse)
    )
)]
pub async fn list_flows(State(state): State<AppState>) -> Json<FlowListResponse> {
    let flows = state.get_flows().await;
    Json(FlowListResponse { flows })
}

/// Get available source flows for subscription.
///
/// Returns all flows that have InterOutput blocks, along with information
/// about whether each output is currently active (flow is running).
/// This scans all flow definitions, not just running flows.
#[utoipa::path(
    get,
    path = "/api/sources",
    tag = "flows",
    responses(
        (status = 200, description = "List of available source flows", body = AvailableSourcesResponse)
    )
)]
pub async fn get_available_sources(
    State(state): State<AppState>,
) -> Json<AvailableSourcesResponse> {
    use strom_types::element::MediaType;
    use strom_types::PropertyValue;

    // Get all active channels from registry to check which are running
    let active_channels = state.channels().list_all().await;
    let active_channel_names: std::collections::HashSet<_> = active_channels
        .iter()
        .map(|ch| ch.channel_name.clone())
        .collect();

    // Scan all flows for InterOutput blocks
    let flows = state.get_flows().await;
    let mut sources: Vec<SourceFlowInfo> = Vec::new();

    for flow in flows {
        let mut outputs: Vec<AvailableOutput> = Vec::new();

        for block in &flow.blocks {
            if block.block_definition_id == "builtin.inter_output" {
                // Generate the channel name (same logic as InterOutputBuilder)
                let channel_name = format!("strom_{}_{}", flow.id, block.id);

                // Get description from block properties
                let description = block.properties.get("description").and_then(|v| match v {
                    PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                });

                // Check if this channel is active (flow is running)
                let is_active = active_channel_names.contains(&channel_name);

                outputs.push(AvailableOutput {
                    name: block.id.clone(),
                    channel_name,
                    flow_name: flow.name.clone(),
                    description,
                    media_type: MediaType::Generic, // rsinter is format-agnostic
                    is_active,
                });
            }
        }

        if !outputs.is_empty() {
            sources.push(SourceFlowInfo {
                flow_id: flow.id,
                flow_name: flow.name.clone(),
                outputs,
            });
        }
    }

    info!(
        "Returning {} source flows with {} total outputs",
        sources.len(),
        sources.iter().map(|s| s.outputs.len()).sum::<usize>()
    );
    Json(AvailableSourcesResponse { sources })
}

/// Get a specific flow by ID.
#[utoipa::path(
    get,
    path = "/api/flows/{id}",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "Flow found", body = FlowResponse),
        (status = 404, description = "Flow not found", body = ErrorResponse)
    )
)]
pub async fn get_flow(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<FlowResponse>, (StatusCode, Json<ErrorResponse>)> {
    match state.get_flow(&id).await {
        Some(flow) => Ok(Json(FlowResponse { flow })),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Flow not found")),
        )),
    }
}

/// Create a new flow.
#[utoipa::path(
    post,
    path = "/api/flows",
    tag = "flows",
    request_body = CreateFlowRequest,
    responses(
        (status = 201, description = "Flow created", body = FlowResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn create_flow(
    State(state): State<AppState>,
    ValidatedJson(req): ValidatedJson<CreateFlowRequest>,
) -> Result<(StatusCode, Json<FlowResponse>), (StatusCode, Json<ErrorResponse>)> {
    info!("Received create flow request: name='{}'", req.name);

    let mut flow = Flow::new(req.name);

    // Set description if provided
    if let Some(description) = req.description {
        flow.properties.description = Some(description);
    }

    // Set creation timestamp
    let now = Local::now().to_rfc3339();
    flow.properties.created_at = Some(now.clone());
    flow.properties.last_modified = Some(now);

    info!("Creating flow: {} ({})", flow.name, flow.id);

    if let Err(e) = state.upsert_flow(flow.clone()).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::with_details(
                "Failed to save flow",
                e.to_string(),
            )),
        ));
    }

    Ok((StatusCode::CREATED, Json(FlowResponse { flow })))
}

/// Update an existing flow.
#[utoipa::path(
    post,
    path = "/api/flows/{id}",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    request_body = Flow,
    responses(
        (status = 200, description = "Flow updated", body = FlowResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 404, description = "Flow not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn update_flow(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
    JsonBody(mut flow): JsonBody<Flow>,
) -> Result<Json<FlowResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Ensure the ID in the path matches the flow
    if id != flow.id {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("Flow ID mismatch")),
        ));
    }

    // Get old flow to compare for live updates
    let old_flow = state.get_flow(&id).await.ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Flow not found")),
    ))?;

    info!("Updating flow: {} ({})", flow.name, flow.id);

    // Compute external pads for all block instances based on their properties
    for block in &mut flow.blocks {
        if let Some(builder) = crate::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }

    // Remove links that reference pads that no longer exist on blocks
    // This can happen when block properties change (e.g., reducing num_audio_tracks)
    // We need to collect pad info before calling retain to avoid borrow checker issues
    let mut valid_block_pads = std::collections::HashSet::new();
    let mut blocks_with_computed_pads = std::collections::HashSet::new();

    for block in &flow.blocks {
        if let Some(ref external_pads) = block.computed_external_pads {
            blocks_with_computed_pads.insert(block.id.clone());
            for input in &external_pads.inputs {
                valid_block_pads.insert(format!("{}:{}", block.id, input.name));
            }
            for output in &external_pads.outputs {
                valid_block_pads.insert(format!("{}:{}", block.id, output.name));
            }
        }
    }

    let element_ids: std::collections::HashSet<String> =
        flow.elements.iter().map(|e| e.id.clone()).collect();
    let block_ids: std::collections::HashSet<String> =
        flow.blocks.iter().map(|b| b.id.clone()).collect();

    let initial_link_count = flow.links.len();
    flow.links.retain(|link| {
        let from_valid = is_pad_valid(
            &link.from,
            &valid_block_pads,
            &element_ids,
            &block_ids,
            &blocks_with_computed_pads,
        );
        let to_valid = is_pad_valid(
            &link.to,
            &valid_block_pads,
            &element_ids,
            &block_ids,
            &blocks_with_computed_pads,
        );

        if !from_valid || !to_valid {
            info!(
                "Removing invalid link: {} -> {} (pad no longer exists)",
                link.from, link.to
            );
            false
        } else {
            true
        }
    });

    if flow.links.len() < initial_link_count {
        info!(
            "Removed {} invalid link(s) from flow '{}'",
            initial_link_count - flow.links.len(),
            flow.name
        );
    }

    // Apply auto-layout if needed
    if layout::needs_auto_layout(&flow) {
        info!(
            "Flow '{}' needs auto-layout (elements stacked or missing positions)",
            flow.name
        );
        layout::apply_auto_layout(&mut flow);
    }

    // Update last_modified timestamp (preserve created_at from old flow)
    flow.properties.last_modified = Some(Local::now().to_rfc3339());
    if flow.properties.created_at.is_none() {
        flow.properties.created_at = old_flow.properties.created_at.clone();
    }

    // Check if the flow is currently running
    let is_running = old_flow.running;

    if let Err(e) = state.upsert_flow(flow.clone()).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::with_details(
                "Failed to save flow",
                e.to_string(),
            )),
        ));
    }

    // If the flow is running, apply pad property changes live
    if is_running {
        for element in &flow.elements {
            // Find the corresponding old element
            if let Some(_old_element) = old_flow.elements.iter().find(|e| e.id == element.id) {
                // Always apply pad properties if they exist (we can't easily compare HashMaps)
                if !element.pad_properties.is_empty() {
                    info!(
                        "Pad properties changed for element {} in running flow",
                        element.id
                    );

                    // Apply all pad properties for this element
                    for (pad_name, properties) in &element.pad_properties {
                        for (prop_name, prop_value) in properties {
                            info!(
                                "Applying live update: {}:{}:{} = {:?}",
                                element.id, pad_name, prop_name, prop_value
                            );

                            // Try to update the pad property - ignore errors since some properties
                            // may not be live-updatable
                            if let Err(e) = state
                                .update_pad_property(
                                    &id,
                                    &element.id,
                                    pad_name,
                                    prop_name,
                                    prop_value.clone(),
                                )
                                .await
                            {
                                // Log but don't fail - property might not be mutable in current state
                                info!(
                                    "Could not live-update pad property {}:{}:{}: {}",
                                    element.id, pad_name, prop_name, e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(Json(FlowResponse { flow }))
}

/// Update an existing flow (PUT alias).
///
/// This is an alias for the POST update endpoint, provided for RESTful API conventions.
#[utoipa::path(
    put,
    path = "/api/flows/{id}",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    request_body = Flow,
    responses(
        (status = 200, description = "Flow updated", body = FlowResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 404, description = "Flow not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn update_flow_put(
    state: State<AppState>,
    id: Path<FlowId>,
    flow: JsonBody<Flow>,
) -> Result<Json<FlowResponse>, (StatusCode, Json<ErrorResponse>)> {
    update_flow(state, id, flow).await
}

/// Delete a flow.
#[utoipa::path(
    delete,
    path = "/api/flows/{id}",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 204, description = "Flow deleted"),
        (status = 404, description = "Flow not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn delete_flow(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state.delete_flow(&id).await {
        Ok(true) => {
            info!("Deleted flow: {}", id);
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Flow not found")),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::with_details(
                "Failed to delete flow",
                e.to_string(),
            )),
        )),
    }
}

/// Start a flow (pipeline).
#[utoipa::path(
    post,
    path = "/api/flows/{id}/start",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "Flow started", body = FlowResponse),
        (status = 404, description = "Flow not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn start_flow(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<FlowResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Start the pipeline
    if let Err(e) = state.start_flow(&id).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::with_details(
                "Failed to start flow",
                e.to_string(),
            )),
        ));
    }

    // Return updated flow with state
    match state.get_flow(&id).await {
        Some(flow) => Ok(Json(FlowResponse { flow })),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Flow not found")),
        )),
    }
}

/// Stop a flow (pipeline).
#[utoipa::path(
    post,
    path = "/api/flows/{id}/stop",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "Flow stopped", body = FlowResponse),
        (status = 404, description = "Flow not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn stop_flow(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<FlowResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Stop the pipeline
    if let Err(e) = state.stop_flow(&id).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::with_details(
                "Failed to stop flow",
                e.to_string(),
            )),
        ));
    }

    // Return updated flow with state
    match state.get_flow(&id).await {
        Some(flow) => Ok(Json(FlowResponse { flow })),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Flow not found")),
        )),
    }
}

/// Run the graphviz `dot` command with the given arguments, feeding `dot_content` via stdin.
///
/// Graphviz often produces valid SVG output even when it reports layout warnings
/// (e.g. "triangulation failed", "lost edge") on stderr. These warnings mean some
/// edges couldn't be routed but the rest of the graph is fine. We accept the output
/// as long as it looks like valid SVG, regardless of exit code or stderr content.
fn run_dot(dot_content: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    use std::io::Write;

    let mut child = Command::new("dot")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "Failed to execute 'dot': {}. Ensure Graphviz is installed.",
                e
            )
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(dot_content.as_bytes())
            .map_err(|e| format!("Failed to write DOT content to stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for 'dot' command: {}", e))?;

    // Accept output if it looks like SVG, even if graphviz reported warnings.
    // Complex GStreamer pipelines trigger graphviz layout warnings ("triangulation
    // failed", "lost edge") but still produce usable SVG with some edges missing.
    let stdout = &output.stdout;
    let looks_like_svg =
        stdout.len() > 50 && (stdout.starts_with(b"<?xml") || stdout.starts_with(b"<svg"));

    if looks_like_svg {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                "dot produced SVG despite warnings (exit code {:?}): {}",
                output.status.code(),
                stderr.chars().take(200).collect::<String>()
            );
        }
        return Ok(output.stdout);
    }

    // No usable SVG output — report the error
    let stderr = String::from_utf8_lossy(&output.stderr);
    error!("dot command failed with no SVG output: {}", stderr);
    Err(stderr.to_string())
}

/// Generate a debug DOT/SVG graph for a flow's pipeline.
///
/// This endpoint generates a GraphViz DOT graph of the GStreamer pipeline
/// and converts it to SVG format. The SVG is returned directly and can be
/// viewed in a browser.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/debug-graph",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "SVG debug graph of the pipeline", content_type = "image/svg+xml"),
        (status = 404, description = "Flow not found or not running", body = ErrorResponse),
        (status = 500, description = "Failed to generate graph (Graphviz not installed)", body = ErrorResponse)
    )
)]
pub async fn debug_graph(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    info!("Generating debug graph for flow: {}", id);

    // Generate DOT graph from the pipeline
    let dot_content = state.generate_debug_graph(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Flow not found or not running. Start the flow first.",
            )),
        )
    })?;

    // Convert DOT to SVG using the 'dot' command via stdin
    // (avoids temp file permission issues on Windows corporate machines)
    //
    // Try default splines first (curved, best looking). If graphviz fails
    // (e.g. "triangulation failed" with very complex graphs), retry with
    // polyline splines which bypass the triangulation algorithm entirely.
    let svg_output = run_dot(&dot_content, &["-Tsvg"])
        .or_else(|_| {
            warn!("dot failed with default splines, retrying with polyline splines");
            run_dot(&dot_content, &["-Tsvg", "-Gsplines=polyline"])
        })
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::with_details("SVG conversion failed", e)),
            )
        })?;

    info!("Successfully generated SVG debug graph for flow: {}", id);

    // Return SVG as response
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/svg+xml")],
        svg_output,
    )
        .into_response())
}

/// Get runtime dynamic pads for a flow.
///
/// Returns information about dynamic pads (like decodebin outputs) that were
/// created at runtime and auto-linked to tees. These pads can be connected
/// to other elements in the UI.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/dynamic-pads",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "Dynamic pads information", body = DynamicPadsResponse),
        (status = 404, description = "Flow not found or not running", body = ErrorResponse)
    )
)]
pub async fn get_dynamic_pads(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<DynamicPadsResponse>, (StatusCode, Json<ErrorResponse>)> {
    trace!("Getting dynamic pads for flow: {}", id);

    let pads = state.get_dynamic_pads(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Flow not found or not running. Start the flow first.",
            )),
        )
    })?;

    Ok(Json(DynamicPadsResponse { pads }))
}

/// Generate SDP for a specific block in a flow.
///
/// Returns the SDP (Session Description Protocol) data for AES67 output blocks.
/// This SDP can be used by receivers to connect to the audio stream.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/sdp",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID")
    ),
    responses(
        (status = 200, description = "SDP generated successfully", content_type = "application/sdp"),
        (status = 404, description = "Flow or block not found", body = ErrorResponse),
        (status = 400, description = "Block type does not support SDP", body = ErrorResponse)
    )
)]
pub async fn get_block_sdp(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    info!("Generating SDP for block {} in flow {}", block_id, flow_id);

    // Get the flow
    let flow = state.get_flow(&flow_id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Flow not found")),
        )
    })?;

    // Find the block instance
    let block = flow
        .blocks
        .iter()
        .find(|b| b.id == block_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("Block not found in flow")),
            )
        })?;

    // Check if this is an AES67 output block
    if block.block_definition_id != "builtin.aes67_output" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "SDP generation is only supported for AES67 output blocks",
            )),
        ));
    }

    // Get PTP clock identity from flow properties if available
    let ptp_clock_identity = flow
        .properties
        .ptp_info
        .as_ref()
        .and_then(|info| info.grandmaster_clock_id.as_ref())
        .map(|id| crate::blocks::sdp::convert_clock_id_to_sdp_format(id));

    // Get the multicast destination address for routing lookup
    let multicast_host = block
        .properties
        .get("host")
        .and_then(|v| {
            if let strom_types::PropertyValue::String(s) = v {
                Some(s.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "239.69.1.1".to_string());

    // Determine origin IP:
    // 1. If interface is explicitly set, use that interface's IP
    // 2. Otherwise, ask the kernel which source IP it would use for the multicast address
    let origin_ip = block
        .properties
        .get("interface")
        .and_then(|v| {
            if let strom_types::PropertyValue::String(s) = v {
                if !s.is_empty() {
                    crate::network::get_interface_ipv4(s).map(|ip| ip.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .or_else(|| {
            crate::network::get_source_ipv4_for_destination(&multicast_host)
                .map(|ip| ip.to_string())
        })
        .or_else(|| crate::network::get_default_ipv4().map(|ip| ip.to_string()));

    // Check if RAVENNA extensions are enabled for this block
    let ravenna_extensions = block
        .properties
        .get("ravenna_extensions")
        .map(|v| matches!(v, strom_types::PropertyValue::Bool(true)))
        .unwrap_or(false);

    // Get session name: use custom if set, otherwise fall back to flow name
    let session_name = block
        .properties
        .get("session_name")
        .and_then(|v| match v {
            strom_types::PropertyValue::String(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| flow.name.clone());
    let session_name = crate::blocks::sdp::sanitize_session_name(&session_name);

    // Generate SDP (using default sample rate and channels since we can't query caps here)
    // Pass flow properties for correct clock signaling (RFC 7273)
    let sdp = crate::blocks::sdp::generate_aes67_output_sdp(
        block,
        &session_name,
        None,
        None,
        Some(&flow.properties),
        ptp_clock_identity.as_deref(),
        origin_ip.as_deref(),
        ravenna_extensions,
    );

    info!("Successfully generated SDP for block {}", block_id);

    // Return SDP as plain text response
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/sdp")],
        sdp,
    )
        .into_response())
}

/// Get current property values from a running element.
///
/// Returns all readable properties and their current values from an element
/// in a running pipeline. The pipeline must be started for this endpoint to work.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/elements/{element_id}/properties",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("element_id" = String, Path, description = "Element instance ID")
    ),
    responses(
        (status = 200, description = "Properties retrieved successfully", body = ElementPropertiesResponse),
        (status = 404, description = "Flow not running or element not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_element_properties(
    State(state): State<AppState>,
    Path((flow_id, element_id)): Path<(FlowId, String)>,
) -> Result<Json<ElementPropertiesResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!(
        "Getting properties for element {} in flow {}",
        element_id, flow_id
    );

    let properties = state
        .get_element_properties(&flow_id, &element_id)
        .await
        .map_err(|e| {
            error!("Failed to get element properties: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to get element properties",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(ElementPropertiesResponse {
        element_id,
        properties,
    }))
}

/// Update a property on a running pipeline element.
///
/// Allows live modification of element properties while the pipeline is running.
/// Only properties marked as mutable in the current pipeline state can be updated.
/// The property mutability flags (mutable_in_playing, etc.) can be checked via
/// the element info endpoint.
#[utoipa::path(
    patch,
    path = "/api/flows/{flow_id}/elements/{element_id}/properties",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("element_id" = String, Path, description = "Element instance ID")
    ),
    request_body = UpdatePropertyRequest,
    responses(
        (status = 200, description = "Property updated successfully", body = ElementPropertiesResponse),
        (status = 400, description = "Property cannot be changed in current state or invalid value", body = ErrorResponse),
        (status = 404, description = "Flow not running or element not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn update_element_property(
    State(state): State<AppState>,
    Path((flow_id, element_id)): Path<(FlowId, String)>,
    ValidatedJson(req): ValidatedJson<UpdatePropertyRequest>,
) -> Result<Json<ElementPropertiesResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .update_element_property(&flow_id, &element_id, &req.property_name, req.value)
        .await
        .map_err(|e| {
            error!("Failed to update property: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to update property",
                    e.to_string(),
                )),
            )
        })?;

    // Return updated properties
    let properties = state
        .get_element_properties(&flow_id, &element_id)
        .await
        .map_err(|e| {
            error!("Failed to get updated properties: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::with_details(
                    "Property updated but failed to retrieve new values",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(ElementPropertiesResponse {
        element_id,
        properties,
    }))
}

/// Get current property values from a pad in a running element.
///
/// Returns all readable properties and their current values from a specific pad
/// on an element in a running pipeline. This is useful for elements like compositor
/// where you need to control individual sink pad properties (alpha, xpos, ypos, zorder).
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/elements/{element_id}/pads/{pad_name}/properties",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("element_id" = String, Path, description = "Element instance ID"),
        ("pad_name" = String, Path, description = "Pad name (e.g., sink_0, sink_1)")
    ),
    responses(
        (status = 200, description = "Pad properties retrieved successfully", body = PadPropertiesResponse),
        (status = 404, description = "Flow not running, element not found, or pad not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_pad_properties(
    State(state): State<AppState>,
    Path((flow_id, element_id, pad_name)): Path<(FlowId, String, String)>,
) -> Result<Json<PadPropertiesResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!(
        "Getting properties for pad {}:{} in flow {}",
        element_id, pad_name, flow_id
    );

    let properties = state
        .get_pad_properties(&flow_id, &element_id, &pad_name)
        .await
        .map_err(|e| {
            error!("Failed to get pad properties: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to get pad properties",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(PadPropertiesResponse {
        element_id,
        pad_name,
        properties,
    }))
}

/// Update a property on a pad in a running pipeline element.
///
/// Allows live modification of pad properties while the pipeline is running.
/// This is essential for elements like compositor, glvideomixer, and audiomixer
/// where you need to control individual input pad properties.
/// Common pad properties include:
/// - alpha: Opacity/transparency (0.0 to 1.0)
/// - xpos, ypos: Position in pixels
/// - width, height: Size in pixels
/// - zorder: Layer order (higher values are on top)
#[utoipa::path(
    patch,
    path = "/api/flows/{flow_id}/elements/{element_id}/pads/{pad_name}/properties",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("element_id" = String, Path, description = "Element instance ID"),
        ("pad_name" = String, Path, description = "Pad name (e.g., sink_0, sink_1)")
    ),
    request_body = UpdatePadPropertyRequest,
    responses(
        (status = 200, description = "Pad property updated successfully", body = PadPropertiesResponse),
        (status = 400, description = "Property cannot be changed in current state or invalid value", body = ErrorResponse),
        (status = 404, description = "Flow not running, element not found, or pad not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn update_pad_property(
    State(state): State<AppState>,
    Path((flow_id, element_id, pad_name)): Path<(FlowId, String, String)>,
    ValidatedJson(req): ValidatedJson<UpdatePadPropertyRequest>,
) -> Result<Json<PadPropertiesResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!(
        "Updating pad property {}:{}:{} in flow {}",
        element_id, pad_name, req.property_name, flow_id
    );

    state
        .update_pad_property(
            &flow_id,
            &element_id,
            &pad_name,
            &req.property_name,
            req.value,
        )
        .await
        .map_err(|e| {
            error!("Failed to update pad property: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to update pad property",
                    e.to_string(),
                )),
            )
        })?;

    // Return updated properties
    let properties = state
        .get_pad_properties(&flow_id, &element_id, &pad_name)
        .await
        .map_err(|e| {
            error!("Failed to get updated pad properties: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::with_details(
                    "Property updated but failed to retrieve new values",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(PadPropertiesResponse {
        element_id,
        pad_name,
        properties,
    }))
}

/// Update flow properties (description, clock type, etc.).
///
/// Updates the configuration properties of a flow. The flow must be stopped
/// to change certain properties like the clock type.
#[utoipa::path(
    patch,
    path = "/api/flows/{id}/properties",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    request_body = UpdateFlowPropertiesRequest,
    responses(
        (status = 200, description = "Properties updated successfully", body = FlowResponse),
        (status = 404, description = "Flow not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn update_flow_properties(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
    JsonBody(req): JsonBody<UpdateFlowPropertiesRequest>,
) -> Result<Json<FlowResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!("Updating properties for flow {}", id);

    // Get the flow
    let mut flow = state.get_flow(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Flow not found")),
        )
    })?;

    // Update properties while preserving timestamps
    let old_created_at = flow.properties.created_at.clone();
    let old_started_at = flow.properties.started_at.clone();
    flow.properties = req.properties;
    flow.properties.created_at = old_created_at;
    flow.properties.started_at = old_started_at;
    flow.properties.last_modified = Some(Local::now().to_rfc3339());

    // Save the updated flow
    if let Err(e) = state.upsert_flow(flow.clone()).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::with_details(
                "Failed to save flow properties",
                e.to_string(),
            )),
        ));
    }

    info!("Successfully updated properties for flow {}", id);

    Ok(Json(FlowResponse { flow }))
}

/// Get WebRTC statistics from a running flow.
///
/// Returns statistics from all webrtcbin elements in the pipeline, including
/// those nested in bins like whepclientsrc and whipclientsink. Stats include
/// RTP stream information, ICE connection state, and raw stats data.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/webrtc-stats",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "WebRTC statistics retrieved", body = WebRtcStatsResponse),
        (status = 404, description = "Flow not running", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_webrtc_stats(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<WebRtcStatsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let stats = state.get_webrtc_stats(&id).await.map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::with_details(
                "Pipeline not running or no WebRTC elements found",
                e.to_string(),
            )),
        )
    })?;

    Ok(Json(WebRtcStatsResponse { flow_id: id, stats }))
}

/// Get pipeline latency for a running flow.
///
/// Returns the latency information for a running pipeline. The flow must be
/// started and in PLAYING state for latency information to be available.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/latency",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "Latency retrieved successfully", body = LatencyResponse),
        (status = 404, description = "Flow not running or latency not available", body = ErrorResponse)
    )
)]
pub async fn get_flow_latency(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<LatencyResponse>, (StatusCode, Json<ErrorResponse>)> {
    trace!("Getting latency for flow {}", id);

    let latency = state.get_flow_latency(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Flow not running or latency not available",
            )),
        )
    })?;

    let (min_ns, max_ns, live) = latency;
    trace!(
        "Flow {} latency: min={}ns, max={}ns, live={}",
        id,
        min_ns,
        max_ns,
        live
    );

    Ok(Json(LatencyResponse::new(min_ns, max_ns, live)))
}

/// Get runtime statistics for a flow's pipeline.
///
/// Returns RTP statistics from running pipeline elements, such as jitterbuffer
/// statistics for AES67 input blocks. The flow must be started and running
/// for statistics to be available.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/rtp-stats",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "RTP statistics retrieved successfully", body = FlowStatsResponse),
        (status = 404, description = "Flow not running or no RTP statistics available", body = ErrorResponse)
    )
)]
pub async fn get_flow_rtp_stats(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<FlowStatsResponse>, (StatusCode, Json<ErrorResponse>)> {
    trace!("Getting RTP statistics for flow {}", id);

    let rtp_stats = state.get_flow_rtp_stats(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Flow not running or no RTP statistics available",
            )),
        )
    })?;

    trace!(
        "Flow {} RTP stats: {} blocks with statistics",
        id,
        rtp_stats.block_stats.len()
    );

    Ok(Json(FlowStatsResponse {
        flow_id: rtp_stats.flow_id,
        flow_name: rtp_stats.flow_name,
        blocks: rtp_stats.block_stats,
        collected_at: rtp_stats.collected_at,
    }))
}

/// Get debug information for a running flow.
///
/// Returns pipeline timing information including base_time, clock_time, and
/// running_time. This is useful for debugging AES67/RFC 7273 RTP timestamp
/// issues where precise clock synchronization is critical.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/debug",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "Debug information retrieved successfully", body = FlowDebugInfo),
        (status = 404, description = "Flow not running", body = ErrorResponse)
    )
)]
pub async fn get_flow_debug_info(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<FlowDebugInfo>, (StatusCode, Json<ErrorResponse>)> {
    trace!("Getting debug info for flow {}", id);

    let debug_info = state.get_flow_debug_info(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Flow not running. Start the flow first.",
            )),
        )
    })?;

    trace!(
        "Flow {} debug: base_time={:?}ns, clock_time={:?}ns, running_time={:?}ns",
        id,
        debug_info.base_time_ns,
        debug_info.clock_time_ns,
        debug_info.running_time_ns
    );

    Ok(Json(debug_info))
}

/// Trigger a scene transition on a compositor block.
///
/// Animates the transition between two inputs on a compositor/mixer block.
/// Supported transition types:
/// - `cut`: Instant switch (no animation)
/// - `fade`: Cross-fade via alpha blending
/// - `slide_left`: New input slides in from the right
/// - `slide_right`: New input slides in from the left
/// - `slide_up`: New input slides in from the bottom
/// - `slide_down`: New input slides in from the top
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/transition",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID (e.g., 'comp_1')")
    ),
    request_body = TriggerTransitionRequest,
    responses(
        (status = 200, description = "Transition triggered successfully", body = TransitionResponse),
        (status = 400, description = "Invalid transition parameters", body = ErrorResponse),
        (status = 404, description = "Flow not running or block not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn trigger_transition(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    ValidatedJson(req): ValidatedJson<TriggerTransitionRequest>,
) -> Result<Json<TransitionResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Triggering {} transition on block {} in flow {} ({} -> {}, {}ms)",
        req.transition_type, block_id, flow_id, req.from_input, req.to_input, req.duration_ms
    );

    state
        .trigger_transition(
            &flow_id,
            &block_id,
            req.from_input,
            req.to_input,
            &req.transition_type,
            req.duration_ms,
        )
        .await
        .map_err(|e| {
            error!("Failed to trigger transition: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to trigger transition",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(TransitionResponse {
        message: format!(
            "Transition {} started: input {} -> {}",
            req.transition_type, req.from_input, req.to_input
        ),
        transition_type: req.transition_type,
        duration_ms: req.duration_ms,
    }))
}

/// Select a preview source on a vision mixer block.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/preview",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = strom_types::api::SelectPreviewRequest,
    responses(
        (status = 200, description = "Preview source selected", body = strom_types::api::SelectPreviewResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Flow or block not found", body = ErrorResponse),
    )
)]
pub async fn select_preview(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<strom_types::api::SelectPreviewRequest>,
) -> Result<Json<strom_types::api::SelectPreviewResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!(
        "Selecting preview input {} on vision mixer {} in flow {}",
        req.input, block_id, flow_id
    );

    let (pvw, pgm) = state
        .select_vision_mixer_preview(&flow_id, &block_id, req.input)
        .await
        .map_err(|e| {
            error!("Failed to select preview: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to select preview",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(strom_types::api::SelectPreviewResponse {
        message: format!("Preview set to input {}", req.input),
        preview_input: pvw,
        program_input: pgm,
    }))
}

/// Toggle a DSK (Downstream Keyer) layer on a vision mixer block.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/dsk",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = strom_types::api::DskToggleRequest,
    responses(
        (status = 200, description = "DSK toggled", body = strom_types::api::DskToggleResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
    )
)]
pub async fn toggle_dsk(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<strom_types::api::DskToggleRequest>,
) -> Result<Json<strom_types::api::DskToggleResponse>, (StatusCode, Json<ErrorResponse>)> {
    if req.dsk < 1 || req.dsk > strom_types::vision_mixer::MAX_DSK_INPUTS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details(
                "Invalid DSK number",
                format!(
                    "DSK must be 1-{}, got {}",
                    strom_types::vision_mixer::MAX_DSK_INPUTS,
                    req.dsk
                ),
            )),
        ));
    }

    info!(
        "Toggling DSK {} {} on vision mixer {} in flow {}",
        req.dsk,
        if req.enabled { "on" } else { "off" },
        block_id,
        flow_id
    );

    // Convert 1-based DSK number to 0-based internal index
    let dsk_index = req.dsk - 1;

    state
        .set_dsk_enabled(&flow_id, &block_id, dsk_index, req.enabled)
        .await
        .map_err(|e| {
            error!("Failed to toggle DSK: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to toggle DSK",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(strom_types::api::DskToggleResponse {
        message: format!(
            "DSK {} {}",
            req.dsk,
            if req.enabled { "enabled" } else { "disabled" }
        ),
        dsk: req.dsk,
        enabled: req.enabled,
    }))
}

/// Reset accumulated loudness measurements on an EBU R128 meter block.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/loudness/reset",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID")
    ),
    responses(
        (status = 204, description = "Loudness measurements reset"),
        (status = 400, description = "Failed to reset", body = ErrorResponse),
        (status = 404, description = "Flow not running or block not found", body = ErrorResponse)
    )
)]
pub async fn reset_loudness(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .reset_loudness(&flow_id, &block_id)
        .await
        .map_err(|e| {
            error!("Failed to reset loudness: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to reset loudness",
                    e.to_string(),
                )),
            )
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Force an immediate file split on a recorder block.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/recorder/split",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID")
    ),
    responses(
        (status = 204, description = "File split triggered"),
        (status = 400, description = "Failed to trigger split (e.g. ts_passthrough mode)", body = ErrorResponse),
        (status = 404, description = "Flow not running or block not found", body = ErrorResponse)
    )
)]
pub async fn recorder_split_now(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .recorder_split_now(&flow_id, &block_id)
        .await
        .map_err(|e| {
            error!("Failed to trigger recorder split: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to trigger recorder split",
                    e.to_string(),
                )),
            )
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Animate a single input's position and/or size.
///
/// Smoothly animates the specified input from its current position/size
/// to the target values over the specified duration.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/animate",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID (e.g., 'comp_1')")
    ),
    request_body = AnimateInputRequest,
    responses(
        (status = 200, description = "Animation started successfully"),
        (status = 400, description = "Invalid parameters", body = ErrorResponse),
        (status = 404, description = "Flow not running or block not found", body = ErrorResponse)
    )
)]
pub async fn animate_input(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    ValidatedJson(req): ValidatedJson<AnimateInputRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    info!(
        "Animating input {} on block {} in flow {} to ({:?}, {:?}, {:?}, {:?}) over {}ms",
        req.input, block_id, flow_id, req.xpos, req.ypos, req.width, req.height, req.duration_ms
    );

    state
        .animate_input(
            &flow_id,
            &block_id,
            req.input,
            req.xpos,
            req.ypos,
            req.width,
            req.height,
            req.duration_ms,
        )
        .await
        .map_err(|e| {
            error!("Failed to animate input: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to animate input",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(serde_json::json!({
        "message": format!("Animation started for input {}", req.input),
        "duration_ms": req.duration_ms
    })))
}

/// Path parameters for block thumbnail endpoint.
#[derive(Debug, Deserialize)]
pub struct BlockThumbnailPath {
    /// Flow ID (UUID)
    pub id: FlowId,
    /// Block instance ID (e.g., "b0")
    pub block_id: String,
}

/// Query parameters for block thumbnail endpoint.
#[derive(Debug, Deserialize)]
pub struct BlockThumbnailQuery {
    /// Tap index (default 0). The meaning depends on the block type —
    /// e.g. compositor input index, or 0 for a single-tee thumbnail block.
    #[serde(default)]
    pub index: usize,
}

/// Get a thumbnail image from a block's video tap.
///
/// Works with any block that exposes thumbnail tee elements, including
/// `builtin.thumbnail` (index 0) and compositor blocks (one per input).
/// The first request activates the thumbnail branch; subsequent requests
/// are served from cache.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/blocks/{block_id}/thumbnail",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID (e.g., 'b0')"),
        ("index" = Option<usize>, Query, description = "Tap index (default 0, e.g. compositor input index)")
    ),
    responses(
        (status = 200, description = "JPEG thumbnail image", content_type = "image/jpeg"),
        (status = 404, description = "Flow not running or block not found", body = ErrorResponse),
        (status = 504, description = "Frame capture timed out (retry shortly)", body = ErrorResponse)
    )
)]
pub async fn get_block_thumbnail(
    State(state): State<AppState>,
    Path(path): Path<BlockThumbnailPath>,
    axum::extract::Query(query): axum::extract::Query<BlockThumbnailQuery>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    trace!(
        "Getting block thumbnail for flow {} block {} index {}",
        path.id,
        path.block_id,
        query.index
    );

    let jpeg_bytes = state
        .capture_block_thumbnail(&path.id, &path.block_id, query.index)
        .await
        .map_err(|e| {
            let error_msg = e.to_string();
            if error_msg.contains("timed out") || error_msg.contains("Timeout") {
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(ErrorResponse::with_details(
                        "Frame capture timed out",
                        error_msg,
                    )),
                )
            } else if error_msg.contains("not running") || error_msg.contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::with_details(
                        "Flow not running or block not found",
                        error_msg,
                    )),
                )
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::with_details(
                        "Thumbnail capture failed",
                        error_msg,
                    )),
                )
            }
        })?;

    trace!(
        "Block thumbnail captured: {} bytes for flow {} block {} index {}",
        jpeg_bytes.len(),
        path.id,
        path.block_id,
        query.index
    );

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/jpeg")],
        jpeg_bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ========================================================================
    // is_pad_valid() tests - prevent regression of gst-launch import bug
    // ========================================================================

    /// Helper to create element_ids set from a slice
    fn element_ids(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Helper to create valid_block_pads set from a slice
    fn block_pads(pads: &[&str]) -> HashSet<String> {
        pads.iter().map(|s| s.to_string()).collect()
    }

    /// Helper to create blocks_with_computed_pads set from a slice
    fn computed_blocks(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Helper to create block_ids set from a slice
    fn block_ids_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_is_pad_valid_ui_created_element() {
        // UI-created elements have IDs starting with 'e' like "e1234abcd..."
        let elements = element_ids(&["e1234567890abcdef"]);
        let blocks = block_pads(&[]);
        let block_ids = block_ids_set(&[]);
        let computed = computed_blocks(&[]);

        assert!(
            is_pad_valid(
                "e1234567890abcdef:src",
                &blocks,
                &elements,
                &block_ids,
                &computed
            ),
            "UI-created element pads should be valid"
        );
        assert!(
            is_pad_valid(
                "e1234567890abcdef:sink",
                &blocks,
                &elements,
                &block_ids,
                &computed
            ),
            "UI-created element sink pads should be valid"
        );
    }

    #[test]
    fn test_is_pad_valid_gst_launch_imported_element() {
        // gst-launch imported elements have IDs like "videotestsrc_0", "videoconvert_1"
        // This was the bug - these were incorrectly rejected because they don't start with 'e'
        let elements = element_ids(&["videotestsrc_0", "videoconvert_1", "fakesink_2"]);
        let blocks = block_pads(&[]);
        let block_ids = block_ids_set(&[]);
        let computed = computed_blocks(&[]);

        assert!(
            is_pad_valid(
                "videotestsrc_0:src",
                &blocks,
                &elements,
                &block_ids,
                &computed
            ),
            "gst-launch imported element pads should be valid"
        );
        assert!(
            is_pad_valid(
                "videoconvert_1:sink",
                &blocks,
                &elements,
                &block_ids,
                &computed
            ),
            "gst-launch imported element sink pads should be valid"
        );
        assert!(
            is_pad_valid("fakesink_2:sink", &blocks, &elements, &block_ids, &computed),
            "gst-launch imported sink element pads should be valid"
        );
    }

    #[test]
    fn test_is_pad_valid_user_named_element() {
        // Users can name elements anything, e.g., "mysource", "output"
        let elements = element_ids(&["mysource", "myfilter", "output"]);
        let blocks = block_pads(&[]);
        let block_ids = block_ids_set(&[]);
        let computed = computed_blocks(&[]);

        assert!(
            is_pad_valid("mysource:src", &blocks, &elements, &block_ids, &computed),
            "User-named element pads should be valid"
        );
        assert!(
            is_pad_valid("output:sink", &blocks, &elements, &block_ids, &computed),
            "User-named sink pads should be valid"
        );
    }

    #[test]
    fn test_is_pad_valid_block_with_computed_pads() {
        // Blocks have IDs starting with 'b' and computed external pads
        let elements = element_ids(&[]);
        let blocks = block_pads(&[
            "b1234:audio_in",
            "b1234:audio_out",
            "b5678:video_in",
            "b5678:video_out",
        ]);
        let block_ids = block_ids_set(&["b1234", "b5678"]);
        let computed = computed_blocks(&["b1234", "b5678"]);

        assert!(
            is_pad_valid("b1234:audio_in", &blocks, &elements, &block_ids, &computed),
            "Block with computed pads - valid pad should work"
        );
        assert!(
            is_pad_valid("b5678:video_out", &blocks, &elements, &block_ids, &computed),
            "Block with computed pads - valid output should work"
        );
        assert!(
            !is_pad_valid(
                "b1234:nonexistent",
                &blocks,
                &elements,
                &block_ids,
                &computed
            ),
            "Block with computed pads - invalid pad should fail"
        );
    }

    #[test]
    fn test_is_pad_valid_block_without_computed_pads() {
        // Blocks without computed pads use static definitions - assume valid
        let elements = element_ids(&[]);
        let blocks = block_pads(&[]);
        let block_ids = block_ids_set(&["b9999"]); // b9999 exists but not in computed set
        let computed = computed_blocks(&[]);

        assert!(
            is_pad_valid("b9999:any_pad", &blocks, &elements, &block_ids, &computed),
            "Block without computed pads should be assumed valid"
        );
    }

    #[test]
    fn test_is_pad_valid_nonexistent_element() {
        let elements = element_ids(&["elem1"]);
        let blocks = block_pads(&[]);
        let block_ids = block_ids_set(&[]);
        let computed = computed_blocks(&[]);

        assert!(
            !is_pad_valid("nonexistent:src", &blocks, &elements, &block_ids, &computed),
            "Non-existent element should be invalid"
        );
    }

    #[test]
    fn test_is_pad_valid_malformed_pad_ref() {
        let elements = element_ids(&["elem1"]);
        let blocks = block_pads(&[]);
        let block_ids = block_ids_set(&[]);
        let computed = computed_blocks(&[]);

        assert!(
            !is_pad_valid("no_colon", &blocks, &elements, &block_ids, &computed),
            "Pad ref without colon should be invalid"
        );
        assert!(
            !is_pad_valid("", &blocks, &elements, &block_ids, &computed),
            "Empty pad ref should be invalid"
        );
    }

    #[test]
    fn test_is_pad_valid_mixed_elements_and_blocks() {
        // Realistic scenario with both UI elements and blocks
        let elements = element_ids(&["e123", "videotestsrc_0"]);
        let blocks = block_pads(&["b456:audio_in", "b456:audio_out"]);
        let block_ids = block_ids_set(&["b456"]);
        let computed = computed_blocks(&["b456"]);

        // Elements
        assert!(is_pad_valid(
            "e123:src", &blocks, &elements, &block_ids, &computed
        ));
        assert!(is_pad_valid(
            "videotestsrc_0:src",
            &blocks,
            &elements,
            &block_ids,
            &computed
        ));

        // Blocks
        assert!(is_pad_valid(
            "b456:audio_in",
            &blocks,
            &elements,
            &block_ids,
            &computed
        ));
        assert!(!is_pad_valid(
            "b456:nonexistent",
            &blocks,
            &elements,
            &block_ids,
            &computed
        ));

        // Invalid
        assert!(!is_pad_valid(
            "unknown:src",
            &blocks,
            &elements,
            &block_ids,
            &computed
        ));
    }
}
