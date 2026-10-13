//! An `AppState` on throwaway storage.

use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use strom::state::AppState;
use strom::storage::JsonFileStorage;
use tempfile::TempDir;

/// An `AppState` whose flow and block storage live in a temp dir that is
/// removed when the last clone is dropped. Derefs to the state.
///
/// `Clone` is what keeps `common::state::new().clone()` safe: without it the
/// call derefs to `AppState::clone` and the temp dir is removed at once. Use
/// [`TestState::app`] where a handler needs an owned `AppState`.
#[derive(Clone)]
pub struct TestState {
    state: AppState,
    _dir: Arc<TempDir>,
}

impl TestState {
    /// An owned handle to the state, e.g. for `State(...)`. Keep the
    /// `TestState` alive while it is in use: the temp dir goes with it.
    pub fn app(&self) -> AppState {
        self.state.clone()
    }
}

impl Deref for TestState {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.state
    }
}

/// A fresh [`TestState`], with media under the system temp dir.
pub fn new() -> TestState {
    let dir = tempfile::tempdir().expect("state dir");
    let state = in_dir(dir.path(), &std::env::temp_dir());
    TestState {
        state,
        _dir: Arc::new(dir),
    }
}

/// An `AppState` on `flows.json` and `blocks.json` in `dir`. Build a second
/// one on the same `dir` to test what survives a restart.
pub fn in_dir(dir: &Path, media: &Path) -> AppState {
    AppState::new(
        JsonFileStorage::new(dir.join("flows.json")),
        dir.join("blocks.json"),
        media,
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    )
}
