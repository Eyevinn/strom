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

/// Block types that combine several inputs into one picture or sound. Their
/// outputs no longer carry the picture of the pad being searched from, so the
/// WHEP search stops at them.
const COMBINING_BLOCKS: &[&str] = &[
    "builtin.vision_mixer",
    "builtin.compositor",
    "builtin.mixer",
    "builtin.audiorouter",
    "builtin.liveaudiorouter",
];

/// GStreamer elements that combine several inputs (see [`COMBINING_BLOCKS`]).
const COMBINING_ELEMENTS: &[&str] = &[
    "compositor",
    "glvideomixer",
    "glvideomixerelement",
    "videomixer",
    "audiomixer",
    "liveadder",
    "adder",
];

/// Whether an output pad is the counterpart of an input pad on a block with
/// several inputs: same trailing index (`in_1` / `out_1`), or the same name
/// with `in` replaced by `out` (`video_in` / `video_out`).
fn output_matches_input(input: &str, output: &str) -> bool {
    let index = |s: &str| {
        let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        (digits > 0).then(|| s[s.len() - digits..].to_string())
    };
    match (index(input), index(output)) {
        (Some(a), Some(b)) => a == b,
        _ => input.replace("in", "out") == output,
    }
}

/// The nearest WHEP output downstream of a block's output pad. Returns the
/// WHEP endpoint path (e.g. `/whep/{endpoint_id}`), or an empty string if
/// none is found or the nearest WHEP output has no endpoint yet.
///
/// Searches breadth first, so a pad linked twice (a tee) or an intermediate
/// block with several outputs cannot hide the endpoint. The rule for which
/// blocks the picture passes through:
///
/// - A block or element that combines inputs (vision mixer, compositor,
///   audio mixer or router) stops the search: its outputs show something else.
/// - A block with one linked input passes it to all its outputs (an encoder,
///   a tee).
/// - A block with several linked inputs passes the input reached to the
///   outputs that match it by index or name (`in_1` -> `out_1`,
///   `video_in` -> `video_out`). If none match (a muxer with one output),
///   it passes to all outputs.
/// - The nearest WHEP output is the answer. If it has no endpoint id yet (not
///   started), the result is empty rather than a farther WHEP output, so a
///   client does not lock on to the wrong stream.
///
/// Browser pages should call the `pgm-endpoint` / `multiview-endpoint` API
/// rather than walk the flow themselves.
pub fn find_whep_endpoint_for_pad(flow: &Flow, block_id: &str, pad_name: &str) -> String {
    use std::collections::{HashSet, VecDeque};

    let block_of = |id: &str| flow.blocks.iter().find(|b| b.id == id);
    let is_combining = |id: &str| match block_of(id) {
        Some(b) => COMBINING_BLOCKS.contains(&b.block_definition_id.as_str()),
        None => flow
            .elements
            .iter()
            .any(|e| e.id == id && COMBINING_ELEMENTS.contains(&e.element_type.as_str())),
    };
    let pads_of = |blk: &str, outputs: bool| {
        let prefix = format!("{}:", blk);
        let mut pads: Vec<&str> = Vec::new();
        for l in &flow.links {
            let pad = if outputs { &l.from } else { &l.to };
            if pad.starts_with(&prefix) && !pads.contains(&pad.as_str()) {
                pads.push(pad.as_str());
            }
        }
        pads
    };
    let pad_name_of = |pad: &str| pad.split_once(':').map_or("", |(_, p)| p).to_string();

    let start = format!("{}:{}", block_id, pad_name);
    let mut queue: VecDeque<&str> = VecDeque::from([start.as_str()]);
    // Input pads already reached. Keyed per pad, not per block, so a block
    // reached on two inputs passes on both.
    let mut seen: HashSet<&str> = HashSet::new();

    while let Some(pad) = queue.pop_front() {
        for link in flow.links.iter().filter(|l| l.from == pad) {
            let target = link.to.split(':').next().unwrap_or("");
            if target == block_id || !seen.insert(link.to.as_str()) {
                continue;
            }
            if let Some(b) = block_of(target) {
                if b.block_definition_id == "builtin.whep_output" {
                    return b
                        .runtime_data
                        .as_ref()
                        .and_then(|rd| rd.get("whep_endpoint_id"))
                        .map(|eid| format!("/whep/{}", eid))
                        .unwrap_or_default();
                }
            }
            if is_combining(target) {
                continue;
            }
            let outputs = pads_of(target, true);
            if pads_of(target, false).len() > 1 {
                let input = pad_name_of(&link.to);
                let matching: Vec<&str> = outputs
                    .iter()
                    .copied()
                    .filter(|o| output_matches_input(&input, &pad_name_of(o)))
                    .collect();
                if !matching.is_empty() {
                    queue.extend(matching);
                    continue;
                }
            }
            queue.extend(outputs);
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
        (status = 200, description = "Multiview endpoint info", body = strom_types::api::WhepEndpointResponse),
        (status = 404, description = "Flow or block not found"),
    ),
    tag = "vision-mixer"
)]
pub async fn get_multiview_endpoint(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<strom_types::api::WhepEndpointResponse>, axum::http::StatusCode> {
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

    Ok(Json(strom_types::api::WhepEndpointResponse { endpoint }))
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
        (status = 200, description = "PGM endpoint info", body = strom_types::api::WhepEndpointResponse),
        (status = 404, description = "Flow or block not found"),
    ),
    tag = "vision-mixer"
)]
pub async fn get_pgm_endpoint(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<strom_types::api::WhepEndpointResponse>, axum::http::StatusCode> {
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
    Ok(Json(strom_types::api::WhepEndpointResponse { endpoint }))
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

    #[test]
    fn stops_at_compositing_blocks_and_elements() {
        // PGM also feeds a compositor (PiP on a monitor) and an audio mixer;
        // their outputs are not PGM even though they are reached first.
        let mut f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("comp", "builtin.compositor", None),
                ("mon", "builtin.whep_output", Some("mon")),
                ("enc", "builtin.videoenc", None),
                ("pgm", "builtin.whep_output", Some("pgm")),
            ],
            &[
                ("vm:pgm_out", "comp:video_in_1"),
                ("vm:pgm_out", "gst_mix:sink_0"),
                ("comp:video_out", "mon:video_in"),
                ("gst_mix:src", "mon:video_in"),
                ("vm:pgm_out", "enc:video_in"),
                ("enc:video_out", "pgm:video_in"),
            ],
        );
        f.elements.push(
            serde_json::from_value(serde_json::json!({
                "id": "gst_mix",
                "element_type": "compositor",
                "position": [0.0, 0.0],
            }))
            .unwrap(),
        );
        assert_eq!(find_whep_endpoint_for_pad(&f, "vm", "pgm_out"), "/whep/pgm");
    }

    #[test]
    fn follows_the_output_matching_the_input_reached() {
        let f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("x", "builtin.videoformat", None),
                ("pgm", "builtin.whep_output", Some("pgm")),
                ("mv", "builtin.whep_output", Some("mv")),
            ],
            &[
                ("vm:pgm_out", "x:in_0"),
                ("vm:multiview_out", "x:in_1"),
                ("x:out_0", "pgm:video_in"),
                ("x:out_1", "mv:video_in"),
            ],
        );
        assert_eq!(
            find_whep_endpoint_for_pad(&f, "vm", "multiview_out"),
            "/whep/mv"
        );
        assert_eq!(find_whep_endpoint_for_pad(&f, "vm", "pgm_out"), "/whep/pgm");
    }

    #[test]
    fn multi_input_block_without_matching_output_passes_to_all() {
        // A muxer: video and audio in, one output.
        let f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("aud", "builtin.audioenc", None),
                ("mux", "builtin.videoenc", None),
                ("pgm", "builtin.whep_output", Some("pgm")),
            ],
            &[
                ("vm:pgm_out", "mux:video_in"),
                ("aud:out", "mux:audio_in"),
                ("mux:out", "pgm:video_in"),
            ],
        );
        assert_eq!(find_whep_endpoint_for_pad(&f, "vm", "pgm_out"), "/whep/pgm");
    }

    #[test]
    fn nearest_endpoint_not_ready_gives_empty() {
        let f = flow(
            &[
                ("vm", "builtin.vision_mixer", None),
                ("enc", "builtin.videoenc", None),
                ("far", "builtin.whep_output", Some("far")),
                ("near", "builtin.whep_output", None),
            ],
            &[
                ("vm:pgm_out", "enc:in"),
                ("enc:out", "far:video_in"),
                ("vm:pgm_out", "near:video_in"),
            ],
        );
        assert_eq!(find_whep_endpoint_for_pad(&f, "vm", "pgm_out"), "");
    }
}
