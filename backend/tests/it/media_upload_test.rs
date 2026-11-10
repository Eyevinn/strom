//! Browser uploads into the media library (`POST /api/media/upload`).
//!
//! The upload is streamed into a hidden temporary file and renamed into place
//! when it is complete. These tests run the real router and check that a
//! finished upload lands byte for byte, that the file is written while it
//! arrives (not buffered in memory first), and that an upload which stops
//! early, or whose request is dropped, leaves nothing behind.

use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
    Router,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use strom::state::AppState;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tower::ServiceExt;

const BOUNDARY: &str = "strom-upload-test-boundary";

struct TestApp {
    router: Router,
    dir: TempDir,
}

impl TestApp {
    async fn new() -> Self {
        gstreamer::init().unwrap();
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("media/clips")).unwrap();
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
        let router = strom::create_app_with_state(state).await;
        Self { router, dir }
    }

    fn folder(&self) -> PathBuf {
        self.dir.path().join("media/clips")
    }

    fn request(body: Body) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/media/upload?path=clips")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
            .unwrap()
    }
}

fn part_head(filename: &str) -> Vec<u8> {
    format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes()
}

fn part_tail() -> Vec<u8> {
    format!("\r\n--{BOUNDARY}--\r\n").into_bytes()
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn temp_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".part"))
        .collect()
}

/// A request body fed chunk by chunk from the test.
fn channel_body() -> (mpsc::Sender<Result<Bytes, std::io::Error>>, Body) {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (tx, Body::from_stream(stream))
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn upload_lands_with_the_right_bytes_and_no_temporary_file() {
    let app = TestApp::new().await;
    let data = payload(3 * 1024 * 1024 + 17);
    let mut body = part_head("clip.bin");
    body.extend_from_slice(&data);
    body.extend_from_slice(&part_tail());

    let response = app
        .router
        .clone()
        .oneshot(TestApp::request(Body::from(body)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(std::fs::read(app.folder().join("clip.bin")).unwrap(), data);
    assert_eq!(names(&app.folder()), vec!["clip.bin".to_string()]);
}

#[tokio::test]
async fn upload_is_written_while_it_arrives_and_a_dropped_request_leaves_nothing() {
    let app = TestApp::new().await;
    let (tx, body) = channel_body();
    let router = app.router.clone();
    let request = tokio::spawn(async move { router.oneshot(TestApp::request(body)).await });

    let chunk = payload(256 * 1024);
    tx.send(Ok(Bytes::from(part_head("clip.bin"))))
        .await
        .unwrap();
    tx.send(Ok(Bytes::from(chunk.clone()))).await.unwrap();

    // The first chunk reaches the disk before the body has ended: the upload
    // is streamed, not collected in memory first.
    let folder = app.folder();
    wait_until("the temporary file to hold the first chunk", || {
        temp_files(&folder)
            .first()
            .and_then(|p| std::fs::metadata(p).ok())
            .is_some_and(|m| m.len() >= chunk.len() as u64)
    })
    .await;
    assert!(
        !folder.join("clip.bin").exists(),
        "a partial upload must not appear under its final name"
    );

    // The client goes away: the server drops the handler mid-upload.
    request.abort();
    let _ = request.await;
    drop(tx);

    wait_until("the temporary file to be removed", || {
        temp_files(&folder).is_empty()
    })
    .await;
    assert!(
        names(&folder).is_empty(),
        "left behind: {:?}",
        names(&folder)
    );
}

#[tokio::test]
async fn upload_cut_off_mid_body_fails_and_leaves_nothing() {
    let app = TestApp::new().await;
    let (tx, body) = channel_body();
    let router = app.router.clone();
    let request = tokio::spawn(async move { router.oneshot(TestApp::request(body)).await });

    tx.send(Ok(Bytes::from(part_head("clip.bin"))))
        .await
        .unwrap();
    tx.send(Ok(Bytes::from(payload(128 * 1024)))).await.unwrap();
    // Let the first bytes reach the temporary file before the body breaks,
    // so the failure happens mid-file, not while the part header is read.
    let folder = app.folder();
    wait_until("the temporary file to hold the first chunk", || {
        temp_files(&folder)
            .first()
            .and_then(|p| std::fs::metadata(p).ok())
            .is_some_and(|m| m.len() > 0)
    })
    .await;
    tx.send(Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "client went away",
    )))
    .await
    .unwrap();
    drop(tx);

    let response = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .expect("upload did not end")
        .unwrap()
        .unwrap();
    assert!(
        !response.status().is_success(),
        "a cut-off upload must fail, got {}",
        response.status()
    );
    assert!(
        names(&app.folder()).is_empty(),
        "left behind: {:?}",
        names(&app.folder())
    );
}
