//! A flow's time offset per source over REST (`/api/flows/{id}/source-offsets`):
//! set, read, refused when bad, removed with `null`; saved with the flow and
//! taken at once by a running replay player of the flow, whose own
//! `offset_ms` is the fallback.
//!
//! Here and not among the library's tests: those check that a replay group
//! is gone once its players are, and an `AppState` built next to them slows
//! the process enough to race them.

use crate::common;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use std::collections::BTreeMap;
use std::sync::Arc;
use strom::api::replay::{get_source_offsets, patch_source_offsets};
use strom::blocks::builtin::mediaplayer::replay::timeline::{self, PlayerConfig, PlayerStats};
use strom::json_rejection::JsonBody;
use strom_types::replay::ReplaySourceOffsetsPatch;
use strom_types::Flow;

const MS: i64 = 1_000_000;

fn patch(entries: &[(&str, Option<i64>)]) -> ReplaySourceOffsetsPatch {
    ReplaySourceOffsetsPatch {
        offsets_ms: entries.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_offsets_over_rest() {
    gstreamer::init().unwrap();
    let state = common::state::new().app();
    let flow = Flow::new("offsets");
    let id = flow.id;
    state.upsert_flow(flow).await.unwrap();
    let tl = timeline::get_or_create(id, "g");
    let p = Arc::new(PlayerStats::with_config(
        "p",
        PlayerConfig {
            source_id: Some("cam1".into()),
            offset_ns: 300 * MS,
            flow_id: Some(id),
            ..Default::default()
        },
    ));
    tl.register_player(&p);
    let send = |body: ReplaySourceOffsetsPatch| {
        patch_source_offsets(State(state.clone()), Path(id), JsonBody(body))
    };

    let Json(r) = send(patch(&[("cam1", Some(1500)), ("cam2", Some(-20))]))
        .await
        .unwrap();
    let want: BTreeMap<String, i64> = [("cam1".into(), 1500), ("cam2".into(), -20)].into();
    assert_eq!(r.offsets_ms, want);
    assert_eq!(p.offset_ns(), 1500 * MS);
    let Json(r) = get_source_offsets(State(state.clone()), Path(id))
        .await
        .unwrap();
    assert_eq!(r.offsets_ms, want);
    assert_eq!(
        state
            .get_flow(&id)
            .await
            .unwrap()
            .properties
            .source_offsets_ms,
        want
    );
    // Refused whole: nothing changes.
    let err = send(patch(&[("cam2", Some(0)), ("cam1", Some(90_000_000))]))
        .await
        .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(p.offset_ns(), 1500 * MS);
    // Removed: the player's own offset again; the other source stays.
    let Json(r) = send(patch(&[("cam1", None)])).await.unwrap();
    assert_eq!(r.offsets_ms, [("cam2".into(), -20)].into());
    assert_eq!(p.offset_ns(), 300 * MS);
    let missing = get_source_offsets(State(state.clone()), Path(uuid::Uuid::new_v4())).await;
    assert_eq!(missing.unwrap_err().0, StatusCode::NOT_FOUND);
    drop(tl);
}

/// A properties save or a whole-flow save whose body leaves the source
/// offsets out (a client that does not know them, or a copy taken before
/// they were set) keeps the stored ones: running players do not jump back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_save_without_offsets_keeps_them() {
    use strom::api::flows::{update_flow, update_flow_properties};
    use strom_types::api::UpdateFlowPropertiesRequest;

    gstreamer::init().unwrap();
    let state = common::state::new().app();
    let flow = Flow::new("offsets kept");
    let id = flow.id;
    state.upsert_flow(flow).await.unwrap();
    let tl = timeline::get_or_create(id, "g");
    let p = Arc::new(PlayerStats::with_config(
        "p",
        PlayerConfig {
            source_id: Some("cam1".into()),
            flow_id: Some(id),
            ..Default::default()
        },
    ));
    tl.register_player(&p);
    // A copy taken before the offsets were set.
    let stale = state.get_flow(&id).await.unwrap();
    let _ = patch_source_offsets(
        State(state.clone()),
        Path(id),
        JsonBody(patch(&[("cam1", Some(1500))])),
    )
    .await
    .unwrap();
    let want: BTreeMap<String, i64> = [("cam1".into(), 1500)].into();
    let stored = |state: strom::state::AppState| async move {
        state
            .get_flow(&id)
            .await
            .unwrap()
            .properties
            .source_offsets_ms
    };

    let mut props = stale.properties.clone();
    props.description = Some("changed".into());
    let Json(r) = update_flow_properties(
        State(state.clone()),
        Path(id),
        JsonBody(UpdateFlowPropertiesRequest { properties: props }),
    )
    .await
    .unwrap();
    assert_eq!(r.flow.properties.source_offsets_ms, want);
    assert_eq!(stored(state.clone()).await, want);
    assert_eq!(p.offset_ns(), 1500 * MS);

    let Json(r) = update_flow(State(state.clone()), Path(id), JsonBody(stale.clone()))
        .await
        .unwrap();
    assert_eq!(r.flow.properties.source_offsets_ms, want);
    assert_eq!(stored(state.clone()).await, want);
    assert_eq!(p.offset_ns(), 1500 * MS);

    // A whole-flow save that names offsets still sets them.
    let mut named = stale;
    named.properties.source_offsets_ms = [("cam1".into(), -200)].into();
    let _ = update_flow(State(state.clone()), Path(id), JsonBody(named))
        .await
        .unwrap();
    assert_eq!(p.offset_ns(), -200 * MS);
    drop(tl);
}

/// Changes for different sources sent at once all land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_offset_changes_all_land() {
    gstreamer::init().unwrap();
    let state = common::state::new().app();
    let flow = Flow::new("offsets concurrent");
    let id = flow.id;
    state.upsert_flow(flow).await.unwrap();
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let state = state.clone();
            tokio::spawn(async move {
                let key = format!("cam{}", i);
                let _ = patch_source_offsets(
                    State(state),
                    Path(id),
                    JsonBody(patch(&[(key.as_str(), Some(i * 10))])),
                )
                .await
                .unwrap();
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let stored = state
        .get_flow(&id)
        .await
        .unwrap()
        .properties
        .source_offsets_ms;
    assert_eq!(stored.len(), 16);
    let missing = patch_source_offsets(
        State(state.clone()),
        Path(uuid::Uuid::new_v4()),
        JsonBody(patch(&[("cam1", Some(1))])),
    )
    .await;
    assert_eq!(missing.unwrap_err().0, StatusCode::NOT_FOUND);
}
