//! Snapshot test for the OpenAPI specification.
//!
//! This test ensures that changes to the OpenAPI spec are intentional and reviewed.
//! If the spec changes, the test fails with instructions on how to update the snapshot.

use strom::openapi::openapi_spec;

/// Strip the `info.version` field from the OpenAPI JSON so that patch-level
/// version bumps (which don't change the API) don't cause snapshot failures.
fn strip_version(json: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json) {
        if let Some(info) = value.get_mut("info") {
            info.as_object_mut().map(|obj| obj.remove("version"));
        }
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| json.to_string())
    } else {
        json.to_string()
    }
}

#[test]
fn openapi_spec_snapshot() {
    let spec = openapi_spec();
    let json = spec
        .to_pretty_json()
        .expect("Failed to serialize OpenAPI spec");

    let snapshot_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Failed to find workspace root")
        .join("openapi.json");

    if !snapshot_path.exists() {
        std::fs::write(&snapshot_path, &json).expect("Failed to write snapshot");
        panic!(
            "OpenAPI snapshot did not exist and has been created at {}. \
             Commit it and re-run the test.",
            snapshot_path.display()
        );
    }

    let existing = std::fs::read_to_string(&snapshot_path).expect("Failed to read snapshot");

    // Compare without version field — version bumps without API changes should not fail
    if strip_version(&existing) != strip_version(&json) {
        let new_path = snapshot_path.with_extension("json.new");
        std::fs::write(&new_path, &json).expect("Failed to write new snapshot");
        panic!(
            "OpenAPI spec has changed!\n\
             Review the diff:\n  diff {} {}\n\
             If the change is intentional, update the snapshot:\n  \
             cp {} {}",
            snapshot_path.display(),
            new_path.display(),
            new_path.display(),
            snapshot_path.display()
        );
    }
}
