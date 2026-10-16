//! Tally from the vision mixers: which camera is on program, which on
//! preview.
//!
//! A camera-bound SRT input that is visible on a mixer's program in any way
//! (the main input, the background of a PiP on program, or a source in any of
//! its zones) is on program. The same rule gives preview. Program wins over
//! preview, across every mixer in every running flow.
//!
//! The visible inputs are traced back through the flow's links to the blocks
//! that feed them, through intermediate blocks (a scaler, a delay) but not
//! through another vision mixer: what a nested mixer shows depends on its own
//! buses, which this does not follow.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use strom_types::cameras::CameraTally;
use strom_types::vision_mixer::Zone;
use strom_types::Flow;

use crate::blocks::builtin::vision_mixer::overlay::VisionMixerOverlayState;

const VISION_MIXER: &str = "builtin.vision_mixer";

/// Program wins over preview, preview over off.
pub fn combine(a: CameraTally, b: CameraTally) -> CameraTally {
    fn rank(t: CameraTally) -> u8 {
        match t {
            CameraTally::Off => 0,
            CameraTally::Preview => 1,
            CameraTally::Program => 2,
        }
    }
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// The inputs one vision mixer shows on each bus, by input index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MixerBuses {
    pub program: BTreeSet<usize>,
    pub preview: BTreeSet<usize>,
}

impl MixerBuses {
    /// From a bus description: each bus shows either one input or one PiP.
    /// `pip_inputs(i)` lists what PiP `i` shows (background and zones).
    pub fn from_parts(
        program_input: Option<usize>,
        program_pip: Option<usize>,
        preview_input: Option<usize>,
        preview_pip: Option<usize>,
        pip_inputs: impl Fn(usize) -> Vec<usize>,
    ) -> Self {
        let bus = |input: Option<usize>, pip: Option<usize>| {
            let mut set: BTreeSet<usize> = input.into_iter().collect();
            if let Some(p) = pip {
                set.extend(pip_inputs(p));
            }
            set
        };
        Self {
            program: bus(program_input, program_pip),
            preview: bus(preview_input, preview_pip),
        }
    }

    /// What a running mixer shows now.
    pub fn from_overlay(state: &VisionMixerOverlayState) -> Self {
        Self::from_parts(
            state.pgm_input(),
            state.pgm_pip(),
            state.pvw_input(),
            state.pvw_pip(),
            |pip| pip_inputs(state.pip_bg_input(pip), &state.pip_zones(pip)),
        )
    }
}

/// Everything a PiP composition shows: its background and every source that
/// a zone actually draws (capacity applied).
pub fn pip_inputs(background: Option<usize>, zones: &[Zone]) -> Vec<usize> {
    background
        .into_iter()
        .chain(
            zones
                .iter()
                .flat_map(|z| z.effective_sources().iter().copied()),
        )
        .collect()
}

fn block_of(endpoint: &str) -> &str {
    endpoint.split(':').next().unwrap_or(endpoint)
}

/// Does a link endpoint (`block:pad` or a bare `block`) belong to `block`?
fn is_on_block(endpoint: &str, block: &str) -> bool {
    block_of(endpoint) == block
}

/// The ids of every block (or element) upstream of the input `pad`
/// (`block:pad`), following links back through intermediate blocks but not
/// through a vision mixer.
pub fn upstream_of(flow: &Flow, pad: &str) -> HashSet<String> {
    let mixers: HashSet<&str> = flow
        .blocks
        .iter()
        .filter(|b| b.block_definition_id == VISION_MIXER)
        .map(|b| b.id.as_str())
        .collect();
    let mut found = HashSet::new();
    let mut queue: VecDeque<&str> = flow
        .links
        .iter()
        .filter(|l| l.to == pad)
        .map(|l| block_of(&l.from))
        .collect();
    while let Some(block) = queue.pop_front() {
        if !found.insert(block.to_string()) || mixers.contains(block) {
            continue;
        }
        queue.extend(
            flow.links
                .iter()
                .filter(|l| is_on_block(&l.to, block))
                .map(|l| block_of(&l.from)),
        );
    }
    found
}

/// The tally of each camera in one flow. `cameras` maps a camera-bound block
/// id to its device id; `mixers` is what each running vision mixer (by block
/// id) shows. Every device in `cameras` gets an entry.
pub fn resolve_flow(
    flow: &Flow,
    cameras: &HashMap<String, String>,
    mixers: &[(String, MixerBuses)],
) -> HashMap<String, CameraTally> {
    let mut tally: HashMap<String, CameraTally> = cameras
        .values()
        .map(|d| (d.clone(), CameraTally::Off))
        .collect();
    for (mixer, buses) in mixers {
        for (inputs, state) in [
            (&buses.program, CameraTally::Program),
            (&buses.preview, CameraTally::Preview),
        ] {
            for input in inputs {
                for block in upstream_of(flow, &format!("{mixer}:video_in_{input}")) {
                    if let Some(device) = cameras.get(&block) {
                        let t = tally.entry(device.clone()).or_default();
                        *t = combine(*t, state);
                    }
                }
            }
        }
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::element::Link;
    use strom_types::BlockInstance;

    fn block(id: &str, def: &str) -> BlockInstance {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "block_definition_id": def,
            "properties": {},
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

    /// cam_a -> mixer input 0
    /// cam_b -> scaler -> mixer input 1
    /// cam_c -> mixer input 2
    /// cam_d -> inner mixer -> mixer input 3 (not followed)
    /// clip  -> mixer input 4 (not a camera)
    fn flow() -> Flow {
        let mut flow = Flow::new("tally test");
        flow.blocks = vec![
            block("cam_a", "builtin.mpegtssrt_input"),
            block("cam_b", "builtin.mpegtssrt_input"),
            block("cam_c", "builtin.mpegtssrt_input"),
            block("cam_d", "builtin.mpegtssrt_input"),
            block("scaler", "builtin.videoscale"),
            block("inner", VISION_MIXER),
            block("clip", "builtin.media_player"),
            block("mixer", VISION_MIXER),
        ];
        flow.links = vec![
            link("cam_a:video_out", "mixer:video_in_0"),
            link("cam_a:audio_out_0", "mixer:audio_in_0"),
            link("cam_b:video_out", "scaler:video_in"),
            link("scaler:video_out", "mixer:video_in_1"),
            link("cam_c:video_out", "mixer:video_in_2"),
            link("cam_d:video_out", "inner:video_in_0"),
            link("inner:program_out", "mixer:video_in_3"),
            link("clip:video_out", "mixer:video_in_4"),
        ];
        flow
    }

    fn cameras() -> HashMap<String, String> {
        [
            ("cam_a", "phone-a"),
            ("cam_b", "phone-b"),
            ("cam_c", "phone-c"),
            ("cam_d", "phone-d"),
        ]
        .into_iter()
        .map(|(b, d)| (b.to_string(), d.to_string()))
        .collect()
    }

    fn zone(sources: &[usize], capacity: Option<usize>) -> Zone {
        Zone {
            rect: None,
            capacity,
            sources: sources.to_vec(),
            border: None,
        }
    }

    #[test]
    fn upstream_follows_intermediate_blocks_but_not_mixers() {
        let flow = flow();
        let up = upstream_of(&flow, "mixer:video_in_1");
        assert_eq!(
            up,
            ["scaler", "cam_b"].into_iter().map(String::from).collect()
        );
        let up = upstream_of(&flow, "mixer:video_in_3");
        assert!(up.contains("inner"));
        assert!(!up.contains("cam_d"), "nested mixers are not followed");
    }

    #[test]
    fn main_inputs_give_program_and_preview() {
        let buses = MixerBuses::from_parts(Some(0), None, Some(1), None, |_| vec![]);
        let t = resolve_flow(&flow(), &cameras(), &[("mixer".into(), buses)]);
        assert_eq!(t["phone-a"], CameraTally::Program);
        assert_eq!(t["phone-b"], CameraTally::Preview, "through the scaler");
        assert_eq!(t["phone-c"], CameraTally::Off);
        assert_eq!(t["phone-d"], CameraTally::Off);
    }

    #[test]
    fn pip_on_program_lights_background_and_every_zone_source() {
        // PiP 0: background input 2, zone with inputs 1 and 4 (clip).
        // PiP 1: zone of capacity 1 holding [0, 1]: only 1 is drawn.
        let pips = |p: usize| match p {
            0 => pip_inputs(Some(2), &[zone(&[1, 4], None)]),
            1 => pip_inputs(None, &[zone(&[0, 1], Some(1))]),
            _ => vec![],
        };
        let buses = MixerBuses::from_parts(None, Some(0), Some(0), None, pips);
        assert_eq!(buses.program, BTreeSet::from([1, 2, 4]));
        let t = resolve_flow(&flow(), &cameras(), &[("mixer".into(), buses)]);
        assert_eq!(t["phone-c"], CameraTally::Program, "PiP background");
        assert_eq!(t["phone-b"], CameraTally::Program, "PiP zone source");
        assert_eq!(t["phone-a"], CameraTally::Preview);

        let buses = MixerBuses::from_parts(Some(2), None, None, Some(1), pips);
        assert_eq!(buses.preview, BTreeSet::from([1]), "capacity applied");
        let t = resolve_flow(&flow(), &cameras(), &[("mixer".into(), buses)]);
        assert_eq!(t["phone-b"], CameraTally::Preview);
        assert_eq!(t["phone-a"], CameraTally::Off, "dropped by capacity");
    }

    #[test]
    fn program_wins_over_preview_on_one_bus_and_across_mixers() {
        // The same camera on both buses of one mixer.
        let both = MixerBuses::from_parts(Some(0), None, Some(0), None, |_| vec![]);
        let t = resolve_flow(&flow(), &cameras(), &[("mixer".into(), both)]);
        assert_eq!(t["phone-a"], CameraTally::Program);

        // Two mixers in the flow watching the same camera: one previews it,
        // the other has it on air.
        let mut flow = flow();
        flow.blocks.push(block("mixer2", VISION_MIXER));
        flow.links
            .push(link("cam_a:video_out", "mixer2:video_in_0"));
        let pvw = MixerBuses::from_parts(Some(1), None, Some(0), None, |_| vec![]);
        let pgm = MixerBuses::from_parts(Some(0), None, None, None, |_| vec![]);
        let t = resolve_flow(
            &flow,
            &cameras(),
            &[("mixer".into(), pvw), ("mixer2".into(), pgm)],
        );
        assert_eq!(t["phone-a"], CameraTally::Program);
        assert_eq!(
            combine(CameraTally::Preview, CameraTally::Off),
            CameraTally::Preview
        );
        assert_eq!(
            combine(CameraTally::Program, CameraTally::Preview),
            CameraTally::Program
        );
    }
}
