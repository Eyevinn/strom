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
        AnimateInputRequest, AvailableOutput, AvailableSourcesResponse, BlockPropertiesResponse,
        DynamicPadsResponse, ElementPropertiesResponse, ErrorResponse, FlowDebugInfo,
        FlowListResponse, FlowResponse, FlowStatsResponse, LatencyResponse, PadPropertiesResponse,
        SourceFlowInfo, SrtStatsResponse, TransitionResponse, TriggerTransitionRequest,
        UpdateBlockPropertiesRequest, UpdateFlowPropertiesRequest, UpdatePadPropertyRequest,
        UpdatePropertyRequest, WebRtcStatsResponse,
    },
    Flow, FlowId,
};
use tracing::{debug, error, info, trace, warn};

use crate::gst::PipelineError;
use crate::layout;
use crate::state::AppState;

/// Which end of a link an endpoint sits on.
///
/// A bare node reference means "the element's own pad", so which pad that is
/// depends on the side: the source end of a link needs an output pad, the
/// destination end an input pad.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkSide {
    From,
    To,
}

impl LinkSide {
    fn pad_kind(self) -> &'static str {
        match self {
            LinkSide::From => "output",
            LinkSide::To => "input",
        }
    }
}

/// Why a link endpoint could not be used as sent.
#[derive(Debug)]
enum EndpointProblem {
    /// The caller named something this flow does not contain, or a bare
    /// reference that does not resolve to exactly one pad. A caller mistake:
    /// the request is rejected rather than quietly trimmed.
    Rejected(String),
    /// The endpoint names a pad on a block whose current properties no longer
    /// produce it (e.g. the track count was lowered). Expected drift, not a
    /// caller mistake, so the link is pruned.
    StaleBlockPad(String),
}

impl EndpointProblem {
    fn message(&self) -> &str {
        match self {
            EndpointProblem::Rejected(m) | EndpointProblem::StaleBlockPad(m) => m,
        }
    }
}

/// The external pads a block instance exposes.
struct BlockPads {
    inputs: Vec<String>,
    outputs: Vec<String>,
    /// True when the pads were computed from the instance's properties. Only
    /// then is the pad list authoritative enough to call a named pad stale;
    /// blocks that fall back to their static definition are trusted and
    /// validated at pipeline build time.
    computed: bool,
}

impl BlockPads {
    fn pads_on(&self, side: LinkSide) -> &[String] {
        match side {
            LinkSide::From => &self.outputs,
            LinkSide::To => &self.inputs,
        }
    }
}

/// True for the two element-level spellings of a pad reference: a bare id, and
/// the `"id::"` marker `ElementPadRef::to_string_format()` produces.
fn is_element_only_ref(pad_ref: &str) -> bool {
    !pad_ref.contains(':') || pad_ref.ends_with("::")
}

/// Resolve one end of a link against the nodes in the flow.
///
/// Returns the endpoint to store: `None` keeps what the caller sent, `Some`
/// replaces it with a resolved form. A bare reference to an element stays bare,
/// since GStreamer picks compatible pads when linking element to element. A
/// bare reference to a block is rewritten to `block_id:pad_name`, because block
/// expansion resolves external pads by name.
fn resolve_link_endpoint(
    pad_ref: &str,
    side: LinkSide,
    element_ids: &std::collections::HashSet<String>,
    block_pads: &std::collections::HashMap<String, BlockPads>,
) -> Result<Option<String>, EndpointProblem> {
    // "element_id", "element_id::", "node_id:pad_name". The node id is the
    // first segment; a block's internal element is not addressable from a flow
    // link, so everything after the first colon is the pad name.
    let (node_id, pad_name) = match pad_ref.strip_suffix("::") {
        Some(node) => (node, None),
        None => match pad_ref.split_once(':') {
            Some((node, pad)) => (node, Some(pad)),
            None => (pad_ref, None),
        },
    };

    if node_id.is_empty() {
        return Err(EndpointProblem::Rejected(format!(
            "'{}' does not name an element or block",
            pad_ref
        )));
    }

    if element_ids.contains(node_id) {
        // Elements are checked for existence only; the pad itself is resolved
        // when the pipeline is built, where request and dynamic pads are known.
        return Ok(None);
    }

    let Some(pads) = block_pads.get(node_id) else {
        return Err(EndpointProblem::Rejected(format!(
            "'{}' does not name an element or block in this flow",
            pad_ref
        )));
    };

    let Some(pad_name) = pad_name else {
        // Bare block reference - resolve it to the block's only pad on this side.
        let candidates = pads.pads_on(side);
        return match candidates {
            [only] => Ok(Some(format!("{}:{}", node_id, only))),
            [] => Err(EndpointProblem::Rejected(format!(
                "block '{}' has no {} pad to link",
                node_id,
                side.pad_kind()
            ))),
            many => Err(EndpointProblem::Rejected(format!(
                "block '{}' has {} {} pads ({}); name one, e.g. '{}:{}'",
                node_id,
                many.len(),
                side.pad_kind(),
                many.join(", "),
                node_id,
                many[0]
            ))),
        };
    };

    if !pads.computed {
        // Static pad definition - trusted, validated at pipeline build time.
        return Ok(None);
    }

    if pads
        .inputs
        .iter()
        .chain(pads.outputs.iter())
        .any(|name| name == pad_name)
    {
        return Ok(None);
    }

    Err(EndpointProblem::StaleBlockPad(format!(
        "block '{}' no longer has a pad '{}'",
        node_id, pad_name
    )))
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

/// Prepare a flow for storage: trim endpoint strings, compute pads,
/// resolve links, and apply auto-layout if needed.
///
/// Returns `Err` with a caller-facing description of every link that could not
/// be resolved. The flow is not fit to store in that case - a request that
/// carries an unusable link is answered with 400 rather than persisted with the
/// link missing.
fn prepare_flow(flow: &mut Flow) -> Result<(), String> {
    // Trim endpoint string properties to avoid whitespace-related issues
    for block in &mut flow.blocks {
        let prop_name = match block.block_definition_id.as_str() {
            "builtin.whip_input" | "builtin.whep_output" => Some("endpoint_id"),
            "builtin.whip_output" => Some("whip_endpoint"),
            "builtin.whep_input" => Some("whep_endpoint"),
            _ => None,
        };
        if let Some(name) = prop_name {
            if let Some(strom_types::PropertyValue::String(s)) = block.properties.get_mut(name) {
                let trimmed = s.trim().to_string();
                if *s != trimmed {
                    *s = trimmed;
                }
            }
        }
    }

    // Migrate legacy `mode` enum on WHEP Output blocks to explicit track counts.
    // Old flows had `mode: "audio"|"video"|"audio_video"`; the property panel now
    // exposes `num_audio_tracks` / `num_video_tracks` instead.
    for block in &mut flow.blocks {
        if block.block_definition_id == "builtin.whep_output" {
            crate::blocks::builtin::whep::migrate_legacy_mode(&mut block.properties);
        }
    }

    // Compute external pads for all block instances based on their properties
    for block in &mut flow.blocks {
        if let Some(builder) = crate::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }

    // Resolve every link the caller sent. Links are only dropped when a block
    // pad genuinely disappeared; anything else is reported back so the caller
    // learns about it instead of getting a stored flow with links missing.
    let mut block_pads: std::collections::HashMap<String, BlockPads> =
        std::collections::HashMap::new();
    for block in &flow.blocks {
        let pads = match &block.computed_external_pads {
            Some(computed) => BlockPads {
                inputs: computed.inputs.iter().map(|p| p.name.clone()).collect(),
                outputs: computed.outputs.iter().map(|p| p.name.clone()).collect(),
                computed: true,
            },
            None => BlockPads {
                inputs: Vec::new(),
                outputs: Vec::new(),
                computed: false,
            },
        };
        block_pads.insert(block.id.clone(), pads);
    }

    // Blocks without computed pads expose the pads of their static definition.
    // Those are only needed to resolve a bare block reference, so build the
    // definition list only when the flow actually contains one.
    if block_pads.values().any(|pads| !pads.computed)
        && flow
            .links
            .iter()
            .any(|link| is_element_only_ref(&link.from) || is_element_only_ref(&link.to))
    {
        let definitions = crate::blocks::builtin::get_all_builtin_blocks();
        for block in &flow.blocks {
            let Some(pads) = block_pads.get_mut(&block.id) else {
                continue;
            };
            if pads.computed {
                continue;
            }
            if let Some(definition) = definitions
                .iter()
                .find(|d| d.id == block.block_definition_id)
            {
                pads.inputs = definition
                    .external_pads
                    .inputs
                    .iter()
                    .map(|p| p.name.clone())
                    .collect();
                pads.outputs = definition
                    .external_pads
                    .outputs
                    .iter()
                    .map(|p| p.name.clone())
                    .collect();
            }
        }
    }

    let element_ids: std::collections::HashSet<String> =
        flow.elements.iter().map(|e| e.id.clone()).collect();

    let mut kept: Vec<strom_types::Link> = Vec::with_capacity(flow.links.len());
    let mut rejected: Vec<String> = Vec::new();
    let mut pruned = 0usize;

    for link in std::mem::take(&mut flow.links) {
        let from = resolve_link_endpoint(&link.from, LinkSide::From, &element_ids, &block_pads);
        let to = resolve_link_endpoint(&link.to, LinkSide::To, &element_ids, &block_pads);

        match (from, to) {
            (Ok(from_resolved), Ok(to_resolved)) => {
                kept.push(strom_types::Link {
                    from: from_resolved.unwrap_or(link.from),
                    to: to_resolved.unwrap_or(link.to),
                });
            }
            (from, to) => {
                let problems: Vec<EndpointProblem> =
                    [from.err(), to.err()].into_iter().flatten().collect();
                let detail = problems
                    .iter()
                    .map(|problem| problem.message())
                    .collect::<Vec<_>>()
                    .join("; ");

                if problems
                    .iter()
                    .any(|problem| matches!(problem, EndpointProblem::Rejected(_)))
                {
                    warn!(
                        "Rejecting flow '{}': invalid link {} -> {} ({})",
                        flow.name, link.from, link.to, detail
                    );
                    rejected.push(format!("'{}' -> '{}': {}", link.from, link.to, detail));
                } else {
                    warn!(
                        "Pruning stale link {} -> {} from flow '{}': {}",
                        link.from, link.to, flow.name, detail
                    );
                    pruned += 1;
                }
            }
        }
    }

    flow.links = kept;

    if pruned > 0 {
        info!(
            "Pruned {} stale link(s) from flow '{}' (block pads changed)",
            pruned, flow.name
        );
    }

    if !rejected.is_empty() {
        return Err(rejected.join(" | "));
    }

    // Apply auto-layout if needed
    if layout::needs_auto_layout(flow) {
        info!(
            "Flow '{}' needs auto-layout (elements stacked or missing positions)",
            flow.name
        );
        layout::apply_auto_layout(flow);
    }

    Ok(())
}

/// Create a new flow.
#[utoipa::path(
    post,
    path = "/api/flows",
    tag = "flows",
    description = "Creates a flow under the `id` supplied in the body, so a caller can \
                   pre-generate an id and then start the flow by it. `id` is a required \
                   field: to have the server assign one instead, send the nil uuid \
                   (`00000000-0000-0000-0000-000000000000`) and read the assigned id from \
                   the `flow.id` of the response. Reusing the id of an existing flow is a \
                   409; use `POST /api/flows/{id}` to update that flow instead.",
    request_body = Flow,
    responses(
        (status = 201, description = "Flow created", body = FlowResponse),
        (status = 400, description = "Invalid flow name, or a link that names an unknown element, block or pad", body = ErrorResponse),
        (status = 409, description = "A flow with the supplied id already exists", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn create_flow(
    State(state): State<AppState>,
    JsonBody(mut flow): JsonBody<Flow>,
) -> Result<(StatusCode, Json<FlowResponse>), (StatusCode, Json<ErrorResponse>)> {
    let name_len = flow.name.trim().len();
    if name_len == 0 || name_len > 255 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "Flow name must be between 1 and 255 characters",
            )),
        ));
    }

    info!("Received create flow request: name='{}'", flow.name);
    debug!("Create flow request body: {:?}", flow);

    // Honour the client-supplied id. The schema requires it, so silently replacing
    // it left callers unable to POST a flow and then start it by the id they chose.
    // `id` cannot be omitted — it is required, so leaving it out is a 422 — which is
    // why the nil uuid is the documented way to ask the server to assign one.
    if flow.id.is_nil() {
        flow.id = FlowId::new_v4();
    }

    // Clear runtime state
    flow.running = false;
    flow.gst_state = None;
    for block in &mut flow.blocks {
        block.runtime_data = None;
    }

    // Set timestamps
    let now = Local::now().to_rfc3339();
    flow.properties.created_at = Some(now.clone());
    flow.properties.last_modified = Some(now);

    if let Err(details) = prepare_flow(&mut flow) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details(
                "Flow contains link(s) that cannot be resolved",
                details,
            )),
        ));
    }

    info!("Creating flow: {} ({})", flow.name, flow.id);

    // Import and copy in the frontend already regenerate ids client-side
    // (`regenerate_flow_ids`), so a clash here is a genuine one the caller needs to
    // know about rather than something to paper over. The id is claimed inside the
    // same write lock that checks it, so two concurrent creates supplying the same
    // id cannot both pass and overwrite one another.
    match state.insert_flow_if_absent(flow.clone()).await {
        Ok(true) => {}
        Ok(false) => {
            return Err((
                StatusCode::CONFLICT,
                Json(ErrorResponse::new(
                    "A flow with this id already exists; use POST /api/flows/{id} to update it",
                )),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::with_details(
                    "Failed to save flow",
                    e.to_string(),
                )),
            ));
        }
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
    debug!("Update flow request body: {:?}", flow);

    if let Err(details) = prepare_flow(&mut flow) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details(
                "Flow contains link(s) that cannot be resolved",
                details,
            )),
        ));
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
        (status = 400, description = "Flow definition rejected", body = ErrorResponse),
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
        // A property the flow definition got wrong is the client's mistake, not
        // the server's: the same errors are a 400 on the live update path.
        let (status, message) = match e {
            PipelineError::FlowNotFound(_) => (StatusCode::NOT_FOUND, "Flow not found"),
            PipelineError::InvalidProperty { .. } | PipelineError::PropertyNotMutable { .. } => {
                (StatusCode::BAD_REQUEST, "Failed to start flow")
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to start flow"),
        };
        return Err((
            status,
            Json(ErrorResponse::with_details(message, e.to_string())),
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
        .update_element_property(
            &flow_id,
            &element_id,
            &req.property_name,
            req.value,
            req.ramp_ms,
        )
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

/// Get current block-level exposed property values from a running pipeline.
///
/// Returns each live exposed property's current value in block-level (user-facing)
/// units — e.g. a `ch1_pfl` bool, a `fader_db` float in dB — by reading the
/// underlying GStreamer element and applying the declared inverse transform.
/// Non-live properties and those bound to the `_block` virtual element are
/// omitted.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/properties",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID")
    ),
    responses(
        (status = 200, description = "Block properties retrieved", body = BlockPropertiesResponse),
        (status = 404, description = "Flow not running or block not found", body = ErrorResponse),
    )
)]
pub async fn get_block_properties(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<BlockPropertiesResponse>, (StatusCode, Json<ErrorResponse>)> {
    let properties = state
        .get_block_properties(&flow_id, &block_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::with_details(
                    "Failed to get block properties",
                    e.to_string(),
                )),
            )
        })?;
    Ok(Json(BlockPropertiesResponse {
        block_id,
        properties,
        rejected: Default::default(),
    }))
}

/// Update one or more exposed properties on a block instance live.
///
/// Properties are expressed in block-level (user-facing) units — e.g.
/// `{"ch1_pfl": true, "fader_db": -3.0}`. The backend resolves each name to its
/// underlying GStreamer element via the block definition's PropertyMapping,
/// applies the declared transform (`bool_to_volume`, `db_to_linear`, …), and
/// writes through the standard live-property path (so `ramp_ms` produces the
/// usual anti-click fade where applicable). `ramp_ms_overrides` lets the
/// caller pin a different ramp duration for individual properties in the same
/// batch — useful for crossfades where one fader rises while another falls at
/// the same rate but other properties move instantly.
///
/// Only properties marked `live: true` are accepted. Unknown, non-live, or
/// type-mismatched entries are returned in the `rejected` map without aborting
/// the rest of the batch.
#[utoipa::path(
    patch,
    path = "/api/flows/{flow_id}/blocks/{block_id}/properties",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block instance ID")
    ),
    request_body = UpdateBlockPropertiesRequest,
    responses(
        (status = 200, description = "Block properties applied (see `rejected` for partial failures)", body = BlockPropertiesResponse),
        (status = 404, description = "Flow not running or block not found", body = ErrorResponse),
    )
)]
pub async fn update_block_properties(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    ValidatedJson(req): ValidatedJson<UpdateBlockPropertiesRequest>,
) -> Result<Json<BlockPropertiesResponse>, (StatusCode, Json<ErrorResponse>)> {
    let (properties, rejected) = state
        .update_block_properties(
            &flow_id,
            &block_id,
            req.properties,
            req.ramp_ms,
            req.ramp_ms_overrides,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::with_details(
                    "Failed to update block properties",
                    e.to_string(),
                )),
            )
        })?;
    Ok(Json(BlockPropertiesResponse {
        block_id,
        properties,
        rejected,
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
    debug!("Update flow properties request body: {:?}", req);

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

/// Get SRT statistics from a running flow.
///
/// Returns statistics for every `srtsink` (output) and `srtsrc` (input) element in
/// the pipeline. Each entry is keyed by element name (e.g. `<block_id>:srtsink`),
/// so the frontend can filter the response per SRT block.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/srt-stats",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "SRT statistics retrieved", body = SrtStatsResponse),
        (status = 404, description = "Flow not running", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_srt_stats(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<SrtStatsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let stats = state.get_srt_stats(&id).await.map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::with_details(
                "Pipeline not running",
                e.to_string(),
            )),
        )
    })?;

    Ok(Json(SrtStatsResponse { flow_id: id, stats }))
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

/// Get negotiated caps for all pads in a running flow's pipeline.
///
/// Returns a JSON map of element_name → [(pad_name, direction, caps_string)].
/// Useful for debugging caps negotiation failures.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/pad-caps",
    tag = "flows",
    params(
        ("id" = String, Path, description = "Flow ID (UUID)")
    ),
    responses(
        (status = 200, description = "Pad caps for all elements"),
        (status = 404, description = "Flow not running", body = ErrorResponse)
    )
)]
pub async fn get_flow_pad_caps(
    State(state): State<AppState>,
    Path(id): Path<FlowId>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let caps = state.get_flow_pad_caps(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Flow not running")),
        )
    })?;

    // Convert to JSON-friendly format
    let json: serde_json::Map<String, serde_json::Value> = caps
        .into_iter()
        .map(|(elem, pads)| {
            let pads_json: Vec<serde_json::Value> = pads
                .into_iter()
                .map(|(name, dir, caps_str)| {
                    serde_json::json!({
                        "pad": name,
                        "direction": dir,
                        "caps": caps_str,
                    })
                })
                .collect();
            (elem, serde_json::Value::Array(pads_json))
        })
        .collect();

    Ok(Json(serde_json::Value::Object(json)))
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

    let actual_transition_type = state
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
        actual_transition_type,
        duration_ms: req.duration_ms,
    }))
}

/// Select a preview source on a vision mixer block.
#[utoipa::path(
    put,
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
    use strom_types::vision_mixer::Source;

    if let Source::Pip(pip_idx) = req.source {
        info!(
            "Selecting preview to PiP {} on vision mixer {} in flow {}",
            pip_idx, block_id, flow_id
        );
        state
            .select_vision_mixer_pip_for_preview(&flow_id, &block_id, pip_idx)
            .await
            .map_err(|e| {
                error!("Failed to select PiP preview: {}", e);
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::with_details(
                        "Failed to select PiP preview",
                        e.to_string(),
                    )),
                )
            })?;

        // Read back authoritative state for response.
        let overlay = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(&block_id);
        return Ok(Json(strom_types::api::SelectPreviewResponse {
            message: format!("Preview set to PiP {}", pip_idx),
            preview_input: overlay.as_ref().and_then(|s| s.pvw_input()),
            program_input: overlay.as_ref().and_then(|s| s.pgm_input()),
            preview_pip: Some(pip_idx),
            program_pip: overlay.as_ref().and_then(|s| s.pgm_pip()),
        }));
    }

    let Source::Input(input) = req.source else {
        unreachable!("Source::Pip handled above");
    };

    info!(
        "Selecting preview input {} on vision mixer {} in flow {}",
        input, block_id, flow_id
    );

    let (new_pvw, pgm) = state
        .select_vision_mixer_preview(&flow_id, &block_id, input)
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

    let pgm_pip = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(&block_id)
        .as_ref()
        .and_then(|s| s.pgm_pip());

    Ok(Json(strom_types::api::SelectPreviewResponse {
        message: format!("Preview set to input {}", input),
        preview_input: new_pvw,
        program_input: pgm,
        preview_pip: None,
        program_pip: pgm_pip,
    }))
}

/// Get the current composition of a single PiP on a vision mixer block.
///
/// Returns the same per-PiP state as the corresponding entry in
/// `VisionMixerState::pips`. Useful for exporting one PiP's composition
/// (e.g. to save it as a reusable layout preset); restore it with a `PUT`
/// to the same path.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/pip/{pip_idx}",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID"),
        ("pip_idx" = usize, Path, description = "PiP index (0-based)")
    ),
    responses(
        (status = 200, description = "Current PiP composition", body = strom_types::api::PipState),
        (status = 404, description = "Block has no live state (pipeline not running) or PiP index out of range", body = ErrorResponse),
    )
)]
pub async fn get_pip_config(
    Path((flow_id, block_id, pip_idx)): Path<(FlowId, String, usize)>,
) -> Result<Json<strom_types::api::PipState>, (StatusCode, Json<ErrorResponse>)> {
    let overlay = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(&block_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::with_details(
                    "Vision mixer state not available",
                    format!(
                        "No live overlay state for block {} in flow {} (pipeline not running)",
                        block_id, flow_id
                    ),
                )),
            )
        })?;

    if pip_idx >= overlay.num_pips {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::with_details(
                "PiP index out of range",
                format!(
                    "PiP index {} out of range (block {} has {} PiPs)",
                    pip_idx, block_id, overlay.num_pips
                ),
            )),
        ));
    }

    Ok(Json(strom_types::api::PipState {
        bg: overlay.pip_bg_input(pip_idx),
        zones: overlay.pip_zones(pip_idx),
        transforms: overlay.pip_transforms(pip_idx),
    }))
}

/// Update a PiP composition (background + overlay inputs) on a vision mixer block.
///
/// The change is applied live to all places where the PiP is currently visible:
/// the multiview PiP thumbnail tile, plus PGM/PVW if either bus is showing this PiP.
#[utoipa::path(
    put,
    path = "/api/flows/{flow_id}/blocks/{block_id}/pip/{pip_idx}",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID"),
        ("pip_idx" = usize, Path, description = "PiP index (0-based)")
    ),
    request_body = strom_types::api::UpdatePipConfigRequest,
    responses(
        (status = 200, description = "PiP config updated", body = strom_types::api::UpdatePipConfigResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Flow or block not found", body = ErrorResponse),
    )
)]
pub async fn update_pip_config(
    State(state): State<AppState>,
    Path((flow_id, block_id, pip_idx)): Path<(FlowId, String, usize)>,
    Json(req): Json<strom_types::api::UpdatePipConfigRequest>,
) -> Result<Json<strom_types::api::UpdatePipConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
    use strom_types::vision_mixer::MAX_PIP_OVERLAYS;
    let total_sources: usize = req.zones.iter().map(|z| z.sources.len()).sum();
    if total_sources > MAX_PIP_OVERLAYS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details(
                "Too many PiP overlay sources",
                format!(
                    "Got {} total sources across {} zones, but MAX_PIP_OVERLAYS is {}",
                    total_sources,
                    req.zones.len(),
                    MAX_PIP_OVERLAYS
                ),
            )),
        ));
    }

    info!(
        "Updating PiP {} on vision mixer {} in flow {}: bg={:?}, zones={:?}, transforms={:?}",
        pip_idx, block_id, flow_id, req.bg, req.zones, req.transforms
    );
    state
        .apply_vision_mixer_pip_config(
            &flow_id,
            &block_id,
            pip_idx,
            req.bg,
            req.zones.clone(),
            req.transforms.clone(),
        )
        .await
        .map_err(|e| {
            error!("Failed to update PiP config: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to update PiP config",
                    e.to_string(),
                )),
            )
        })?;

    // Read back authoritative state. Validation runs in
    // `apply_vision_mixer_pip_config`, so the only mutation vs. the request
    // is rect/crop clamping (NormRect → [0,1], SourceCrop clamped + zero
    // entries dropped).
    let (bg, zones, transforms) = if let Some(s) =
        crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(&block_id)
    {
        (
            s.pip_bg_input(pip_idx),
            s.pip_zones(pip_idx),
            s.pip_transforms(pip_idx),
        )
    } else {
        (req.bg, req.zones, req.transforms)
    };

    Ok(Json(strom_types::api::UpdatePipConfigResponse {
        message: format!("PiP {} updated", pip_idx),
        pip_idx,
        bg,
        zones,
        transforms,
    }))
}

/// Get the current runtime state of a vision mixer block.
///
/// Reflects the live overlay state — bus inputs, PiP visibility, FTB, DSK,
/// overlay alpha, per-PiP composition. Clients use this on (re)connect to
/// reconcile state; subsequent changes flow over the `VisionMixerStateChanged`
/// WebSocket event.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/state",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    responses(
        (status = 200, description = "Current vision mixer state", body = strom_types::api::VisionMixerState),
        (status = 404, description = "Block has no live state (pipeline not running)", body = ErrorResponse),
    )
)]
pub async fn get_vision_mixer_state(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<strom_types::api::VisionMixerState>, (StatusCode, Json<ErrorResponse>)> {
    let overlay = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(&block_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::with_details(
                    "Vision mixer state not available",
                    format!(
                        "No live overlay state for block {} in flow {} (pipeline not running)",
                        block_id, flow_id
                    ),
                )),
            )
        })?;

    let pips: Vec<strom_types::api::PipState> = (0..overlay.num_pips)
        .map(|i| strom_types::api::PipState {
            bg: overlay.pip_bg_input(i),
            zones: overlay.pip_zones(i),
            transforms: overlay.pip_transforms(i),
        })
        .collect();

    let dsk_enabled: Vec<bool> = overlay
        .dsk_enabled
        .iter()
        .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
        .collect();

    let input_resolutions = state
        .vision_mixer_input_resolutions(&flow_id, &block_id, overlay.num_inputs)
        .await;

    let fx_available = state.vision_mixer_fx_available(&flow_id, &block_id).await;
    let input_effects: Vec<strom_types::effects::VideoEffect> = if fx_available {
        overlay
            .input_effects
            .iter()
            .map(|m| m.lock().map(|e| e.clone()).unwrap_or_default())
            .collect()
    } else {
        Vec::new()
    };
    let master_effect = overlay
        .master_effect
        .lock()
        .map(|e| e.clone())
        .unwrap_or_default();

    Ok(Json(strom_types::api::VisionMixerState {
        program_input: overlay.pgm_input(),
        preview_input: overlay.pvw_input(),
        program_pip: overlay.pgm_pip(),
        preview_pip: overlay.pvw_pip(),
        ftb_active: overlay
            .ftb_active
            .load(std::sync::atomic::Ordering::Relaxed),
        dsk_enabled,
        overlay_alpha: overlay.overlay_alpha(),
        pips,
        input_resolutions,
        fx_available,
        input_effects,
        master_effect,
    }))
}

/// Set the multiview overlay alpha on a vision mixer block.
#[utoipa::path(
    put,
    path = "/api/flows/{flow_id}/blocks/{block_id}/overlay-alpha",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = strom_types::api::OverlayAlphaRequest,
    responses(
        (status = 200, description = "Overlay alpha set", body = strom_types::api::OverlayAlphaResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
    )
)]
pub async fn set_overlay_alpha(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<strom_types::api::OverlayAlphaRequest>,
) -> Result<Json<strom_types::api::OverlayAlphaResponse>, (StatusCode, Json<ErrorResponse>)> {
    let alpha = req.alpha.clamp(0.0, 1.0);

    state
        .set_overlay_alpha(&flow_id, &block_id, alpha)
        .await
        .map_err(|e| {
            error!("Failed to set overlay alpha: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to set overlay alpha",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(strom_types::api::OverlayAlphaResponse {
        message: format!("Overlay alpha set to {}", alpha),
        alpha,
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

/// Toggle Fade to Black on a vision mixer block.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/ftb",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = strom_types::api::FadeToBlackRequest,
    responses(
        (status = 200, description = "FTB toggled", body = strom_types::api::FadeToBlackResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
    )
)]
pub async fn fade_to_black(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<strom_types::api::FadeToBlackRequest>,
) -> Result<Json<strom_types::api::FadeToBlackResponse>, (StatusCode, Json<ErrorResponse>)> {
    let active = state
        .fade_to_black(&flow_id, &block_id, req.duration_ms)
        .await
        .map_err(|e| {
            error!("Failed to toggle FTB: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to toggle FTB",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(strom_types::api::FadeToBlackResponse {
        message: format!("FTB {}", if active { "activated" } else { "deactivated" }),
        active,
    }))
}

/// Set a shader video effect on a vision mixer block (input look or PGM master).
///
/// Requires the shader FX engine (GPU backend with Shader FX enabled) —
/// returns 400 when the engine is not built into the running pipeline.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/effect",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = strom_types::effects::SetVideoEffectRequest,
    responses(
        (status = 200, description = "Effect applied", body = strom_types::effects::SetVideoEffectResponse),
        (status = 400, description = "Invalid request or FX engine unavailable", body = ErrorResponse),
    )
)]
pub async fn set_vision_mixer_effect(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<strom_types::effects::SetVideoEffectRequest>,
) -> Result<Json<strom_types::effects::SetVideoEffectResponse>, (StatusCode, Json<ErrorResponse>)> {
    let applied = state
        .set_vision_mixer_effect(&flow_id, &block_id, req.target, &req.effect)
        .await
        .map_err(|e| {
            error!("Failed to set video effect: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Failed to set video effect",
                    e.to_string(),
                )),
            )
        })?;

    Ok(Json(strom_types::effects::SetVideoEffectResponse {
        message: format!("Effect '{}' applied to {}", applied.kind(), req.target),
        effect: applied,
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
    use std::collections::{HashMap, HashSet};

    // ========================================================================
    // resolve_link_endpoint() tests - these cover the gst-launch import bug
    // (element ids that do not start with 'e') and the bare-reference form
    // documented on `Link`.
    // ========================================================================

    /// Helper to create the element id set from a slice
    fn element_ids(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Helper to build a block whose pads were computed from its properties
    fn computed_block(inputs: &[&str], outputs: &[&str]) -> BlockPads {
        BlockPads {
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            computed: true,
        }
    }

    /// Helper to build a block that falls back to its static pad definition
    fn static_block(inputs: &[&str], outputs: &[&str]) -> BlockPads {
        BlockPads {
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            computed: false,
        }
    }

    fn blocks(entries: Vec<(&str, BlockPads)>) -> HashMap<String, BlockPads> {
        entries
            .into_iter()
            .map(|(id, pads)| (id.to_string(), pads))
            .collect()
    }

    /// `Ok(None)` means "store what the caller sent".
    fn assert_kept_as_sent(pad_ref: &str, side: LinkSide, elements: &HashSet<String>) {
        let resolved = resolve_link_endpoint(pad_ref, side, elements, &HashMap::new());
        match resolved {
            Ok(None) => {}
            Ok(Some(other)) => panic!("'{}' should be stored unchanged, got '{}'", pad_ref, other),
            Err(problem) => panic!("'{}' should be accepted: {}", pad_ref, problem.message()),
        }
    }

    #[test]
    fn ui_created_element_pads_are_accepted() {
        // UI-created elements have IDs starting with 'e' like "e1234abcd..."
        let elements = element_ids(&["e1234567890abcdef"]);

        assert_kept_as_sent("e1234567890abcdef:src", LinkSide::From, &elements);
        assert_kept_as_sent("e1234567890abcdef:sink", LinkSide::To, &elements);
    }

    #[test]
    fn gst_launch_imported_element_pads_are_accepted() {
        // gst-launch imported elements have IDs like "videotestsrc_0", "videoconvert_1"
        // This was a bug once - these were rejected because they don't start with 'e'
        let elements = element_ids(&["videotestsrc_0", "videoconvert_1", "fakesink_2"]);

        assert_kept_as_sent("videotestsrc_0:src", LinkSide::From, &elements);
        assert_kept_as_sent("videoconvert_1:sink", LinkSide::To, &elements);
        assert_kept_as_sent("fakesink_2:sink", LinkSide::To, &elements);
    }

    #[test]
    fn user_named_element_pads_are_accepted() {
        // Users can name elements anything, e.g., "mysource", "output"
        let elements = element_ids(&["mysource", "myfilter", "output"]);

        assert_kept_as_sent("mysource:src", LinkSide::From, &elements);
        assert_kept_as_sent("output:sink", LinkSide::To, &elements);
    }

    #[test]
    fn bare_element_reference_is_accepted_and_left_alone() {
        // The documented element-level form: GStreamer picks compatible pads
        // when the pipeline is built, so there is nothing to resolve here.
        let elements = element_ids(&["src0", "caps0"]);

        assert_kept_as_sent("src0", LinkSide::From, &elements);
        assert_kept_as_sent("caps0", LinkSide::To, &elements);
        // The "::" spelling that ElementPadRef::to_string_format() produces.
        assert_kept_as_sent("src0::", LinkSide::From, &elements);
    }

    #[test]
    fn bare_block_reference_resolves_to_its_only_pad_on_that_side() {
        let elements = element_ids(&[]);
        let block_pads = blocks(vec![("b1234", computed_block(&["sink"], &["src"]))]);

        assert_eq!(
            resolve_link_endpoint("b1234", LinkSide::From, &elements, &block_pads)
                .expect("a block with one output resolves"),
            Some("b1234:src".to_string()),
            "a bare block on the source side resolves to its only output pad"
        );
        assert_eq!(
            resolve_link_endpoint("b1234", LinkSide::To, &elements, &block_pads)
                .expect("a block with one input resolves"),
            Some("b1234:sink".to_string()),
            "a bare block on the destination side resolves to its only input pad"
        );
    }

    #[test]
    fn bare_block_reference_with_several_pads_is_rejected_by_name() {
        let elements = element_ids(&[]);
        let block_pads = blocks(vec![(
            "b1234",
            computed_block(&["audio_0", "audio_1"], &["video"]),
        )]);

        let problem = resolve_link_endpoint("b1234", LinkSide::To, &elements, &block_pads)
            .expect_err("an ambiguous bare reference must be rejected");
        assert!(matches!(problem, EndpointProblem::Rejected(_)));
        assert!(
            problem.message().contains("audio_0") && problem.message().contains("audio_1"),
            "the error must name the candidate pads: {}",
            problem.message()
        );
    }

    #[test]
    fn bare_block_reference_uses_static_pads_when_none_were_computed() {
        let elements = element_ids(&[]);
        let block_pads = blocks(vec![("b9999", static_block(&["sink"], &["src"]))]);

        assert_eq!(
            resolve_link_endpoint("b9999", LinkSide::From, &elements, &block_pads).unwrap(),
            Some("b9999:src".to_string())
        );
    }

    #[test]
    fn block_pad_that_was_computed_away_is_stale_not_a_caller_error() {
        let elements = element_ids(&[]);
        let block_pads = blocks(vec![(
            "b1234",
            computed_block(&["audio_in"], &["audio_out"]),
        )]);

        assert_kept_as_sent_with_blocks("b1234:audio_in", LinkSide::To, &elements, &block_pads);
        assert_kept_as_sent_with_blocks("b1234:audio_out", LinkSide::From, &elements, &block_pads);

        let problem =
            resolve_link_endpoint("b1234:nonexistent", LinkSide::To, &elements, &block_pads)
                .expect_err("a pad the block no longer has must be flagged");
        assert!(
            matches!(problem, EndpointProblem::StaleBlockPad(_)),
            "a vanished block pad is drift, so the link is pruned rather than the request rejected"
        );
    }

    #[test]
    fn named_pad_on_a_block_without_computed_pads_is_trusted() {
        let elements = element_ids(&[]);
        let block_pads = blocks(vec![("b9999", static_block(&[], &[]))]);

        assert_kept_as_sent_with_blocks("b9999:any_pad", LinkSide::To, &elements, &block_pads);
    }

    #[test]
    fn unknown_node_is_a_caller_error() {
        let elements = element_ids(&["elem1"]);
        let block_pads = blocks(vec![("b456", computed_block(&["sink"], &["src"]))]);

        for pad_ref in ["nonexistent:src", "nonexistent", ""] {
            let problem = resolve_link_endpoint(pad_ref, LinkSide::From, &elements, &block_pads)
                .expect_err("an unknown node must be rejected");
            assert!(
                matches!(problem, EndpointProblem::Rejected(_)),
                "'{}' names nothing in the flow, so the request must fail",
                pad_ref
            );
        }
    }

    #[test]
    fn mixed_elements_and_blocks() {
        // Realistic scenario with both UI elements and blocks
        let elements = element_ids(&["e123", "videotestsrc_0"]);
        let block_pads = blocks(vec![(
            "b456",
            computed_block(&["audio_in"], &["audio_out"]),
        )]);

        assert_kept_as_sent_with_blocks("e123:src", LinkSide::From, &elements, &block_pads);
        assert_kept_as_sent_with_blocks(
            "videotestsrc_0:src",
            LinkSide::From,
            &elements,
            &block_pads,
        );
        assert_kept_as_sent_with_blocks("b456:audio_in", LinkSide::To, &elements, &block_pads);

        assert!(matches!(
            resolve_link_endpoint("b456:nonexistent", LinkSide::To, &elements, &block_pads),
            Err(EndpointProblem::StaleBlockPad(_))
        ));
        assert!(matches!(
            resolve_link_endpoint("unknown:src", LinkSide::From, &elements, &block_pads),
            Err(EndpointProblem::Rejected(_))
        ));
    }

    fn assert_kept_as_sent_with_blocks(
        pad_ref: &str,
        side: LinkSide,
        elements: &HashSet<String>,
        block_pads: &HashMap<String, BlockPads>,
    ) {
        match resolve_link_endpoint(pad_ref, side, elements, block_pads) {
            Ok(None) => {}
            Ok(Some(other)) => panic!("'{}' should be stored unchanged, got '{}'", pad_ref, other),
            Err(problem) => panic!("'{}' should be accepted: {}", pad_ref, problem.message()),
        }
    }
}
