//! Vision mixer input labels that follow the camera on the input.
//!
//! A mixer input fed by a camera-bound SRT input is labelled with the
//! camera's display name, and the label follows when the name changes. An
//! operator's own label is left alone: a label is only replaced when it is
//! empty, the mixer's default ("In N"), or a name the camera had before.

use std::collections::{HashMap, HashSet};

use strom_types::{Flow, PropertyValue};

use super::tally::upstream_of;
use super::{camera_of, SRT_INPUT_BLOCK};
use crate::blocks::builtin::vision_mixer::properties::{default_input_label, parse_num_inputs};

const VISION_MIXER: &str = "builtin.vision_mixer";

/// How many earlier names of a camera are remembered.
const NAME_HISTORY: usize = 8;

/// A vision mixer input fed by exactly one camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInput {
    pub mixer: String,
    pub input: usize,
    pub device: String,
}

/// The `input_N_label` property of input `input`.
pub fn label_property(input: usize) -> String {
    format!("input_{input}_label")
}

/// The camera-bound SRT inputs of `flow`: block id to device id.
pub fn camera_blocks(flow: &Flow) -> HashMap<String, String> {
    flow.blocks
        .iter()
        .filter(|b| b.block_definition_id == SRT_INPUT_BLOCK)
        .filter_map(|b| camera_of(&b.properties).map(|d| (b.id.clone(), d)))
        .collect()
}

/// Every vision mixer input of `flow` whose upstream (through the flow's
/// links, not through another mixer) holds exactly one camera.
pub fn camera_inputs(flow: &Flow) -> Vec<CameraInput> {
    let cameras = camera_blocks(flow);
    if cameras.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for mixer in flow
        .blocks
        .iter()
        .filter(|b| b.block_definition_id == VISION_MIXER)
    {
        for input in 0..parse_num_inputs(&mixer.properties) {
            let devices: HashSet<&String> =
                upstream_of(flow, &format!("{}:video_in_{input}", mixer.id))
                    .iter()
                    .filter_map(|block| cameras.get(block))
                    .collect();
            if let [device] = devices.into_iter().collect::<Vec<_>>()[..] {
                found.push(CameraInput {
                    mixer: mixer.id.clone(),
                    input,
                    device: device.clone(),
                });
            }
        }
    }
    found
}

/// Whether input `input`'s label (`current`) may be replaced by the camera's
/// name `name`: it is unset, empty, the default "In N", or one of the
/// camera's `previous` names. Not when it already is `name`.
pub fn should_relabel(
    current: Option<&PropertyValue>,
    input: usize,
    name: &str,
    previous: &[String],
) -> bool {
    let current = match current {
        None => return true,
        Some(PropertyValue::String(s)) => s.as_str(),
        Some(_) => return false,
    };
    if current == name {
        return false;
    }
    current.trim().is_empty()
        || current == default_input_label(input)
        || previous.iter().any(|p| p == current)
}

/// The names each camera has had in this run, to recognise a label that is
/// only a camera's earlier name.
#[derive(Debug, Default)]
pub struct NameHistory {
    names: HashMap<String, Vec<String>>,
}

impl NameHistory {
    /// Record `name` as `device`'s current name. Returns whether it is new
    /// (a camera first seen, or a changed name).
    pub fn note(&mut self, device: &str, name: &str) -> bool {
        let names = self.names.entry(device.to_string()).or_default();
        if names.last().map(String::as_str) == Some(name) {
            return false;
        }
        names.retain(|n| n != name);
        names.push(name.to_string());
        if names.len() > NAME_HISTORY {
            names.remove(0);
        }
        true
    }

    /// The names `device` had before its current one.
    pub fn previous(&self, device: &str) -> Vec<String> {
        self.names
            .get(device)
            .map(|n| n[..n.len().saturating_sub(1)].to_vec())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::element::Link;
    use strom_types::BlockInstance;

    fn block(id: &str, def: &str, props: serde_json::Value) -> BlockInstance {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "block_definition_id": def,
            "properties": props,
            "position": {"x": 0.0, "y": 0.0},
        }))
        .unwrap()
    }

    fn link(from: &str, to: &str) -> Link {
        Link {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    /// cam_a -> mixer 0; cam_b -> scaler -> mixer 1; plain SRT -> mixer 2;
    /// cam_a and cam_b -> switch -> mixer 3 (two cameras: no label);
    /// cam_c -> inner mixer -> mixer 4 (not followed).
    fn flow() -> Flow {
        let srt = SRT_INPUT_BLOCK;
        let mut flow = Flow::new("labels");
        flow.blocks = vec![
            block("cam_a", srt, serde_json::json!({"camera": "phone-a"})),
            block("cam_b", srt, serde_json::json!({"camera": "phone-b"})),
            block("cam_c", srt, serde_json::json!({"camera": "phone-c"})),
            block("plain", srt, serde_json::json!({"camera": ""})),
            block("scaler", "builtin.videoscale", serde_json::json!({})),
            block("switch", "builtin.video_switch", serde_json::json!({})),
            block("inner", VISION_MIXER, serde_json::json!({})),
            block("mixer", VISION_MIXER, serde_json::json!({"num_inputs": 5})),
        ];
        flow.links = vec![
            link("cam_a:video_out", "mixer:video_in_0"),
            link("cam_b:video_out", "scaler:video_in"),
            link("scaler:video_out", "mixer:video_in_1"),
            link("plain:video_out", "mixer:video_in_2"),
            link("cam_a:video_out", "switch:video_in_0"),
            link("cam_b:video_out", "switch:video_in_1"),
            link("switch:video_out", "mixer:video_in_3"),
            link("cam_c:video_out", "inner:video_in_0"),
            link("inner:program_out", "mixer:video_in_4"),
        ];
        flow
    }

    #[test]
    fn inputs_fed_by_one_camera_are_found() {
        let found = camera_inputs(&flow());
        let mixer: Vec<_> = found
            .iter()
            .filter(|c| c.mixer == "mixer")
            .map(|c| (c.input, c.device.as_str()))
            .collect();
        assert_eq!(mixer, vec![(0, "phone-a"), (1, "phone-b")]);
        // The inner mixer's own input 0 is cam_c.
        assert!(found.contains(&CameraInput {
            mixer: "inner".to_string(),
            input: 0,
            device: "phone-c".to_string(),
        }));
    }

    #[test]
    fn only_unset_default_or_earlier_camera_names_are_replaced() {
        let s = |v: &str| PropertyValue::String(v.to_string());
        let previous = vec!["iPhone 16 · 55dd".to_string()];
        let name = "Stage left";
        assert!(should_relabel(None, 0, name, &previous));
        assert!(should_relabel(Some(&s("")), 0, name, &previous));
        assert!(should_relabel(Some(&s("In 1")), 0, name, &previous));
        assert!(should_relabel(
            Some(&s("iPhone 16 · 55dd")),
            0,
            name,
            &previous
        ));
        // An operator's label, another input's default, the name itself.
        assert!(!should_relabel(Some(&s("Wide shot")), 0, name, &previous));
        assert!(!should_relabel(Some(&s("In 2")), 0, name, &previous));
        assert!(!should_relabel(Some(&s(name)), 0, name, &previous));
    }

    #[test]
    fn history_knows_earlier_names() {
        let mut h = NameHistory::default();
        assert!(h.note("p", "iPhone 16 · 55dd"));
        assert!(!h.note("p", "iPhone 16 · 55dd"));
        assert!(h.previous("p").is_empty());
        assert!(h.note("p", "Stage left"));
        assert_eq!(h.previous("p"), vec!["iPhone 16 · 55dd".to_string()]);
        assert!(h.note("p", "iPhone 16 · 55dd"));
        assert_eq!(h.previous("p"), vec!["Stage left".to_string()]);
    }
}
