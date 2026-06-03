//! Vision Mixer control page and helpers.

use axum::{
    extract::{Path, State},
    response::Html,
    Json,
};
use strom_types::{Flow, FlowId};

use crate::blocks::builtin::vision_mixer::overlay;
use crate::blocks::builtin::vision_mixer::properties as vm_props;
use crate::state::AppState;

/// Follow links from a block's output pad through intermediate blocks until
/// a WHEP output block is reached. Returns the WHEP endpoint path
/// (e.g. `/whep/{endpoint_id}`) or an empty string if none is found.
pub fn find_whep_endpoint_for_pad(flow: &Flow, block_id: &str, pad_name: &str) -> String {
    let whep_block_ids: std::collections::HashSet<&str> = flow
        .blocks
        .iter()
        .filter(|b| b.block_definition_id == "builtin.whep_output")
        .map(|b| b.id.as_str())
        .collect();

    let mut current = format!("{}:{}", block_id, pad_name);
    let mut visited = std::collections::HashSet::new();

    while let Some(link) = flow.links.iter().find(|l| l.from == current) {
        let target_block_id = link.to.split(':').next().unwrap_or("");
        if !visited.insert(target_block_id.to_string()) {
            break;
        }
        if whep_block_ids.contains(target_block_id) {
            if let Some(eid) = flow
                .blocks
                .iter()
                .find(|b| b.id == target_block_id)
                .and_then(|b| b.runtime_data.as_ref())
                .and_then(|rd| rd.get("whep_endpoint_id"))
            {
                return format!("/whep/{}", eid);
            }
            break;
        }
        // Follow through: find an output link from this intermediate block
        match flow
            .links
            .iter()
            .find(|l| l.from.starts_with(&format!("{}:", target_block_id)))
        {
            Some(next) => current = next.from.clone(),
            None => break,
        }
    }

    String::new()
}

const VISION_MIXER_HTML: &str = include_str!("../../static/vision-mixer.html");

/// Serve the vision mixer control page (first vision mixer in the flow).
/// GET /player/vision-mixer/{flow_id}
pub async fn vision_mixer_page(
    State(state): State<AppState>,
    Path(flow_id): Path<FlowId>,
) -> Html<String> {
    render_vision_mixer_page(&state, &flow_id, None).await
}

/// Serve the vision mixer control page for a specific block.
/// GET /player/vision-mixer/{flow_id}/{block_id}
pub async fn vision_mixer_page_for_block(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Html<String> {
    render_vision_mixer_page(&state, &flow_id, Some(&block_id)).await
}

async fn render_vision_mixer_page(
    state: &AppState,
    flow_id: &FlowId,
    requested_block_id: Option<&str>,
) -> Html<String> {
    let flows = state.get_flows().await;

    let Some(flow) = flows.iter().find(|f| f.id == *flow_id) else {
        return Html(format!(
            "<html><body>Flow {} not found</body></html>",
            flow_id
        ));
    };

    let vm_block = match requested_block_id {
        Some(bid) => flow
            .blocks
            .iter()
            .find(|b| b.id == bid && b.block_definition_id == "builtin.vision_mixer"),
        None => flow
            .blocks
            .iter()
            .find(|b| b.block_definition_id == "builtin.vision_mixer"),
    };

    let Some(vm_block) = vm_block else {
        return Html(
            "<html><body>No vision mixer block found in this flow</body></html>".to_string(),
        );
    };

    let block_id = &vm_block.id;
    let num_inputs = vm_props::parse_num_inputs(&vm_block.properties);
    let labels = vm_props::parse_input_labels(&vm_block.properties, num_inputs);
    let num_dsk_inputs = vm_props::parse_num_dsk_inputs(&vm_block.properties);
    let num_pips = vm_props::parse_num_pips(&vm_block.properties);
    let (pgm_w, pgm_h) = vm_props::parse_resolution(
        &vm_block.properties,
        "pgm_resolution",
        strom_types::vision_mixer::DEFAULT_PGM_RESOLUTION,
    );

    // Get current state from live overlay state or fall back to defaults.
    // `None` means the bus is showing a PiP (see `pvw_pip` / `pgm_pip` below).
    let overlay = overlay::get_overlay_state(block_id);
    let initial_pgm: Option<usize> = overlay.as_ref().map(|s| s.pgm_input()).unwrap_or_else(|| {
        Some(vm_props::parse_initial_pgm(
            &vm_block.properties,
            num_inputs,
        ))
    });
    let initial_pvw: Option<usize> = overlay.as_ref().map(|s| s.pvw_input()).unwrap_or_else(|| {
        Some(vm_props::parse_initial_pvw(
            &vm_block.properties,
            num_inputs,
        ))
    });
    let ftb_active = overlay
        .as_ref()
        .map(|s| s.ftb_active.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    let dsk_states: Vec<bool> = overlay
        .as_ref()
        .map(|s| {
            s.dsk_enabled
                .iter()
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .collect()
        })
        .unwrap_or_else(|| vec![true; num_dsk_inputs]);
    let overlay_alpha = overlay.as_ref().map(|s| s.overlay_alpha()).unwrap_or(1.0);

    // Per-PiP runtime state (bg + zones). Fallback when the pipeline isn't
    // built yet: bg comes from the block property; zones are runtime-only and
    // therefore empty until the operator configures them.
    let pips: Vec<serde_json::Value> = (0..num_pips)
        .map(|i| {
            let (bg, zones, transforms) = if let Some(s) = overlay.as_ref() {
                (s.pip_bg_input(i), s.pip_zones(i), s.pip_transforms(i))
            } else {
                (
                    vm_props::parse_pip_bg(&vm_block.properties, i, num_inputs),
                    Vec::<strom_types::vision_mixer::Zone>::new(),
                    strom_types::vision_mixer::PipTransforms::new(),
                )
            };
            serde_json::json!({
                "bg": bg,
                "zones": zones,
                "transforms": transforms,
            })
        })
        .collect();
    let pvw_pip: Option<usize> = overlay.as_ref().and_then(|s| s.pvw_pip());
    let pgm_pip: Option<usize> = overlay.as_ref().and_then(|s| s.pgm_pip());

    // Negotiated per-input resolutions (None until caps arrive). The crop
    // editor needs the real source aspect — inputs are NOT normalized to the
    // PGM resolution/aspect.
    let input_resolutions = state
        .vision_mixer_input_resolutions(flow_id, block_id, num_inputs)
        .await;

    // Build a single JSON config object (safe injection via <script type="application/json">)
    let config = serde_json::json!({
        "flow_id": flow_id.to_string(),
        "block_id": block_id,
        "num_inputs": num_inputs,
        "input_labels": labels,
        "initial_pgm": initial_pgm,
        "initial_pvw": initial_pvw,
        "num_dsk_inputs": num_dsk_inputs,
        "ftb_active": ftb_active,
        "dsk_states": dsk_states,
        "overlay_alpha": overlay_alpha,
        "num_pips": num_pips,
        "pips": pips,
        "pvw_pip": pvw_pip,
        "pgm_pip": pgm_pip,
        "pgm_w": pgm_w,
        "pgm_h": pgm_h,
        "input_resolutions": input_resolutions,
    });

    let html = VISION_MIXER_HTML.replace("{{VM_CONFIG_JSON}}", &config.to_string());

    Html(html)
}

/// Get the multiview WHEP endpoint for a vision mixer block.
/// GET /api/flows/{flow_id}/blocks/{block_id}/multiview-endpoint
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/multiview-endpoint",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block ID"),
    ),
    responses(
        (status = 200, description = "Multiview endpoint info", body = strom_types::api::MultiviewEndpointResponse),
        (status = 404, description = "Flow or block not found"),
    ),
    tag = "vision-mixer"
)]
pub async fn get_multiview_endpoint(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<strom_types::api::MultiviewEndpointResponse>, axum::http::StatusCode> {
    let flows = state.get_flows().await;
    let flow = flows
        .iter()
        .find(|f| f.id == flow_id)
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;

    // Verify the block exists and is a vision mixer
    flow.blocks
        .iter()
        .find(|b| b.id == block_id && b.block_definition_id == "builtin.vision_mixer")
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;

    let endpoint = find_whep_endpoint_for_pad(flow, &block_id, "multiview_out");

    Ok(Json(strom_types::api::MultiviewEndpointResponse {
        endpoint,
    }))
}
