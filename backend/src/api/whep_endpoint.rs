//! The WHEP endpoint fed by a block's output pad: the search through a flow
//! and the generic `whep-endpoint` API. The vision mixer's `pgm-endpoint`
//! and `multiview-endpoint` use the same search.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use strom_types::api::WhepEndpointResponse;
use strom_types::{Flow, FlowId};

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
/// Browser pages should call the `whep-endpoint` API (or a vision mixer's
/// `pgm-endpoint` / `multiview-endpoint`) rather than walk the flow
/// themselves.
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

/// The WHEP endpoint fed by `pad` of `block_id` in a flow, or `None` if the
/// flow has no such block or element. `block_type`, when given, is the block
/// type the block must have.
pub(crate) fn endpoint_in_flow(
    flow: &Flow,
    block_id: &str,
    block_type: Option<&str>,
    pad: &str,
) -> Option<WhepEndpointResponse> {
    let found = match block_type {
        Some(t) => flow
            .blocks
            .iter()
            .any(|b| b.id == block_id && b.block_definition_id == t),
        None => {
            flow.blocks.iter().any(|b| b.id == block_id)
                || flow.elements.iter().any(|e| e.id == block_id)
        }
    };
    found.then(|| WhepEndpointResponse {
        endpoint: find_whep_endpoint_for_pad(flow, block_id, pad),
    })
}

/// Look the flow up and answer with the WHEP endpoint fed by `pad` of
/// `block_id`; 404 when the flow or block is unknown.
pub(crate) async fn endpoint_response(
    state: &AppState,
    flow_id: &FlowId,
    block_id: &str,
    block_type: Option<&str>,
    pad: &str,
) -> Result<Json<WhepEndpointResponse>, StatusCode> {
    let flows = state.get_flows().await;
    let flow = flows
        .iter()
        .find(|f| f.id == *flow_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    endpoint_in_flow(flow, block_id, block_type, pad)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Query of `whep-endpoint`.
#[derive(Debug, Deserialize)]
pub struct WhepEndpointQuery {
    /// Output pad of the block to search from (e.g. `video_out`).
    pub pad: String,
}

/// Get the WHEP endpoint fed by an output pad of any block or element.
///
/// The nearest WHEP Output downstream, followed through blocks that pass the
/// picture on (an encoder, a tee). The search stops at blocks that combine
/// inputs (vision mixer, compositor, audio mixer). The endpoint is empty when
/// there is none or the nearest WHEP Output is not started.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/whep-endpoint",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block or element ID"),
        ("pad" = String, Query, description = "Output pad of the block to search from (e.g. `video_out`)"),
    ),
    responses(
        (status = 200, description = "WHEP endpoint info", body = WhepEndpointResponse),
        (status = 400, description = "Missing `pad` query parameter"),
        (status = 404, description = "Flow or block not found"),
    ),
    tag = "whep"
)]
pub async fn get_whep_endpoint(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Query(query): Query<WhepEndpointQuery>,
) -> Result<Json<WhepEndpointResponse>, StatusCode> {
    endpoint_response(&state, &flow_id, &block_id, None, &query.pad).await
}

#[cfg(test)]
mod tests {
    use super::{endpoint_in_flow, find_whep_endpoint_for_pad};
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

    /// A replay player feeding a WHEP Output directly.
    #[test]
    fn finds_whep_output_directly_after_a_player() {
        let f = flow(
            &[
                ("p", "builtin.media_player", None),
                ("whep", "builtin.whep_output", Some("cam1")),
            ],
            &[("p:video_out", "whep:video_in")],
        );
        let r = endpoint_in_flow(&f, "p", None, "video_out").unwrap();
        assert_eq!(r.endpoint, "/whep/cam1");
    }

    #[test]
    fn finds_whep_output_behind_a_non_combining_block() {
        let f = flow(
            &[
                ("p", "builtin.media_player", None),
                ("enc", "builtin.videoenc", None),
                ("whep", "builtin.whep_output", Some("cam1")),
            ],
            &[
                ("p:video_out", "enc:video_in"),
                ("enc:out", "whep:video_in"),
            ],
        );
        let r = endpoint_in_flow(&f, "p", None, "video_out").unwrap();
        assert_eq!(r.endpoint, "/whep/cam1");
    }

    #[test]
    fn player_search_stops_at_a_compositor() {
        let f = flow(
            &[
                ("p", "builtin.media_player", None),
                ("comp", "builtin.compositor", None),
                ("whep", "builtin.whep_output", Some("comp")),
            ],
            &[
                ("p:video_out", "comp:video_in_0"),
                ("comp:video_out", "whep:video_in"),
            ],
        );
        let r = endpoint_in_flow(&f, "p", None, "video_out").unwrap();
        assert_eq!(r.endpoint, "");
    }

    #[test]
    fn player_endpoint_empty_when_whep_output_not_started() {
        let f = flow(
            &[
                ("p", "builtin.media_player", None),
                ("whep", "builtin.whep_output", None),
            ],
            &[("p:video_out", "whep:video_in")],
        );
        let r = endpoint_in_flow(&f, "p", None, "video_out").unwrap();
        assert_eq!(r.endpoint, "");
    }

    #[test]
    fn unknown_or_wrong_type_block_is_none() {
        let f = flow(
            &[
                ("p", "builtin.media_player", None),
                ("whep", "builtin.whep_output", Some("cam1")),
            ],
            &[("p:video_out", "whep:video_in")],
        );
        assert!(endpoint_in_flow(&f, "nope", None, "video_out").is_none());
        assert!(endpoint_in_flow(&f, "p", Some("builtin.vision_mixer"), "pgm_out").is_none());
    }
}
