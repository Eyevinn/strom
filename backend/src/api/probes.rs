//! Buffer age probe API endpoints.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use gstreamer as gst;
use strom_types::api::{ActivateProbeRequest, ActiveProbesResponse, ErrorResponse, ProbeResponse};
use strom_types::FlowId;
use tracing::info;

use crate::state::AppState;

/// Activate a buffer age probe on a pad.
#[utoipa::path(
    post,
    path = "/api/flows/{id}/probes",
    params(("id" = String, Path, description = "Flow ID")),
    request_body = ActivateProbeRequest,
    responses(
        (status = 200, description = "Probe activated", body = ProbeResponse),
        (status = 404, description = "Flow not found or not running"),
        (status = 400, description = "Invalid request"),
    ),
    tag = "probes"
)]
pub async fn activate_probe(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ActivateProbeRequest>,
) -> impl IntoResponse {
    let flow_id: FlowId = match id.parse::<uuid::Uuid>() {
        Ok(uuid) => FlowId::from(uuid),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!(ErrorResponse::new("Invalid flow ID"))),
            )
                .into_response();
        }
    };

    let sample_interval = req.sample_interval.unwrap_or(1);
    let timeout_secs = req.timeout_secs.unwrap_or(60);

    // Resolve block input pads BEFORE taking the pipeline lock.
    // For blocks: look up external input pads -> internal element IDs to probe,
    // along with each external pad's display label (so events can distinguish
    // inputs that share the same internal "sink" pad name).
    // For standalone elements: empty list (handled below with find_gst_element).
    let block_input_element_ids: Vec<(String, String)> = {
        let flows = state.get_flows().await;
        let block_def_id = flows
            .iter()
            .find(|f| f.id == flow_id)
            .and_then(|f| f.blocks.iter().find(|b| b.id == req.element_id))
            .map(|b| {
                // Use computed_external_pads if available
                let inputs = b.computed_external_pads.as_ref().map(|p| {
                    p.inputs
                        .iter()
                        .map(|pad| {
                            let element_key =
                                format!("{}:{}", req.element_id, pad.internal_element_id);
                            let display = pad.label.clone().unwrap_or_else(|| pad.name.clone());
                            (element_key, display)
                        })
                        .collect::<Vec<_>>()
                });
                (b.block_definition_id.clone(), inputs)
            });

        if let Some((def_id, computed)) = block_def_id {
            if let Some(ids) = computed {
                ids
            } else {
                // Fallback to block definition
                let def = state.blocks().get_by_id(&def_id).await;
                def.map(|d| {
                    d.external_pads
                        .inputs
                        .iter()
                        .map(|pad| {
                            let element_key =
                                format!("{}:{}", req.element_id, pad.internal_element_id);
                            let display = pad.label.clone().unwrap_or_else(|| pad.name.clone());
                            (element_key, display)
                        })
                        .collect()
                })
                .unwrap_or_default()
            }
        } else {
            Vec::new()
        }
    };

    let pipelines = state.pipelines_read().await;
    let manager = match pipelines.get(&flow_id) {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ErrorResponse::new(
                    "Flow not found or not running"
                ))),
            )
                .into_response();
        }
    };

    let pipeline = manager.pipeline().clone();

    // Collect (element, display_pad_name) pairs to probe (clone elements to
    // release borrow on manager). For standalone the display name is None so
    // each pad is labeled with its own GStreamer pad name; for blocks it is
    // the external pad's label so the UI can tell sibling inputs apart.
    let elements_to_probe: Vec<(gst::Element, Option<String>)> =
        if block_input_element_ids.is_empty() {
            // Standalone element
            match manager.find_gst_element(&req.element_id) {
                Some(el) => vec![(el.clone(), None)],
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!(ErrorResponse::new(format!(
                            "Element or block '{}' not found",
                            req.element_id
                        )))),
                    )
                        .into_response();
                }
            }
        } else {
            // Block: collect internal elements for each input port, paired
            // with the external pad's display label.
            block_input_element_ids
                .iter()
                .filter_map(|(id, label)| {
                    manager
                        .find_gst_element(id)
                        .map(|el| (el.clone(), Some(label.clone())))
                })
                .collect()
        };

    let probe_manager = manager.probe_manager();
    let mut all_probe_ids = Vec::new();
    let mut last_error = None;

    for (el, display_pad_name) in &elements_to_probe {
        match probe_manager.activate_all_sinks(
            &pipeline,
            el,
            req.element_id.clone(),
            display_pad_name.clone(),
            sample_interval,
            timeout_secs,
        ) {
            Ok(ids) => all_probe_ids.extend(ids),
            Err(e) => last_error = Some(e),
        }
    }

    if all_probe_ids.is_empty() {
        let err = last_error.unwrap_or_else(|| "No probes could be activated".to_string());
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!(ErrorResponse::new(err))),
        )
            .into_response()
    } else {
        info!(
            flow_id = %flow_id,
            element_id = %req.element_id,
            count = all_probe_ids.len(),
            "Buffer age probes activated"
        );
        let probe_id = all_probe_ids.into_iter().next().unwrap_or_default();
        Json(serde_json::json!(ProbeResponse { probe_id })).into_response()
    }
}

/// List active probes on a flow.
#[utoipa::path(
    get,
    path = "/api/flows/{id}/probes",
    params(("id" = String, Path, description = "Flow ID")),
    responses(
        (status = 200, description = "Active probes", body = ActiveProbesResponse),
        (status = 404, description = "Flow not found or not running"),
    ),
    tag = "probes"
)]
pub async fn list_probes(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let flow_id: FlowId = match id.parse::<uuid::Uuid>() {
        Ok(uuid) => FlowId::from(uuid),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!(ErrorResponse::new("Invalid flow ID"))),
            )
                .into_response();
        }
    };

    let pipelines = state.pipelines_read().await;
    let manager = match pipelines.get(&flow_id) {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ErrorResponse::new(
                    "Flow not found or not running"
                ))),
            )
                .into_response();
        }
    };

    let probes = manager.probe_manager().list();

    Json(serde_json::json!(ActiveProbesResponse { probes })).into_response()
}

/// Deactivate a probe.
#[utoipa::path(
    delete,
    path = "/api/flows/{id}/probes/{probe_id}",
    params(
        ("id" = String, Path, description = "Flow ID"),
        ("probe_id" = String, Path, description = "Probe ID"),
    ),
    responses(
        (status = 200, description = "Probe deactivated"),
        (status = 404, description = "Flow or probe not found"),
    ),
    tag = "probes"
)]
pub async fn deactivate_probe(
    State(state): State<AppState>,
    Path((id, probe_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let flow_id: FlowId = match id.parse::<uuid::Uuid>() {
        Ok(uuid) => FlowId::from(uuid),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!(ErrorResponse::new("Invalid flow ID"))),
            )
                .into_response();
        }
    };

    let pipelines = state.pipelines_read().await;
    let manager = match pipelines.get(&flow_id) {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!(ErrorResponse::new(
                    "Flow not found or not running"
                ))),
            )
                .into_response();
        }
    };

    match manager.probe_manager().deactivate(&probe_id) {
        Ok(()) => {
            info!(flow_id = %flow_id, probe_id = %probe_id, "Buffer age probe deactivated");
            Json(serde_json::json!({"message": "Probe deactivated"})).into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!(ErrorResponse::new(e))),
        )
            .into_response(),
    }
}
