//! Built-in block definitions organized by protocol/function.

pub mod aes67;
pub mod audioanalyzer;
pub mod audioformat;
pub mod audiorouter;
pub mod compositor;
pub mod decklink;
#[cfg(feature = "efp")]
pub mod efpsrt;
#[cfg(feature = "efp")]
pub mod efpsrt_input;
pub mod inter;
pub mod latency;
pub mod loudness;
pub mod mediaplayer;
pub mod meter;
pub mod mixer;
pub mod mpegtssrt;
pub mod mpegtssrt_input;
pub mod ndi;
pub mod recorder;
pub mod spectrum;
pub mod thumbnail;
pub mod videoenc;
pub mod videoformat;
pub mod vision_mixer;
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

    // Add AudioAnalyzer blocks
    blocks.extend(audioanalyzer::get_blocks());

    // Add AudioFormat blocks
    blocks.extend(audioformat::get_blocks());

    // Add AudioRouter blocks
    blocks.extend(audiorouter::get_blocks());

    // Add Compositor blocks (unified CPU/GPU)
    blocks.extend(compositor::get_blocks());

    // Add DeckLink blocks
    blocks.extend(decklink::get_blocks());

    // Add EFP/SRT blocks
    #[cfg(feature = "efp")]
    blocks.extend(efpsrt::get_blocks());

    // Add EFP/SRT Input blocks
    #[cfg(feature = "efp")]
    blocks.extend(efpsrt_input::get_blocks());

    // Add Inter-pipeline blocks
    blocks.extend(inter::get_blocks());

    // Add Latency blocks
    blocks.extend(latency::get_blocks());

    // Add Loudness blocks
    blocks.extend(loudness::get_blocks());

    // Add Media Player blocks
    blocks.extend(mediaplayer::get_blocks());

    // Add Meter blocks
    blocks.extend(meter::get_blocks());

    // Add Mixer blocks
    blocks.extend(mixer::get_blocks());

    // Add MPEG-TS/SRT blocks
    blocks.extend(mpegtssrt::get_blocks());

    // Add MPEG-TS/SRT Input blocks
    blocks.extend(mpegtssrt_input::get_blocks());

    // Add NDI blocks
    blocks.extend(ndi::get_blocks());

    // Add Recorder blocks
    blocks.extend(recorder::get_blocks());

    // Add Spectrum blocks
    blocks.extend(spectrum::get_blocks());

    // Add Thumbnail blocks
    blocks.extend(thumbnail::get_blocks());

    // Add VideoEncoder blocks
    blocks.extend(videoenc::get_blocks());

    // Add VideoFormat blocks
    blocks.extend(videoformat::get_blocks());

    // Add Vision Mixer blocks
    blocks.extend(vision_mixer::get_blocks());

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
        "builtin.audioanalyzer" => Some(Arc::new(audioanalyzer::AudioAnalyzerBuilder)),
        "builtin.audioformat" => Some(Arc::new(audioformat::AudioFormatBuilder)),
        "builtin.audiorouter" => Some(Arc::new(audiorouter::AudioRouterBuilder)),
        "builtin.compositor" => Some(Arc::new(compositor::CompositorBuilder)),
        "builtin.decklink_video_input" => Some(Arc::new(decklink::DeckLinkVideoInputBuilder)),
        "builtin.decklink_audio_input" => Some(Arc::new(decklink::DeckLinkAudioInputBuilder)),
        "builtin.decklink_video_output" => Some(Arc::new(decklink::DeckLinkVideoOutputBuilder)),
        "builtin.decklink_audio_output" => Some(Arc::new(decklink::DeckLinkAudioOutputBuilder)),
        "builtin.inter_output" => Some(Arc::new(inter::InterOutputBuilder)),
        "builtin.inter_input" => Some(Arc::new(inter::InterInputBuilder)),
        "builtin.latency" => Some(Arc::new(latency::LatencyBuilder)),
        "builtin.loudness" => Some(Arc::new(loudness::LoudnessBuilder)),
        "builtin.media_player" => Some(Arc::new(mediaplayer::MediaPlayerBuilder)),
        "builtin.meter" => Some(Arc::new(meter::MeterBuilder)),
        "builtin.mixer" => Some(Arc::new(mixer::MixerBuilder)),
        #[cfg(feature = "efp")]
        "builtin.efpsrt_output" => Some(Arc::new(efpsrt::EfpSrtOutputBuilder)),
        #[cfg(feature = "efp")]
        "builtin.efpsrt_input" => Some(Arc::new(efpsrt_input::EfpSrtInputBuilder)),
        "builtin.mpegtssrt_output" => Some(Arc::new(mpegtssrt::MpegTsSrtOutputBuilder)),
        "builtin.mpegtssrt_input" => Some(Arc::new(mpegtssrt_input::MpegTsSrtInputBuilder)),
        "builtin.ndi_input" => Some(Arc::new(ndi::NDIInputBuilder)),
        "builtin.ndi_output" => Some(Arc::new(ndi::NDIOutputBuilder)),
        "builtin.recorder" => Some(Arc::new(recorder::RecorderBuilder)),
        "builtin.spectrum" => Some(Arc::new(spectrum::SpectrumBuilder)),
        "builtin.thumbnail" => Some(Arc::new(thumbnail::ThumbnailBuilder)),
        "builtin.videoenc" => Some(Arc::new(videoenc::VideoEncBuilder)),
        "builtin.videoformat" => Some(Arc::new(videoformat::VideoFormatBuilder)),
        "builtin.vision_mixer" => Some(Arc::new(vision_mixer::VisionMixerBuilder)),
        "builtin.whip_output" => Some(Arc::new(whip::WHIPOutputBuilder)),
        "builtin.whip_input" => Some(Arc::new(whip::WHIPInputBuilder)),
        "builtin.whep_input" => Some(Arc::new(whep::WHEPInputBuilder)),
        "builtin.whep_output" => Some(Arc::new(whep::WHEPOutputBuilder)),
        // Future: Add more builders here
        _ => None,
    }
}
