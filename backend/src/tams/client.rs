//! HTTP client for the Eyevinn TAMS Gateway.
//!
//! The gateway separates a CouchDB segment index from S3 essence storage and
//! hands out presigned PUT/GET URLs, so this client only ever talks to the
//! gateway base URL (for flow/segment metadata and storage allocation) and to
//! the presigned S3 URLs it returns. No S3 credentials live in Strom.
//!
//! Contract (verified against the tams-gateway source):
//! - `PUT  /flows/{id}`           create/replace a flow (auto-creates its source)
//! - `POST /flows/{id}/storage`   allocate media objects, returns presigned PUT URLs
//! - `PUT  <presigned url>`       upload the segment bytes to S3
//! - `POST /flows/{id}/segments`  register a segment on the flow timeline
//! - `GET  /flows/{id}/segments?timerange=[..)`  list segments (presigned GET URLs)

use crate::client_auth::AuthMethod;
use anyhow::{anyhow, Context, Result};
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use std::time::Duration;

// Protocol constants and timerange formatting live in `strom_types::tams` so the
// frontend can share them. Re-exported here for ergonomic use within the backend.
pub use strom_types::tams::{format_timerange, FORMAT_AUDIO, FORMAT_MULTI, FORMAT_VIDEO};

/// A non-success HTTP response from the gateway or presigned storage, carrying
/// the status code so the uploader can decide whether retrying could ever help.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: reqwest::StatusCode,
    /// Human context, e.g. `presigned PUT` or `POST http://.../flows/x/segments`.
    pub context: String,
    pub body: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} -> {}: {}", self.context, self.status, self.body)
    }
}

impl std::error::Error for HttpStatusError {}

impl HttpStatusError {
    /// A 4xx (other than 408 Request Timeout and 429 Too Many Requests) means the
    /// request itself must change before it can succeed — retrying the identical
    /// bytes/headers will fail the same way (e.g. 413 Payload Too Large, 401
    /// Unauthorized). Network errors and 5xx/408/429 are worth retrying.
    pub fn is_retryable(&self) -> bool {
        let code = self.status.as_u16();
        !((400..500).contains(&code) && code != 408 && code != 429)
    }
}

/// Metadata describing a flow to create on the gateway.
#[derive(Debug, Clone)]
pub struct FlowSpec {
    pub flow_id: String,
    pub source_id: String,
    /// NMOS format URN (see [`FORMAT_VIDEO`] / [`FORMAT_AUDIO`] / [`FORMAT_MULTI`]).
    pub format: String,
    /// Codec string, e.g. `video/h264`, `video/h265`, `audio/aac`. `None` for a
    /// grouping Multi-Flow that carries no essence of its own.
    pub codec: Option<String>,
    /// Container MIME, e.g. `video/mp4`. `None` for a grouping Multi-Flow.
    pub container: Option<String>,
    /// Human-readable title, stored as the flow's `label`.
    pub label: Option<String>,
    /// Longer free-text, stored as the flow's `description`.
    pub description: Option<String>,
    /// Flow tags (key -> value), e.g. `production` -> `Studio A`.
    pub tags: Vec<(String, String)>,
    /// Member flows collected by this flow, as `(flow_id, role)` pairs. Non-empty
    /// only for a Multi-Flow that groups per-essence Flows under one NMOS
    /// `format:multi` flow (the canonical TAMS multi-essence model). Serialized as
    /// the TAMS `flow_collection` array of `{ id, role }` objects.
    pub flow_collection: Vec<(String, String)>,
}

/// Build the JSON body for `PUT /flows/{id}` from a [`FlowSpec`]. Pure (no I/O) so
/// the gateway wire format can be unit-tested.
///
/// Wire-format contract (matches what the Eyevinn TAMS gateway accepts):
/// - `id`, `source_id`, `format`, `essence_parameters` always present.
/// - `codec`/`container` emitted only when `Some` (a grouping Multi-Flow omits them).
/// - `flow_collection` members are `{ id, role, container_mapping: { track_index } }`,
///   where `track_index` is the member's position in the collection.
/// - `label`/`description`/`tags` emitted only when present.
fn flow_request_body(spec: &FlowSpec) -> serde_json::Value {
    let mut body = serde_json::json!({
        "id": spec.flow_id,
        "source_id": spec.source_id,
        "format": spec.format,
        "essence_parameters": {},
    });
    // codec/container apply to flows that carry essence; a grouping Multi-Flow
    // omits them and instead lists its members in flow_collection.
    if let Some(codec) = &spec.codec {
        body["codec"] = serde_json::Value::String(codec.clone());
    }
    if let Some(container) = &spec.container {
        body["container"] = serde_json::Value::String(container.clone());
    }
    if !spec.flow_collection.is_empty() {
        // Each member is a collection-item (id + role) plus a container_mapping,
        // which the gateway requires. A grouping Multi-Flow has no real shared
        // container, so we use the generic, container-agnostic track_index =
        // position in the collection (video first, then audio).
        let members: Vec<serde_json::Value> = spec
            .flow_collection
            .iter()
            .enumerate()
            .map(|(i, (id, role))| {
                serde_json::json!({
                    "id": id,
                    "role": role,
                    "container_mapping": { "track_index": i },
                })
            })
            .collect();
        body["flow_collection"] = serde_json::Value::Array(members);
    }
    if let Some(label) = &spec.label {
        body["label"] = serde_json::Value::String(label.clone());
    }
    if let Some(description) = &spec.description {
        body["description"] = serde_json::Value::String(description.clone());
    }
    if !spec.tags.is_empty() {
        let tags: serde_json::Map<String, serde_json::Value> = spec
            .tags
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        body["tags"] = serde_json::Value::Object(tags);
    }
    body
}

/// A storage object allocated by the gateway, ready to receive bytes.
#[derive(Debug, Clone)]
pub struct AllocatedObject {
    /// `<bucket>/<key>` — the same id resolves to a GET URL when segments are listed.
    pub object_id: String,
    /// Presigned S3 PUT URL.
    pub put_url: String,
    /// Content-Type the presigned PUT was signed with; must be sent verbatim.
    pub content_type: String,
}

#[derive(Deserialize)]
struct StorageResponse {
    media_objects: Vec<MediaObjectResp>,
}

#[derive(Deserialize)]
struct MediaObjectResp {
    object_id: String,
    put_url: PutUrlResp,
}

#[derive(Deserialize)]
struct PutUrlResp {
    url: String,
    #[serde(rename = "content-type")]
    content_type: String,
}

/// Client for a single TAMS gateway endpoint.
#[derive(Clone)]
pub struct TamsClient {
    /// Base URL without trailing slash, e.g. `http://localhost:8000`.
    base_url: String,
    auth: AuthMethod,
    http: reqwest::Client,
}

impl TamsClient {
    pub fn new(base_url: impl Into<String>, auth: AuthMethod) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building TAMS HTTP client")?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(Self {
            base_url,
            auth,
            http,
        })
    }

    /// Create or replace a flow (and, gateway-side, its source). Idempotent.
    pub async fn ensure_flow(&self, spec: &FlowSpec) -> Result<()> {
        let url = format!("{}/flows/{}", self.base_url, spec.flow_id);
        let body = flow_request_body(spec);

        let resp = self
            .auth
            .apply(self.http.put(&url))
            .await?
            .json(&body)
            .send()
            .await
            .with_context(|| format!("PUT {}", url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpStatusError {
                status,
                context: format!("PUT {}", url),
                body,
            }
            .into());
        }
        Ok(())
    }

    /// Allocate one media object and get its presigned PUT URL.
    pub async fn allocate_object(
        &self,
        flow_id: &str,
        content_type: &str,
    ) -> Result<AllocatedObject> {
        let url = format!("{}/flows/{}/storage", self.base_url, flow_id);
        let body = serde_json::json!({ "limit": 1, "content_type": content_type });
        let resp = self
            .auth
            .apply(self.http.post(&url))
            .await?
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpStatusError {
                status,
                context: format!("POST {}", url),
                body,
            }
            .into());
        }
        let parsed: StorageResponse = resp.json().await.context("parsing storage response")?;
        let obj = parsed
            .media_objects
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("gateway returned no media objects"))?;
        Ok(AllocatedObject {
            object_id: obj.object_id,
            put_url: obj.put_url.url,
            content_type: obj.put_url.content_type,
        })
    }

    /// Upload segment bytes to a presigned S3 PUT URL.
    pub async fn upload_object(
        &self,
        put_url: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        // Note: no auth header here — the URL itself is presigned.
        let resp = self
            .http
            .put(put_url)
            .header(CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await
            .context("PUT presigned S3 url")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpStatusError {
                status,
                context: "presigned PUT".to_string(),
                body,
            }
            .into());
        }
        Ok(())
    }

    /// Register a segment on the flow timeline.
    pub async fn register_segment(
        &self,
        flow_id: &str,
        object_id: &str,
        timerange: &str,
    ) -> Result<()> {
        let url = format!("{}/flows/{}/segments", self.base_url, flow_id);
        let body = serde_json::json!({ "object_id": object_id, "timerange": timerange });
        let resp = self
            .auth
            .apply(self.http.post(&url))
            .await?
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;
        let status = resp.status();
        // 201 = all registered; 200 = partial failure (failed_segments listed).
        if status.as_u16() == 200 {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("segment registration partially failed: {}", text));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpStatusError {
                status,
                context: format!("POST {}", url),
                body,
            }
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Timerange/timestamp formatting is tested in `strom_types::tams`.

    #[test]
    fn base_url_trailing_slash_trimmed() {
        let c = TamsClient::new("http://localhost:8000/", AuthMethod::None).unwrap();
        assert_eq!(c.base_url, "http://localhost:8000");
    }

    fn spec() -> FlowSpec {
        FlowSpec {
            flow_id: "flow-1".into(),
            source_id: "src-1".into(),
            format: "urn:x-nmos:format:video".into(),
            codec: Some("video/h264".into()),
            container: Some("video/mp4".into()),
            label: None,
            description: None,
            tags: vec![],
            flow_collection: vec![],
        }
    }

    #[test]
    fn mono_flow_body_has_codec_and_no_collection() {
        let body = flow_request_body(&spec());
        assert_eq!(body["id"], "flow-1");
        assert_eq!(body["source_id"], "src-1");
        assert_eq!(body["format"], "urn:x-nmos:format:video");
        assert_eq!(body["codec"], "video/h264");
        assert_eq!(body["container"], "video/mp4");
        assert!(body.get("essence_parameters").is_some());
        // Optional fields absent when not set.
        assert!(body.get("flow_collection").is_none());
        assert!(body.get("label").is_none());
        assert!(body.get("tags").is_none());
    }

    #[test]
    fn grouping_flow_omits_codec_and_container() {
        let mut s = spec();
        s.codec = None;
        s.container = None;
        let body = flow_request_body(&s);
        // A grouping Multi-Flow carries no essence: codec/container must be absent,
        // not serialized as null.
        assert!(body.get("codec").is_none());
        assert!(body.get("container").is_none());
    }

    #[test]
    fn flow_collection_members_carry_role_and_track_index() {
        let mut s = spec();
        s.format = FORMAT_MULTI.into();
        s.codec = None;
        s.container = None;
        s.flow_collection = vec![
            ("video-flow".into(), "video".into()),
            ("audio-flow".into(), "audio".into()),
        ];
        let body = flow_request_body(&s);
        let coll = body["flow_collection"].as_array().expect("array");
        assert_eq!(coll.len(), 2);
        assert_eq!(coll[0]["id"], "video-flow");
        assert_eq!(coll[0]["role"], "video");
        assert_eq!(coll[0]["container_mapping"]["track_index"], 0);
        assert_eq!(coll[1]["id"], "audio-flow");
        assert_eq!(coll[1]["role"], "audio");
        assert_eq!(coll[1]["container_mapping"]["track_index"], 1);
    }

    #[test]
    fn tags_serialized_as_object() {
        let mut s = spec();
        s.tags = vec![("prod".into(), "Studio A".into())];
        let body = flow_request_body(&s);
        assert_eq!(body["tags"]["prod"], "Studio A");
    }
}
