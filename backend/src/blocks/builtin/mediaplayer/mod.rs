//! Media player block for file playback with playlist support.
//!
//! Architecture: an **internal pipeline** (owned by the block) handles decoding/parsing
//! and paces buffers through `clocksync`. An appsink→appsrc bridge feeds the main
//! pipeline, isolating downstream from seeks, file switches, and EOS.
//!
//! Supports two modes:
//! - **Decode** (`decode=true`): `uridecodebin` → raw video/audio
//! - **Passthrough** (`decode=false`): `urisourcebin` → encoded elementary streams

mod bridge;
mod builder;
mod definition;
mod state;

pub use builder::MediaPlayerBuilder;
pub use definition::get_blocks;
pub use state::{MediaPlayerKey, MediaPlayerState, MEDIA_PLAYER_REGISTRY};

use std::path::Path;
use tracing::debug;

/// Normalize a file path to a proper URI.
///
/// Converts relative paths to absolute file:// URIs resolved against `media_path`.
/// Passes through URIs that already have a scheme (file://, http://, https://).
///
/// Relative paths are resolved relative to `media_path` (the configured media directory).
/// Legacy paths starting with `./media/` have that prefix stripped before resolution.
pub fn normalize_uri(path: &str, media_path: &Path) -> String {
    // If it already has a scheme, pass through
    if path.starts_with("file://") || path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }

    // Strip legacy "./media/" prefix
    let clean_path = path
        .strip_prefix("./media/")
        .or_else(|| path.strip_prefix("media/"))
        .unwrap_or(path);

    // Check if it's an absolute path
    let file_path = if Path::new(clean_path).is_absolute() {
        std::path::PathBuf::from(clean_path)
    } else {
        // Resolve against media_path
        media_path.join(clean_path)
    };

    // Try to canonicalize the path (resolves symlinks, normalizes ..)
    let resolved = if file_path.exists() {
        file_path
            .canonicalize()
            .unwrap_or_else(|_| file_path.clone())
    } else {
        file_path
    };

    let uri = format!("file://{}", resolved.display());
    debug!("Normalized '{}' → '{}'", path, uri);
    uri
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::state::{
        MediaPlayerKey, MediaPlayerRegistry, MediaPlayerState, Playlist,
    };
    use gstreamer as gst;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::{Arc, RwLock};
    use strom_types::block::PropertyType;
    use strom_types::PropertyValue;
    use uuid::Uuid;

    /// Helper to create a MediaPlayerState for testing (no GStreamer elements).
    fn test_state(flow_id: Uuid, block_id: &str, playlist: Vec<String>) -> MediaPlayerState {
        MediaPlayerState {
            instance_id: Uuid::new_v4(),
            source_element: gst::glib::WeakRef::new(),
            internal_pipeline: RwLock::new(None),
            video_appsrc: None,
            audio_appsrc: None,
            playlist: RwLock::new(Playlist {
                files: playlist,
                current_index: 0,
            }),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(true),
            block_id: block_id.to_string(),
            flow_id,
            switching_file: AtomicBool::new(false),
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
            decode: false,
            sync: true,
            media_path: std::path::PathBuf::from("/media"),
            ts_offset: Arc::new(AtomicI64::new(i64::MIN)),
            main_pipeline: gst::glib::WeakRef::new(),
            bus_watch: std::sync::Mutex::new(None),
        }
    }

    #[test]
    fn test_normalize_uri_file_scheme() {
        let media_path = std::path::Path::new("/media");
        assert_eq!(
            normalize_uri("file:///path/to/video.mp4", media_path),
            "file:///path/to/video.mp4"
        );
    }

    #[test]
    fn test_normalize_uri_http_scheme() {
        let media_path = std::path::Path::new("/media");
        assert_eq!(
            normalize_uri("http://example.com/video.mp4", media_path),
            "http://example.com/video.mp4"
        );
    }

    #[test]
    fn test_normalize_uri_https_scheme() {
        let media_path = std::path::Path::new("/media");
        assert_eq!(
            normalize_uri("https://example.com/video.mp4", media_path),
            "https://example.com/video.mp4"
        );
    }

    #[test]
    fn test_normalize_uri_relative_path() {
        let media_path = std::path::Path::new("/media");
        let result = normalize_uri("video.mp4", media_path);
        assert!(result.starts_with("file://"));
        assert!(result.ends_with("video.mp4"));
    }

    #[test]
    fn test_normalize_uri_absolute_path() {
        let media_path = std::path::Path::new("/media");
        assert_eq!(
            normalize_uri("/tmp/video.mp4", media_path),
            "file:///tmp/video.mp4"
        );
    }

    #[test]
    fn test_registry_register_and_get() {
        let registry = MediaPlayerRegistry::new();
        let key = MediaPlayerKey {
            flow_id: Uuid::new_v4(),
            block_id: "test".to_string(),
        };

        assert!(!registry.contains(&key));

        let state = Arc::new(test_state(key.flow_id, "test", vec!["a.mp4".into()]));
        registry.register(key.clone(), Arc::clone(&state));
        assert!(registry.contains(&key));
        assert!(registry.get(&key).is_some());

        registry.unregister(&key);
        assert!(!registry.contains(&key));
    }

    #[test]
    fn test_state_playlist() {
        let state = test_state(Uuid::new_v4(), "t", vec![]);
        assert_eq!(state.playlist_len(), 0);
        assert!(state.current_file().is_none());

        state.set_playlist(vec!["a.mp4".into(), "b.mp4".into(), "c.mp4".into()]);
        assert_eq!(state.playlist_len(), 3);
        assert_eq!(state.current_file(), Some("a.mp4".to_string()));
    }

    #[test]
    fn test_set_playlist_clamps_index() {
        let state = test_state(
            Uuid::new_v4(),
            "t",
            vec!["a.mp4".into(), "b.mp4".into(), "c.mp4".into()],
        );
        // Advance index to 2 (last file)
        state.playlist.write().unwrap().current_index = 2;
        assert_eq!(state.current_file(), Some("c.mp4".to_string()));

        // Replace with shorter playlist — index should clamp to 0
        state.set_playlist(vec!["x.mp4".into()]);
        assert_eq!(state.current_index(), 0);
        assert_eq!(state.current_file(), Some("x.mp4".to_string()));
    }

    #[test]
    fn test_player_state() {
        use strom_types::mediaplayer::PlayerState;

        let state = test_state(Uuid::new_v4(), "t", vec![]);
        assert_eq!(state.state(), PlayerState::Stopped);

        state.set_playlist(vec!["file.mp4".into()]);
        assert_eq!(state.state(), PlayerState::Playing);

        state.is_paused.store(true, Ordering::SeqCst);
        assert_eq!(state.state(), PlayerState::Paused);
    }

    #[test]
    fn test_block_definition() {
        let def = definition::media_player_definition();

        assert_eq!(def.id, "builtin.media_player");
        assert_eq!(def.category, "Inputs");
        assert!(def.built_in);
        assert_eq!(def.exposed_properties.len(), 4);

        let decode = def
            .exposed_properties
            .iter()
            .find(|p| p.name == "decode")
            .unwrap();
        assert!(matches!(decode.property_type, PropertyType::Bool));
        assert!(matches!(
            decode.default_value,
            Some(PropertyValue::Bool(false))
        ));

        let sync = def
            .exposed_properties
            .iter()
            .find(|p| p.name == "sync")
            .unwrap();
        assert!(matches!(
            sync.default_value,
            Some(PropertyValue::Bool(true))
        ));

        assert!(def
            .exposed_properties
            .iter()
            .any(|p| p.name == "loop_playlist"));

        assert_eq!(def.external_pads.inputs.len(), 0);
        assert_eq!(def.external_pads.outputs.len(), 2);
        assert!(def
            .external_pads
            .outputs
            .iter()
            .any(|p| p.name == "video_out"));
        assert!(def
            .external_pads
            .outputs
            .iter()
            .any(|p| p.name == "audio_out"));
    }
}
