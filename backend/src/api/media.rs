//! Media file listing API.

use axum::{extract::Query, Json};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Query parameters for listing media files.
#[derive(Debug, Deserialize)]
pub struct ListMediaQuery {
    /// Subdirectory path relative to media folder (optional).
    pub path: Option<String>,
}

/// A media file or folder entry.
#[derive(Debug, Serialize)]
pub struct MediaEntry {
    /// File or folder name.
    pub name: String,
    /// Full path relative to media folder.
    pub path: String,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
}

/// Response for listing media files.
#[derive(Debug, Serialize)]
pub struct ListMediaResponse {
    /// Current path relative to media folder.
    pub current_path: String,
    /// Parent path (None if at root).
    pub parent_path: Option<String>,
    /// List of entries in this directory.
    pub entries: Vec<MediaEntry>,
}

/// List files and folders in the media directory.
pub async fn list_media_files(
    Query(query): Query<ListMediaQuery>,
) -> Result<Json<ListMediaResponse>, (axum::http::StatusCode, String)> {
    let media_root = PathBuf::from("./media");

    // Ensure media directory exists
    if !media_root.exists() {
        // Create it if it doesn't exist
        std::fs::create_dir_all(&media_root).map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create media directory: {}", e),
            )
        })?;
    }

    // Get the requested subdirectory
    let relative_path = query.path.unwrap_or_default();
    let current_path = if relative_path.is_empty() {
        media_root.clone()
    } else {
        // Sanitize path to prevent directory traversal
        let sanitized = sanitize_path(&relative_path);
        media_root.join(&sanitized)
    };

    // Verify the path is within media root (prevent directory traversal)
    let canonical_root = media_root.canonicalize().map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to resolve media root: {}", e),
        )
    })?;

    let canonical_current = current_path.canonicalize().map_err(|_| {
        (
            axum::http::StatusCode::NOT_FOUND,
            "Directory not found".to_string(),
        )
    })?;

    if !canonical_current.starts_with(&canonical_root) {
        return Err((
            axum::http::StatusCode::FORBIDDEN,
            "Access denied".to_string(),
        ));
    }

    // Read directory entries
    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&current_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read directory: {}", e),
        )
    })?;

    for entry in read_dir.flatten() {
        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files
        if name.starts_with('.') {
            continue;
        }

        // For files, only include media files
        if !is_dir && !is_media_file(&name) {
            continue;
        }

        // Calculate relative path from media root
        let entry_path = entry.path();
        let rel_path = entry_path
            .strip_prefix(&media_root)
            .unwrap_or(&entry_path)
            .to_string_lossy()
            .to_string();

        entries.push(MediaEntry {
            name,
            path: rel_path,
            is_dir,
            size,
        });
    }

    // Sort: directories first, then alphabetically
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    // Calculate parent path
    let parent_path = if relative_path.is_empty() {
        None
    } else {
        let p = Path::new(&relative_path);
        p.parent()
            .map(|parent| parent.to_string_lossy().to_string())
    };

    Ok(Json(ListMediaResponse {
        current_path: relative_path,
        parent_path,
        entries,
    }))
}

/// Sanitize a path to prevent directory traversal attacks.
fn sanitize_path(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .collect::<Vec<_>>()
        .join("/")
}

/// Check if a file is a media file based on extension.
fn is_media_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    let media_extensions = [
        // Video
        ".mp4", ".mkv", ".avi", ".mov", ".webm", ".m4v", ".wmv", ".flv", ".ts", ".mts", ".m2ts",
        // Audio
        ".mp3", ".wav", ".flac", ".aac", ".ogg", ".m4a", ".wma", ".opus", // Containers
        ".mxf", ".mpg", ".mpeg",
    ];
    media_extensions.iter().any(|ext| lower.ends_with(ext))
}
