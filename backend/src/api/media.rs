//! Media file management API handlers.

use axum::{
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::Response,
    Json,
};
use serde::Deserialize;
use std::path::{Path as StdPath, PathBuf};
use strom_types::api::{
    CreateDirectoryRequest, ErrorResponse, ListMediaResponse, MediaFileEntry,
    MediaOperationResponse, RenameMediaRequest,
};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

use crate::state::AppState;

/// Query parameters for listing media.
#[derive(Debug, Deserialize)]
pub struct ListMediaQuery {
    /// Path relative to media root (empty or "/" for root)
    #[serde(default)]
    pub path: String,
}

/// Query parameters for uploading media.
#[derive(Debug, Deserialize)]
pub struct UploadMediaQuery {
    /// Directory path to upload to (relative to media root)
    #[serde(default)]
    pub path: String,
}

/// Validate and resolve a path against the media root.
/// Returns the full path if valid, or an error status if the path is invalid.
fn validate_path(
    media_root: &StdPath,
    relative_path: &str,
) -> Result<PathBuf, (StatusCode, Json<ErrorResponse>)> {
    // Clean the path: remove leading slashes and any path traversal attempts
    let cleaned = relative_path
        .trim_start_matches('/')
        .replace("../", "")
        .replace("..\\", "");

    // Empty path means root
    let full_path = if cleaned.is_empty() {
        media_root.to_path_buf()
    } else {
        media_root.join(&cleaned)
    };

    // For existing paths, canonicalize and verify
    if full_path.exists() {
        match full_path.canonicalize() {
            Ok(canonical) => {
                if let Ok(root_canonical) = media_root.canonicalize() {
                    if canonical.starts_with(&root_canonical) {
                        return Ok(canonical);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to canonicalize path: {}", e);
            }
        }
    } else {
        // For new paths that don't exist yet, verify the parent exists and is valid
        if let Some(parent) = full_path.parent() {
            if parent.exists() {
                if let Ok(parent_canonical) = parent.canonicalize() {
                    if let Ok(root_canonical) = media_root.canonicalize() {
                        if parent_canonical.starts_with(&root_canonical) {
                            return Ok(full_path);
                        }
                    }
                }
            }
        }
    }

    Err((
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("Invalid path")),
    ))
}

/// Get the MIME type for a file based on its extension.
fn get_mime_type(path: &StdPath) -> Option<String> {
    let extension = path.extension()?.to_str()?;
    let mime = match extension.to_lowercase().as_str() {
        // Video
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" | "mts" | "m2ts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        // Audio
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "opus" => "audio/opus",
        // Images
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        // Documents
        "pdf" => "application/pdf",
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "srt" => "text/plain",
        "vtt" => "text/vtt",
        "sdp" => "application/sdp",
        _ => "application/octet-stream",
    };
    Some(mime.to_string())
}

/// List contents of a media directory.
#[utoipa::path(
    get,
    path = "/api/media",
    tag = "Media",
    params(
        ("path" = Option<String>, Query, description = "Directory path relative to media root")
    ),
    responses(
        (status = 200, description = "Directory listing", body = ListMediaResponse),
        (status = 400, description = "Invalid path", body = ErrorResponse),
        (status = 404, description = "Directory not found", body = ErrorResponse)
    )
)]
pub async fn list_media(
    State(state): State<AppState>,
    Query(query): Query<ListMediaQuery>,
) -> Result<Json<ListMediaResponse>, (StatusCode, Json<ErrorResponse>)> {
    let media_root = state.media_path();
    let dir_path = validate_path(media_root, &query.path)?;

    if !dir_path.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Directory not found")),
        ));
    }

    let mut entries = Vec::new();

    // Canonicalize media_root once for path comparisons (dir_path is already canonicalized)
    let canonical_root = media_root
        .canonicalize()
        .unwrap_or_else(|_| media_root.to_path_buf());

    let mut read_dir = fs::read_dir(&dir_path).await.map_err(|e| {
        error!("Failed to read directory: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Failed to read directory")),
        )
    })?;

    while let Some(entry) = read_dir.next_entry().await.map_err(|e| {
        error!("Failed to read directory entry: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Failed to read directory entry")),
        )
    })? {
        let path = entry.path();
        let metadata = entry.metadata().await.map_err(|e| {
            error!("Failed to read metadata: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("Failed to read file metadata")),
            )
        })?;

        let name = entry.file_name().to_string_lossy().to_string();
        let is_directory = metadata.is_dir();

        // Calculate relative path from media root
        let relative_path = path
            .strip_prefix(&canonical_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| name.clone());

        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mime_type = if is_directory {
            None
        } else {
            get_mime_type(&path)
        };

        entries.push(MediaFileEntry {
            name,
            path: relative_path,
            is_directory,
            size: if is_directory { 0 } else { metadata.len() },
            modified,
            mime_type,
        });
    }

    // Sort: directories first, then by name
    entries.sort_by(|a, b| match (a.is_directory, b.is_directory) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    // Calculate current path relative to media root
    let current_path = dir_path
        .strip_prefix(&canonical_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // Calculate parent path
    let parent_path = if current_path.is_empty() {
        None
    } else {
        let parent = StdPath::new(&current_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        Some(parent)
    };

    Ok(Json(ListMediaResponse {
        current_path,
        parent_path,
        entries,
    }))
}

/// Download a file.
#[utoipa::path(
    get,
    path = "/api/media/file/{path}",
    tag = "Media",
    params(
        ("path" = String, Path, description = "File path relative to media root")
    ),
    responses(
        (status = 200, description = "File content"),
        (status = 400, description = "Invalid path", body = ErrorResponse),
        (status = 404, description = "File not found", body = ErrorResponse)
    )
)]
pub async fn download_file(
    State(state): State<AppState>,
    Path(file_path): Path<String>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let media_root = state.media_path();
    let full_path = validate_path(media_root, &file_path)?;

    if !full_path.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("File not found")),
        ));
    }

    let file = fs::File::open(&full_path).await.map_err(|e| {
        error!("Failed to open file: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Failed to open file")),
        )
    })?;

    let metadata = file.metadata().await.map_err(|e| {
        error!("Failed to read file metadata: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Failed to read file metadata")),
        )
    })?;

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let filename = full_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());

    let content_type =
        get_mime_type(&full_path).unwrap_or_else(|| "application/octet-stream".to_string());

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, metadata.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(body)
        .map_err(|e| {
            error!("Failed to build response: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("Failed to build response")),
            )
        })?;

    Ok(response)
}

/// Upload files to a directory.
#[utoipa::path(
    post,
    path = "/api/media/upload",
    tag = "Media",
    params(
        ("path" = Option<String>, Query, description = "Directory path to upload to")
    ),
    responses(
        (status = 200, description = "Upload successful", body = MediaOperationResponse),
        (status = 400, description = "Invalid path or upload", body = ErrorResponse),
        (status = 500, description = "Upload failed", body = ErrorResponse)
    )
)]
pub async fn upload_files(
    State(state): State<AppState>,
    Query(query): Query<UploadMediaQuery>,
    mut multipart: Multipart,
) -> Result<Json<MediaOperationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let media_root = state.media_path();
    let target_dir = validate_path(media_root, &query.path)?;

    if !target_dir.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("Target directory does not exist")),
        ));
    }

    let mut uploaded_count = 0;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        error!("Failed to read multipart field: {}", e);
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("Failed to read upload")),
        )
    })? {
        let filename = match field.file_name() {
            Some(name) => {
                // Sanitize filename: remove path separators
                let name = name.replace(['/', '\\'], "_");
                if name.is_empty() || name.starts_with('.') {
                    continue;
                }
                name
            }
            None => continue,
        };

        let file_path = target_dir.join(&filename);

        // Verify the file path is still within media root
        if let Ok(canonical_parent) = target_dir.canonicalize() {
            if let Ok(root_canonical) = media_root.canonicalize() {
                if !canonical_parent.starts_with(&root_canonical) {
                    continue;
                }
            }
        }

        let data = field.bytes().await.map_err(|e| {
            error!("Failed to read file data: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("Failed to read file data")),
            )
        })?;

        let mut file = fs::File::create(&file_path).await.map_err(|e| {
            error!("Failed to create file: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("Failed to create file")),
            )
        })?;

        file.write_all(&data).await.map_err(|e| {
            error!("Failed to write file: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("Failed to write file")),
            )
        })?;

        info!("Uploaded file: {}", file_path.display());
        uploaded_count += 1;
    }

    Ok(Json(MediaOperationResponse::success(format!(
        "Uploaded {} file(s)",
        uploaded_count
    ))))
}

/// Rename a file or directory.
#[utoipa::path(
    post,
    path = "/api/media/rename",
    tag = "Media",
    request_body = RenameMediaRequest,
    responses(
        (status = 200, description = "Rename successful", body = MediaOperationResponse),
        (status = 400, description = "Invalid path or name", body = ErrorResponse),
        (status = 404, description = "File or directory not found", body = ErrorResponse)
    )
)]
pub async fn rename_media(
    State(state): State<AppState>,
    Json(request): Json<RenameMediaRequest>,
) -> Result<Json<MediaOperationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let media_root = state.media_path();
    let old_path = validate_path(media_root, &request.old_path)?;

    if !old_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("File or directory not found")),
        ));
    }

    // Validate new name: no path separators allowed
    let new_name = request.new_name.trim();
    if new_name.is_empty() || new_name.contains(['/', '\\']) || new_name.starts_with('.') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("Invalid new name")),
        ));
    }

    let new_path = old_path
        .parent()
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("Invalid path")),
            )
        })?
        .join(new_name);

    if new_path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "A file or directory with that name already exists",
            )),
        ));
    }

    fs::rename(&old_path, &new_path).await.map_err(|e| {
        error!("Failed to rename: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Failed to rename")),
        )
    })?;

    info!("Renamed: {} -> {}", old_path.display(), new_path.display());

    Ok(Json(MediaOperationResponse::success(format!(
        "Renamed to {}",
        new_name
    ))))
}

/// Delete a file.
#[utoipa::path(
    delete,
    path = "/api/media/file/{path}",
    tag = "Media",
    params(
        ("path" = String, Path, description = "File path relative to media root")
    ),
    responses(
        (status = 200, description = "Delete successful", body = MediaOperationResponse),
        (status = 400, description = "Invalid path", body = ErrorResponse),
        (status = 404, description = "File not found", body = ErrorResponse)
    )
)]
pub async fn delete_file(
    State(state): State<AppState>,
    Path(file_path): Path<String>,
) -> Result<Json<MediaOperationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let media_root = state.media_path();
    let full_path = validate_path(media_root, &file_path)?;

    if !full_path.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("File not found")),
        ));
    }

    let filename = full_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    fs::remove_file(&full_path).await.map_err(|e| {
        error!("Failed to delete file: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Failed to delete file")),
        )
    })?;

    info!("Deleted file: {}", full_path.display());

    Ok(Json(MediaOperationResponse::success(format!(
        "Deleted {}",
        filename
    ))))
}

/// Create a directory.
#[utoipa::path(
    post,
    path = "/api/media/directory",
    tag = "Media",
    request_body = CreateDirectoryRequest,
    responses(
        (status = 200, description = "Directory created", body = MediaOperationResponse),
        (status = 400, description = "Invalid path", body = ErrorResponse),
        (status = 409, description = "Directory already exists", body = ErrorResponse)
    )
)]
pub async fn create_directory(
    State(state): State<AppState>,
    Json(request): Json<CreateDirectoryRequest>,
) -> Result<Json<MediaOperationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let media_root = state.media_path();
    let dir_path = validate_path(media_root, &request.path)?;

    if dir_path.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new("Directory already exists")),
        ));
    }

    fs::create_dir_all(&dir_path).await.map_err(|e| {
        error!("Failed to create directory: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Failed to create directory")),
        )
    })?;

    info!("Created directory: {}", dir_path.display());

    let dirname = dir_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    Ok(Json(MediaOperationResponse::success(format!(
        "Created directory {}",
        dirname
    ))))
}

/// Delete a directory (must be empty).
#[utoipa::path(
    delete,
    path = "/api/media/directory/{path}",
    tag = "Media",
    params(
        ("path" = String, Path, description = "Directory path relative to media root")
    ),
    responses(
        (status = 200, description = "Delete successful", body = MediaOperationResponse),
        (status = 400, description = "Invalid path or directory not empty", body = ErrorResponse),
        (status = 404, description = "Directory not found", body = ErrorResponse)
    )
)]
pub async fn delete_directory(
    State(state): State<AppState>,
    Path(dir_path): Path<String>,
) -> Result<Json<MediaOperationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let media_root = state.media_path();
    let full_path = validate_path(media_root, &dir_path)?;

    // Don't allow deleting the media root itself
    if let (Ok(full_canonical), Ok(root_canonical)) =
        (full_path.canonicalize(), media_root.canonicalize())
    {
        if full_canonical == root_canonical {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("Cannot delete the root media directory")),
            ));
        }
    }

    if !full_path.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Directory not found")),
        ));
    }

    let dirname = full_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Try to remove the directory - will fail if not empty
    fs::remove_dir(&full_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
            || e.to_string().contains("not empty")
            || e.to_string().contains("Directory not empty")
        {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("Directory is not empty")),
            )
        } else {
            error!("Failed to delete directory: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("Failed to delete directory")),
            )
        }
    })?;

    info!("Deleted directory: {}", full_path.display());

    Ok(Json(MediaOperationResponse::success(format!(
        "Deleted directory {}",
        dirname
    ))))
}
