//! Request/response behaviour of `ApiClient`, checked against a stub HTTP server.
//!
//! Each test pins what one request shape sends and how each kind of response
//! maps to a result or an `ApiError` variant.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use instant::Instant;

use strom_types::api::{ActivateProbeRequest, FlowListResponse, LatencyResponse};
use strom_types::{Flow, FlowId, PropertyValue};

use super::{ApiClient, ApiError};

/// One request as the stub server received it.
struct Recorded {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

enum Reply {
    Respond {
        status: u16,
        body: Vec<u8>,
    },
    /// Accept the connection and never answer.
    Hang,
}

fn respond(status: u16, body: impl Into<Vec<u8>>) -> Reply {
    Reply::Respond {
        status,
        body: body.into(),
    }
}

/// Serve `replies` in order, one connection each, and record every request.
fn serve(replies: Vec<Reply>) -> (String, mpsc::Receiver<Recorded>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = tx.send(read_request(&mut stream));
            match reply {
                Reply::Respond { status, body } => {
                    let head = format!(
                        "HTTP/1.1 {} Stub\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        status,
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
                Reply::Hang => std::thread::sleep(Duration::from_secs(5)),
            }
        }
    });

    (base, rx)
}

fn read_request(stream: &mut std::net::TcpStream) -> Recorded {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed before the request head ended");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap().split(' ');
    let method = request_line.next().unwrap().to_string();
    let target = request_line.next().unwrap().to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();

    let len: usize = headers
        .get("content-length")
        .map_or(0, |v| v.parse().unwrap());
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed before the request body ended");
        body.extend_from_slice(&chunk[..n]);
    }

    Recorded {
        method,
        target,
        headers,
        body,
    }
}

fn client(base: &str) -> ApiClient {
    ApiClient::new_with_auth(base, None)
}

fn flow_id() -> FlowId {
    uuid::Uuid::nil()
}

fn latency() -> LatencyResponse {
    LatencyResponse {
        min_latency_ns: 1_000_000,
        max_latency_ns: 2_000_000,
        live: true,
        min_latency_formatted: "1 ms".to_string(),
        max_latency_formatted: "2 ms".to_string(),
    }
}

#[tokio::test]
async fn json_body_is_returned_as_is() {
    let (base, rx) = serve(vec![respond(200, serde_json::to_vec(&latency()).unwrap())]);

    let got = client(&base).get_flow_latency(flow_id()).await.unwrap();

    assert_eq!(got.min_latency_ns, 1_000_000);
    assert!(got.live);
    let req = rx.recv().unwrap();
    assert_eq!(req.method, "GET");
    assert_eq!(req.target, format!("/flows/{}/latency", flow_id()));
}

#[tokio::test]
async fn json_envelope_is_unwrapped() {
    let body = FlowListResponse {
        flows: vec![Flow::new("stub flow")],
    };
    let (base, _rx) = serve(vec![respond(200, serde_json::to_vec(&body).unwrap())]);

    let flows = client(&base).list_flows().await.unwrap();

    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0].name, "stub flow");
}

#[tokio::test]
async fn post_sends_the_json_body() {
    let (base, rx) = serve(vec![respond(200, r#"{"probe_id":"p1"}"#)]);

    let got = client(&base)
        .activate_probe(&flow_id(), "src", Some(2), Some(30))
        .await
        .unwrap();

    assert_eq!(got.probe_id, "p1");
    let req = rx.recv().unwrap();
    assert_eq!(req.method, "POST");
    assert_eq!(req.target, format!("/flows/{}/probes", flow_id()));
    let sent: ActivateProbeRequest = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(sent.element_id, "src");
    assert_eq!(sent.sample_interval, Some(2));
    assert_eq!(sent.timeout_secs, Some(30));
}

#[tokio::test]
async fn put_sends_the_json_body() {
    let (base, rx) = serve(vec![respond(
        200,
        r#"{"current":"debug","default":"info"}"#,
    )]);

    let got = client(&base).set_log_level("debug").await.unwrap();

    assert_eq!(got.current, "debug");
    let req = rx.recv().unwrap();
    assert_eq!(req.method, "PUT");
    assert_eq!(req.target, "/log-level");
    assert_eq!(req.body, br#"{"filter":"debug"}"#);
}

#[tokio::test]
async fn patch_with_empty_204_is_ok() {
    let (base, rx) = serve(vec![respond(204, "")]);
    let mut properties = HashMap::new();
    properties.insert("mute".to_string(), PropertyValue::Bool(true));

    client(&base)
        .update_block_properties(&flow_id(), "mixer", properties, Some(50))
        .await
        .unwrap();

    let req = rx.recv().unwrap();
    assert_eq!(req.method, "PATCH");
    assert_eq!(
        req.target,
        format!("/flows/{}/blocks/mixer/properties", flow_id())
    );
    let sent: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(sent["properties"]["mute"], serde_json::json!(true));
    assert_eq!(sent["ramp_ms"], 50);
}

#[tokio::test]
async fn delete_ignores_the_response_body() {
    let (base, rx) = serve(vec![respond(200, "not json")]);

    client(&base).delete_flow(flow_id()).await.unwrap();

    let req = rx.recv().unwrap();
    assert_eq!(req.method, "DELETE");
    assert_eq!(req.target, format!("/flows/{}", flow_id()));
}

#[tokio::test]
async fn invalid_json_is_a_decode_error() {
    let (base, _rx) = serve(vec![respond(200, "not json")]);

    let err = client(&base).get_flow_latency(flow_id()).await.unwrap_err();

    assert!(matches!(err, ApiError::Decode(_)), "got {err:?}");
}

#[tokio::test]
async fn non_success_carries_status_and_body() {
    let (base, _rx) = serve(vec![respond(404, "no such flow")]);

    let err = client(&base).get_flow_latency(flow_id()).await.unwrap_err();

    assert!(
        matches!(&err, ApiError::Http(404, body) if body == "no such flow"),
        "got {err:?}"
    );
}

/// `fetch_system_clock_info` matches `Http(501, _)` to mean "unsupported platform".
#[tokio::test]
async fn system_clock_501_is_http_501() {
    let (base, _rx) = serve(vec![respond(501, "not supported")]);

    let err = client(&base).get_system_clock().await.unwrap_err();

    assert!(matches!(err, ApiError::Http(501, _)), "got {err:?}");
}

#[tokio::test]
async fn refused_connection_is_a_network_error() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let err = client(&format!("http://127.0.0.1:{port}"))
        .delete_flow(flow_id())
        .await
        .unwrap_err();

    assert!(matches!(err, ApiError::Network(_)), "got {err:?}");
}

#[tokio::test]
async fn bearer_token_is_sent_when_set() {
    let (base, rx) = serve(vec![respond(200, "")]);

    ApiClient::new_with_auth(&base, Some("secret".to_string()))
        .start_flow(flow_id())
        .await
        .unwrap();

    let req = rx.recv().unwrap();
    assert_eq!(
        req.headers.get("authorization").map(String::as_str),
        Some("Bearer secret")
    );
}

#[tokio::test]
async fn no_authorization_header_without_a_token() {
    let (base, rx) = serve(vec![respond(200, "")]);

    client(&base).start_flow(flow_id()).await.unwrap();

    assert!(!rx.recv().unwrap().headers.contains_key("authorization"));
}

#[tokio::test]
async fn thumbnail_returns_raw_bytes() {
    let jpeg = vec![0xFF, 0xD8, 0xFF, 0x00, 0x42];
    let (base, rx) = serve(vec![respond(200, jpeg.clone()), respond(200, jpeg.clone())]);
    let api = client(&base);

    assert_eq!(api.get_block_thumbnail("f1", "b1", 0).await.unwrap(), jpeg);
    assert_eq!(api.get_block_thumbnail("f1", "b1", 2).await.unwrap(), jpeg);

    assert_eq!(rx.recv().unwrap().target, "/flows/f1/blocks/b1/thumbnail");
    assert_eq!(
        rx.recv().unwrap().target,
        "/flows/f1/blocks/b1/thumbnail?index=2"
    );
}

#[tokio::test]
async fn sdp_is_returned_as_text() {
    let sdp = "v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=stub\r\n";
    let (base, rx) = serve(vec![respond(200, sdp)]);

    assert_eq!(client(&base).get_stream_sdp("s1").await.unwrap(), sdp);
    assert_eq!(rx.recv().unwrap().target, "/discovery/streams/s1/sdp");
}

/// A failed status request means the NDI plugin is absent, not an error.
#[tokio::test]
async fn ndi_status_failure_means_unavailable() {
    let (base, rx) = serve(vec![respond(404, "")]);

    let (available, sources) = client(&base).get_ndi_sources().await.unwrap();

    assert!(!available);
    assert!(sources.is_empty());
    assert_eq!(rx.recv().unwrap().target, "/discovery/ndi/status");
    assert!(
        rx.try_recv().is_err(),
        "no sources request after a failed status"
    );
}

#[tokio::test]
async fn ndi_sources_are_fetched_when_available() {
    let sources = r#"[{"id":"n1","name":"Camera 1","device_class":"Source/Network",
        "category":"networksource","provider":"ndideviceprovider","properties":{},
        "first_seen_secs_ago":3,"last_seen_secs_ago":1}]"#;
    let (base, rx) = serve(vec![
        respond(200, r#"{"available":true,"source_count":1}"#),
        respond(200, sources),
    ]);

    let (available, got) = client(&base).get_ndi_sources().await.unwrap();

    assert!(available);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "Camera 1");
    assert_eq!(rx.recv().unwrap().target, "/discovery/ndi/status");
    assert_eq!(rx.recv().unwrap().target, "/discovery/ndi/sources");
}

/// The compositor editor rolls back its optimistic swap on failure, so a take
/// that gets no answer must fail on the 3 s request timeout, not hang.
#[tokio::test]
async fn transition_times_out_as_a_network_error() {
    let (base, _rx) = serve(vec![Reply::Hang]);
    let started = Instant::now();

    let err = client(&base)
        .trigger_transition("f1", "comp", 0, 1, "fade", 500)
        .await
        .unwrap_err();

    let elapsed = started.elapsed();
    assert!(matches!(err, ApiError::Network(_)), "got {err:?}");
    assert!(
        elapsed >= Duration::from_millis(2500) && elapsed <= Duration::from_millis(4500),
        "timed out after {elapsed:?}"
    );
}

#[tokio::test]
async fn media_path_is_url_encoded() {
    let (base, rx) = serve(vec![respond(
        200,
        r#"{"current_path":"a b/c","entries":[]}"#,
    )]);

    let got = client(&base).list_media("a b/c").await.unwrap();

    assert_eq!(got.current_path, "a b/c");
    assert_eq!(rx.recv().unwrap().target, "/media?path=a%20b%2Fc");
}
