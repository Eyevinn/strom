use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

fn main() {
    // Set version and build information
    set_version_info();

    let frontend_dir = PathBuf::from("../frontend");
    let dist_dir = PathBuf::from("dist");
    let hash_file = PathBuf::from(".frontend-build-hash");

    // Tell cargo to rerun this build script if frontend files change
    println!("cargo:rerun-if-changed=../frontend/src");
    println!("cargo:rerun-if-changed=../frontend/Cargo.toml");
    println!("cargo:rerun-if-changed=../frontend/index.html");
    println!("cargo:rerun-if-changed=../frontend/Trunk.toml");
    // Also rerun if dist directory changes (manual builds)
    println!("cargo:rerun-if-changed=dist");
    // Rerun if .git/HEAD changes (new commits)
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");

    // Compute current hash of frontend sources
    let current_hash = compute_frontend_hash(&frontend_dir);

    // Check if rebuild is needed
    let needs_rebuild = !dist_dir.exists()
        || is_dir_empty(&dist_dir).unwrap_or(true)
        || hash_changed(&hash_file, current_hash);

    if needs_rebuild {
        println!(
            "cargo:warning=Frontend code changed or dist missing - rebuilding WASM with trunk..."
        );
        build_frontend(&frontend_dir);
        save_hash(&hash_file, current_hash);
        println!("cargo:warning=WASM build complete");
    } else {
        println!("cargo:warning=Frontend unchanged - skipping WASM rebuild");
    }
}

/// Set version and build information as environment variables
fn set_version_info() {
    // Get git commit hash (short)
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Get git tag (if on a tagged commit)
    let git_tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // Get build timestamp (ISO 8601 format)
    let build_timestamp = chrono::Utc::now().to_rfc3339();

    // Get git branch
    let git_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Check if working directory is dirty
    let git_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);

    // Generate a unique build ID (UUID) for this build
    // This is used by the frontend to detect when the backend has been rebuilt
    // and trigger a reload to ensure frontend/backend are in sync
    let build_id = Uuid::new_v4().to_string();

    // Set environment variables for compile-time embedding
    println!("cargo:rustc-env=GIT_HASH={}", git_hash);
    println!("cargo:rustc-env=GIT_TAG={}", git_tag);
    println!("cargo:rustc-env=GIT_BRANCH={}", git_branch);
    println!("cargo:rustc-env=GIT_DIRTY={}", git_dirty);
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", build_timestamp);
    println!("cargo:rustc-env=BUILD_ID={}", build_id);

    // Print warnings for visibility during build
    println!(
        "cargo:warning=Building version: {} ({})",
        if git_tag.is_empty() {
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string())
        } else {
            git_tag.clone()
        },
        git_hash
    );
}

/// Compute a hash of all frontend source files
fn compute_frontend_hash(frontend_dir: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();

    // Hash all .rs files in src/
    let src_dir = frontend_dir.join("src");
    if src_dir.exists() {
        hash_directory(&src_dir, &mut hasher);
    }

    // Hash configuration files
    hash_file(&frontend_dir.join("Cargo.toml"), &mut hasher);
    hash_file(&frontend_dir.join("index.html"), &mut hasher);
    hash_file(&frontend_dir.join("Trunk.toml"), &mut hasher);

    hasher.finish()
}

/// Recursively hash all files in a directory
fn hash_directory(dir: &Path, hasher: &mut DefaultHasher) {
    if let Ok(entries) = fs::read_dir(dir) {
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        // Sort for deterministic hashing
        entries.sort_by_key(|e| e.path());

        for entry in entries {
            let path = entry.path();
            if path.is_file() {
                hash_file(&path, hasher);
            } else if path.is_dir() {
                hash_directory(&path, hasher);
            }
        }
    }
}

/// Hash a single file's contents
fn hash_file(path: &Path, hasher: &mut DefaultHasher) {
    if let Ok(contents) = fs::read(path) {
        // Hash the path name for uniqueness
        path.to_string_lossy().hash(hasher);
        // Hash the contents
        contents.hash(hasher);
    }
}

/// Check if the hash has changed since last build
fn hash_changed(hash_file: &Path, current_hash: u64) -> bool {
    match fs::read_to_string(hash_file) {
        Ok(stored_hash_str) => {
            if let Ok(stored_hash) = stored_hash_str.trim().parse::<u64>() {
                stored_hash != current_hash
            } else {
                true // Invalid hash file, rebuild
            }
        }
        Err(_) => true, // No hash file, rebuild
    }
}

/// Save the current hash to file
fn save_hash(hash_file: &Path, hash: u64) {
    let _ = fs::write(hash_file, hash.to_string());
}

/// Check if a directory is empty
fn is_dir_empty(dir: &Path) -> Result<bool, std::io::Error> {
    Ok(fs::read_dir(dir)?.next().is_none())
}

/// Build the frontend using trunk
fn build_frontend(frontend_dir: &Path) {
    // Check if trunk is available
    let trunk_check = Command::new("trunk").arg("--version").output();

    if trunk_check.is_err() {
        println!("cargo:warning=trunk not found - skipping frontend build");
        println!("cargo:warning=Install trunk with: cargo install trunk");
        println!("cargo:warning=Backend will compile without embedded frontend");

        // Create empty dist directory with placeholder
        let dist_dir = PathBuf::from("dist");
        if !dist_dir.exists() {
            fs::create_dir(&dist_dir).expect("Failed to create dist directory");
        }
        fs::write(
            dist_dir.join(".placeholder"),
            "Frontend not built - trunk not available during compilation",
        )
        .expect("Failed to create placeholder file");

        return;
    }

    let status = Command::new("trunk")
        .arg("build")
        .arg("--release")
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to execute trunk build. Make sure trunk is installed: cargo install trunk");

    if !status.success() {
        panic!("trunk build failed");
    }
}
