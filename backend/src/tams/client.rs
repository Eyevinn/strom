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
pub use strom_types::tams::{format_timerange, FORMAT_AUDIO, FORMAT_VIDEO};

/// Metadata describing a flow to create on the gateway.
#[derive(Debug, Clone)]
pub struct FlowSpec {
    pub flow_id: String,
    pub source_id: String,
    /// NMOS format URN (see [`FORMAT_VIDEO`] / [`FORMAT_AUDIO`]).
    pub format: String,
    /// Codec string, e.g. `video/h264`, `video/h265`, `audio/aac`.
    pub codec: String,
    /// Container MIME, e.g. `video/mp4`.
    pub container: String,
    /// Human-readable title, stored as the flow's `label`.
    pub label: Option<String>,
    /// Longer free-text, stored as the flow's `description`.
    pub description: Option<String>,
    /// Flow tags (key -> value), e.g. `production` -> `Studio A`.
    pub tags: Vec<(String, String)>,
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
        let mut body = serde_json::json!({
            "id": spec.flow_id,
            "source_id": spec.source_id,
            "format": spec.format,
            "codec": spec.codec,
            "container": spec.container,
            "essence_parameters": {},
        });
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
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("PUT {} -> {}: {}", url, status, text));
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
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("POST {} -> {}: {}", url, status, text));
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
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("presigned PUT -> {}: {}", status, text));
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
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("POST {} -> {}: {}", url, status, text));
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
}
