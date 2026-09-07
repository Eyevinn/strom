//! Integration tests for the MCP Streamable HTTP endpoint.
//!
//! These drive `/api/mcp` through the real router, which is the only way to
//! catch the transport-level mistakes: a response emitted for a notification,
//! a missing method, a tool failure escalated into a protocol error.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`

async fn create_test_app() -> Router {
    use strom::create_app;

    gstreamer::init().unwrap();
    create_app().await
}

/// POST a JSON-RPC message to /api/mcp and return (status, body).
async fn post_mcp(app: &Router, session_id: Option<&str>, message: Value) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/mcp")
        .header("content-type", "application/json");
    if let Some(sid) = session_id {
        builder = builder.header("mcp-session-id", sid);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(message.to_string())).unwrap())
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

async fn post_mcp_json(app: &Router, session_id: Option<&str>, message: Value) -> Value {
    let (status, body) = post_mcp(app, session_id, message).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "body: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).expect("response is JSON")
}

/// Run the handshake and return the session id the server assigned.
async fn initialize(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 0,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "clientInfo": { "name": "test", "version": "1" }
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .expect("initialize assigns a session id")
        .to_str()
        .unwrap()
        .to_string();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["jsonrpc"], "2.0");
    assert!(json["result"]["protocolVersion"].is_string());
    assert_eq!(json["result"]["serverInfo"]["name"], "strom");
    assert!(json["error"].is_null(), "initialize failed: {}", json);

    session_id
}

#[tokio::test]
async fn initialize_assigns_a_session() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;
    assert!(!session_id.is_empty());
}

/// The regression: `notifications/initialized` is what every client sends
/// after the handshake, and the server used to answer it with a
/// "Method not found" error carrying a null id — a response to a notification,
/// which the client cannot match to any request it made.
#[tokio::test]
async fn notifications_are_accepted_without_a_response() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    for notification in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
        // A notification this server has never heard of is still a
        // notification: silence, not an error.
        json!({"jsonrpc": "2.0", "method": "notifications/from/the/future"}),
        // The old, non-standard spelling stays accepted.
        json!({"jsonrpc": "2.0", "method": "initialized"}),
    ] {
        let (status, body) = post_mcp(&app, Some(&session_id), notification.clone()).await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "{} should be accepted with 202, got {} and body {}",
            notification["method"],
            status,
            String::from_utf8_lossy(&body)
        );
        assert!(
            body.is_empty(),
            "{} must not get a response body, got {}",
            notification["method"],
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test]
async fn ping_is_answered() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    let json = post_mcp_json(
        &app,
        Some(&session_id),
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
    )
    .await;

    assert!(json["error"].is_null(), "ping failed: {}", json);
    assert_eq!(json["id"], 1);
    assert!(json["result"].is_object());
}

#[tokio::test]
async fn tools_list_returns_the_tool_set() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    let json = post_mcp_json(
        &app,
        Some(&session_id),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;

    let tools = json["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // The tools an assistant needs to be useful at all. Asserting the names
    // rather than the count so that adding a tool does not fail the test but
    // silently dropping one does.
    for expected in [
        "list_flows",
        "get_flow",
        "create_flow",
        "update_flow",
        "delete_flow",
        "start_flow",
        "stop_flow",
        "list_elements",
        "get_element_info",
    ] {
        assert!(
            names.contains(&expected),
            "{} is missing from {:?}",
            expected,
            names
        );
    }

    // Every tool needs a description and a schema, or a model cannot call it.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{} has no description",
            name
        );
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "{} has no object schema",
            name
        );
    }
}

#[tokio::test]
async fn a_round_trip_creates_and_deletes_a_flow() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    let created = post_mcp_json(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "create_flow", "arguments": { "name": "MCP test flow" } }
        }),
    )
    .await;

    assert!(
        created["error"].is_null(),
        "create_flow failed: {}",
        created
    );
    let payload: Value = serde_json::from_str(
        created["result"]["content"][0]["text"]
            .as_str()
            .expect("tool result carries text content"),
    )
    .expect("tool result text is JSON");
    let flow_id = payload["flow"]["id"].as_str().expect("new flow has an id");
    assert_eq!(payload["flow"]["name"], "MCP test flow");

    let listed = post_mcp_json(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "list_flows", "arguments": {} }
        }),
    )
    .await;
    let listed_payload: Value =
        serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        listed_payload["flows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == flow_id),
        "created flow is missing from list_flows"
    );

    let deleted = post_mcp_json(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "delete_flow", "arguments": { "flow_id": flow_id } }
        }),
    )
    .await;
    assert!(
        deleted["error"].is_null(),
        "delete_flow failed: {}",
        deleted
    );
}

/// A tool that ran and failed is a result with `isError`, not a JSON-RPC
/// error — the model has to be able to read the reason and recover.
#[tokio::test]
async fn a_failing_tool_returns_an_is_error_result() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    let json = post_mcp_json(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_flow",
                // Well-formed UUID, no such flow.
                "arguments": { "flow_id": "00000000-0000-0000-0000-000000000000" }
            }
        }),
    )
    .await;

    assert!(
        json["error"].is_null(),
        "a tool failure must not surface as a protocol error: {}",
        json
    );
    assert_eq!(
        json["result"]["isError"], true,
        "expected isError: {}",
        json
    );
    let text = json["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("not found"),
        "error text should say what went wrong, got: {}",
        text
    );
}

/// A call the server cannot make sense of is a JSON-RPC error, and the message
/// has to be actionable — the old one leaked a UUID parser's internals
/// ("invalid character: found `n` at 0").
#[tokio::test]
async fn a_malformed_call_returns_an_actionable_protocol_error() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    let unknown_tool = post_mcp_json(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "no_such_tool", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(unknown_tool["error"]["code"], -32602);
    assert!(unknown_tool["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no_such_tool"));

    let bad_uuid = post_mcp_json(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_flow", "arguments": { "flow_id": "not-a-uuid" } }
        }),
    )
    .await;
    assert_eq!(bad_uuid["error"]["code"], -32602);
    let message = bad_uuid["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("UUID") && message.contains("not-a-uuid"),
        "message should name the offending value and what was wanted, got: {}",
        message
    );

    let missing_argument = post_mcp_json(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "create_flow", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(missing_argument["error"]["code"], -32602);
    assert!(missing_argument["error"]["message"]
        .as_str()
        .unwrap()
        .contains("name"));
}

#[tokio::test]
async fn an_unknown_request_method_is_a_method_not_found_error() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    let json = post_mcp_json(
        &app,
        Some(&session_id),
        json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}),
    )
    .await;

    assert_eq!(json["error"]["code"], -32601);
    assert_eq!(json["id"], 1);
}

#[tokio::test]
async fn an_unknown_session_is_rejected_and_delete_ends_a_known_one() {
    let app = create_test_app().await;
    let session_id = initialize(&app).await;

    let (status, _) = post_mcp(
        &app,
        Some("00000000-0000-0000-0000-000000000000"),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/mcp")
                .header("mcp-session-id", &session_id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // The session is gone, so the next request on it is refused.
    let (status, _) = post_mcp(
        &app,
        Some(&session_id),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_cross_site_origin_is_refused() {
    let app = create_test_app().await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header("content-type", "application/json")
                .header("origin", "https://evil.example.net")
                .header("host", "strom.example.com")
                .body(Body::from(
                    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
