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

/// The nearest WHEP output downstream of a block's output pad. Returns the
/// WHEP endpoint path (e.g. `/whep/{endpoint_id}`) or an empty string if none
/// is found.
///
/// Searches breadth first through every link of a pad and every output of the
/// blocks on the way, so a pad linked twice (a tee) or an intermediate block
/// with several outputs cannot hide the endpoint. Stops at another vision
/// mixer, whose outputs no longer carry this pad's picture. A page that
/// resolves WHEP endpoints in the browser should walk the flow the same way.
pub fn find_whep_endpoint_for_pad(flow: &Flow, block_id: &str, pad_name: &str) -> String {
    use std::collections::{HashSet, VecDeque};

    let block_of = |id: &str| flow.blocks.iter().find(|b| b.id == id);
    let outputs_of = |blk: &str| {
        let prefix = format!("{}:", blk);
        let mut pads: Vec<&str> = Vec::new();
        for l in &flow.links {
            if l.from.starts_with(&prefix) && !pads.contains(&l.from.as_str()) {
                pads.push(l.from.as_str());
            }
        }
        pads
    };

    let start = format!("{}:{}", block_id, pad_name);
    let mut queue: VecDeque<&str> = VecDeque::from([start.as_str()]);
    let mut seen: HashSet<&str> = HashSet::from([block_id]);

    while let Some(pad) = queue.pop_front() {
        for link in flow.links.iter().filter(|l| l.from == pad) {
            let target = link.to.split(':').next().unwrap_or("");
            if !seen.insert(target) {
                continue;
            }
            match block_of(target).map(|b| b.block_definition_id.as_str()) {
                Some("builtin.whep_output") => {
                    if let Some(eid) = block_of(target)
                        .and_then(|b| b.runtime_data.as_ref())
                        .and_then(|rd| rd.get("whep_endpoint_id"))
                    {
                        return format!("/whep/{}", eid);
                    }
                }
                Some("builtin.vision_mixer") => {}
                _ => queue.extend(outputs_of(target)),
            }
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
    let overlay = overlay::get_overlay_state(flow_id, block_id);
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
        "stinger_enabled": vm_props::parse_bool(
            &vm_block.properties,
            strom_types::stinger::ENABLE_STINGER_PROPERTY,
            false,
        ),
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

/// Get the WHEP endpoint fed by a vision mixer's PGM output (followed through
/// intermediate blocks such as an encoder).
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/pgm-endpoint",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block ID"),
    ),
    responses(
        (status = 200, description = "PGM endpoint info", body = strom_types::api::MultiviewEndpointResponse),
        (status = 404, description = "Flow or block not found"),
    ),
    tag = "vision-mixer"
)]
pub async fn get_pgm_endpoint(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<strom_types::api::MultiviewEndpointResponse>, axum::http::StatusCode> {
    let flows = state.get_flows().await;
    let flow = flows
        .iter()
        .find(|f| f.id == flow_id)
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;
    flow.blocks
        .iter()
        .find(|b| b.id == block_id && b.block_definition_id == "builtin.vision_mixer")
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;
    let endpoint = find_whep_endpoint_for_pad(flow, &block_id, "pgm_out");
    Ok(Json(strom_types::api::MultiviewEndpointResponse {
        endpoint,
    }))
}

#[cfg(test)]
mod tests {
    use super::find_whep_endpoint_for_pad;
    use strom_types::Flow;

    fn flow(blocks: &[(&str, &str, Option<&str>)], links: &[(&str, &str)]) -> Flow {
        let mut flow = Flow::new("test");
        flow.blocks = blocks
            .iter()
            .map(|(id, def, eid)| {
                serde_json::from_value(serde_json::json!({
                    "id": id,
                    "block_definition_id": def,
                    "properties": {},
                    "position": {"x": 0.0, "y": 0.0},
                    "runtime_data": eid.map(|e| serde_json::json!({"whep_endpoint_id": e})),
                }))
                .unwrap()
            })
            .collect();
        flow.links = links
            .iter()
            .map(|(from, to)| strom_types::element::Link {
                from: (*from).to_string(),
                to: (*to).to_string(),
            })
            .collect();
        flow
    }

    #[test]
    fn follows_second_link_of_a_pad() {
        let f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("rec", "builtin.mpegtssrt_output", None),
                ("whep", "builtin.whep_output", Some("pgm")),
            ],
            &[
                ("vm:pgm_out", "rec:video_in"),
                ("vm:pgm_out", "whep:video_in"),
            ],
        );
        assert_eq!(find_whep_endpoint_for_pad(&f, "vm", "pgm_out"), "/whep/pgm");
    }

    #[test]
    fn follows_second_output_of_an_intermediate_block() {
        let f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("enc", "builtin.videoenc", None),
                ("sink", "builtin.mpegtssrt_output", None),
                ("whep", "builtin.whep_output", Some("pgm")),
            ],
            &[
                ("vm:pgm_out", "enc:video_in"),
                ("enc:out_a", "sink:video_in"),
                ("enc:out_b", "whep:video_in"),
            ],
        );
        assert_eq!(find_whep_endpoint_for_pad(&f, "vm", "pgm_out"), "/whep/pgm");
    }

    #[test]
    fn stops_at_another_vision_mixer_and_on_cycles() {
        let f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("a", "builtin.videoenc", None),
                ("b", "builtin.videoenc", None),
                ("vm2", "builtin.vision_mixer", None),
                ("whep", "builtin.whep_output", Some("other")),
            ],
            &[
                ("vm:pgm_out", "a:in"),
                ("a:out", "b:in"),
                ("b:out", "a:in"),
                ("b:out2", "vm2:video_in_0"),
                ("vm2:pgm_out", "whep:video_in"),
            ],
        );
        assert_eq!(find_whep_endpoint_for_pad(&f, "vm", "pgm_out"), "");
    }

    #[test]
    fn nearest_endpoint_wins() {
        let f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("enc", "builtin.videoenc", None),
                ("far", "builtin.whep_output", Some("far")),
                ("near", "builtin.whep_output", Some("near")),
            ],
            &[
                ("vm:pgm_out", "enc:in"),
                ("enc:out", "far:video_in"),
                ("vm:pgm_out", "near:video_in"),
            ],
        );
        assert_eq!(
            find_whep_endpoint_for_pad(&f, "vm", "pgm_out"),
            "/whep/near"
        );
    }
}
