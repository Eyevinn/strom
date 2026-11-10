//! Guards the test layout: with `autotests = false` in `backend/Cargo.toml`,
//! Cargo builds only the test targets listed there, so a test file that is
//! neither listed nor a module of `tests/it/main.rs` is never compiled and its
//! tests never run, without a word.

use std::collections::BTreeSet;
use std::path::Path;

fn rs_stems(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "rs"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn every_test_file_is_built() {
    let backend = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo_toml = std::fs::read_to_string(backend.join("Cargo.toml")).expect("read Cargo.toml");
    let main_rs =
        std::fs::read_to_string(backend.join("tests/it/main.rs")).expect("read tests/it/main.rs");

    let unbuilt_targets: Vec<String> = rs_stems(&backend.join("tests"))
        .into_iter()
        .filter(|stem| !cargo_toml.contains(&format!("path = \"tests/{stem}.rs\"")))
        .collect();
    assert!(
        unbuilt_targets.is_empty(),
        "backend/tests/ files with no [[test]] target in backend/Cargo.toml, so never built: \
         {unbuilt_targets:?}. Move each into tests/it/ (the default), or give it a target and \
         say there why it needs a process of its own."
    );

    let unbuilt_modules: Vec<String> = rs_stems(&backend.join("tests/it"))
        .into_iter()
        .filter(|stem| stem != "main" && !main_rs.contains(&format!("pub mod {stem};")))
        .collect();
    assert!(
        unbuilt_modules.is_empty(),
        "tests/it/ files not declared in tests/it/main.rs, so never built: {unbuilt_modules:?}. \
         Add `pub mod <name>;` for each."
    );
}
