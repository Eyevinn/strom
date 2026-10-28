//! Regression test for issue #928.
//!
//! The element and pad property endpoints document `404` for a flow that is
//! not running and for an element or pad that is not there, but answered
//! `400` for every error. A client could not tell "not there" from "bad
//! value". A missing or stopped flow was also reported as an invalid flow.
//!
//! `videotestsrc` is in gstreamer1.0-plugins-base and `fakesink` in core,
//! both installed in every CI test job.

pub mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use std::collections::HashMap;
use strom_types::{Flow, FlowId, Link};
use tower::ServiceExt; // for `oneshot`

/// `videotestsrc is-live=true → fakesink`.
fn build_flow() -> Flow {
    let mut flow = Flow::new("property_not_found_status_test");
    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "videotestsrc".to_string(),
        properties: HashMap::from([(
            "is-live".to_string(),
            strom_types::PropertyValue::Bool(true),
        )]),
        position: [100.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });
    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::new(),
        position: [300.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });
    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "sink:sink".to_string(),
    });
    flow
}

async fn status(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> StatusCode {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

/// GET and PATCH on the element and on its `sink` pad, with the status each
/// returned, labelled for the assertion message.
async fn all_four(
    app: &Router,
    flow_id: &FlowId,
    element: &str,
    pad: &str,
) -> Vec<(String, StatusCode)> {
    let element_uri = format!("/api/flows/{}/elements/{}/properties", flow_id, element);
    let pad_uri = format!(
        "/api/flows/{}/elements/{}/pads/{}/properties",
        flow_id, element, pad
    );
    let patch = serde_json::json!({ "property_name": "name", "value": "x" });
    vec![
        (
            format!("GET {}", element_uri),
            status(app, Method::GET, &element_uri, None).await,
        ),
        (
            format!("PATCH {}", element_uri),
            status(app, Method::PATCH, &element_uri, Some(patch.clone())).await,
        ),
        (
            format!("GET {}", pad_uri),
            status(app, Method::GET, &pad_uri, None).await,
        ),
        (
            format!("PATCH {}", pad_uri),
            status(app, Method::PATCH, &pad_uri, Some(patch)).await,
        ),
    ]
}

fn assert_all(results: &[(String, StatusCode)], expected: StatusCode) {
    for (call, got) in results {
        assert_eq!(
            *got, expected,
            "{} answered {}, expected {}",
            call, got, expected
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_flow_element_or_pad_answers_404() {
    gstreamer::init().unwrap();
    let state = common::state::new();
    let flow = build_flow();
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");
    let app = strom::create_app_with_state(state.app()).await;

    // A flow that does not exist.
    let unknown: FlowId = "00000000-0000-4000-8000-000000000928".parse().unwrap();
    assert_all(
        &all_four(&app, &unknown, "src", "src").await,
        StatusCode::NOT_FOUND,
    );

    // A flow that exists but is not running.
    assert_all(
        &all_four(&app, &flow_id, "src", "src").await,
        StatusCode::NOT_FOUND,
    );

    state.start_flow(&flow_id).await.expect("start_flow failed");

    // A running flow without that element.
    assert_all(
        &all_four(&app, &flow_id, "nope", "src").await,
        StatusCode::NOT_FOUND,
    );

    // A running element without that pad. fakesink has no request pads, so
    // the PATCH cannot create one either.
    let pad_uri = format!("/api/flows/{}/elements/sink/pads/nope/properties", flow_id);
    assert_eq!(
        status(&app, Method::GET, &pad_uri, None).await,
        StatusCode::NOT_FOUND
    );
    let patch = serde_json::json!({ "property_name": "offset", "value": 0 });
    assert_eq!(
        status(&app, Method::PATCH, &pad_uri, Some(patch)).await,
        StatusCode::NOT_FOUND
    );

    // Everything is there: the reads succeed.
    let element_uri = format!("/api/flows/{}/elements/src/properties", flow_id);
    assert_eq!(
        status(&app, Method::GET, &element_uri, None).await,
        StatusCode::OK
    );
    let pad_uri = format!("/api/flows/{}/elements/src/pads/src/properties", flow_id);
    assert_eq!(
        status(&app, Method::GET, &pad_uri, None).await,
        StatusCode::OK
    );

    // A property the element does not have stays a 400: the element is there,
    // the request is wrong.
    let patch = serde_json::json!({ "property_name": "no-such-property", "value": 1 });
    assert_eq!(
        status(&app, Method::PATCH, &element_uri, Some(patch)).await,
        StatusCode::BAD_REQUEST
    );

    state.stop_flow(&flow_id).await.expect("stop_flow failed");
}
