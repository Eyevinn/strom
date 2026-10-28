//! DeckLink Output's audio input: raw audio in any channel count, rate and
//! sample format, adapted to what `decklinkaudiosink` takes.
//!
//! The card takes 2, 8 or 16 interleaved channels (fewer on a card with fewer
//! audio channels) at 48 kHz, S16LE or S32LE. The block takes the smallest
//! count the card takes that holds every input channel, and pads the rest
//! with silence: input channel N plays on SDI/HDMI channel N. Mono with a
//! position plays on both channels of a stereo pair. Rate and sample format
//! are converted by `audioresample` and `audioconvert` against the card's
//! caps. More channels than the card takes, and encoded audio, fail the flow
//! with the block's own message.

use super::super::devicesrc::MAX_AUDIO_CHANNELS;
use super::super::local_output::set_mix_matrix;
use super::super::refusal::{accept_any_caps, accept_input, drop_gaps_while_refused, refuse_input};
use super::{BlockElements, BlockLinks};
use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::element::ElementPadRef;
use tracing::info;

/// The element DeckLink Output's `audio_in` pad points at: its audio input
/// front.
pub const AUDIO_INPUT_ELEMENT: &str = "audio_input";

/// The capsfilter in front of `decklinkaudiosink`, which carries the channel
/// count the block decided on.
pub const AUDIO_CARD_CAPS_ELEMENT: &str = "audio_card_caps";

/// DeckLink Output's audio chain, ending in `audiosink`:
/// `audio_input` (identity front) → `audioconvert` → `audioresample` →
/// `audio_card_caps` → `decklinkaudiosink`.
///
/// The front decides the channel count from the input's Caps event and the
/// card's caps ([`plan_audio`]). Until then `audio_card_caps` is open.
pub(super) fn build_audio_input(
    instance_id: &str,
    audiosink: &gst::Element,
) -> Result<(BlockElements, BlockLinks), BlockBuildError> {
    let make = |factory: &str, name: &str| -> Result<(String, gst::Element), BlockBuildError> {
        let id = format!("{}:{}", instance_id, name);
        let element = gst::ElementFactory::make(factory)
            .name(&id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))?;
        Ok((id, element))
    };
    let (input_id, input) = make("identity", AUDIO_INPUT_ELEMENT)?;
    input.set_property("silent", true);
    let (convert_id, convert) = make("audioconvert", "audioconvert")?;
    let (resample_id, resample) = make("audioresample", "audioresample")?;
    let (card_caps_id, card_caps) = make("capsfilter", AUDIO_CARD_CAPS_ELEMENT)?;
    card_caps.set_property("caps", gst::Caps::new_empty_simple("audio/x-raw"));

    adapt_audio_input(instance_id, &input, &convert, &card_caps)?;

    let chain = [
        input_id.as_str(),
        convert_id.as_str(),
        resample_id.as_str(),
        card_caps_id.as_str(),
        audiosink.name().as_str(),
    ]
    .map(str::to_string);
    let links = chain
        .windows(2)
        .map(|pair| {
            (
                ElementPadRef::pad(&pair[0], "src"),
                ElementPadRef::pad(&pair[1], "sink"),
            )
        })
        .collect();
    Ok((
        vec![
            (input_id, input),
            (convert_id, convert),
            (resample_id, resample),
            (card_caps_id, card_caps),
        ],
        links,
    ))
}

/// How the block hands the input to the card.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AudioPlan {
    /// The channel count the card gets.
    pub channels: u32,
    /// The channel-mask to pin on the card's caps: stereo, when mono is
    /// spread over both channels by position. `None` leaves the label to
    /// `audioconvert`: the card ignores it.
    pub mask: Option<u64>,
    /// `audioconvert`'s mix-matrix (rows = card channels, columns = input
    /// channels), channel for channel, or `None` for none.
    pub matrix: Option<Vec<Vec<f32>>>,
}

/// Decide how `in_channels` arriving with `in_mask` reach a card that takes
/// `card`.
///
/// The card gets the smallest channel count it takes that holds every input
/// channel. Input channel N plays on card channel N and the channels above the
/// input's stay silent, whatever layout the input carries: SDI and HDMI audio
/// channels are numbered, not positioned. The one exception is mono with a
/// position, which plays on both channels of a stereo pair. More channels than
/// the card takes are refused with a reason for the operator.
pub(super) fn plan_audio(
    in_channels: u32,
    in_mask: Option<u64>,
    card: &gst::Caps,
) -> Result<AudioPlan, String> {
    let in_channels = in_channels.max(1);
    let channels = (in_channels..=MAX_AUDIO_CHANNELS)
        .find(|&n| card.can_intersect(&channel_caps(n, None)))
        .ok_or_else(|| too_many_channels(in_channels, card))?;
    let positioned_mono = in_channels == 1 && in_mask != Some(0);
    let by_position = positioned_mono && channels == 2;
    let matrix = (channels != in_channels && !by_position).then(|| {
        (0..channels as usize)
            .map(|out| {
                (0..in_channels as usize)
                    .map(|i| if i == out { 1.0 } else { 0.0 })
                    .collect()
            })
            .collect()
    });
    Ok(AudioPlan {
        channels,
        mask: by_position.then_some(0x3),
        matrix,
    })
}

/// Raw audio caps with `channels`, and `mask` as the channel-mask when given.
fn channel_caps(channels: u32, mask: Option<u64>) -> gst::Caps {
    let mut caps = gst::Caps::builder("audio/x-raw").field("channels", channels as i32);
    if let Some(mask) = mask {
        caps = caps.field("channel-mask", gst::Bitmask::new(mask));
    }
    caps.build()
}

/// The channel counts `card` takes, lowest first.
fn card_channel_counts(card: &gst::Caps) -> Vec<u32> {
    (1..=MAX_AUDIO_CHANNELS)
        .filter(|&n| card.can_intersect(&channel_caps(n, None)))
        .collect()
}

fn too_many_channels(in_channels: u32, card: &gst::Caps) -> String {
    let counts = card_channel_counts(card);
    let takes = match counts.as_slice() {
        [] => "no raw audio it reports".to_string(),
        [one] => one.to_string(),
        [rest @ .., last] => format!(
            "{} or {}",
            rest.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            last
        ),
    };
    format!(
        "DeckLink Output receives {} audio channels, and the card takes {} channels. \
         Reduce the channels before it with an Audio Router block",
        in_channels, takes
    )
}

fn encoded_audio_refusal(caps_name: &str) -> String {
    format!(
        "DeckLink Output plays raw audio, but its audio input carries {}. Link a raw audio \
         pad to it, not an encoder's encoded_out",
        caps_name
    )
}

/// Decide, from the Caps event at the audio input front, how the stream
/// reaches the card ([`plan_audio`]), and set `audioconvert`'s mix-matrix and
/// the `card_caps` filter to match. The card's caps are asked of whatever
/// `card_caps` feeds, so a card with fewer than 16 channels is honoured.
///
/// The front answers upstream's caps queries itself, so the input arrives as
/// it is and the decision is made here. An EVENT_DOWNSTREAM probe: per Caps
/// event, never per buffer. Weak handles only, since the probe is owned by a
/// pad of `input` itself.
fn adapt_audio_input(
    instance_id: &str,
    input: &gst::Element,
    convert: &gst::Element,
    card_caps: &gst::Element,
) -> Result<(), BlockBuildError> {
    let sink_pad = input
        .static_pad("sink")
        .ok_or_else(|| BlockBuildError::ElementCreation("identity has no sink pad".into()))?;
    let src_pad = input
        .static_pad("src")
        .ok_or_else(|| BlockBuildError::ElementCreation("identity has no src pad".into()))?;
    accept_any_caps(&sink_pad);
    let refused = Arc::new(AtomicBool::new(false));
    drop_gaps_while_refused(&src_pad, refused.clone());

    let convert_weak = convert.downgrade();
    let card_caps_weak = card_caps.downgrade();
    let instance_id = instance_id.to_string();
    sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(s) = caps.caps().structure(0) else {
            return gst::PadProbeReturn::Ok;
        };
        let refuse = |reason: &str| {
            refused.store(true, Ordering::SeqCst);
            if let Some(input) = pad.parent_element() {
                refuse_input(&input, reason);
            }
            // The identity never sees caps it cannot pass on, so it raises
            // no not-negotiated error of its own after ours.
            gst::PadProbeReturn::Drop
        };
        if s.name() != "audio/x-raw" {
            return refuse(&encoded_audio_refusal(s.name()));
        }
        let (Some(convert), Some(card_caps)) = (convert_weak.upgrade(), card_caps_weak.upgrade())
        else {
            return gst::PadProbeReturn::Ok;
        };
        let in_channels = s.get::<i32>("channels").unwrap_or(0).max(0) as u32;
        let in_mask = s.get::<gst::Bitmask>("channel-mask").ok().map(|m| m.0);
        let card = card_caps
            .static_pad("src")
            .map(|p| p.peer_query_caps(None))
            .unwrap_or_else(gst::Caps::new_any);
        let plan = match plan_audio(in_channels, in_mask, &card) {
            Ok(plan) => plan,
            Err(reason) => return refuse(&reason),
        };
        if refused.swap(false, Ordering::SeqCst) {
            if let Some(input) = pad.parent_element() {
                accept_input(&input);
            }
        }
        let in_rate = s.get::<i32>("rate").ok();
        let card_rate = card.structure(0).and_then(|c| c.get::<i32>("rate").ok());
        if plan.channels != in_channels || in_rate != card_rate {
            info!(
                "DeckLink Output {}: adapting the audio input to the card: {} channels at \
                 {:?} Hz play as {} channels at {:?} Hz{}",
                instance_id,
                in_channels,
                in_rate,
                plan.channels,
                card_rate,
                match (plan.matrix.is_some(), plan.channels > in_channels) {
                    (true, true) => format!(
                        ", channel for channel, channels {}-{} silent",
                        in_channels + 1,
                        plan.channels
                    ),
                    (false, true) => ", mono on both channels".to_string(),
                    _ => String::new(),
                }
            );
        }
        set_mix_matrix(&convert, plan.matrix.as_deref());
        card_caps.set_property("caps", channel_caps(plan.channels, plan.mask));
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `decklinkaudiosink` answers without a card: its template.
    fn card() -> gst::Caps {
        "audio/x-raw, format={S16LE,S32LE}, channels={2,8,16}, rate=48000, layout=interleaved"
            .parse()
            .unwrap()
    }

    /// What it answers for a card with 8 audio channels.
    fn card_with_8() -> gst::Caps {
        "audio/x-raw, format={S16LE,S32LE}, channels={8,2}, rate=48000, layout=interleaved"
            .parse()
            .unwrap()
    }

    fn one_to_one(outputs: usize, inputs: usize) -> Option<Vec<Vec<f32>>> {
        Some(
            (0..outputs)
                .map(|o| {
                    (0..inputs)
                        .map(|i| if i == o { 1.0 } else { 0.0 })
                        .collect()
                })
                .collect(),
        )
    }

    #[test]
    fn counts_the_card_takes_pass_as_they_are() {
        gst::init().unwrap();
        for (n, mask) in [(2, Some(0x3)), (2, Some(0)), (8, Some(0)), (16, Some(0))] {
            assert_eq!(
                plan_audio(n, mask, &card()),
                Ok(AudioPlan {
                    channels: n,
                    mask: None,
                    matrix: None
                })
            );
        }
    }

    #[test]
    fn other_counts_are_padded_to_the_next_count_channel_for_channel() {
        gst::init().unwrap();
        assert_eq!(
            plan_audio(6, Some(0), &card()),
            Ok(AudioPlan {
                channels: 8,
                mask: None,
                matrix: one_to_one(8, 6)
            })
        );
        // A 5.1 layout is numbered channels on SDI as well: no reordering.
        assert_eq!(
            plan_audio(6, Some(0x3f), &card()).unwrap().matrix,
            one_to_one(8, 6)
        );
        assert_eq!(plan_audio(4, Some(0), &card()).unwrap().channels, 8);
        assert_eq!(plan_audio(10, Some(0), &card()).unwrap().channels, 16);
        assert_eq!(
            plan_audio(3, Some(0), &card()).unwrap().matrix,
            one_to_one(8, 3)
        );
    }

    #[test]
    fn mono_plays_on_both_channels_of_a_stereo_pair() {
        gst::init().unwrap();
        for mask in [None, Some(0x4)] {
            assert_eq!(
                plan_audio(1, mask, &card()),
                Ok(AudioPlan {
                    channels: 2,
                    mask: Some(0x3),
                    matrix: None
                })
            );
        }
        // Unpositioned mono is a numbered channel: channel 1 only.
        assert_eq!(
            plan_audio(1, Some(0), &card()),
            Ok(AudioPlan {
                channels: 2,
                mask: None,
                matrix: one_to_one(2, 1)
            })
        );
    }

    #[test]
    fn more_channels_than_the_card_takes_are_refused_with_what_it_takes() {
        gst::init().unwrap();
        let reason = plan_audio(24, Some(0), &card()).unwrap_err();
        assert!(reason.contains("24 audio channels"), "{reason}");
        assert!(reason.contains("2, 8 or 16"), "{reason}");
        assert!(reason.contains("Audio Router"), "{reason}");
        let reason = plan_audio(12, Some(0), &card_with_8()).unwrap_err();
        assert!(reason.contains("2 or 8"), "{reason}");
    }
}
