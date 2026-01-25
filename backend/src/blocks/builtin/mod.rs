//! Built-in block definitions organized by protocol/function.

pub mod aes67;
pub mod audioformat;
pub mod compositor;
pub mod decklink;
pub mod inter;
pub mod mediaplayer;
pub mod meter;
pub mod mpegtssrt;
pub mod ndi;
pub mod videoenc;
pub mod videoformat;
pub mod whep;
pub mod whip;

use crate::blocks::BlockBuilder;
use std::sync::Arc;
use strom_types::BlockDefinition;

/// Get all built-in block definitions.
pub fn get_all_builtin_blocks() -> Vec<BlockDefinition> {
    let mut blocks = Vec::new();

    // Add AES67 blocks
    blocks.extend(aes67::get_blocks());

    // Add AudioFormat blocks
    blocks.extend(audioformat::get_blocks());

    // Add Compositor blocks (unified CPU/GPU)
    blocks.extend(compositor::get_blocks());

    // Add DeckLink blocks
    blocks.extend(decklink::get_blocks());

    // Add Inter-pipeline blocks
    blocks.extend(inter::get_blocks());

    // Add Media Player blocks
    blocks.extend(mediaplayer::get_blocks());

    // Add Meter blocks
    blocks.extend(meter::get_blocks());

    // Add MPEG-TS/SRT blocks
    blocks.extend(mpegtssrt::get_blocks());

    // Add NDI blocks
    blocks.extend(ndi::get_blocks());

    // Add VideoEncoder blocks
    blocks.extend(videoenc::get_blocks());

    // Add VideoFormat blocks
    blocks.extend(videoformat::get_blocks());

    // Add WHIP blocks
    blocks.extend(whip::get_blocks());

    // Add WHEP blocks
    blocks.extend(whep::get_blocks());

    // Future: Add more protocols here
    // blocks.extend(rtmp::get_blocks());
    // blocks.extend(hls::get_blocks());

    blocks
}

/// Get a BlockBuilder instance for a built-in block by its definition ID.
pub fn get_builder(block_definition_id: &str) -> Option<Arc<dyn BlockBuilder>> {
    match block_definition_id {
        "builtin.aes67_input" => Some(Arc::new(aes67::AES67InputBuilder)),
        "builtin.aes67_output" => Some(Arc::new(aes67::AES67OutputBuilder)),
        "builtin.audioformat" => Some(Arc::new(audioformat::AudioFormatBuilder)),
        "builtin.compositor" => Some(Arc::new(compositor::CompositorBuilder)),
        "builtin.decklink_video_input" => Some(Arc::new(decklink::DeckLinkVideoInputBuilder)),
        "builtin.decklink_audio_input" => Some(Arc::new(decklink::DeckLinkAudioInputBuilder)),
        "builtin.decklink_video_output" => Some(Arc::new(decklink::DeckLinkVideoOutputBuilder)),
        "builtin.decklink_audio_output" => Some(Arc::new(decklink::DeckLinkAudioOutputBuilder)),
        "builtin.inter_output" => Some(Arc::new(inter::InterOutputBuilder)),
        "builtin.inter_input" => Some(Arc::new(inter::InterInputBuilder)),
        "builtin.media_player" => Some(Arc::new(mediaplayer::MediaPlayerBuilder)),
        "builtin.meter" => Some(Arc::new(meter::MeterBuilder)),
        "builtin.mpegtssrt_output" => Some(Arc::new(mpegtssrt::MpegTsSrtOutputBuilder)),
        "builtin.ndi_input" => Some(Arc::new(ndi::NDIInputBuilder)),
        "builtin.ndi_output" => Some(Arc::new(ndi::NDIOutputBuilder)),
        "builtin.videoenc" => Some(Arc::new(videoenc::VideoEncBuilder)),
        "builtin.videoformat" => Some(Arc::new(videoformat::VideoFormatBuilder)),
        "builtin.whip_output" => Some(Arc::new(whip::WHIPOutputBuilder)),
        "builtin.whep_input" => Some(Arc::new(whep::WHEPInputBuilder)),
        "builtin.whep_output" => Some(Arc::new(whep::WHEPOutputBuilder)),
        // Future: Add more builders here
        _ => None,
    }
}
