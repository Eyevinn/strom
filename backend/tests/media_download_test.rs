//! URL downloads into the media library (`POST /api/media/download`).
//!
//! Each test runs the real router and download job against a small HTTP
//! server on 127.0.0.1. Since that is a loopback address, tests that expect a
//! download to succeed turn on `allow_private_addresses`; the guard tests run
//! with it off. Host names are resolved by an injected resolver, never DNS,
//! and nothing connects outside this host.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use strom::media_download::guard::HostResolver;
use strom::media_download::MediaDownloadSettings;
use strom::state::AppState;
use strom_types::media_download::{MediaDownloadJob, MediaDownloadState};
use strom_types::StromEvent;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Notify};
use tower::ServiceExt;

const BODY_LEN: usize = 4096;

fn body_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

// ---------------------------------------------------------------------------
// Test HTTP server
// ---------------------------------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
    gate: Arc<Notify>,
}

impl TestServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        let (count, g) = (requests.clone(), gate.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve(stream, addr, g.clone()));
            }
        });
        Self {
            addr,
            requests,
            gate,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

async fn serve(mut stream: TcpStream, addr: SocketAddr, gate: Arc<Notify>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let path = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();

    let ok = |len: usize, extra: &str| {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n{extra}Connection: close\r\n\r\n")
    };
    let redirect = |location: String| {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    };
    let body = body_bytes(BODY_LEN);

    let _ = match path.as_str() {
        "/clip.bin" | "/other/clip.bin" => {
            stream.write_all(ok(BODY_LEN, "").as_bytes()).await.ok();
            stream.write_all(&body).await
        }
        // Content-Disposition names a file outside the target directory.
        "/disposition" => {
            let header = "Content-Disposition: attachment; filename=\"../../escape.bin\"\r\n";
            stream.write_all(ok(BODY_LEN, header).as_bytes()).await.ok();
            stream.write_all(&body).await
        }
        // Half the body, then wait for the test to open the gate.
        "/gated.bin" => {
            stream.write_all(ok(BODY_LEN, "").as_bytes()).await.ok();
            stream.write_all(&body[..BODY_LEN / 2]).await.ok();
            stream.flush().await.ok();
            gate.notified().await;
            stream.write_all(&body[BODY_LEN / 2..]).await
        }
        // Announces more than the size limit the cap tests use.
        "/announced-big.bin" => {
            stream.write_all(ok(1_000_000, "").as_bytes()).await.ok();
            stream.write_all(&body).await
        }
        // No length announced; streams past the limit in chunks.
        "/chunked-big.bin" => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .ok();
            for _ in 0..20 {
                let data = &body[..500];
                stream.write_all(b"1f4\r\n").await.ok();
                stream.write_all(data).await.ok();
                stream.write_all(b"\r\n").await.ok();
            }
            stream.write_all(b"0\r\n\r\n").await
        }
        "/redirect-ok" => {
            stream
                .write_all(redirect("/other/clip.bin".to_string()).as_bytes())
                .await
        }
        "/redirect-metadata" => {
            stream
                .write_all(
                    redirect("http://169.254.169.254/latest/meta-data/".to_string()).as_bytes(),
                )
                .await
        }
        "/redirect-name" => {
            stream
                .write_all(
                    redirect(format!(
                        "http://unroutable.example.com:{}/clip.bin",
                        addr.port()
                    ))
                    .as_bytes(),
                )
                .await
        }
        "/redirect-loop" => {
            stream
                .write_all(redirect("/redirect-loop".to_string()).as_bytes())
                .await
        }
        _ => {
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
        }
    };
    let _ = stream.shutdown().await;
}

// ---------------------------------------------------------------------------
// Resolver and app
// ---------------------------------------------------------------------------

/// Resolves from a fixed table; anything else fails like an unknown name.
struct MapResolver(HashMap<String, Vec<IpAddr>>);

impl HostResolver for MapResolver {
    fn lookup(&self, host: String, _port: u16) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        let result = self.0.get(&host).cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("unknown host {host}"))
        });
        Box::pin(async move { result })
    }
}

struct TestApp {
    router: Router,
    events: broadcast::Receiver<StromEvent>,
    dir: TempDir,
}

impl TestApp {
    async fn new(settings: MediaDownloadSettings) -> Self {
        gstreamer::init().unwrap();
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("media/stingers")).unwrap();
        let state = AppState::with_json_storage(
            dir.path().join("flows.json"),
            dir.path().join("blocks.json"),
            dir.path().join("media"),
            vec![],
            "all".to_string(),
            vec![],
            false,
            false,
        );
        state.media_downloads().configure(settings);
        let resolver: HashMap<String, Vec<IpAddr>> = [
            ("media.example.com", "127.0.0.1"),
            ("private.example.com", "10.1.2.3"),
            ("metadata.example.com", "169.254.169.254"),
            ("unroutable.example.com", "0.0.0.0"),
        ]
        .into_iter()
        .map(|(h, ip)| (h.to_string(), vec![ip.parse().unwrap()]))
        .collect();
        state
            .media_downloads()
            .set_resolver(Arc::new(MapResolver(resolver)));
        let events = state.events().subscribe();
        let router = strom::create_app_with_state(state.clone()).await;
        Self {
            router,
            events,
            dir,
        }
    }

    fn media(&self) -> PathBuf {
        self.dir.path().join("media")
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(json) => builder
                .header("content-type", "application/json")
                .body(Body::from(json.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn download(&self, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.request("POST", "/api/media/download", Some(body))
            .await
    }

    /// Collect this job's events until it ends.
    async fn wait_for_end(&mut self, job_id: &str) -> Vec<MediaDownloadJob> {
        let mut seen = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match self.events.recv().await {
                    Ok(StromEvent::MediaDownloadProgress(job)) if job.job_id == job_id => {
                        let finished = job.state.is_finished();
                        seen.push(job);
                        if finished {
                            return;
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("event channel closed: {e}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("download {job_id} did not end; events: {seen:?}"));
        seen
    }
}

fn lab_settings() -> MediaDownloadSettings {
    MediaDownloadSettings {
        allow_private_addresses: true,
        ..Default::default()
    }
}

fn temp_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part"))
        .collect()
}

fn job_id(json: &serde_json::Value) -> String {
    json["job_id"]
        .as_str()
        .expect("job_id in response")
        .to_string()
}

// ---------------------------------------------------------------------------
// Success, atomicity, progress
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_lands_in_the_folder_with_the_right_bytes() {
    let server = TestServer::start().await;
    let mut app = TestApp::new(lab_settings()).await;

    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/clip.bin"), "path": "stingers" }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    assert_eq!(json["path"], "stingers/clip.bin");
    assert_eq!(json["total"], BODY_LEN as u64);

    let events = app.wait_for_end(&job_id(&json)).await;
    let last = events.last().unwrap();
    assert_eq!(last.state, MediaDownloadState::Done, "{last:?}");
    assert_eq!(last.bytes, BODY_LEN as u64);
    assert_eq!(
        events.first().unwrap().state,
        MediaDownloadState::Downloading
    );

    let target = app.media().join("stingers/clip.bin");
    assert_eq!(std::fs::read(&target).unwrap(), body_bytes(BODY_LEN));
    assert!(temp_files(&app.media().join("stingers")).is_empty());

    // The finished job is listed for a client that reconnects.
    let (status, list) = app.request("GET", "/api/media/downloads", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["downloads"][0]["state"], "done");
}

#[tokio::test]
async fn partial_download_never_appears_under_the_final_name() {
    let server = TestServer::start().await;
    let mut app = TestApp::new(lab_settings()).await;

    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/gated.bin") }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    let id = job_id(&json);
    let target = app.media().join("gated.bin");

    // Wait until the first half is on disk, under whatever name.
    let media = app.media();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            // `std::fs::metadata`, not `DirEntry::metadata`: on Windows the
            // latter is the size in the directory listing, which NTFS does
            // not update while the writer holds the file open.
            let half_written = std::fs::read_dir(&media).unwrap().any(|entry| {
                entry
                    .and_then(|e| std::fs::metadata(e.path()))
                    .map(|m| m.is_file() && m.len() >= (BODY_LEN / 2) as u64)
                    .unwrap_or(false)
            });
            if half_written {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first half never reached the disk");

    assert!(
        !target.exists(),
        "a half-downloaded file is visible under its final name"
    );
    let (_, listing) = app.request("GET", "/api/media", None).await;
    assert_eq!(
        listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["is_directory"] == false)
            .count(),
        0,
        "the media listing shows an unfinished download: {listing}"
    );

    // Long enough for a progress tick before the rest arrives.
    tokio::time::sleep(Duration::from_millis(400)).await;
    server.gate.notify_one();

    let events = app.wait_for_end(&id).await;
    let last = events.last().unwrap();
    assert_eq!(last.state, MediaDownloadState::Done, "{last:?}");
    assert!(
        events
            .iter()
            .any(|e| e.state == MediaDownloadState::Downloading && e.bytes > 0),
        "no progress event with bytes: {events:?}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), body_bytes(BODY_LEN));
    assert!(temp_files(&app.media()).is_empty());
}

#[tokio::test]
async fn cancel_removes_the_temporary_file() {
    let server = TestServer::start().await;
    let mut app = TestApp::new(lab_settings()).await;

    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/gated.bin") }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    let id = job_id(&json);

    let (status, body) = app
        .request("DELETE", &format!("/api/media/downloads/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let events = app.wait_for_end(&id).await;
    assert_eq!(events.last().unwrap().state, MediaDownloadState::Cancelled);
    server.gate.notify_one();
    assert!(temp_files(&app.media()).is_empty());
    assert!(!app.media().join("gated.bin").exists());

    let (status, _) = app
        .request("DELETE", &format!("/api/media/downloads/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = app
        .request("DELETE", "/api/media/downloads/nonexistent", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Address guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn guard_refuses_local_and_private_addresses_by_default() {
    let server = TestServer::start().await;
    let app = TestApp::new(MediaDownloadSettings::default()).await;
    let port = server.addr.port();

    for url in [
        server.url("/clip.bin"), // loopback, server is listening
        format!("http://media.example.com:{port}/clip.bin"), // public-looking name -> 127.0.0.1
        format!("http://[::1]:{port}/clip.bin"),
        "http://10.0.0.1/clip.bin".to_string(),
        "http://192.168.1.20/clip.bin".to_string(),
        "http://172.16.5.5/clip.bin".to_string(),
        "http://100.64.0.1/clip.bin".to_string(),
        "http://169.254.169.254/latest/meta-data/".to_string(),
        "http://[fe80::1]/clip.bin".to_string(),
        "http://[fd00::1]/clip.bin".to_string(),
        "http://[::ffff:127.0.0.1]/clip.bin".to_string(),
        "http://private.example.com/clip.bin".to_string(),
        "http://metadata.example.com/latest/meta-data/".to_string(),
        "http://0.0.0.0/clip.bin".to_string(),
    ] {
        let (status, json) = app.download(serde_json::json!({ "url": url })).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{url} was not refused: {json}"
        );
    }

    assert_eq!(server.requests(), 0, "a refused URL reached the server");
    let files: Vec<_> = std::fs::read_dir(app.media())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_file())
        .collect();
    assert!(files.is_empty(), "files appeared: {files:?}");
}

#[tokio::test]
async fn guard_keeps_metadata_and_unroutable_refused_when_private_is_allowed() {
    let server = TestServer::start().await;
    let app = TestApp::new(lab_settings()).await;

    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://metadata.example.com/latest/meta-data/",
        "http://unroutable.example.com/clip.bin",
        "http://[ff02::1]/clip.bin",
    ] {
        let (status, json) = app.download(serde_json::json!({ "url": url })).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{url} was not refused: {json}"
        );
    }
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn guard_checks_every_redirect() {
    let server = TestServer::start().await;
    let mut app = TestApp::new(lab_settings()).await;

    // An allowed redirect is followed, and the name comes from the last hop.
    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/redirect-ok") }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    assert_eq!(json["filename"], "clip.bin");
    let events = app.wait_for_end(&job_id(&json)).await;
    assert_eq!(events.last().unwrap().state, MediaDownloadState::Done);

    // A redirect from an allowed address to a refused one is refused.
    for path in ["/redirect-metadata", "/redirect-name"] {
        let (status, json) = app
            .download(serde_json::json!({ "url": server.url(path) }))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {json}");
    }

    // Redirects are capped.
    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/redirect-loop") }))
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{json}");
    assert!(
        json["error"].as_str().unwrap().contains("redirects"),
        "{json}"
    );
}

#[tokio::test]
async fn only_http_and_https_are_fetched() {
    let app = TestApp::new(lab_settings()).await;
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/clip.bin",
        "not a url",
    ] {
        let (status, json) = app.download(serde_json::json!({ "url": url })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url}: {json}");
    }
}

// ---------------------------------------------------------------------------
// Size limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn announced_size_over_the_limit_is_refused_up_front() {
    let server = TestServer::start().await;
    let app = TestApp::new(MediaDownloadSettings {
        max_bytes: 10_000,
        ..lab_settings()
    })
    .await;

    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/announced-big.bin") }))
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{json}");
    assert!(temp_files(&app.media()).is_empty());
    assert!(!app.media().join("announced-big.bin").exists());
}

#[tokio::test]
async fn unannounced_body_over_the_limit_fails_and_leaves_nothing() {
    let server = TestServer::start().await;
    let mut app = TestApp::new(MediaDownloadSettings {
        max_bytes: 2_000,
        ..lab_settings()
    })
    .await;

    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/chunked-big.bin") }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    assert_eq!(json.get("total"), None);

    let events = app.wait_for_end(&job_id(&json)).await;
    let last = events.last().unwrap();
    assert_eq!(last.state, MediaDownloadState::Failed, "{last:?}");
    assert!(last.error.as_deref().unwrap().contains("limit"), "{last:?}");
    assert!(last.bytes <= 2_500, "kept reading past the limit: {last:?}");
    assert!(temp_files(&app.media()).is_empty());
    assert!(!app.media().join("chunked-big.bin").exists());
}

// ---------------------------------------------------------------------------
// Names, containment and overwriting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_disposition_cannot_leave_the_folder() {
    let server = TestServer::start().await;
    let mut app = TestApp::new(lab_settings()).await;

    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/disposition"), "path": "stingers" }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    assert_eq!(json["path"], "stingers/escape.bin");
    let events = app.wait_for_end(&job_id(&json)).await;
    assert_eq!(events.last().unwrap().state, MediaDownloadState::Done);

    assert!(app.media().join("stingers/escape.bin").is_file());
    assert!(!app.dir.path().join("escape.bin").exists());
    assert!(!app.media().join("escape.bin").exists());
}

#[tokio::test]
async fn names_and_paths_outside_the_media_root_are_refused() {
    let server = TestServer::start().await;
    let app = TestApp::new(lab_settings()).await;
    std::fs::create_dir_all(app.dir.path().join("outside")).unwrap();

    for filename in ["../escape.bin", "sub/clip.bin", "..", ".hidden"] {
        let (status, json) = app
            .download(serde_json::json!({ "url": server.url("/clip.bin"), "filename": filename }))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{filename}: {json}");
    }
    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/clip.bin"), "path": "../outside" }))
        .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
        "{status}: {json}"
    );
    assert!(std::fs::read_dir(app.dir.path().join("outside"))
        .unwrap()
        .next()
        .is_none());
    assert_eq!(server.requests(), 0, "a refused request reached the server");
}

#[tokio::test]
async fn existing_file_needs_overwrite() {
    let server = TestServer::start().await;
    let mut app = TestApp::new(lab_settings()).await;
    let target = app.media().join("clip.bin");
    std::fs::write(&target, b"keep me").unwrap();

    // Named by the URL: refused once the name is known, before the body.
    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/clip.bin") }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");

    // Named explicitly: refused before any request is made.
    let before = server.requests();
    let (status, json) = app
        .download(
            serde_json::json!({ "url": server.url("/other/clip.bin"), "filename": "clip.bin" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(server.requests(), before);
    assert_eq!(std::fs::read(&target).unwrap(), b"keep me");

    let (status, json) = app
        .download(serde_json::json!({ "url": server.url("/clip.bin"), "overwrite": true }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{json}");
    let events = app.wait_for_end(&job_id(&json)).await;
    assert_eq!(events.last().unwrap().state, MediaDownloadState::Done);
    assert_eq!(std::fs::read(&target).unwrap(), body_bytes(BODY_LEN));
}

// ---------------------------------------------------------------------------
// Review fixes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_request_does_not_show_the_url_query() {
    // A port nothing listens on: the connect fails, which is where reqwest's
    // message used to carry the full URL.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let app = TestApp::new(lab_settings()).await;
    let (status, json) = app
        .download(serde_json::json!({
            "url": format!("http://media.example.com:{port}/clip.bin?sig=SECRET123"),
        }))
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{json}");
    assert!(!json.to_string().contains("SECRET123"), "{json}");
}

#[tokio::test]
async fn file_name_over_the_limit_names_the_limit() {
    let app = TestApp::new(lab_settings()).await;
    let name = format!("{}.bin", "a".repeat(226));
    let (status, json) = app
        .download(serde_json::json!({
            "url": "http://media.example.com/clip.bin",
            "filename": name,
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    let limit = strom_types::media_download::MEDIA_DOWNLOAD_MAX_FILENAME_BYTES.to_string();
    assert!(json.to_string().contains(&limit), "{json}");
}

#[test]
fn left_over_temporary_files_are_swept() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("stingers")).unwrap();
    for name in [
        ".download-a.part",
        "stingers/.download-b.part",
        "clip.part",
        "clip.mp4",
    ] {
        std::fs::write(root.join(name), b"x").unwrap();
    }

    // Recently written files may belong to a running transfer.
    assert_eq!(
        strom::media_download::remove_orphan_temp_files(root, Duration::from_secs(3600)),
        0
    );

    assert_eq!(
        strom::media_download::remove_orphan_temp_files(root, Duration::ZERO),
        2
    );
    assert!(!root.join(".download-a.part").exists());
    assert!(!root.join("stingers/.download-b.part").exists());
    assert!(root.join("clip.part").exists());
    assert!(root.join("clip.mp4").exists());
}
