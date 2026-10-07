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
mod timing;

pub use builder::MediaPlayerBuilder;
pub use definition::get_blocks;
pub use state::{MediaPlayerKey, MediaPlayerState, MEDIA_PLAYER_REGISTRY};

use std::path::Path;
use tracing::debug;

/// Normalize a file path to a proper URI.
///
/// Converts relative paths to absolute file:// URIs resolved against `media_path`.
/// Passes through anything that already is a URI - `scheme://...` with any
/// scheme GStreamer may have a source for (file, http(s) including HLS and DASH,
/// rtsp, srt, udp, rtmp, ...).
///
/// Relative paths are resolved relative to `media_path` (the configured media directory).
/// Legacy paths starting with `./media/` have that prefix stripped before resolution.
/// Whether `s` starts with an RFC 3986 scheme followed by `://`: a letter, then
/// letters, digits, `+`, `-` or `.`.
fn has_uri_scheme(s: &str) -> bool {
    let Some((scheme, _)) = s.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

pub fn normalize_uri(path: &str, media_path: &Path) -> String {
    // If it already has a scheme, pass through. Only file, http and https
    // used to, so an rtsp:// or srt:// URL became a path in the media
    // directory.
    if has_uri_scheme(path) {
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

    let uri = file_uri(&resolved);
    debug!("Normalized '{}' → '{}'", path, uri);
    uri
}

/// A `file://` URI for `path`. `glib::filename_to_uri` gets a Windows drive
/// and backslashes right (`file:///C:/media/clip.mp4`) and escapes spaces and
/// the like; pasting `path.display()` after `file://` gave
/// `file://C:\media\clip.mp4` on Windows, which no source element accepts. A
/// path it refuses (not absolute) keeps the old form.
pub(super) fn file_uri(path: &Path) -> String {
    gstreamer::glib::filename_to_uri(path, None)
        .map(|uri| uri.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::state::{
        MediaPlayerKey, MediaPlayerRegistry, MediaPlayerState, Playlist,
    };
    use gstreamer as gst;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, RwLock};
    use uuid::Uuid;

    /// Helper to create a MediaPlayerState for testing (no GStreamer elements).
    fn test_state(flow_id: Uuid, block_id: &str, playlist: Vec<String>) -> MediaPlayerState {
        MediaPlayerState {
            instance_id: Uuid::new_v4(),
            source_element: gst::glib::WeakRef::new(),
            internal_pipeline: RwLock::new(None),
            video_appsrcs: Vec::new(),
            audio_appsrcs: Vec::new(),
            playlist: RwLock::new(Playlist {
                files: playlist,
                current_index: 0,
            }),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(true),
            block_id: block_id.to_string(),
            flow_id,
            control: std::sync::Mutex::new(()),
            switch_generation: std::sync::atomic::AtomicU64::new(0),
            source_ready: std::sync::Mutex::new(true),
            source_ready_cv: std::sync::Condvar::new(),
            starting: std::sync::Mutex::new(None),
            starting_cv: std::sync::Condvar::new(),
            video_slots: MediaPlayerState::free_slots(0),
            audio_slots: MediaPlayerState::free_slots(0),
            decode: false,
            sync: true,
            media_path: std::path::PathBuf::from("/media"),
            timing: Arc::new(super::timing::Timing::new(0)),
            main_pipeline: gst::glib::WeakRef::new(),
            bus_watch: std::sync::Mutex::new(None),
        }
    }

    /// A file URI has to turn back into the same path, on every platform. On
    /// Windows `file://` plus the path gave `file://C:\\...`, which neither
    /// this nor any source element accepts, so no local file played there.
    #[test]
    fn a_file_uri_turns_back_into_its_path() {
        let path = std::env::temp_dir().join("strom media").join("my clip.mkv");
        let uri = file_uri(&path);
        assert!(uri.starts_with("file:///"), "{}", uri);
        let (back, _) = gstreamer::glib::filename_from_uri(&uri)
            .unwrap_or_else(|e| panic!("{} is not a valid file URI: {}", uri, e));
        assert_eq!(back, path, "{}", uri);
    }

    #[test]
    fn test_normalize_uri() {
        // A media dir that does not exist, so canonicalize() leaves paths alone
        let media_path = std::path::Path::new("/nonexistent-strom-media");
        let in_media = file_uri(&media_path.join("video.mp4"));

        let cases: Vec<(&str, String)> = vec![
            // URIs with a scheme pass through
            (
                "file:///path/to/video.mp4",
                "file:///path/to/video.mp4".into(),
            ),
            (
                "http://example.com/video.mp4",
                "http://example.com/video.mp4".into(),
            ),
            (
                "https://example.com/video.mp4",
                "https://example.com/video.mp4".into(),
            ),
            (
                "https://example.com/live/master.m3u8?format=hls",
                "https://example.com/live/master.m3u8?format=hls".into(),
            ),
            (
                "rtsp://192.0.2.10:8554/stream",
                "rtsp://192.0.2.10:8554/stream".into(),
            ),
            (
                "srt://192.0.2.10:9000?mode=caller",
                "srt://192.0.2.10:9000?mode=caller".into(),
            ),
            ("udp://239.0.0.1:5000", "udp://239.0.0.1:5000".into()),
            // Relative paths resolve against media_path, legacy prefixes stripped
            ("video.mp4", in_media.clone()),
            ("./media/video.mp4", in_media.clone()),
            ("media/video.mp4", in_media),
            // Absolute paths are kept
            (
                "/nonexistent-strom-abs/video.mp4",
                "file:///nonexistent-strom-abs/video.mp4".into(),
            ),
        ];
        // Not a scheme: a file whose name merely contains "://" further on
        let odd = "my video ://.mp4";
        let odd_expected = file_uri(&media_path.join(odd));
        for (input, expected) in cases.into_iter().chain([(odd, odd_expected)]) {
            assert_eq!(normalize_uri(input, media_path), expected, "{}", input);
        }
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
}
