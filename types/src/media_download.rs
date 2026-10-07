//! Types for downloading a file from a URL into the media library.
//!
//! Shared by the backend (`POST /api/media/download` and the
//! `MediaDownloadProgress` WebSocket event) and the frontend Media tab.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Default largest file a URL download may write: 2 GiB.
pub const DEFAULT_MEDIA_DOWNLOAD_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Redirects followed before a download is refused.
pub const MEDIA_DOWNLOAD_MAX_REDIRECTS: usize = 5;

/// Shortest interval between two progress events for one download, in milliseconds.
pub const MEDIA_DOWNLOAD_PROGRESS_INTERVAL_MS: u64 = 250;

/// Request to download a file from a URL into the media library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct MediaDownloadRequest {
    /// The http or https URL to fetch.
    #[cfg_attr(feature = "validation", garde(length(min = 1, max = 8192)))]
    pub url: String,
    /// Directory to save into, relative to the media root. Empty means the root.
    #[serde(default)]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub path: String,
    /// File name to save as. When absent, the name comes from the response's
    /// `Content-Disposition` header, then from the URL's last path segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(length(min = 1, max = 255)))]
    pub filename: Option<String>,
    /// Replace an existing file of the same name. Without it, an existing file
    /// makes the request fail with 409 Conflict.
    #[serde(default)]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub overwrite: bool,
}

/// Where a URL download is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MediaDownloadState {
    /// Bytes are arriving.
    Downloading,
    /// The file is complete and in place under its final name.
    Done,
    /// The download stopped with an error; nothing was left behind.
    Failed,
    /// The download was cancelled; nothing was left behind.
    Cancelled,
}

impl MediaDownloadState {
    /// Whether the job has ended (done, failed or cancelled).
    pub fn is_finished(self) -> bool {
        !matches!(self, MediaDownloadState::Downloading)
    }
}

/// One URL download into the media library.
///
/// Carried by `POST /api/media/download`, `GET /api/media/downloads` and the
/// `MediaDownloadProgress` WebSocket event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MediaDownloadJob {
    /// Job identifier, used to cancel the download.
    pub job_id: String,
    /// The requested URL without user info, query or fragment, for display.
    pub url: String,
    /// Directory the file is saved into, relative to the media root.
    pub directory: String,
    /// File name the download is saved as.
    pub filename: String,
    /// Path of the finished file, relative to the media root.
    pub path: String,
    /// Bytes received so far.
    pub bytes: u64,
    /// Total size, when the server announced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Current state.
    pub state: MediaDownloadState,
    /// Why the download failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response listing active and recently finished URL downloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MediaDownloadListResponse {
    /// Downloads, oldest first.
    pub downloads: Vec<MediaDownloadJob>,
}
