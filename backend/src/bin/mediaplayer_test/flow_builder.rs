//! Flow builder for test scenarios
//!
//! Constructs flows with MediaPlayer and MPEGTSSRT blocks for testing.

#![allow(dead_code)] // Some functions reserved for future tests

use std::collections::HashMap;

use strom_types::block::Position;
use strom_types::{BlockInstance, Flow, Link, PropertyValue};

/// Build a flow with MediaPlayer -> MPEGTSSRT (single file)
///
/// # Arguments
/// * `name` - Flow name
/// * `decode_mode` - true for decode mode, false for passthrough
/// * `srt_uri` - SRT output URI (e.g., "srt://:5100?mode=listener")
/// * `media_file` - Path to the media file to play
pub fn build_mediaplayer_to_srt_flow(
    name: &str,
    decode_mode: bool,
    srt_uri: &str,
    media_file: &str,
) -> Flow {
    build_mediaplayer_to_srt_flow_with_playlist(
        name,
        decode_mode,
        srt_uri,
        &[media_file.to_string()],
    )
}

/// Build a flow with MediaPlayer -> MPEGTSSRT (with playlist)
///
/// # Arguments
/// * `name` - Flow name
/// * `decode_mode` - true for decode mode, false for passthrough
/// * `srt_uri` - SRT output URI (e.g., "srt://:5100?mode=listener")
/// * `playlist` - List of media files to play
pub fn build_mediaplayer_to_srt_flow_with_playlist(
    name: &str,
    decode_mode: bool,
    srt_uri: &str,
    playlist: &[String],
) -> Flow {
    build_mediaplayer_to_srt_flow_full(name, decode_mode, srt_uri, playlist, false)
}

/// Build a flow with MediaPlayer -> MPEGTSSRT (full options)
///
/// # Arguments
/// * `name` - Flow name
/// * `decode_mode` - true for decode mode, false for passthrough
/// * `srt_uri` - SRT output URI (e.g., "srt://:5100?mode=listener")
/// * `playlist` - List of media files to play
/// * `loop_playlist` - Whether to loop the playlist
pub fn build_mediaplayer_to_srt_flow_full(
    name: &str,
    decode_mode: bool,
    srt_uri: &str,
    playlist: &[String],
    loop_playlist: bool,
) -> Flow {
    let mut flow = Flow::new(name);

    // Create MediaPlayer block
    let mut mediaplayer_props = HashMap::new();
    mediaplayer_props.insert("decode".to_string(), PropertyValue::Bool(decode_mode));
    mediaplayer_props.insert(
        "loop_playlist".to_string(),
        PropertyValue::Bool(loop_playlist),
    );
    mediaplayer_props.insert(
        "playlist".to_string(),
        PropertyValue::String(serde_json::to_string(playlist).unwrap()),
    );

    let mediaplayer = BlockInstance {
        id: "mediaplayer".to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: Some("Media Player".to_string()),
        properties: mediaplayer_props,
        position: Position { x: 100.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    };

    // Create MPEGTSSRT block
    let mut srt_props = HashMap::new();
    srt_props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(srt_uri.to_string()),
    );
    srt_props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    srt_props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    srt_props.insert("sync".to_string(), PropertyValue::Bool(true)); // Sync to wall clock for real-time playback

    let mpegtssrt = BlockInstance {
        id: "srt_output".to_string(),
        block_definition_id: "builtin.mpegtssrt_output".to_string(),
        name: Some("SRT Output".to_string()),
        properties: srt_props,
        position: Position { x: 400.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    };

    // Add blocks to flow
    flow.blocks.push(mediaplayer);
    flow.blocks.push(mpegtssrt);

    // Link video: mediaplayer:video_out -> srt_output:video_in
    flow.links.push(Link {
        from: "mediaplayer:video_out".to_string(),
        to: "srt_output:video_in".to_string(),
    });

    // Link audio: mediaplayer:audio_out -> srt_output:audio_in_0
    flow.links.push(Link {
        from: "mediaplayer:audio_out".to_string(),
        to: "srt_output:audio_in_0".to_string(),
    });

    flow
}

/// Build a decode mode flow: MediaPlayer (decode) -> VideoEncoder -> MPEGTSSRT
///
/// This flow decodes video to raw frames, re-encodes with VideoEncoder, then outputs via SRT.
///
/// # Arguments
/// * `name` - Flow name
/// * `srt_uri` - SRT output URI
/// * `playlist` - List of media files to play
/// * `loop_playlist` - Whether to loop the playlist
pub fn build_decode_mode_flow(
    name: &str,
    srt_uri: &str,
    playlist: &[String],
    loop_playlist: bool,
) -> Flow {
    let mut flow = Flow::new(name);

    // Create MediaPlayer block with decode=true
    let mut mediaplayer_props = HashMap::new();
    mediaplayer_props.insert("decode".to_string(), PropertyValue::Bool(true));
    mediaplayer_props.insert(
        "loop_playlist".to_string(),
        PropertyValue::Bool(loop_playlist),
    );
    mediaplayer_props.insert(
        "playlist".to_string(),
        PropertyValue::String(serde_json::to_string(playlist).unwrap()),
    );

    let mediaplayer = BlockInstance {
        id: "mediaplayer".to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: Some("Media Player".to_string()),
        properties: mediaplayer_props,
        position: Position { x: 100.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    };

    // Create VideoEncoder block
    let mut encoder_props = HashMap::new();
    encoder_props.insert(
        "codec".to_string(),
        PropertyValue::String("h264".to_string()),
    );
    encoder_props.insert("bitrate".to_string(), PropertyValue::UInt(4000)); // 4 Mbps
    encoder_props.insert(
        "quality_preset".to_string(),
        PropertyValue::String("ultrafast".to_string()),
    );

    let videoenc = BlockInstance {
        id: "videoenc".to_string(),
        block_definition_id: "builtin.videoenc".to_string(),
        name: Some("Video Encoder".to_string()),
        properties: encoder_props,
        position: Position { x: 300.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    };

    // Create MPEGTSSRT block
    let mut srt_props = HashMap::new();
    srt_props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(srt_uri.to_string()),
    );
    srt_props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    srt_props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    srt_props.insert("sync".to_string(), PropertyValue::Bool(true));

    let mpegtssrt = BlockInstance {
        id: "srt_output".to_string(),
        block_definition_id: "builtin.mpegtssrt_output".to_string(),
        name: Some("SRT Output".to_string()),
        properties: srt_props,
        position: Position { x: 500.0, y: 200.0 },
        runtime_data: None,
        computed_external_pads: None,
    };

    // Add blocks to flow
    flow.blocks.push(mediaplayer);
    flow.blocks.push(videoenc);
    flow.blocks.push(mpegtssrt);

    // Link video: mediaplayer:video_out -> videoenc:video_in -> srt_output:video_in
    flow.links.push(Link {
        from: "mediaplayer:video_out".to_string(),
        to: "videoenc:video_in".to_string(),
    });
    flow.links.push(Link {
        from: "videoenc:encoded_out".to_string(),
        to: "srt_output:video_in".to_string(),
    });

    // Link audio: mediaplayer:audio_out -> srt_output:audio_in_0
    flow.links.push(Link {
        from: "mediaplayer:audio_out".to_string(),
        to: "srt_output:audio_in_0".to_string(),
    });

    flow
}
