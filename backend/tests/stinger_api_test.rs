//! The stinger API as an outside client uses it: editing the library on the
//! mixer, guarding index-addressed calls with the file meant, following a
//! take through its id, and taking through the transition endpoint without
//! naming inputs.

pub mod common;
#[path = "common/http_file.rs"]
mod http_file;
#[path = "common/stinger.rs"]
pub mod rig;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rig::*;
use serde_json::{json, Value};
use strom_types::StromEvent;
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(match body {
            Some(b) => Body::from(b.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn the_library_is_edited_on_the_mixer_and_guarded_by_file() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-lib", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let base = format!("/api/flows/{}/blocks/{}/stinger", r.flow_id, r.mixer());
    let (_, state) = call(&app, "GET", &base, None).await;
    let files: Vec<String> = state["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(files.len(), 3);

    // Add: a fourth clip (a copy of the mask), and adding it again is a no-op.
    let extra = std::path::Path::new(&files[2]).with_file_name("extra.mov");
    std::fs::copy(&files[2], &extra).unwrap();
    let extra = extra.to_string_lossy().to_string();
    let (status, clip) = call(
        &app,
        "POST",
        &format!("{base}/clips"),
        Some(json!({"file": extra})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{clip}");
    assert_eq!(clip["index"], 3);
    let (status, again) = call(
        &app,
        "POST",
        &format!("{base}/clips"),
        Some(json!({"file": extra})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["index"], 3, "adding the same file twice returns it");
    let (status, _) = call(
        &app,
        "POST",
        &format!("{base}/clips"),
        Some(json!({"file": "/nonexistent/stinger.mkv"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a missing file is refused");

    // Guarded settings: the right file is accepted, a wrong one is a conflict.
    let q = |f: &str| urlencoding(f);
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/clips/3?file={}", q(&extra)),
        Some(json!({"invert_matte": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        "PUT",
        &format!("{base}/clips/3?file={}", q(&files[0])),
        Some(json!({"invert_matte": true})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Remove clip 0: the rest move up, and a client still holding the old
    // index for clip 1 gets a conflict instead of editing another clip.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("{base}/clips/0?file={}", q(&files[0])),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = call(&app, "GET", &base, None).await;
    let now: Vec<&str> = state["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap())
        .collect();
    assert_eq!(
        now,
        vec![files[1].as_str(), files[2].as_str(), extra.as_str()]
    );
    assert_eq!(
        state["clips"][2]["settings"]["invert_matte"], true,
        "settings follow their file"
    );
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/clips/1?file={}", q(&files[1])),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn a_take_is_followed_by_its_id() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-take", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let mut events = r.state.events().subscribe();
    let blocks = format!("/api/flows/{}/blocks/{}", r.flow_id, r.mixer());

    // A guarded take with the wrong file does not start.
    let (status, _) = call(
        &app,
        "POST",
        &format!("{blocks}/stinger/take"),
        Some(json!({"index": 0, "file": "not-this.mkv"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Through the transition endpoint, naming no inputs.
    let (status, body) = call(
        &app,
        "POST",
        &format!("{blocks}/transition"),
        Some(json!({"transition_type": "stinger", "stinger_clip": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let report = r.wait_for_report(0).await;
    let first = report.take_id;

    // The take endpoint returns the id the events carry.
    r.wait_until_parked().await;
    let (status, take) = call(
        &app,
        "POST",
        &format!("{blocks}/stinger/take"),
        Some(json!({"index": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{take}");
    let id = take["take_id"].as_u64().unwrap();
    assert_ne!(id, first);
    assert!(take["file"].as_str().unwrap().ends_with("classic.mov"));
    let (mut started, mut completed) = (None, None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while completed.is_none() && std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await {
            Ok(Ok(StromEvent::StingerStarted { take_id, .. })) if take_id == id => {
                started = Some(take_id)
            }
            Ok(Ok(StromEvent::StingerCompleted { report, .. })) if report.take_id == id => {
                completed = Some(report.take_id)
            }
            _ => {}
        }
    }
    assert_eq!(started, Some(id));
    assert_eq!(completed, Some(id));

    // Any other transition still needs its inputs.
    let (status, _) = call(
        &app,
        "POST",
        &format!("{blocks}/transition"),
        Some(json!({"transition_type": "cut"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// Take the cued clip and say which clip aired: the side-by-side clip's
/// graphic carries a yellow marker top left, the classic clip's a green left
/// half and no marker.
async fn take_cued_and_watch(r: &Running) -> (String, bool, bool) {
    r.wait_until_parked().await;
    r.drain().await;
    let take = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), None, None)
        .await
        .expect("take");
    let frames = r.collect(take.take_to_air_ms as u64 + 1600).await;
    let marker = frames
        .iter()
        .any(|(_, f)| colour(px(f, 3, 3)) == Colour::Yellow);
    let green = frames
        .iter()
        .any(|(_, f)| colour(px(f, W / 4, H / 2)) == Colour::Green);
    r.wait_for_report(take.index).await;
    (take.file, marker, green)
}

/// Removing the cued clip cues the one that takes its place: the next take
/// plays that clip, not the removed one still parked in the player.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn removing_the_cued_clip_parks_its_successor() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-rm-cued", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let base = format!("/api/flows/{}/blocks/{}/stinger", r.flow_id, r.mixer());
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert_eq!(s.cued_index, Some(0));
    let classic = s.clips[0].file.clone();

    let (status, _) = call(
        &app,
        "DELETE",
        &format!("{base}/clips/0?file={}", urlencoding(&classic)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (file, marker, green) = take_cued_and_watch(&r).await;
    assert!(file.ends_with("sbs.mov"), "{file}");
    assert!(marker, "the side-by-side clip's graphic never aired");
    assert!(!green, "the removed classic clip aired");

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A take owns its clip source until the clip has played out: playlist
/// edits, transport calls and library additions wait, and the clip on air
/// is not rewound under the mixer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn a_take_owns_its_clip_source() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-owned", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    let files: Vec<String> = s.clips.iter().map(|c| c.file.clone()).collect();
    let extra = std::path::Path::new(&files[2]).with_file_name("extra.mov");
    std::fs::copy(&files[2], &extra).unwrap();
    let player = format!("/api/flows/{}/blocks/sting-api-owned/player", r.flow_id);

    r.state
        .stinger_take(&r.flow_id, &r.mixer(), Some(0), None)
        .await
        .expect("take");
    let (status, body) = call(
        &app,
        "POST",
        &format!("{player}/playlist"),
        Some(json!({"files": [files[1], files[0], files[2]]})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "playlist edit: {body}");
    let (status, body) = call(
        &app,
        "POST",
        &format!("{player}/control"),
        Some(json!({"action": "pause"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "transport: {body}");
    let err = r
        .state
        .stinger_add_clip(&r.flow_id, &r.mixer(), &extra.to_string_lossy(), None)
        .await
        .expect_err("library addition during a take");
    assert!(err.to_string().contains("on air"), "{err}");

    let report = r.wait_for_report(0).await;
    assert_eq!(report.frames_arrived, r.clip_frames(0).await, "{report:?}");
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    let after: Vec<String> = s.clips.iter().map(|c| c.file.clone()).collect();
    assert_eq!(after, files, "the library changed during the take");

    // Once it is over, the edits go through.
    let (status, body) = call(
        &app,
        "POST",
        &format!("{player}/playlist"),
        Some(json!({"files": files})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// Clips added at the same moment all land in the library: each addition
/// reads the playlist, extends it and writes it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn clips_added_together_all_land() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-adds", "cpu").await;
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    let mask = s.clips[2].file.clone();
    let extras: Vec<String> = (0..6)
        .map(|i| {
            let p = std::path::Path::new(&mask).with_file_name(format!("extra{i}.mov"));
            std::fs::copy(&mask, &p).unwrap();
            p.to_string_lossy().to_string()
        })
        .collect();
    let adds: Vec<_> = extras
        .iter()
        .cloned()
        .map(|f| {
            let (state, flow, mixer) = (r.state.clone(), r.flow_id, r.mixer());
            tokio::spawn(async move { state.stinger_add_clip(&flow, &mixer, &f, None).await })
        })
        .collect();
    for add in adds {
        add.await.unwrap().expect("add");
    }
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    let files: Vec<String> = s.clips.iter().map(|c| c.file.clone()).collect();
    for f in &extras {
        assert!(files.contains(f), "{f} was lost: {files:?}");
    }
    assert_eq!(files.len(), 3 + extras.len(), "{files:?}");
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A playlist PUT that puts another file at the parked index re-cues it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn a_playlist_put_over_the_parked_index_recues_it() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-put-cued", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert_eq!(s.cued_index, Some(0));
    let files: Vec<String> = s.clips.iter().map(|c| c.file.clone()).collect();

    let (status, body) = call(
        &app,
        "POST",
        &format!(
            "/api/flows/{}/blocks/sting-api-put-cued/player/playlist",
            r.flow_id
        ),
        Some(json!({"files": [files[1], files[0], files[2]]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (file, marker, green) = take_cued_and_watch(&r).await;
    assert!(file.ends_with("sbs.mov"), "{file}");
    assert!(marker, "the side-by-side clip's graphic never aired");
    assert!(!green, "the classic clip parked before the PUT aired");

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// Replace `path` with the side-by-side clip, as an operator re-exporting a
/// clip under the same name does: written next to it and renamed over it, so
/// the parked player still holds the old file. The new file gets an mtime a
/// minute on, so the change shows on any filesystem's timestamp resolution.
fn overwrite_with_sbs(path: &str) {
    let path = std::path::Path::new(path);
    let tmp = path.with_file_name("rewrite.tmp.mov");
    sbs_clip(&tmp);
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&tmp)
        .unwrap()
        .set_modified(later)
        .unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// Wait for clip `index`'s analysis of its file as it is now.
async fn wait_for_layout(
    r: &Running,
    index: usize,
    layout: strom_types::stinger::StingerLayout,
) -> strom_types::stinger::StingerClipInfo {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
        if let Some(info) = s.clips[index].info.clone() {
            if info.detected_layout == layout {
                return info;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "clip {index} never analysed as {layout:?}: {:?}",
            s.clips[index].info
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// A clip rewritten on disk under the same name: reload analyses the new file
/// and parks it again, so the next take plays the new content, not the frames
/// the player loaded before. Reload reports a deleted file per clip instead of
/// failing, and is refused while a stinger is on air.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn reload_picks_up_a_clip_rewritten_on_disk() {
    use strom_types::stinger::StingerLayout;
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-reload", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let base = format!("/api/flows/{}/blocks/{}/stinger", r.flow_id, r.mixer());
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert_eq!(s.cued_index, Some(0));
    assert_eq!(
        s.clips[0].info.as_ref().unwrap().detected_layout,
        StingerLayout::Classic
    );
    let classic = s.clips[0].file.clone();
    let mask = s.clips[2].file.clone();

    overwrite_with_sbs(&classic);
    std::fs::remove_file(&mask).unwrap();
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert!(
        !s.ready,
        "the parked clip's file changed on disk, so it is not ready as it is"
    );

    let (status, state) = call(&app, "POST", &format!("{base}/reload"), None).await;
    assert_eq!(status, StatusCode::OK, "{state}");
    assert_eq!(state["clips"].as_array().unwrap().len(), 3);
    assert_eq!(state["clips"][0]["missing"], false);
    assert_eq!(state["clips"][1]["missing"], false);
    assert_eq!(state["clips"][2]["missing"], true, "{state}");
    assert_eq!(state["ready"], true, "reload parks the new file: {state}");

    let info = wait_for_layout(&r, 0, StingerLayout::SideBySide).await;
    assert_eq!(info.width, 2 * W);
    let (file, marker, green) = take_cued_and_watch(&r).await;
    assert_eq!(file, classic);
    assert!(marker, "the rewritten clip's graphic never aired");
    assert!(!green, "the clip loaded before the rewrite aired");

    // On air: refused.
    r.wait_until_parked().await;
    let take = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), None, None)
        .await
        .expect("take");
    let (status, body) = call(&app, "POST", &format!("{base}/reload"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    r.wait_for_report(take.index).await;

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A plain cue of a clip whose file was rewritten since it was parked loads
/// the new file, with no reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn a_cue_loads_a_parked_clip_rewritten_on_disk() {
    use strom_types::stinger::StingerLayout;
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-recue", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let base = format!("/api/flows/{}/blocks/{}/stinger", r.flow_id, r.mixer());
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert_eq!(s.cued_index, Some(0));
    let classic = s.clips[0].file.clone();

    overwrite_with_sbs(&classic);
    let (status, state) = call(
        &app,
        "POST",
        &format!("{base}/cue"),
        Some(json!({"index": 0, "file": classic})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{state}");
    assert_eq!(state["ready"], true, "{state}");

    wait_for_layout(&r, 0, StingerLayout::SideBySide).await;
    let (file, marker, green) = take_cued_and_watch(&r).await;
    assert_eq!(file, classic);
    assert!(marker, "the rewritten clip's graphic never aired");
    assert!(!green, "the clip parked before the rewrite aired");

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A clip served from a source that cannot seek plays on every take: the
/// re-park after a take cannot rewind it, so it is loaded again instead of
/// staying unparked, and the next cue or take failing with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn a_clip_that_cannot_seek_plays_on_every_take() {
    let mut needed = CODEC_ELEMENTS.to_vec();
    needed.extend(["souphttpsrc", "matroskamux", "matroskademux"]);
    if !common::plugins_available(&needed) {
        return;
    }
    let r = start("api-noseek", "cpu").await;
    // The classic clip as uncompressed video in Matroska: Matroska's demuxer
    // fails a seek it cannot make on a source that cannot seek, and reads the
    // file front to back without one.
    let dir = tempfile::tempdir().unwrap();
    let mov = dir.path().join("classic.mov");
    let mkv = dir.path().join("classic.mkv");
    classic_clip(&mov);
    let remux = gstreamer::parse::launch(
        "filesrc name=src ! qtdemux ! pngdec ! videoconvert ! video/x-raw,format=AYUV ! matroskamux ! filesink name=sink",
    )
    .unwrap()
    .downcast::<gstreamer::Pipeline>()
    .unwrap();
    use gstreamer::prelude::*;
    remux
        .by_name("src")
        .unwrap()
        .set_property("location", mov.to_str().unwrap());
    set_sink_location(&remux, &mkv);
    remux.set_state(gstreamer::State::Playing).unwrap();
    let msg = remux.bus().unwrap().timed_pop_filtered(
        gstreamer::ClockTime::from_seconds(30),
        &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
    );
    remux.set_state(gstreamer::State::Null).unwrap();
    assert!(
        matches!(msg.map(|m| m.type_()), Some(gstreamer::MessageType::Eos)),
        "remux failed"
    );
    let url = http_file::serve_without_ranges(
        std::fs::read(&mkv).unwrap(),
        "classic.mkv",
        "video/x-matroska",
    );

    let clip = r
        .state
        .stinger_add_clip(&r.flow_id, &r.mixer(), &url, None)
        .await
        .expect("add the URL");
    r.state
        .stinger_cue(&r.flow_id, &r.mixer(), clip.index, Some(&url))
        .await
        .expect("cue the URL");
    for take_no in 1..=2 {
        r.wait_until_parked().await;
        r.wait_for_analysis().await;
        let take = r
            .state
            .stinger_take(&r.flow_id, &r.mixer(), None, None)
            .await
            .unwrap_or_else(|e| panic!("take {take_no}: {e}"));
        assert_eq!(take.file, url);
        let report = r.wait_for_report(clip.index).await;
        assert_eq!(report.take_id, take.take_id);
        assert!(
            report.frames_expected + 1 >= N as u32,
            "take {take_no}: {report:?}"
        );
        assert_eq!(
            report.frames_arrived, report.frames_expected,
            "take {take_no} lost frames: {report:?}"
        );
    }
    r.wait_until_parked().await;

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// Percent-encode a query value.
fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}
