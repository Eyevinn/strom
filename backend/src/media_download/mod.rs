//! Download a file from a URL into the media library.
//!
//! `POST /api/media/download` vets the URL (see [`guard`]), waits for the
//! response headers, settles the file name and checks for a conflict, then
//! hands the body to a background task and answers 202 with the job. The task
//! streams into a hidden temporary file in the target directory and renames it
//! into place when complete, so the final name never shows a partial file.
//! Progress and the outcome go out as `StromEvent::MediaDownloadProgress`.

pub mod filename;
pub mod guard;

use crate::events::EventBroadcaster;
use guard::{HostResolver, SystemResolver};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom_types::media_download::{
    MediaDownloadJob, MediaDownloadRequest, MediaDownloadState, DEFAULT_MEDIA_DOWNLOAD_MAX_BYTES,
    MEDIA_DOWNLOAD_PROGRESS_INTERVAL_MS,
};
use strom_types::StromEvent;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

/// How long a finished job stays listed in `GET /api/media/downloads`.
const FINISHED_JOB_RETENTION: Duration = Duration::from_secs(60 * 60);
/// How many finished jobs stay listed at most.
const FINISHED_JOB_LIMIT: usize = 50;

/// Limits and policy for URL downloads.
#[derive(Debug, Clone)]
pub struct MediaDownloadSettings {
    /// Allow loopback, private, link-local and CGNAT addresses. For local and
    /// lab use; off by default. Cloud metadata and unroutable addresses stay
    /// refused either way.
    pub allow_private_addresses: bool,
    /// Largest file a download may write, in bytes.
    pub max_bytes: u64,
    /// Time allowed to resolve and connect.
    pub connect_timeout: Duration,
    /// Time allowed for the response headers once a request is sent.
    pub response_timeout: Duration,
    /// Time allowed between two reads of the body.
    pub read_timeout: Duration,
    /// Time allowed for the whole download.
    pub total_timeout: Duration,
}

/// Default total time allowed for one download.
pub const DEFAULT_TOTAL_TIMEOUT_SECS: u64 = 2 * 60 * 60;

impl Default for MediaDownloadSettings {
    fn default() -> Self {
        Self {
            allow_private_addresses: false,
            max_bytes: DEFAULT_MEDIA_DOWNLOAD_MAX_BYTES,
            connect_timeout: Duration::from_secs(10),
            response_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(DEFAULT_TOTAL_TIMEOUT_SECS),
        }
    }
}

/// Why a download could not start, or stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /// The request itself is wrong (URL, scheme, file name).
    BadRequest(String),
    /// The URL leads to an address downloads may not connect to.
    Forbidden(String),
    /// No such download.
    NotFound(String),
    /// The target file exists, or another download is writing it.
    Conflict(String),
    /// The file is larger than the configured limit.
    TooLarge(String),
    /// The remote server failed or answered with an error.
    Upstream(String),
    /// The remote server did not answer in time.
    Timeout(String),
    /// Something failed on this host (disk, client setup).
    Internal(String),
}

impl DownloadError {
    /// The HTTP status the API answers with.
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            DownloadError::BadRequest(_) => StatusCode::BAD_REQUEST,
            DownloadError::Forbidden(_) => StatusCode::FORBIDDEN,
            DownloadError::NotFound(_) => StatusCode::NOT_FOUND,
            DownloadError::Conflict(_) => StatusCode::CONFLICT,
            DownloadError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            DownloadError::Upstream(_) => StatusCode::BAD_GATEWAY,
            DownloadError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            DownloadError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The message for the operator.
    pub fn message(&self) -> &str {
        match self {
            DownloadError::BadRequest(m)
            | DownloadError::Forbidden(m)
            | DownloadError::NotFound(m)
            | DownloadError::Conflict(m)
            | DownloadError::TooLarge(m)
            | DownloadError::Upstream(m)
            | DownloadError::Timeout(m)
            | DownloadError::Internal(m) => m,
        }
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

struct JobEntry {
    job: MediaDownloadJob,
    final_path: PathBuf,
    cancel: CancellationToken,
    finished_at: Option<Instant>,
}

/// The URL downloads of one Strom: settings, resolver and jobs.
pub struct MediaDownloads {
    settings: parking_lot::RwLock<MediaDownloadSettings>,
    resolver: parking_lot::RwLock<Arc<dyn HostResolver>>,
    /// Jobs in start order.
    jobs: parking_lot::Mutex<Vec<JobEntry>>,
}

impl Default for MediaDownloads {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaDownloads {
    pub fn new() -> Self {
        Self {
            settings: parking_lot::RwLock::new(MediaDownloadSettings::default()),
            resolver: parking_lot::RwLock::new(Arc::new(SystemResolver)),
            jobs: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Replace the settings. Applies to downloads started afterwards.
    pub fn configure(&self, settings: MediaDownloadSettings) {
        if settings.allow_private_addresses {
            warn!("Media downloads may connect to private and local addresses");
        }
        *self.settings.write() = settings;
    }

    /// The current settings.
    pub fn settings(&self) -> MediaDownloadSettings {
        self.settings.read().clone()
    }

    /// Replace the host name resolver. Tests use this to resolve names
    /// without DNS; the server uses the operating system's resolver.
    pub fn set_resolver(&self, resolver: Arc<dyn HostResolver>) {
        *self.resolver.write() = resolver;
    }

    /// Active and recently finished downloads, oldest first.
    pub fn list(&self) -> Vec<MediaDownloadJob> {
        let mut jobs = self.jobs.lock();
        prune(&mut jobs);
        jobs.iter().map(|e| e.job.clone()).collect()
    }

    /// Ask a running download to stop. Its task removes the temporary file
    /// and reports `cancelled`.
    pub fn cancel(&self, job_id: &str) -> Result<(), DownloadError> {
        let jobs = self.jobs.lock();
        let entry = jobs
            .iter()
            .find(|e| e.job.job_id == job_id)
            .ok_or_else(|| DownloadError::NotFound(format!("No download {job_id}")))?;
        if entry.job.state.is_finished() {
            return Err(DownloadError::Conflict(format!(
                "Download {job_id} has already ended"
            )));
        }
        entry.cancel.cancel();
        Ok(())
    }

    /// Start a download into `target_dir`, which the caller has checked is an
    /// existing directory inside the media root (canonical). `directory` is
    /// the same directory relative to the media root.
    ///
    /// Returns once the response headers are in and the file is being
    /// written; the body is streamed by a background task.
    pub async fn start(
        self: &Arc<Self>,
        events: EventBroadcaster,
        target_dir: PathBuf,
        directory: String,
        request: MediaDownloadRequest,
    ) -> Result<MediaDownloadJob, DownloadError> {
        let settings = self.settings();
        let resolver = self.resolver.read().clone();

        let url = Url::parse(request.url.trim())
            .map_err(|e| DownloadError::BadRequest(format!("Invalid URL: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DownloadError::BadRequest(format!(
                "Only http and https URLs can be downloaded, not {}",
                url.scheme()
            )));
        }
        let shown_url = filename::display_url(&url);

        // An explicit name must already be a plain file name: refuse rather
        // than silently save under something else.
        let requested_name = match request.filename.as_deref().map(str::trim) {
            Some(name) => match filename::sanitize_filename(name) {
                Some(clean) if clean == name => Some(clean),
                _ => {
                    return Err(DownloadError::BadRequest(format!(
                        "Invalid file name: {name}"
                    )))
                }
            },
            None => None,
        };
        if let Some(name) = &requested_name {
            self.check_target(&target_dir.join(name), name, request.overwrite)?;
        }

        let (mut response, final_url) = guard::open(url, &settings, resolver.as_ref()).await?;

        let status = response.status();
        if !status.is_success() {
            return Err(DownloadError::Upstream(format!(
                "{} answered {}",
                filename::display_url(&final_url),
                status
            )));
        }

        let total = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if let Some(total) = total {
            if total > settings.max_bytes {
                return Err(DownloadError::TooLarge(format!(
                    "The file is {total} bytes, over the {} byte limit",
                    settings.max_bytes
                )));
            }
        }

        let name = requested_name
            .or_else(|| {
                response
                    .headers()
                    .get(reqwest::header::CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(filename::content_disposition_filename)
                    .and_then(|n| filename::sanitize_filename(&n))
            })
            .or_else(|| {
                filename::url_filename(&final_url).and_then(|n| filename::sanitize_filename(&n))
            })
            .unwrap_or_else(|| filename::FALLBACK_FILENAME.to_string());

        let final_path = target_dir.join(&name);
        let job_id = uuid::Uuid::new_v4().simple().to_string();
        let temp_path = target_dir.join(format!(
            "{}{}{}",
            filename::TEMP_PREFIX,
            job_id,
            filename::TEMP_SUFFIX
        ));
        let relative_path = if directory.is_empty() {
            name.clone()
        } else {
            format!("{directory}/{name}")
        };
        let job = MediaDownloadJob {
            job_id: job_id.clone(),
            url: shown_url,
            directory,
            filename: name.clone(),
            path: relative_path,
            bytes: 0,
            total,
            state: MediaDownloadState::Downloading,
            error: None,
        };
        let cancel = CancellationToken::new();

        // Check and register under one lock, so two downloads cannot both
        // claim the same name.
        let file = {
            let mut jobs = self.jobs.lock();
            check_target_locked(&jobs, &final_path, &name, request.overwrite)?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
                .map_err(|e| {
                    DownloadError::Internal(format!("Could not create a temporary file: {e}"))
                })?;
            prune(&mut jobs);
            jobs.push(JobEntry {
                job: job.clone(),
                final_path: final_path.clone(),
                cancel: cancel.clone(),
                finished_at: None,
            });
            file
        };

        info!(
            "Media download {} started: {} -> {}",
            job.job_id,
            job.url,
            final_path.display()
        );
        events.broadcast(StromEvent::MediaDownloadProgress(job.clone()));

        let this = Arc::clone(self);
        let task_job = job.clone();
        let overwrite = request.overwrite;
        tokio::spawn(async move {
            let mut job = task_job;
            let outcome = tokio::select! {
                _ = cancel.cancelled() => Err(None),
                result = stream_body(
                    &this,
                    &events,
                    &mut response,
                    tokio::fs::File::from_std(file),
                    &mut job,
                    settings.max_bytes,
                ) => result.map_err(Some),
            };
            drop(response);

            let outcome = match outcome {
                Ok(()) => place(&temp_path, &final_path, overwrite).map_err(Some),
                Err(e) => Err(e),
            };
            match outcome {
                Ok(()) => {
                    job.state = MediaDownloadState::Done;
                    info!(
                        "Media download {} done: {} bytes to {}",
                        job.job_id,
                        job.bytes,
                        final_path.display()
                    );
                }
                Err(error) => {
                    if let Err(e) = tokio::fs::remove_file(&temp_path).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            warn!(
                                "Could not remove {} after a failed download: {}",
                                temp_path.display(),
                                e
                            );
                        }
                    }
                    match error {
                        None => {
                            job.state = MediaDownloadState::Cancelled;
                            info!("Media download {} cancelled", job.job_id);
                        }
                        Some(error) => {
                            job.state = MediaDownloadState::Failed;
                            warn!("Media download {} failed: {}", job.job_id, error);
                            job.error = Some(error.message().to_string());
                        }
                    }
                }
            }
            this.update(&job, true);
            events.broadcast(StromEvent::MediaDownloadProgress(job));
        });

        Ok(job)
    }

    /// Refuse a target that exists (unless overwriting a file) or that another
    /// download is writing.
    fn check_target(
        &self,
        final_path: &Path,
        name: &str,
        overwrite: bool,
    ) -> Result<(), DownloadError> {
        check_target_locked(&self.jobs.lock(), final_path, name, overwrite)
    }

    fn update(&self, job: &MediaDownloadJob, finished: bool) {
        let mut jobs = self.jobs.lock();
        if let Some(entry) = jobs.iter_mut().find(|e| e.job.job_id == job.job_id) {
            entry.job = job.clone();
            if finished {
                entry.finished_at = Some(Instant::now());
            }
        }
    }
}

fn check_target_locked(
    jobs: &[JobEntry],
    final_path: &Path,
    name: &str,
    overwrite: bool,
) -> Result<(), DownloadError> {
    if jobs
        .iter()
        .any(|e| !e.job.state.is_finished() && e.final_path == final_path)
    {
        return Err(DownloadError::Conflict(format!(
            "Another download is already writing {name}"
        )));
    }
    match std::fs::symlink_metadata(final_path) {
        Ok(meta) if meta.is_dir() => Err(DownloadError::Conflict(format!("{name} is a directory"))),
        Ok(_) if !overwrite => Err(DownloadError::Conflict(format!(
            "{name} already exists; choose another name or overwrite it"
        ))),
        _ => Ok(()),
    }
}

/// Drop finished jobs that are old, or beyond the retention count.
fn prune(jobs: &mut Vec<JobEntry>) {
    jobs.retain(|e| {
        e.finished_at
            .is_none_or(|t| t.elapsed() < FINISHED_JOB_RETENTION)
    });
    let finished = jobs.iter().filter(|e| e.finished_at.is_some()).count();
    let mut excess = finished.saturating_sub(FINISHED_JOB_LIMIT);
    jobs.retain(|e| {
        if excess > 0 && e.finished_at.is_some() {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

/// Read the body into `file`, counting against the size limit and reporting
/// progress at most every `MEDIA_DOWNLOAD_PROGRESS_INTERVAL_MS`.
async fn stream_body(
    downloads: &MediaDownloads,
    events: &EventBroadcaster,
    response: &mut reqwest::Response,
    mut file: tokio::fs::File,
    job: &mut MediaDownloadJob,
    max_bytes: u64,
) -> Result<(), DownloadError> {
    let interval = Duration::from_millis(MEDIA_DOWNLOAD_PROGRESS_INTERVAL_MS);
    let mut last_report = Instant::now();

    loop {
        let chunk = response.chunk().await.map_err(|e| {
            if e.is_timeout() {
                DownloadError::Timeout("The server stopped sending data".to_string())
            } else {
                DownloadError::Upstream(format!("Download interrupted: {}", guard::error_chain(&e)))
            }
        })?;
        let Some(chunk) = chunk else { break };

        job.bytes += chunk.len() as u64;
        if job.bytes > max_bytes {
            return Err(DownloadError::TooLarge(format!(
                "The file is larger than the {max_bytes} byte limit"
            )));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| DownloadError::Internal(format!("Could not write the file: {e}")))?;

        if last_report.elapsed() >= interval {
            last_report = Instant::now();
            downloads.update(job, false);
            events.broadcast(StromEvent::MediaDownloadProgress(job.clone()));
        }
    }

    if let Some(total) = job.total {
        if job.bytes != total {
            return Err(DownloadError::Upstream(format!(
                "Received {} of {} bytes",
                job.bytes, total
            )));
        }
    }
    file.flush()
        .await
        .and(file.sync_all().await)
        .map_err(|e| DownloadError::Internal(format!("Could not write the file: {e}")))?;
    Ok(())
}

/// Move the finished temporary file to its final name in one step. Without
/// `overwrite`, never replace a file that appeared meanwhile.
fn place(temp: &Path, final_path: &Path, overwrite: bool) -> Result<(), DownloadError> {
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let io_error = |e: std::io::Error| {
        DownloadError::Internal(format!("Could not move the file into place: {e}"))
    };

    if overwrite {
        return std::fs::rename(temp, final_path).map_err(io_error);
    }
    // A hard link fails if the name exists, which a rename would not.
    match std::fs::hard_link(temp, final_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(temp);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(DownloadError::Conflict(
            format!("{name} appeared while downloading; nothing was overwritten"),
        )),
        // Filesystems without hard links: check, then rename.
        Err(_) => {
            if final_path.exists() {
                Err(DownloadError::Conflict(format!(
                    "{name} appeared while downloading; nothing was overwritten"
                )))
            } else {
                std::fs::rename(temp, final_path).map_err(io_error)
            }
        }
    }
}
