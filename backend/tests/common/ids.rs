//! Block IDs that are unique within a test binary.
//!
//! Some block state is process-global and keyed by block ID alone (the vision
//! mixer's overlay registry, for one). Tests in one binary run in parallel, so
//! two tests building a block with the same ID race on that state.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn claimed() -> &'static Mutex<HashSet<String>> {
    static CLAIMED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CLAIMED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Claim `id` for this test. Panics if another test in this binary already
/// claimed it.
pub fn claim(id: &str) -> String {
    assert!(
        claimed().lock().unwrap().insert(id.to_string()),
        "block ID {id:?} is already used by another test in this binary; \
         block state keyed by ID would be shared between them"
    );
    id.to_string()
}
