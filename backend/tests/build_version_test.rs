//! Guards the build-arg fallback that gives published Docker images their git provenance.
//!
//! `build.rs` is a build script and so is not a test target. The part of it that chooses
//! between a build arg and the `git` shell-out therefore lives in `backend/build_version.rs`,
//! which both the build script and this test include — the test exercises that file itself,
//! not a copy of its logic.

include!("../build_version.rs");

const SET: &str = "STROM_TEST_PROVENANCE_SET";
const PADDED: &str = "STROM_TEST_PROVENANCE_PADDED";
const BLANK: &str = "STROM_TEST_PROVENANCE_BLANK";
const WHITESPACE: &str = "STROM_TEST_PROVENANCE_WHITESPACE";
const UNSET: &str = "STROM_TEST_PROVENANCE_UNSET";

#[test]
fn a_set_build_arg_is_preferred() {
    std::env::set_var(SET, "13722d5a");
    assert_eq!(version_field_from_env(SET), Some("13722d5a".to_string()));
}

#[test]
fn a_build_arg_is_trimmed() {
    std::env::set_var(PADDED, " 13722d5a\n");
    assert_eq!(version_field_from_env(PADDED), Some("13722d5a".to_string()));
}

#[test]
fn a_blank_build_arg_counts_as_absent() {
    // An ARG the caller never passed still reaches the builder stage, as an empty string.
    // Treating "set" as "usable" would embed "" and lose the git fallback entirely.
    std::env::set_var(BLANK, "");
    assert_eq!(version_field_from_env(BLANK), None);
}

#[test]
fn a_whitespace_only_build_arg_counts_as_absent() {
    std::env::set_var(WHITESPACE, "  \n");
    assert_eq!(version_field_from_env(WHITESPACE), None);
}

#[test]
fn an_unset_build_arg_is_absent() {
    std::env::remove_var(UNSET);
    assert_eq!(version_field_from_env(UNSET), None);
}
