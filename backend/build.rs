use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

fn main() {
    // Set version and build information
    set_version_info();

    // Embed Windows resources (icon, version info)
    #[cfg(windows)]
    embed_windows_resources();

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
    // Rerun if static assets change (WHEP player, etc.)
    println!("cargo:rerun-if-changed=static");
    // Rerun if icon assets change (favicons, app icons)
    println!("cargo:rerun-if-changed=../assets");
    // Rerun if .git/HEAD changes (new commits)
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
    // Rerun if Windows icon changes
    println!("cargo:rerun-if-changed=strom.ico");

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

        // Create dist directory with a placeholder index.html
        let dist_dir = PathBuf::from("dist");
        if !dist_dir.exists() {
            fs::create_dir(&dist_dir).expect("Failed to create dist directory");
        }
        fs::write(dist_dir.join("index.html"), generate_placeholder_html())
            .expect("Failed to create placeholder index.html");

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

/// Generate a placeholder HTML page shown when the WASM frontend was not compiled.
/// This happens when `trunk` is not installed or the `wasm32-unknown-unknown` target is missing.
fn generate_placeholder_html() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Strom - Frontend Not Available</title>
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            background: #1b1b1b;
            color: #8c8c8c;
            min-height: 100vh;
            display: flex;
            align-items: center;
            justify-content: center;
        }
        .container {
            max-width: 640px;
            padding: 2.5rem;
            background: #1b1b1b;
            border-radius: 12px;
            border: 1px solid #3c3c3c;
        }
        h1 { color: #ffffff; margin-bottom: 1rem; font-size: 1.5rem; }
        p { line-height: 1.7; margin-bottom: 1rem; color: #8c8c8c; }
        .reason { color: #8c8c8c; }
        code {
            background: #0a0a0a;
            padding: 0.15em 0.4em;
            border-radius: 4px;
            font-size: 0.9em;
            color: #5aaaff;
        }
        pre {
            background: #0a0a0a;
            padding: 1rem;
            border-radius: 8px;
            overflow-x: auto;
            margin: 1rem 0;
            line-height: 1.6;
        }
        pre code { background: none; padding: 0; }
        a { color: #5aaaff; text-decoration: none; }
        a:hover { text-decoration: underline; }
        .note {
            margin-top: 1.5rem;
            padding-top: 1rem;
            border-top: 1px solid #3c3c3c;
            font-size: 0.9rem;
            color: #545454;
        }
    </style>
</head>
<body>
    <div class="container">
        <h1>Web GUI Not Available</h1>
        <p>
            The Strom backend is running, but the web-based frontend (WASM) was not
            included in this build.
        </p>
        <p class="reason">This usually means one of the following:</p>
        <ul style="margin: 0.5rem 0 1rem 1.5rem; line-height: 2; color: #b0b0b0;">
            <li><code>trunk</code> was not installed when the backend was compiled</li>
            <li>The <code>wasm32-unknown-unknown</code> Rust target was not installed</li>
        </ul>
        <p>To build with the web frontend, install the required tools and recompile:</p>
        <pre><code>rustup target add wasm32-unknown-unknown
cargo install trunk
cargo build --release</code></pre>
        <p>
            See the
            <a href="https://github.com/Eyevinn/strom#readme" target="_blank" rel="noopener">
                Strom repository
            </a>
            for full setup instructions.
        </p>
        <p class="note">
            The REST API and native GUI (if enabled) are still fully functional.
        </p>
    </div>
</body>
</html>"#
        .to_string()
}

/// Embed Windows resources (icon, version info) into the executable
#[cfg(windows)]
fn embed_windows_resources() {
    use winresource::WindowsResource;

    let mut res = WindowsResource::new();

    // Set the application icon
    res.set_icon("strom.ico");

    // Set version information
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    res.set("ProductName", "Strom");
    res.set("FileDescription", "Strom - GStreamer Flow Engine");
    res.set("CompanyName", "Eyevinn Technology");
    res.set("LegalCopyright", "Copyright (c) Eyevinn Technology");
    res.set("ProductVersion", &version);
    res.set("FileVersion", &version);

    if let Err(e) = res.compile() {
        eprintln!("cargo:warning=Failed to compile Windows resources: {}", e);
    }
}
