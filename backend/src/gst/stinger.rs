//! Stinger transitions — binding resolution and request validation.
//!
//! A stinger plays a keyed clip over the program while another transition runs
//! beneath it. The clip comes from a media player block wired into one of the
//! mixer's keyed (DSK) inputs; this module works out *which* input, and rejects
//! the requests that cannot be honoured, before anything touches the pipeline.
//!
//! Everything here is pure — it reasons over the flow definition, not over
//! GStreamer state — so the failure paths the spec names are unit-testable
//! without a running pipeline. Execution lives in the caller.

use crate::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use crate::blocks::builtin::vision_mixer::properties::parse_num_dsk_inputs;
use strom_types::element::Link;
use strom_types::{BlockInstance, FlowId};
use tracing::{info, warn};

/// Pad name a media player exposes its decoded video on.
const SOURCE_PAD: &str = "video_out";

/// Property by which a media player declares itself a stinger clip source.
///
/// Wiring alone is deliberately not enough: a media player may sit on a keyed
/// input to play a looping graphic, and arming it would park it on frame 0 and
/// stop it looping. Declaration keeps stinger behaviour opt-in.
pub const STINGER_SOURCE_PROPERTY: &str = "stinger_source";

/// Property on a source block declaring how its clip's alpha is encoded.
/// Absent means straight, which is the only kind that composites correctly.
pub const ALPHA_MODE_PROPERTY: &str = "alpha_mode";
pub const ALPHA_MODE_PREMULTIPLIED: &str = "premultiplied";

/// Which keyed input of a mixer a stinger source feeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StingerBinding {
    /// Block id of the media player supplying the clip.
    pub source_block_id: String,
    /// Index of the mixer's keyed (DSK) input it is wired to.
    pub dsk_index: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StingerError {
    #[error("stinger requires a clip source, but none was named")]
    MissingSource,
    #[error("no block '{0}' in this flow to use as a stinger clip source")]
    UnknownSource(String),
    #[error(
        "block '{0}' is not declared as a stinger clip source — enable that on the \
         block so its clip is held ready, or it would fire late"
    )]
    SourceNotDeclared(String),
    #[error(
        "block '{source_block}' is not wired to a keyed (DSK) input of mixer \
         '{mixer}', so it cannot be used as a stinger"
    )]
    SourceNotKeyed { source_block: String, mixer: String },
    #[error("mixer '{0}' has no keyed (DSK) inputs configured, so it cannot play a stinger")]
    NoKeyedInputs(String),
    #[error(
        "clip source '{0}' declares premultiplied alpha, which is not supported — \
         a premultiplied clip composites too dark and no compositor operator corrects it"
    )]
    PremultipliedUnsupported(String),
    #[error(
        "stinger cut point {cut_point_ms} ms is at or beyond the clip length \
         {clip_ms} ms, so the transition beneath would never run"
    )]
    CutPointBeyondClip { cut_point_ms: u64, clip_ms: u64 },
    #[error("a stinger is already running on mixer '{0}'")]
    AlreadyRunning(String),
    #[error("clip source '{0}' has no clip loaded")]
    NoClipLoaded(String),
    #[error("unknown transition '{0}' requested beneath the stinger")]
    UnknownUnderTransition(String),
    #[error("a stinger cannot run beneath a stinger")]
    StingerBeneathStinger,
}

/// Whether a block declares itself a stinger clip source.
///
/// Defaults to false: a player must opt in, so wiring a looping graphic to a
/// keyed input never causes it to be parked or unlooped.
pub fn declares_stinger_source(block: &BlockInstance) -> bool {
    matches!(
        block.properties.get(STINGER_SOURCE_PROPERTY),
        Some(strom_types::PropertyValue::Bool(true))
    )
}

/// Strip any element suffix from a block instance id (`"mixer1:mixer"` ->
/// `"mixer1"`), which is how ids appear in a flow's links.
fn block_id_of(instance_id: &str) -> &str {
    instance_id.split(':').next().unwrap_or(instance_id)
}

/// Work out which keyed input `source_block_id` feeds on `mixer_instance_id`.
///
/// Compares against generated pad names rather than splitting `id:pad`, because
/// a block id may itself contain a colon.
pub fn resolve_binding(
    blocks: &[BlockInstance],
    links: &[Link],
    mixer_instance_id: &str,
    source_block_id: Option<&str>,
) -> Result<StingerBinding, StingerError> {
    let source = source_block_id
        .filter(|s| !s.is_empty())
        .ok_or(StingerError::MissingSource)?;
    let mixer = block_id_of(mixer_instance_id);

    let mixer_block = blocks
        .iter()
        .find(|b| b.id == mixer)
        .ok_or_else(|| StingerError::UnknownSource(mixer.to_string()))?;
    let num_dsk = parse_num_dsk_inputs(&mixer_block.properties);
    if num_dsk == 0 {
        return Err(StingerError::NoKeyedInputs(mixer.to_string()));
    }

    let source_block = blocks
        .iter()
        .find(|b| b.id == source)
        .ok_or_else(|| StingerError::UnknownSource(source.to_string()))?;

    if !declares_stinger_source(source_block) {
        return Err(StingerError::SourceNotDeclared(source.to_string()));
    }

    if let Some(strom_types::PropertyValue::String(mode)) =
        source_block.properties.get(ALPHA_MODE_PROPERTY)
    {
        if mode.eq_ignore_ascii_case(ALPHA_MODE_PREMULTIPLIED) {
            return Err(StingerError::PremultipliedUnsupported(source.to_string()));
        }
    }

    let from = format!("{source}:{SOURCE_PAD}");
    for idx in 0..num_dsk {
        let to = format!("{mixer}:dsk_in_{idx}");
        if links.iter().any(|l| l.from == from && l.to == to) {
            return Ok(StingerBinding {
                source_block_id: source.to_string(),
                dsk_index: idx,
            });
        }
    }

    Err(StingerError::SourceNotKeyed {
        source_block: source.to_string(),
        mixer: mixer.to_string(),
    })
}

/// Check the cut point falls inside the clip, and shorten the underlying
/// transition if it would outlast the clip.
///
/// Returns the duration to actually use, and `Some(requested)` when it had to
/// be shortened so the caller can warn with both numbers.
pub fn fit_under_transition(
    cut_point_ms: u64,
    requested_duration_ms: u64,
    clip_ms: u64,
) -> Result<(u64, Option<u64>), StingerError> {
    if clip_ms == 0 || cut_point_ms >= clip_ms {
        return Err(StingerError::CutPointBeyondClip {
            cut_point_ms,
            clip_ms,
        });
    }
    let remaining = clip_ms - cut_point_ms;
    if requested_duration_ms > remaining {
        Ok((remaining, Some(requested_duration_ms)))
    } else {
        Ok((requested_duration_ms, None))
    }
}

/// Park every declared stinger source in this flow on its first frame.
///
/// Must run after the pipeline is up: the media player auto-plays, so a source
/// that is merely built is not on frame 0. Undeclared sources are untouched,
/// which is what keeps a looping graphic on a keyed input playing.
pub fn arm_declared_sources(flow_id: FlowId, blocks: &[BlockInstance]) {
    for block in blocks.iter().filter(|b| declares_stinger_source(b)) {
        let key = MediaPlayerKey {
            flow_id,
            block_id: block.id.clone(),
        };
        match MEDIA_PLAYER_REGISTRY.get(&key) {
            Some(player) => match player.arm_stinger() {
                Ok(()) => info!("Stinger source {} armed on its first frame", block.id),
                Err(e) => warn!(
                    "Stinger source {} could not be armed ({}) — its first fire will be late",
                    block.id, e
                ),
            },
            None => warn!(
                "Block {} declares itself a stinger source but no media player is \
                 registered for it",
                block.id
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use strom_types::{block::Position, PropertyValue};

    fn block(id: &str, def: &str, props: &[(&str, PropertyValue)]) -> BlockInstance {
        BlockInstance {
            id: id.to_string(),
            block_definition_id: def.to_string(),
            name: None,
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<HashMap<_, _>>(),
            position: Position { x: 0.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: None,
        }
    }

    fn link(from: &str, to: &str) -> Link {
        Link {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    fn mixer_with_dsk(n: u32) -> BlockInstance {
        block(
            "mixer1",
            "builtin.vision_mixer",
            &[("num_dsk_inputs", PropertyValue::UInt(n as u64))],
        )
    }

    fn media_player() -> BlockInstance {
        block(
            "mp1",
            "builtin.media_player",
            &[(STINGER_SOURCE_PROPERTY, PropertyValue::Bool(true))],
        )
    }

    /// Wired to a keyed input, but not declared — e.g. a looping graphic.
    fn undeclared_player() -> BlockInstance {
        block("mp1", "builtin.media_player", &[])
    }

    #[test]
    fn resolves_the_keyed_input_a_source_feeds() {
        let blocks = vec![mixer_with_dsk(2), media_player()];
        let links = vec![link("mp1:video_out", "mixer1:dsk_in_1")];
        assert_eq!(
            resolve_binding(&blocks, &links, "mixer1", Some("mp1")).unwrap(),
            StingerBinding {
                source_block_id: "mp1".to_string(),
                dsk_index: 1,
            }
        );
    }

    #[test]
    fn resolves_when_the_mixer_id_carries_an_element_suffix() {
        let blocks = vec![mixer_with_dsk(1), media_player()];
        let links = vec![link("mp1:video_out", "mixer1:dsk_in_0")];
        assert!(resolve_binding(&blocks, &links, "mixer1:mixer", Some("mp1")).is_ok());
    }

    #[test]
    fn unknown_source_is_rejected() {
        let blocks = vec![mixer_with_dsk(1)];
        assert_eq!(
            resolve_binding(&blocks, &[], "mixer1", Some("nope")),
            Err(StingerError::UnknownSource("nope".to_string()))
        );
    }

    #[test]
    fn source_not_wired_to_a_keyed_input_is_rejected() {
        let blocks = vec![mixer_with_dsk(2), media_player()];
        // Wired to a normal video input, not a keyed one.
        let links = vec![link("mp1:video_out", "mixer1:video_in_0")];
        assert_eq!(
            resolve_binding(&blocks, &links, "mixer1", Some("mp1")),
            Err(StingerError::SourceNotKeyed {
                source_block: "mp1".to_string(),
                mixer: "mixer1".to_string(),
            })
        );
    }

    #[test]
    fn mixer_without_keyed_inputs_is_rejected() {
        let blocks = vec![mixer_with_dsk(0), media_player()];
        let links = vec![link("mp1:video_out", "mixer1:dsk_in_0")];
        assert_eq!(
            resolve_binding(&blocks, &links, "mixer1", Some("mp1")),
            Err(StingerError::NoKeyedInputs("mixer1".to_string()))
        );
    }

    #[test]
    fn missing_source_is_rejected() {
        let blocks = vec![mixer_with_dsk(1)];
        assert_eq!(
            resolve_binding(&blocks, &[], "mixer1", None),
            Err(StingerError::MissingSource)
        );
        assert_eq!(
            resolve_binding(&blocks, &[], "mixer1", Some("")),
            Err(StingerError::MissingSource)
        );
    }

    #[test]
    fn source_wired_but_not_declared_is_rejected() {
        let blocks = vec![mixer_with_dsk(1), undeclared_player()];
        let links = vec![link("mp1:video_out", "mixer1:dsk_in_0")];
        assert_eq!(
            resolve_binding(&blocks, &links, "mixer1", Some("mp1")),
            Err(StingerError::SourceNotDeclared("mp1".to_string()))
        );
    }

    #[test]
    fn declaration_defaults_to_false() {
        assert!(!declares_stinger_source(&undeclared_player()));
        assert!(declares_stinger_source(&media_player()));
    }

    #[test]
    fn premultiplied_source_is_rejected_before_anything_plays() {
        let blocks = vec![
            mixer_with_dsk(1),
            block(
                "mp1",
                "builtin.media_player",
                &[
                    (STINGER_SOURCE_PROPERTY, PropertyValue::Bool(true)),
                    (
                        ALPHA_MODE_PROPERTY,
                        PropertyValue::String(ALPHA_MODE_PREMULTIPLIED.to_string()),
                    ),
                ],
            ),
        ];
        let links = vec![link("mp1:video_out", "mixer1:dsk_in_0")];
        assert_eq!(
            resolve_binding(&blocks, &links, "mixer1", Some("mp1")),
            Err(StingerError::PremultipliedUnsupported("mp1".to_string()))
        );
    }

    #[test]
    fn straight_alpha_declared_explicitly_is_accepted() {
        let blocks = vec![
            mixer_with_dsk(1),
            block(
                "mp1",
                "builtin.media_player",
                &[
                    (STINGER_SOURCE_PROPERTY, PropertyValue::Bool(true)),
                    (
                        ALPHA_MODE_PROPERTY,
                        PropertyValue::String("straight".to_string()),
                    ),
                ],
            ),
        ];
        let links = vec![link("mp1:video_out", "mixer1:dsk_in_0")];
        assert!(resolve_binding(&blocks, &links, "mixer1", Some("mp1")).is_ok());
    }

    #[test]
    fn cut_point_inside_the_clip_keeps_the_requested_duration() {
        assert_eq!(fit_under_transition(400, 300, 2000).unwrap(), (300, None));
    }

    #[test]
    fn cut_point_at_or_beyond_the_clip_is_rejected() {
        assert_eq!(
            fit_under_transition(2000, 300, 2000),
            Err(StingerError::CutPointBeyondClip {
                cut_point_ms: 2000,
                clip_ms: 2000,
            })
        );
        assert!(fit_under_transition(2500, 300, 2000).is_err());
        // An unknown clip length cannot be validated against.
        assert!(fit_under_transition(0, 300, 0).is_err());
    }

    #[test]
    fn under_transition_outlasting_the_clip_is_shortened() {
        // 1800 ms in, 300 ms of clip left, 500 ms requested.
        assert_eq!(
            fit_under_transition(1800, 500, 2100).unwrap(),
            (300, Some(500))
        );
    }
}
