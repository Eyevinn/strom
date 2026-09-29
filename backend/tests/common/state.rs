//! An `AppState` on throwaway storage.

use std::ops::Deref;
use std::path::Path;

use strom::state::AppState;
use strom::storage::JsonFileStorage;
use tempfile::TempDir;

/// An `AppState` whose flow and block storage live in a temp dir that is
/// removed when this is dropped. Derefs to the state.
pub struct TestState {
    state: AppState,
    _dir: TempDir,
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
    TestState { state, _dir: dir }
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
