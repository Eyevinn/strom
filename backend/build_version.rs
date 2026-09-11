// Included by `build.rs` and by `tests/build_version_test.rs`. Build scripts are not test
// targets, so anything about them that needs a guard has to live in a file a test can include.

/// Read a version field supplied at build time, e.g. by a Docker build arg.
///
/// A declared `ARG` the caller never passed arrives as an empty string rather than staying
/// unset, so blank is treated as absent and the caller falls back to `git`.
fn version_field_from_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
