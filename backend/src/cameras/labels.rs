//! Vision mixer input labels that follow the camera on the input.
//!
//! A mixer input fed by a camera-bound SRT input is labelled with the
//! camera's display name, and the label follows when the name changes, or
//! when the input gets another camera. An operator's own label is left
//! alone: a label is only replaced when it is empty, the mixer's default
//! ("In N"), or a name a camera has or had (this camera's earlier name, or
//! the name of a camera the input showed before).

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
/// name `name`: it is unset, empty, the default "In N", or one of
/// `camera_names`, the names cameras have or had (so a label is no
/// operator's own). Not when it already is `name`.
pub fn should_relabel(
    current: Option<&PropertyValue>,
    input: usize,
    name: &str,
    camera_names: &HashSet<String>,
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
        || camera_names.contains(current)
}

/// The label changes for the camera inputs of `flow`, by mixer block id:
/// each input fed by one camera gets the camera's display name (`names`, by
/// device id) where [`should_relabel`] allows. Limited to `only_devices`
/// when given.
pub fn relabels(
    flow: &Flow,
    names: &HashMap<String, String>,
    camera_names: &HashSet<String>,
    only_devices: Option<&[String]>,
) -> HashMap<String, HashMap<String, PropertyValue>> {
    let mut by_mixer: HashMap<String, HashMap<String, PropertyValue>> = HashMap::new();
    for input in camera_inputs(flow) {
        if only_devices.is_some_and(|d| !d.contains(&input.device)) {
            continue;
        }
        let Some(name) = names.get(&input.device) else {
            continue;
        };
        let property = label_property(input.input);
        let current = flow
            .blocks
            .iter()
            .find(|b| b.id == input.mixer)
            .and_then(|b| b.properties.get(&property));
        if should_relabel(current, input.input, name, camera_names) {
            by_mixer
                .entry(input.mixer)
                .or_default()
                .insert(property, PropertyValue::String(name.clone()));
        }
    }
    by_mixer
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

    /// Every name every camera has had in this run, current ones included.
    pub fn all(&self) -> HashSet<String> {
        self.names.values().flatten().cloned().collect()
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
        let previous = HashSet::from(["iPhone 16 · 55dd".to_string()]);
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
        h.note("q", "Wide");
        assert_eq!(
            h.all(),
            HashSet::from(["Stage left", "iPhone 16 · 55dd", "Wide"].map(String::from))
        );
    }

    /// An input that gets another camera (the block's `camera` changed, or
    /// two cameras swapped blocks) shows the new camera's name, not the
    /// name the input was labelled with for the old one. An operator's own
    /// label stays.
    #[test]
    fn an_input_that_gets_another_camera_is_relabelled() {
        let mut flow = flow();
        let mixer = flow.blocks.iter_mut().find(|b| b.id == "mixer").unwrap();
        // Input 0 (cam_a, now phone-a) still has phone-b's name; input 1
        // (cam_b, now phone-b) has phone-a's. Input 2 is the operator's.
        for (input, label) in [(0, "Stage right"), (1, "Stage left"), (2, "Wide shot")] {
            mixer.properties.insert(
                label_property(input),
                PropertyValue::String(label.to_string()),
            );
        }
        let names: HashMap<String, String> = [
            ("phone-a", "Stage left"),
            ("phone-b", "Stage right"),
            ("phone-c", "Overhead"),
        ]
        .into_iter()
        .map(|(d, n)| (d.to_string(), n.to_string()))
        .collect();
        let camera_names: HashSet<String> = names.values().cloned().collect();

        let changes = relabels(&flow, &names, &camera_names, None);
        let s = |v: &str| v.to_string();
        // PropertyValue has no PartialEq: compare the strings.
        let texts = |m: &HashMap<String, PropertyValue>| -> HashMap<String, String> {
            m.iter()
                .map(|(k, v)| match v {
                    PropertyValue::String(t) => (k.clone(), t.clone()),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(
            texts(&changes["mixer"]),
            HashMap::from([
                (label_property(0), s("Stage left")),
                (label_property(1), s("Stage right")),
            ])
        );
        // The inner mixer's unset label gets its camera's name.
        assert_eq!(
            texts(&changes["inner"]),
            HashMap::from([(label_property(0), s("Overhead"))])
        );
        // Limited to some devices.
        let only = relabels(&flow, &names, &camera_names, Some(&["phone-b".into()]));
        assert_eq!(
            texts(&only["mixer"]),
            HashMap::from([(label_property(1), s("Stage right"))])
        );
        assert!(!only.contains_key("inner"));
    }
}
