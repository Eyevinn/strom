//! HTTP client for the Eyevinn TAMS Gateway.
//!
//! The gateway separates a CouchDB segment index from S3 essence storage and
//! hands out presigned PUT/GET URLs, so this client only ever talks to the
//! gateway base URL (for flow/segment metadata and storage allocation) and to
//! the presigned S3 URLs it returns. No S3 credentials live in Strom.
//!
//! Contract (verified against the tams-gateway source):
//! - `PUT  /flows/{id}`           create/replace a flow (auto-creates its source)
//! - `POST /flows/{id}/storage`   allocate media objects (`limit` per request),
//!   returns presigned PUT URLs
//! - `PUT  <presigned url>`       upload the segment bytes to S3
//! - `POST /flows/{id}/segments`  register a segment on the flow timeline, with
//!   an `X-Idempotency-Key` derived from the segment (required by some stores);
//!   the body may be an array of segments (the drainer registers in batches)
//! - `GET  /flows/{id}/segments?timerange=[..)`  list segments (presigned GET URLs)
//! - `GET  /flows/{id}`           read a flow (frame rate, container)
//!
//! Reading follows the TAMS spec rather than gateway specifics: paging uses the
//! opaque `X-Paging-NextKey` as the `page` query parameter, falling back to a
//! `Link: <..>; rel="next"` header.
//!
//! Credentials stay with the gateway: authenticated requests go only to the
//! origin of the base URL (a `Link` to another origin is refused, redirects are
//! followed only within the origin), presigned object URLs are fetched without
//! credentials on a separate client, and OSC SATs only go to a gateway whose
//! host names their service (or to an allow-listed gateway). The instance's own
//! static token (`STROM_TAMS_API_TOKEN`) is handed out by [`env_api_token`] only
//! for allow-listed gateways.

use crate::client_auth::{redact_url, same_origin, same_origin_redirects, AuthMethod};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::warn;

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

/// Longest response body kept in an error. Error bodies are logged, and the
/// server that sent them is not always trusted.
const MAX_ERROR_BODY_CHARS: usize = 300;

/// Turn a non-success response into an [`HttpStatusError`] with a bounded body.
async fn status_error(resp: reqwest::Response, context: String) -> anyhow::Error {
    let status = resp.status();
    let mut body = resp.text().await.unwrap_or_default();
    if let Some((cut, _)) = body.char_indices().nth(MAX_ERROR_BODY_CHARS) {
        body.truncate(cut);
        body.push_str("...");
    }
    HttpStatusError {
        status,
        context,
        body,
    }
    .into()
}

/// Largest media object [`TamsClient::fetch_object`] reads (256 MiB). A TAMS
/// segment is seconds of media; anything near this is not one.
pub const MAX_OBJECT_BYTES: u64 = 256 * 1024 * 1024;

/// Env var listing gateways (comma separated URLs) that may receive this
/// instance's own TAMS credentials, besides `STROM_TAMS_GATEWAY_URL`.
pub const ALLOWED_GATEWAYS_ENV: &str = "STROM_TAMS_ALLOWED_GATEWAYS";

/// Whether `gateway` has the origin of one of the `allowed` URLs.
fn is_allow_listed<'a>(gateway: &str, allowed: impl IntoIterator<Item = &'a str>) -> bool {
    let Ok(gw) = reqwest::Url::parse(gateway) else {
        return false;
    };
    allowed
        .into_iter()
        .filter_map(|a| reqwest::Url::parse(a.trim()).ok())
        .any(|a| same_origin(&a, &gw))
}

/// Whether `gateway` may receive this instance's own credentials: it has the
/// origin of `STROM_TAMS_GATEWAY_URL` or of an entry in
/// [`ALLOWED_GATEWAYS_ENV`]. A flow can name any gateway, so without this check
/// anyone who can edit a flow could collect the instance's token by pointing a
/// TAMS block at their own server.
pub fn gateway_is_allow_listed(gateway: &str) -> bool {
    let configured = strom_types::env::var_opt("STROM_TAMS_GATEWAY_URL");
    let listed = strom_types::env::var_opt(ALLOWED_GATEWAYS_ENV).unwrap_or_default();
    is_allow_listed(
        gateway,
        configured.as_deref().into_iter().chain(listed.split(',')),
    )
}

/// The instance's static TAMS token (`STROM_TAMS_API_TOKEN`) for `gateway`, the
/// fallback when a block sets no token of its own. `None`, with a warning, when
/// the gateway is not allow-listed (see [`gateway_is_allow_listed`]).
pub fn env_api_token(gateway: &str) -> Option<String> {
    let token = strom_types::env::var_opt("STROM_TAMS_API_TOKEN")?;
    if gateway_is_allow_listed(gateway) {
        return Some(token);
    }
    warn!(
        "TAMS: not sending STROM_TAMS_API_TOKEN to {}: the gateway is neither \
         STROM_TAMS_GATEWAY_URL nor listed in {}; set a token on the block, or add the \
         gateway to {}",
        redact_url(gateway),
        ALLOWED_GATEWAYS_ENV,
        ALLOWED_GATEWAYS_ENV
    );
    None
}

/// Check that an OSC SAT for `service_id` may go to `gateway`: the gateway's
/// host must be an OSC instance of that service type (HTTPS), or the gateway
/// allow-listed. An explicit service id that differs from the host's would
/// otherwise mint a SAT for one service and hand it to another host.
///
/// The host is the parsed URL's (`host_str`), the one the request goes to, so
/// user info, a port or a path that look like an OSC host cannot pass. A URL
/// with user info is refused outright: an OSC gateway has no use for it.
fn check_osc_gateway(gateway: &str, service_id: &str, allow_listed: bool) -> Result<(), String> {
    if allow_listed {
        return Ok(());
    }
    let osc_host_of_service = reqwest::Url::parse(gateway).is_ok_and(|u| {
        u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && crate::osc::derive_service_id(u.as_str()).as_deref() == Some(service_id)
    });
    match osc_host_of_service {
        true => Ok(()),
        false => Err(format!(
            "OSC auth: not sending a SAT for service '{}' to {}: the gateway is not an \
             HTTPS OSC instance of that service; add it to {} to allow it",
            service_id,
            redact_url(gateway),
            ALLOWED_GATEWAYS_ENV
        )),
    }
}

/// The OSC service id to mint SATs for when talking to `gateway`: `explicit`
/// when set, else derived from the gateway host. Fails when there is none, or
/// when the SAT may not go to that gateway (see [`TamsClient::new`]).
pub fn osc_service_id(gateway: &str, explicit: Option<&str>) -> Result<String, String> {
    let service_id = match explicit {
        Some(id) => id.to_string(),
        None => crate::osc::derive_service_id(gateway).ok_or_else(|| {
            "OSC auth: cannot derive the service id from the gateway URL. It must be an \
             absolute OSC URL (https://<tenant>-<instance>.<service>.auto.prod.osaas.io); \
             for another gateway set the OSC Service ID property"
                .to_string()
        })?,
    };
    check_osc_gateway(gateway, &service_id, gateway_is_allow_listed(gateway))?;
    Ok(service_id)
}

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

/// Metadata describing a flow to create on the gateway. Serializable so a
/// park manifest can register the flow when its block is not running (see
/// `crate::tams::drain`).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Member flows collected by this flow. Non-empty only for a Multi-Flow
    /// that groups per-essence Flows under one NMOS `format:multi` flow (the
    /// canonical TAMS multi-essence model). Serialized as the TAMS
    /// `flow_collection` array.
    pub flow_collection: Vec<CollectionItem>,
    /// Per-essence member flows that live inside this flow's own container (the
    /// tracks of a muxed MPEG-TS flow). The uploader registers each once its
    /// essence is known and adds it to `flow_collection`; see
    /// [`CollectedFlow`].
    pub collected: Vec<CollectedFlow>,
    /// The flow's own `container_mapping` (where a member flow's essence is in
    /// the container of the flow that collects it). Left out when empty.
    pub container_mapping: serde_json::Map<String, serde_json::Value>,
    /// TAMS `essence_parameters` (e.g. `frame_rate`, `frame_width`), filled in from
    /// caps by the uploader before the flow is created. Empty when unknown.
    pub essence_parameters: serde_json::Map<String, serde_json::Value>,
}

/// One member of a TAMS `flow_collection`: the member flow's id, its role
/// (free text, e.g. `video`, `PGM`, `IS`) and its `container_mapping` (where
/// its essence is in the collecting flow's container; an empty map is left
/// out).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionItem {
    pub id: String,
    pub role: String,
    pub container_mapping: serde_json::Map<String, serde_json::Value>,
}

/// A member flow whose essence is one track of the collecting flow's
/// container: it has no segments of its own. `spec` is registered with the
/// essence detected for `track` (see `DetectedEssence::tracks`), and the
/// collection item gets `container_mapping` plus the track's channel range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectedFlow {
    /// Key into `DetectedEssence::tracks` (`video`, `audio-0`, ...).
    pub track: String,
    pub role: String,
    pub spec: FlowSpec,
    pub container_mapping: serde_json::Map<String, serde_json::Value>,
    /// The member takes the collecting flow's `essence_parameters` (the video
    /// of a muxed flow, whose parameters the flow itself carries) under the
    /// track's own.
    pub inherit_parameters: bool,
}

/// Build the JSON body for `PUT /flows/{id}` from a [`FlowSpec`]. Pure (no I/O) so
/// the gateway wire format can be unit-tested.
///
/// Wire-format contract (matches what the Eyevinn TAMS gateway accepts):
/// - `id`, `source_id`, `format`, `essence_parameters` always present.
/// - `codec`/`container` emitted only when `Some` (a grouping Multi-Flow omits them).
/// - `flow_collection` members are `{ id, role, container_mapping }`, the
///   mapping left out when empty.
/// - `label`/`description`/`tags` emitted only when present.
fn flow_request_body(spec: &FlowSpec) -> serde_json::Value {
    let mut body = serde_json::json!({
        "id": spec.flow_id,
        "source_id": spec.source_id,
        "format": spec.format,
        "essence_parameters": spec.essence_parameters,
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
        let members: Vec<serde_json::Value> = spec
            .flow_collection
            .iter()
            .map(|item| {
                let mut member = serde_json::json!({ "id": item.id, "role": item.role });
                if !item.container_mapping.is_empty() {
                    member["container_mapping"] =
                        serde_json::Value::Object(item.container_mapping.clone());
                }
                member
            })
            .collect();
        body["flow_collection"] = serde_json::Value::Array(members);
    }
    if !spec.container_mapping.is_empty() {
        body["container_mapping"] = serde_json::Value::Object(spec.container_mapping.clone());
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
    /// The parsed base URL: credentials go only to its origin.
    base: reqwest::Url,
    auth: AuthMethod,
    /// Gateway requests (with credentials): redirects only within the origin.
    api: reqwest::Client,
    /// Presigned object URLs (no credentials): any host, normal redirects.
    objects: reqwest::Client,
}

impl TamsClient {
    /// A client for the gateway at `base_url`. Fails for a URL that is not
    /// http(s), and for OSC auth whose SAT may not go to this gateway (see
    /// [`osc_service_id`]).
    pub fn new(base_url: impl Into<String>, auth: AuthMethod) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let base = reqwest::Url::parse(&base_url)
            .with_context(|| format!("gateway url {}", redact_url(&base_url)))?;
        if !matches!(base.scheme(), "http" | "https") {
            bail!("gateway url must be http(s): {}", redact_url(&base_url));
        }
        if let AuthMethod::Osc { service_id, .. } = &auth {
            check_osc_gateway(&base_url, service_id, gateway_is_allow_listed(&base_url))
                .map_err(|e| anyhow!(e))?;
        }
        let api = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(same_origin_redirects())
            .build()
            .context("building TAMS HTTP client")?;
        let objects = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building TAMS object HTTP client")?;
        Ok(Self {
            base_url,
            base,
            auth,
            api,
            objects,
        })
    }

    /// Send a gateway request with credentials. Refuses a URL outside the
    /// gateway's origin.
    async fn send_authed(
        &self,
        method: reqwest::Method,
        url: reqwest::Url,
        body: Option<&serde_json::Value>,
        headers: &[(&'static str, String)],
    ) -> Result<reqwest::Response> {
        if !same_origin(&url, &self.base) {
            bail!(
                "refusing to send gateway credentials to {}: not the gateway origin {}",
                redact_url(url.as_str()),
                self.base.origin().ascii_serialization()
            );
        }
        let context = format!("{} {}", method, redact_url(url.as_str()));
        let mut retried = false;
        loop {
            let mut req = self.api.request(method.clone(), url.clone());
            if let Some(body) = body {
                req = req.json(body);
            }
            for (name, value) in headers {
                req = req.header(*name, value);
            }
            let resp = self
                .auth
                .apply(req)
                .await?
                .send()
                .await
                .map_err(|e| anyhow::Error::new(e.without_url()))
                .context(context.clone())?;
            // A 401 with a cached token (OSC SAT) may only mean the token went
            // stale while the cache still held it (e.g. after the machine
            // slept). Drop it and retry once with a freshly minted one.
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                && !retried
                && self.auth.invalidate_after_rejection().await
            {
                warn!("TAMS: {} -> 401, retrying once with a new token", context);
                retried = true;
                continue;
            }
            return Ok(resp);
        }
    }

    /// A gateway URL: the base URL with `path` appended.
    fn url(&self, path: &str) -> Result<reqwest::Url> {
        let s = format!("{}{}", self.base_url, path);
        reqwest::Url::parse(&s).with_context(|| format!("url {}", s))
    }

    /// Create or replace a flow (and, gateway-side, its source). Idempotent.
    pub async fn ensure_flow(&self, spec: &FlowSpec) -> Result<()> {
        let url = self.url(&format!("/flows/{}", spec.flow_id))?;
        let body = flow_request_body(spec);
        let context = format!("PUT {}", redact_url(url.as_str()));
        let resp = self
            .send_authed(reqwest::Method::PUT, url, Some(&body), &[])
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp, context).await);
        }
        Ok(())
    }

    /// Allocate one media object and get its presigned PUT URL.
    pub async fn allocate_object(
        &self,
        flow_id: &str,
        content_type: &str,
    ) -> Result<AllocatedObject> {
        self.allocate_objects(flow_id, content_type, 1)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("gateway returned no media objects"))
    }

    /// Allocate up to `limit` media objects in one request (TAMS `limit`).
    /// The store may return fewer; never more than `limit` are returned.
    pub async fn allocate_objects(
        &self,
        flow_id: &str,
        content_type: &str,
        limit: usize,
    ) -> Result<Vec<AllocatedObject>> {
        let url = self.url(&format!("/flows/{}/storage", flow_id))?;
        let body = serde_json::json!({ "limit": limit, "content_type": content_type });
        let context = format!("POST {}", redact_url(url.as_str()));
        let resp = self
            .send_authed(reqwest::Method::POST, url, Some(&body), &[])
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp, context).await);
        }
        let parsed: StorageResponse = resp.json().await.context("parsing storage response")?;
        Ok(parsed
            .media_objects
            .into_iter()
            .take(limit)
            .map(|obj| AllocatedObject {
                object_id: obj.object_id,
                put_url: obj.put_url.url,
                content_type: obj.put_url.content_type,
            })
            .collect())
    }

    /// Upload segment bytes to a presigned S3 PUT URL.
    pub async fn upload_object(
        &self,
        put_url: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        // No credentials here: the URL itself is presigned. Errors drop the URL,
        // which carries the signature.
        let resp = self
            .objects
            .put(put_url)
            .header(CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e.without_url()))
            .context("PUT presigned S3 url")?;
        if !resp.status().is_success() {
            return Err(status_error(resp, "presigned PUT".to_string()).await);
        }
        Ok(())
    }

    /// Register a segment on the flow timeline. `key_round` is mixed into the
    /// idempotency key (see [`segment_idempotency_key`]): the caller moves to
    /// the next round after the store answered a try with a failure.
    pub async fn register_segment(
        &self,
        flow_id: &str,
        object_id: &str,
        timerange: &str,
        key_round: u32,
    ) -> Result<()> {
        let url = self.url(&format!("/flows/{}/segments", flow_id))?;
        let body = serde_json::json!({ "object_id": object_id, "timerange": timerange });
        let context = format!("POST {}", redact_url(url.as_str()));
        let key = segment_idempotency_key(flow_id, object_id, timerange, key_round);
        let resp = self
            .send_authed(
                reqwest::Method::POST,
                url,
                Some(&body),
                &[(IDEMPOTENCY_KEY_HEADER, key)],
            )
            .await?;
        let status = resp.status();
        // 201 = all registered; 200 = partial failure (failed_segments listed).
        if status.as_u16() == 200 {
            let mut body = resp.text().await.unwrap_or_default();
            if let Some((cut, _)) = body.char_indices().nth(MAX_ERROR_BODY_CHARS) {
                body.truncate(cut);
                body.push_str("...");
            }
            return Err(PartialFailure { body }.into());
        }
        if !status.is_success() {
            return Err(status_error(resp, context).await);
        }
        Ok(())
    }

    /// Register several segments in one request (a JSON array, as the TAMS
    /// spec allows). Returns the `(object_id, timerange)` of each segment the
    /// store reports as failed (a 200 with `failed_segments`); empty when all
    /// are registered (201).
    pub async fn register_segments(
        &self,
        flow_id: &str,
        segments: &[(String, String)],
    ) -> Result<Vec<(String, Option<String>)>> {
        #[derive(Deserialize)]
        struct Failed {
            object_id: String,
            #[serde(default)]
            timerange: Option<String>,
        }
        #[derive(Deserialize)]
        struct FailedBody {
            #[serde(default)]
            failed_segments: Vec<Failed>,
        }
        let url = self.url(&format!("/flows/{}/segments", flow_id))?;
        let body = serde_json::Value::Array(
            segments
                .iter()
                .map(|(o, t)| serde_json::json!({ "object_id": o, "timerange": t }))
                .collect(),
        );
        let context = format!("POST {}", redact_url(url.as_str()));
        // One key for the whole batch, derived from what it registers, so a
        // retry of the same batch sends the same key.
        let key = segment_idempotency_key(
            flow_id,
            &segments
                .iter()
                .map(|(o, t)| format!("{}@{}", o, t))
                .collect::<Vec<_>>()
                .join(","),
            "batch",
        );
        let resp = self
            .send_authed(
                reqwest::Method::POST,
                url,
                Some(&body),
                &[(IDEMPOTENCY_KEY_HEADER, key)],
            )
            .await?;
        let status = resp.status();
        if status.as_u16() == 200 {
            let text = resp.text().await.unwrap_or_default();
            let failed: FailedBody = serde_json::from_str(&text)
                .with_context(|| format!("parsing failed_segments of {}", context))?;
            return Ok(failed
                .failed_segments
                .into_iter()
                .map(|f| (f.object_id, f.timerange))
                .collect());
        }
        if !status.is_success() {
            return Err(status_error(resp, context).await);
        }
        Ok(Vec::new())
    }
}

/// Header that makes a segment registration safe to repeat. Not in the TAMS
/// spec, but some stores require it (OpenTAMS, REQ-IDEM-01) and others ignore
/// an unknown header.
const IDEMPOTENCY_KEY_HEADER: &str = "X-Idempotency-Key";

/// The idempotency key of a segment registration: derived from what is
/// registered, so a retry of the same segment whose outcome is unknown (a
/// timeout, a lost answer, a 5xx, a 401 retried with a new token) sends the
/// same key, and a different registration never reuses one.
///
/// A store may keep the answer per key and replay it: OpenTAMS caches every
/// answer it finishes, 200 partial failures and 4xx included (it releases the
/// key only on a 5xx). A retry after such an answer with the same key would
/// get the cached failure back and could never succeed, so the caller retries
/// it in a new `round`, which gives a new key. Round 0 is the key without a
/// round.
fn segment_idempotency_key(flow_id: &str, object_id: &str, timerange: &str, round: u32) -> String {
    let mut name = format!("strom:tams-segment:{}/{}/{}", flow_id, object_id, timerange);
    if round > 0 {
        name.push_str(&format!("#{}", round));
    }
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, name.as_bytes()).to_string()
}

/// A segment registration that the store answered with 200: it took the
/// request, but lists the segment as failed. Not an HTTP error, so it is
/// retried, but in a new idempotency round (see [`segment_idempotency_key`]).
#[derive(Debug)]
pub struct PartialFailure {
    pub body: String,
}

impl std::fmt::Display for PartialFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "segment registration partially failed: {}", self.body)
    }
}

impl std::error::Error for PartialFailure {}

/// Whether the store answered a segment registration with a failure it may
/// keep under the idempotency key, so that a retry needs a new key: a 200
/// partial failure, or a 4xx other than 408 and 429 (which ask to retry) and
/// 409 (OpenTAMS: the same key is still in flight, so the same key is
/// retried). Timeouts, network errors and 5xx keep the key.
pub fn needs_new_idempotency_key(e: &anyhow::Error) -> bool {
    if e.downcast_ref::<PartialFailure>().is_some() {
        return true;
    }
    e.downcast_ref::<HttpStatusError>().is_some_and(|h| {
        let code = h.status.as_u16();
        (400..500).contains(&code) && !matches!(code, 408 | 409 | 429)
    })
}

/// One segment as listed by `GET /flows/{id}/segments`. Timeranges and the
/// `ts_offset` timestamp are kept as the raw TAMS strings; see
/// `strom_types::tams::parse_timerange`.
#[derive(Debug, Clone, Deserialize)]
pub struct ListedSegment {
    pub object_id: String,
    pub timerange: String,
    #[serde(default)]
    pub ts_offset: Option<String>,
    #[serde(default)]
    pub object_timerange: Option<String>,
    #[serde(default)]
    pub get_urls: Vec<GetUrl>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetUrl {
    pub url: String,
}

/// Query for `GET /flows/{id}/segments`.
#[derive(Debug, Clone, Default)]
pub struct SegmentQuery {
    pub timerange: Option<String>,
    pub limit: Option<u32>,
    pub reverse_order: bool,
}

impl SegmentQuery {
    /// Query parameters, with `page` set when following a paging key.
    fn params(&self, page: Option<&str>) -> Vec<(&'static str, String)> {
        let mut p = Vec::new();
        if let Some(tr) = &self.timerange {
            p.push(("timerange", tr.clone()));
        }
        if let Some(l) = self.limit {
            p.push(("limit", l.to_string()));
        }
        if self.reverse_order {
            p.push(("reverse_order", "true".to_string()));
        }
        // Lets a spec-complete store return object_timerange for partial use;
        // others ignore it.
        p.push(("include_object_timerange", "true".to_string()));
        if let Some(page) = page {
            p.push(("page", page.to_string()));
        }
        p
    }
}

/// The answer to [`TamsClient::delete_segments`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The store took the request (it may delete later).
    Accepted,
    /// The store has no such flow (nothing registered yet, or deleted).
    NoFlow,
}

/// Where the next page of a paged listing is, from the response headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextPage {
    /// Opaque key for the `page` query parameter (`X-Paging-NextKey`).
    Key(String),
    /// A `Link rel="next"` target, absolute or relative to the gateway.
    Link(String),
}

fn with_query(base: &str, params: &[(&str, String)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).with_context(|| format!("url {}", base))?;
    url.query_pairs_mut()
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
    Ok(url)
}

/// Find the next page from `X-Paging-NextKey` or a `Link` header value.
pub fn next_page(next_key: Option<&str>, link: Option<&str>) -> Option<NextPage> {
    if let Some(k) = next_key.map(str::trim).filter(|k| !k.is_empty()) {
        return Some(NextPage::Key(k.to_string()));
    }
    let link = link?;
    for part in link.split(',') {
        let mut fields = part.split(';');
        let target = fields.next()?.trim();
        let is_next = fields.any(|f| {
            let f = f.trim().replace(' ', "");
            f == "rel=\"next\"" || f == "rel=next"
        });
        if is_next {
            let t = target.trim_start_matches('<').trim_end_matches('>');
            return Some(NextPage::Link(t.to_string()));
        }
    }
    None
}

impl TamsClient {
    /// Base URL without trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// How the client authenticates.
    pub fn auth(&self) -> &AuthMethod {
        &self.auth
    }

    /// `GET /flows/{id}` as JSON.
    pub async fn get_flow(&self, flow_id: &str) -> Result<serde_json::Value> {
        let url = self.url(&format!("/flows/{}", flow_id))?;
        let context = format!("GET {}", redact_url(url.as_str()));
        let resp = self
            .send_authed(reqwest::Method::GET, url, None, &[])
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp, context).await);
        }
        resp.json().await.context("parsing flow")
    }

    /// List segments, following pages until `max_pages` pages or the end.
    /// A `Link` to the next page outside the gateway's origin is an error.
    pub async fn list_segments(
        &self,
        flow_id: &str,
        query: &SegmentQuery,
        max_pages: usize,
    ) -> Result<Vec<ListedSegment>> {
        Ok(self.list_segments_paged(flow_id, query, max_pages).await?.0)
    }

    /// As [`Self::list_segments`], also saying whether the store has more
    /// pages than were followed (the listing stopped at `max_pages`).
    pub async fn list_segments_paged(
        &self,
        flow_id: &str,
        query: &SegmentQuery,
        max_pages: usize,
    ) -> Result<(Vec<ListedSegment>, bool)> {
        let mut out = Vec::new();
        let mut next: Option<NextPage> = None;
        for _ in 0..max_pages.max(1) {
            let (page, n) = self
                .list_segments_page(flow_id, query, next.as_ref())
                .await?;
            let empty = page.is_empty();
            out.extend(page);
            if n.is_none() || empty {
                return Ok((out, false));
            }
            next = n;
        }
        Ok((out, true))
    }

    /// One page of a segment listing: the first when `next` is `None`, else
    /// the page it names. Returns the page and where the one after it is.
    pub async fn list_segments_page(
        &self,
        flow_id: &str,
        query: &SegmentQuery,
        next: Option<&NextPage>,
    ) -> Result<(Vec<ListedSegment>, Option<NextPage>)> {
        let first = format!("{}/flows/{}/segments", self.base_url, flow_id);
        let url = match next {
            None => with_query(&first, &query.params(None))?,
            Some(NextPage::Key(k)) => with_query(&first, &query.params(Some(k)))?,
            Some(NextPage::Link(l)) => self.resolve(l)?,
        };
        let resp = self
            .send_authed(reqwest::Method::GET, url, None, &[])
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp, format!("GET {}", first)).await);
        }
        let h = resp.headers();
        let next = next_page(
            h.get("x-paging-nextkey").and_then(|v| v.to_str().ok()),
            h.get("link").and_then(|v| v.to_str().ok()),
        );
        let page: Vec<ListedSegment> = resp.json().await.context("parsing segments")?;
        Ok((page, next))
    }

    /// Ask the store to delete the segments of `flow_id` that lie wholly
    /// within `timerange` (TAMS `DELETE /flows/{id}/segments?timerange=`).
    /// The store removes the media objects no other segment references. A
    /// store may do the work later (202 with a Flow Delete Request) or at
    /// once (200/204); both are [`DeleteOutcome::Accepted`]. A flow the
    /// store does not have (404) is [`DeleteOutcome::NoFlow`].
    pub async fn delete_segments(&self, flow_id: &str, timerange: &str) -> Result<DeleteOutcome> {
        let path = format!("{}/flows/{}/segments", self.base_url, flow_id);
        let url = with_query(&path, &[("timerange", timerange.to_string())])?;
        let resp = self
            .send_authed(reqwest::Method::DELETE, url, None, &[])
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(DeleteOutcome::NoFlow);
        }
        if !status.is_success() {
            return Err(status_error(resp, format!("DELETE {}", redact_url(&path))).await);
        }
        Ok(DeleteOutcome::Accepted)
    }

    /// Resolve a `Link` target against the gateway base URL. A target outside
    /// the gateway's origin is refused: following it would send the gateway's
    /// credentials to whatever host a (misbehaving) gateway names.
    fn resolve(&self, link: &str) -> Result<reqwest::Url> {
        let base = reqwest::Url::parse(&format!("{}/", self.base_url)).context("base url")?;
        let url = base.join(link).context("next link")?;
        if !same_origin(&url, &self.base) {
            bail!(
                "next-page link to {} leaves the gateway origin {}",
                redact_url(url.as_str()),
                self.base.origin().ascii_serialization()
            );
        }
        Ok(url)
    }

    /// GET the bytes of a media object from one of its `get_urls`, at most
    /// [`MAX_OBJECT_BYTES`]. No credentials: TAMS get_urls are presigned or
    /// otherwise directly fetchable, and an extra Authorization header breaks
    /// S3 presigned requests. Errors never contain the URL, whose query
    /// carries the signature.
    pub async fn fetch_object(&self, url: &str) -> Result<Vec<u8>> {
        self.fetch_object_limited(url, MAX_OBJECT_BYTES).await
    }

    async fn fetch_object_limited(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let mut resp = self
            .objects
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e.without_url()))
            .context("GET media object")?;
        if !resp.status().is_success() {
            return Err(status_error(resp, "GET media object".to_string()).await);
        }
        let declared = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(len) = declared.filter(|len| *len > max_bytes) {
            bail!(
                "media object is {} bytes, over the {} byte limit",
                len,
                max_bytes
            );
        }
        let mut bytes = Vec::with_capacity(declared.unwrap_or(0) as usize);
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| anyhow::Error::new(e.without_url()))
            .context("reading media object")?
        {
            if bytes.len() as u64 + chunk.len() as u64 > max_bytes {
                bail!("media object exceeds the {} byte limit", max_bytes);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_prefers_next_key() {
        assert_eq!(
            next_page(Some("0001"), Some("</x?page=9>; rel=\"next\"")),
            Some(NextPage::Key("0001".into()))
        );
    }

    #[test]
    fn paging_falls_back_to_link() {
        let l = "</flows/f/segments?limit=2&page=01800000012000000000>; rel=\"next\"";
        assert_eq!(
            next_page(None, Some(l)),
            Some(NextPage::Link(
                "/flows/f/segments?limit=2&page=01800000012000000000".into()
            ))
        );
        let l = "<https://a.example.com/p1>; rel=\"prev\", <https://a.example.com/p3>; rel=next";
        assert_eq!(
            next_page(Some(""), Some(l)),
            Some(NextPage::Link("https://a.example.com/p3".into()))
        );
        assert_eq!(next_page(None, Some("<x>; rel=\"prev\"")), None);
        assert_eq!(next_page(None, None), None);
    }

    #[test]
    fn segment_query_params() {
        let q = SegmentQuery {
            timerange: Some("[1:0_2:0)".into()),
            limit: Some(10),
            reverse_order: true,
        };
        let p = q.params(Some("k"));
        assert!(p.contains(&("timerange", "[1:0_2:0)".to_string())));
        assert!(p.contains(&("limit", "10".to_string())));
        assert!(p.contains(&("reverse_order", "true".to_string())));
        assert!(p.contains(&("page", "k".to_string())));
    }

    #[test]
    fn link_resolves_against_base_with_prefix() {
        let c = TamsClient::new("http://gw.example.com/tams", AuthMethod::None).unwrap();
        assert_eq!(
            c.resolve("flows/f/segments?page=2").unwrap().as_str(),
            "http://gw.example.com/tams/flows/f/segments?page=2"
        );
        // Absolute links: same origin is followed, any other origin refused.
        assert_eq!(
            c.resolve("http://gw.example.com/tams/flows/f/segments?page=3")
                .unwrap()
                .as_str(),
            "http://gw.example.com/tams/flows/f/segments?page=3"
        );
        assert!(c.resolve("https://x.example.com/a").is_err());
        assert!(
            c.resolve("https://gw.example.com/tams/a").is_err(),
            "scheme"
        );
        assert!(c.resolve("http://gw.example.com:81/a").is_err(), "port");
        assert!(c.resolve("//x.example.com/a").is_err(), "scheme-relative");
    }

    #[test]
    fn listed_segment_parses_optional_fields() {
        let s: ListedSegment = serde_json::from_str(
            r#"{"object_id":"b/o","timerange":"[1:0_2:0)","ts_offset":"0:5",
                "get_urls":[{"url":"http://s.example.com/o","presigned":true}]}"#,
        )
        .unwrap();
        assert_eq!(s.ts_offset.as_deref(), Some("0:5"));
        assert_eq!(s.get_urls[0].url, "http://s.example.com/o");
        let s: ListedSegment =
            serde_json::from_str(r#"{"object_id":"o","timerange":"[1:0_2:0)"}"#).unwrap();
        assert!(s.get_urls.is_empty() && s.object_timerange.is_none());
    }

    // Timerange/timestamp formatting is tested in `strom_types::tams`.

    #[test]
    fn segment_idempotency_key_is_stable_per_registration() {
        let key = |f, o, t| segment_idempotency_key(f, o, t, 0);
        let a = key("f", "o1", "[0:0_1:0)");
        assert_eq!(a, key("f", "o1", "[0:0_1:0)"));
        assert_ne!(a, key("f", "o2", "[0:0_1:0)"));
        assert_ne!(a, key("f", "o1", "[0:0_2:0)"));
        assert_ne!(a, key("g", "o1", "[0:0_1:0)"));
        // A new round is a new key, stable within the round.
        let b = segment_idempotency_key("f", "o1", "[0:0_1:0)", 1);
        assert_ne!(a, b);
        assert_eq!(b, segment_idempotency_key("f", "o1", "[0:0_1:0)", 1));
    }

    #[test]
    fn new_idempotency_key_only_after_an_answered_failure() {
        let status = |code: u16| {
            anyhow::Error::new(HttpStatusError {
                status: reqwest::StatusCode::from_u16(code).unwrap(),
                context: "POST x".into(),
                body: String::new(),
            })
        };
        assert!(needs_new_idempotency_key(
            &PartialFailure { body: "{}".into() }.into()
        ));
        for code in [400, 403, 404] {
            assert!(needs_new_idempotency_key(&status(code)), "{}", code);
        }
        for code in [408, 409, 429, 500, 503] {
            assert!(!needs_new_idempotency_key(&status(code)), "{}", code);
        }
        assert!(!needs_new_idempotency_key(&anyhow!("timed out")));
    }

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
            collected: vec![],
            container_mapping: Default::default(),
            essence_parameters: Default::default(),
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
        let item = |id: &str, role: &str, track: usize| CollectionItem {
            id: id.into(),
            role: role.into(),
            container_mapping: serde_json::json!({ "track_index": track })
                .as_object()
                .cloned()
                .unwrap(),
        };
        s.flow_collection = vec![
            item("video-flow", "video", 0),
            item("audio-flow", "audio", 1),
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
    fn container_mappings_serialized_when_set() {
        let mut s = spec();
        s.flow_collection = vec![CollectionItem {
            id: "a".into(),
            role: "PGM".into(),
            container_mapping: Default::default(),
        }];
        let body = flow_request_body(&s);
        // An empty mapping is left out, on the member and on the flow.
        assert!(body["flow_collection"][0]
            .get("container_mapping")
            .is_none());
        assert!(body.get("container_mapping").is_none());
        s.container_mapping
            .insert("mp2ts_container".into(), serde_json::json!({ "pid": 66 }));
        assert_eq!(
            flow_request_body(&s)["container_mapping"]["mp2ts_container"]["pid"],
            66
        );
    }

    #[test]
    fn essence_parameters_serialized() {
        let mut s = spec();
        s.essence_parameters.insert(
            "frame_rate".into(),
            serde_json::json!({"numerator": 50, "denominator": 1}),
        );
        let body = flow_request_body(&s);
        assert_eq!(body["essence_parameters"]["frame_rate"]["numerator"], 50);
        assert_eq!(
            flow_request_body(&spec())["essence_parameters"],
            serde_json::json!({})
        );
    }

    #[test]
    fn tags_serialized_as_object() {
        let mut s = spec();
        s.tags = vec![("prod".into(), "Studio A".into())];
        let body = flow_request_body(&s);
        assert_eq!(body["tags"]["prod"], "Studio A");
    }

    #[test]
    fn allow_list_matches_by_origin() {
        let allowed = ["https://gw.example.com/tams", " http://192.0.2.10:8000 "];
        assert!(is_allow_listed("https://gw.example.com/other", allowed));
        assert!(is_allow_listed("http://192.0.2.10:8000/flows", allowed));
        assert!(!is_allow_listed("http://gw.example.com", allowed), "scheme");
        assert!(!is_allow_listed("http://192.0.2.10:8001", allowed), "port");
        assert!(!is_allow_listed(
            "https://gw.example.com.evil.example.net",
            allowed
        ));
        assert!(!is_allow_listed("not a url", allowed));
        assert!(!is_allow_listed("https://gw.example.com", []));
    }

    #[test]
    fn osc_sat_goes_only_to_hosts_of_its_service() {
        let gw = "https://t-i.eyevinn-tams-gateway.auto.prod.osaas.io";
        assert!(check_osc_gateway(gw, "eyevinn-tams-gateway", false).is_ok());
        // An explicit service id for another service, or a host that is not
        // OSC, or plain HTTP: no SAT unless the gateway is allow-listed.
        assert!(check_osc_gateway(gw, "eyevinn-intercom-manager", false).is_err());
        assert!(
            check_osc_gateway("https://evil.example.net", "eyevinn-tams-gateway", false).is_err()
        );
        assert!(check_osc_gateway(
            "http://t-i.eyevinn-tams-gateway.auto.prod.osaas.io",
            "eyevinn-tams-gateway",
            false
        )
        .is_err());
        assert!(
            check_osc_gateway("https://tams.example.com", "eyevinn-tams-gateway", true).is_ok()
        );
    }

    #[test]
    fn osc_sat_check_is_not_fooled_by_user_info_or_lookalikes() {
        for gw in [
            // User info that looks like the OSC host: the request would go to
            // evil.example.
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io:pw@evil.example/",
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io@evil.example",
            "https://evil.example/x.eyevinn-tams-gateway.auto.prod.osaas.io",
            "https://evil.example\\x.eyevinn-tams-gateway.auto.prod.osaas.io",
            "https://evil.example?x.eyevinn-tams-gateway.auto.prod.osaas.io",
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io.evil.example",
            // User info on the right host is refused too.
            "https://u:p@x.eyevinn-tams-gateway.auto.prod.osaas.io",
            // Not a URL.
            "x.eyevinn-tams-gateway.auto.prod.osaas.io",
        ] {
            assert!(
                check_osc_gateway(gw, "eyevinn-tams-gateway", false).is_err(),
                "{gw}"
            );
            assert!(osc_service_id(gw, None).is_err(), "{gw}");
            assert!(
                osc_service_id(gw, Some("eyevinn-tams-gateway")).is_err(),
                "{gw}"
            );
        }
        // A port on the right host is fine.
        assert!(check_osc_gateway(
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io:443/flows",
            "eyevinn-tams-gateway",
            false
        )
        .is_ok());
    }

    #[test]
    fn a_gateway_url_without_scheme_says_it_must_be_absolute() {
        let err = osc_service_id("x.eyevinn-tams-gateway.auto.prod.osaas.io", None).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn osc_client_refuses_a_foreign_gateway() {
        let osc = |gw: &str, service: &str| {
            TamsClient::new(
                gw,
                AuthMethod::Osc {
                    provider: std::sync::Arc::new(
                        crate::osc::SatProvider::new(None, None).unwrap(),
                    ),
                    service_id: service.into(),
                    credential_key: "k".into(),
                },
            )
        };
        assert!(osc(
            "https://t-i.eyevinn-tams-gateway.auto.prod.osaas.io",
            "eyevinn-tams-gateway"
        )
        .is_ok());
        assert!(osc("https://evil.example.net/flows", "eyevinn-tams-gateway").is_err());
        assert!(osc(
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io:pw@evil.example/",
            "eyevinn-tams-gateway"
        )
        .is_err());
    }

    /// HTTP-level tests of the request loop against local stub servers.
    mod http {
        use super::*;
        use axum::extract::{Query, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::Router;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        /// Authorization headers seen by a stub, per path and query.
        type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

        async fn serve(app: Router) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            base
        }

        fn seg(n: u32) -> serde_json::Value {
            serde_json::json!({"object_id": format!("o{}", n), "timerange": format!("[{}:0_{}:0)", n, n + 1)})
        }

        fn record(seen: &Seen, what: String, h: &HeaderMap) {
            let auth = h
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            seen.lock().unwrap().push((what, auth));
        }

        /// A stub that hands out 3 pages, with `X-Paging-NextKey` or `Link`,
        /// and a `Link` to `foreign` when asked to.
        async fn gateway(seen: Seen, mode: &'static str, foreign: String) -> String {
            let app = Router::new()
                .route(
                    "/flows/f/segments",
                    get(
                        move |State(seen): State<Seen>,
                              Query(q): Query<HashMap<String, String>>,
                              h: HeaderMap| {
                            let foreign = foreign.clone();
                            async move {
                                let page: u32 =
                                    q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0);
                                record(&seen, format!("page {}", page), &h);
                                let mut headers = HeaderMap::new();
                                if page < 2 {
                                    let next = page + 1;
                                    match mode {
                                        "key" => {
                                            headers.insert(
                                                "x-paging-nextkey",
                                                next.to_string().parse().unwrap(),
                                            );
                                        }
                                        "link" => {
                                            headers.insert(
                                                "link",
                                                format!(
                                                    "</flows/f/segments?page={}>; rel=\"next\"",
                                                    next
                                                )
                                                .parse()
                                                .unwrap(),
                                            );
                                        }
                                        _ => {
                                            headers.insert(
                                                "link",
                                                format!(
                                                    "<{}/steal?page={}>; rel=\"next\"",
                                                    foreign, next
                                                )
                                                .parse()
                                                .unwrap(),
                                            );
                                        }
                                    }
                                }
                                (headers, axum::Json(vec![seg(page)])).into_response()
                            }
                        },
                    ),
                )
                .route(
                    "/flows/bad/segments",
                    get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "x".repeat(10_000)) }),
                )
                .route(
                    "/flows/moved/segments",
                    get(move |State(seen): State<Seen>, h: HeaderMap| async move {
                        record(&seen, "moved".into(), &h);
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [("location", "/flows/f/segments")],
                        )
                    }),
                )
                .route(
                    "/flows/away/segments",
                    get(|State(_): State<Seen>, Query(q): Query<HashMap<String, String>>| async move {
                        let to = q.get("to").cloned().unwrap_or_default();
                        (StatusCode::TEMPORARY_REDIRECT, [("location", to)])
                    }),
                )
                .with_state(seen);
            serve(app).await
        }

        /// Another origin that records whatever reaches it.
        async fn foreign(seen: Seen) -> String {
            let app = Router::new().fallback(move |h: HeaderMap, uri: axum::http::Uri| {
                let seen = seen.clone();
                async move {
                    record(&seen, uri.to_string(), &h);
                    axum::Json(Vec::<serde_json::Value>::new())
                }
            });
            serve(app).await
        }

        fn client(base: &str) -> TamsClient {
            TamsClient::new(base, AuthMethod::Bearer("secret".into())).unwrap()
        }

        /// An OSC token endpoint that mints `sat-1`, `sat-2`, ... in turn.
        async fn token_endpoint(minted: Arc<Mutex<u32>>) -> String {
            let app = Router::new().route(
                "/servicetoken",
                axum::routing::post(move || {
                    let minted = minted.clone();
                    async move {
                        let mut n = minted.lock().unwrap();
                        *n += 1;
                        axum::Json(serde_json::json!({ "token": format!("sat-{}", *n) }))
                    }
                }),
            );
            format!("{}/servicetoken", serve(app).await)
        }

        /// A gateway that accepts only `Bearer <accepted>` on `GET /flows/f`
        /// and `POST /flows/f/segments`, and answers 401 otherwise.
        async fn gated_gateway(seen: Seen, accepted: &'static str) -> String {
            let check = move |what: &'static str| {
                move |State(seen): State<Seen>, h: HeaderMap| async move {
                    record(&seen, what.into(), &h);
                    let ok = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v == format!("Bearer {}", accepted));
                    if ok {
                        (StatusCode::CREATED, axum::Json(serde_json::json!({}))).into_response()
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }
            };
            let app = Router::new()
                .route("/flows/f", get(check("flow")))
                .route("/flows/f/segments", axum::routing::post(check("register")))
                .with_state(seen);
            serve(app).await
        }

        /// An OSC client for a local stub. `TamsClient::new` refuses a non-OSC
        /// host for OSC auth, so swap the auth in after construction.
        fn osc_client(base: &str, token_url: String) -> TamsClient {
            let mut c = TamsClient::new(base, AuthMethod::None).unwrap();
            c.auth = AuthMethod::Osc {
                provider: Arc::new(
                    crate::osc::SatProvider::new(Some("pat".into()), Some(token_url)).unwrap(),
                ),
                service_id: "svc".into(),
                credential_key: "k".into(),
            };
            c
        }

        fn auths(seen: &Seen) -> Vec<Option<String>> {
            seen.lock()
                .unwrap()
                .iter()
                .map(|(_, a)| a.clone())
                .collect()
        }

        #[tokio::test]
        async fn rejected_sat_is_reminted_and_the_request_retried_once() {
            let minted = Arc::new(Mutex::new(0));
            let token_url = token_endpoint(minted.clone()).await;
            let seen: Seen = Arc::default();
            // sat-1 is "stale" (rejected); sat-2 is accepted.
            let gw = gated_gateway(seen.clone(), "sat-2").await;
            let c = osc_client(&gw, token_url);

            // Writer path: registering a segment survives the stale SAT.
            c.register_segment("f", "o1", "[0:0_1:0)", 0).await.unwrap();
            assert_eq!(
                auths(&seen),
                vec![Some("Bearer sat-1".into()), Some("Bearer sat-2".into())]
            );
            // The new SAT is cached: the reader path uses it without a mint.
            c.get_flow("f").await.unwrap();
            assert_eq!(*minted.lock().unwrap(), 2);
            assert_eq!(seen.lock().unwrap().len(), 3);
        }

        /// A registration retried after a 401 with a new token (the re-mint
        /// loop in `send_authed`) carries the same key on both tries: the
        /// store never saw the first. A different registration gets another.
        #[tokio::test]
        async fn register_sends_the_same_idempotency_key_on_a_retry() {
            type Keys = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;
            let minted = Arc::new(Mutex::new(0));
            let token_url = token_endpoint(minted.clone()).await;
            let keys: Keys = Arc::default();
            let app = Router::new()
                .route(
                    "/flows/f/segments",
                    axum::routing::post(|State(keys): State<Keys>, h: HeaderMap| async move {
                        let get = |name: &str| {
                            h.get(name).and_then(|v| v.to_str().ok()).map(String::from)
                        };
                        let auth = get("authorization");
                        keys.lock()
                            .unwrap()
                            .push((get("x-idempotency-key"), auth.clone()));
                        // Only the second minted token is accepted.
                        if auth.as_deref() == Some("Bearer sat-1") {
                            StatusCode::UNAUTHORIZED
                        } else {
                            StatusCode::CREATED
                        }
                    }),
                )
                .with_state(keys.clone());
            let c = osc_client(&serve(app).await, token_url);
            c.register_segment("f", "o1", "[0:0_1:0)", 0).await.unwrap();
            c.register_segment("f", "o2", "[1:0_2:0)", 0).await.unwrap();
            let k = keys.lock().unwrap().clone();
            assert_eq!(k.len(), 3, "{:?}", k);
            assert_eq!(k[0].1.as_deref(), Some("Bearer sat-1"));
            assert_eq!(k[1].1.as_deref(), Some("Bearer sat-2"));
            assert!(k[0].0.is_some(), "no key");
            assert_eq!(k[0].0, k[1].0, "the retry after the 401 changed the key");
            assert_ne!(k[0].0, k[2].0);
            assert_eq!(*minted.lock().unwrap(), 2);
        }

        #[tokio::test]
        async fn a_401_is_retried_only_once() {
            let minted = Arc::new(Mutex::new(0));
            let token_url = token_endpoint(minted.clone()).await;
            let seen: Seen = Arc::default();
            let gw = gated_gateway(seen.clone(), "never").await;
            let c = osc_client(&gw, token_url);

            let err = c.get_flow("f").await.unwrap_err();
            let e = err.downcast_ref::<HttpStatusError>().expect("status error");
            assert_eq!(e.status, StatusCode::UNAUTHORIZED);
            assert_eq!(seen.lock().unwrap().len(), 2);
            assert_eq!(*minted.lock().unwrap(), 2);
        }

        #[tokio::test]
        async fn a_401_with_a_static_token_is_not_retried() {
            let seen: Seen = Arc::default();
            let gw = gated_gateway(seen.clone(), "never").await;
            assert!(client(&gw).get_flow("f").await.is_err());
            assert_eq!(seen.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn pages_by_next_key_and_link_with_auth() {
            for mode in ["key", "link"] {
                let seen = Seen::default();
                let gw = gateway(seen.clone(), mode, String::new()).await;
                let got = client(&gw)
                    .list_segments("f", &SegmentQuery::default(), 10)
                    .await
                    .unwrap();
                let ids: Vec<_> = got.iter().map(|s| s.object_id.as_str()).collect();
                assert_eq!(ids, ["o0", "o1", "o2"], "{}", mode);
                let seen = seen.lock().unwrap();
                assert_eq!(seen.len(), 3);
                assert!(seen
                    .iter()
                    .all(|(_, a)| a.as_deref() == Some("Bearer secret")));
            }
        }

        #[tokio::test]
        async fn max_pages_truncates() {
            let gw = gateway(Seen::default(), "key", String::new()).await;
            let got = client(&gw)
                .list_segments("f", &SegmentQuery::default(), 2)
                .await
                .unwrap();
            assert_eq!(got.len(), 2);
        }

        /// A `Link` to another origin is refused: the request (and the token)
        /// never reaches that host.
        #[tokio::test]
        async fn cross_origin_link_is_refused() {
            let stolen = Seen::default();
            let other = foreign(stolen.clone()).await;
            let gw = gateway(Seen::default(), "foreign", other).await;
            let err = client(&gw)
                .list_segments("f", &SegmentQuery::default(), 10)
                .await
                .unwrap_err();
            assert!(
                format!("{:#}", err).contains("leaves the gateway origin"),
                "{:#}",
                err
            );
            assert!(stolen.lock().unwrap().is_empty());
        }

        /// Redirects with credentials are followed within the gateway's
        /// origin, and not to another one.
        #[tokio::test]
        async fn redirects_stay_on_the_gateway_origin() {
            let seen = Seen::default();
            let gw = gateway(seen.clone(), "key", String::new()).await;
            let got = client(&gw)
                .list_segments("moved", &SegmentQuery::default(), 1)
                .await
                .unwrap();
            assert_eq!(got.len(), 1, "same-origin redirect followed");
            let auth: Vec<_> = seen
                .lock()
                .unwrap()
                .iter()
                .map(|(_, a)| a.clone())
                .collect();
            assert_eq!(auth, vec![Some("Bearer secret".to_string()); 2]);

            let stolen = Seen::default();
            let other = foreign(stolen.clone()).await;
            let c = TamsClient::new(&gw, AuthMethod::Bearer("secret".into())).unwrap();
            let url = with_query(
                &format!("{}/flows/away/segments", gw),
                &[("to", format!("{}/steal", other))],
            )
            .unwrap();
            let resp = c
                .send_authed(reqwest::Method::GET, url, None, &[])
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
            assert!(
                stolen.lock().unwrap().is_empty(),
                "redirect left the origin"
            );
        }

        #[tokio::test]
        async fn error_bodies_are_bounded() {
            let gw = gateway(Seen::default(), "key", String::new()).await;
            let err = client(&gw)
                .list_segments("bad", &SegmentQuery::default(), 1)
                .await
                .unwrap_err();
            let e = err.downcast_ref::<HttpStatusError>().expect("status error");
            assert_eq!(e.status.as_u16(), 500);
            assert!(e.body.len() <= MAX_ERROR_BODY_CHARS + 3, "{}", e.body.len());
        }

        /// Objects are fetched without credentials, within a size limit, and
        /// errors do not carry the presigned URL.
        #[tokio::test]
        async fn objects_are_fetched_without_auth_and_bounded() {
            let seen = Seen::default();
            let store = {
                let seen = seen.clone();
                serve(Router::new().fallback(move |h: HeaderMap| {
                    let seen = seen.clone();
                    async move {
                        record(&seen, "object".into(), &h);
                        vec![7u8; 1000]
                    }
                }))
                .await
            };
            let c = client("http://gw.example.com");
            let url = format!("{}/b/o.ts?X-Amz-Signature=topsecret", store);
            assert_eq!(c.fetch_object(&url).await.unwrap().len(), 1000);
            assert_eq!(seen.lock().unwrap()[0].1, None, "no credentials on objects");
            let err = c.fetch_object_limited(&url, 999).await.unwrap_err();
            assert!(format!("{:#}", err).contains("limit"), "{:#}", err);
            // A connection error does not print the signed URL.
            let err = c
                .fetch_object("http://127.0.0.1:1/b/o.ts?X-Amz-Signature=topsecret")
                .await
                .unwrap_err();
            assert!(!format!("{:#}", err).contains("topsecret"), "{:#}", err);
        }

        #[tokio::test]
        async fn delete_segments_sends_the_timerange_with_auth() {
            let seen: Seen = Arc::default();
            let app = Router::new()
                .route(
                    "/flows/f/segments",
                    axum::routing::delete(
                        |State(seen): State<Seen>,
                         Query(q): Query<HashMap<String, String>>,
                         h: HeaderMap| async move {
                            record(&seen, q.get("timerange").cloned().unwrap_or_default(), &h);
                            StatusCode::ACCEPTED
                        },
                    ),
                )
                .route(
                    "/flows/bad/segments",
                    axum::routing::delete(|| async { (StatusCode::BAD_GATEWAY, "down") }),
                )
                .with_state(seen.clone());
            let base = serve(app).await;
            let c = client(&base);
            assert_eq!(
                c.delete_segments("f", "_100:0)").await.unwrap(),
                DeleteOutcome::Accepted
            );
            assert_eq!(
                seen.lock().unwrap()[0],
                ("_100:0)".to_string(), Some("Bearer secret".to_string()))
            );
            // No such flow (nothing registered yet): not an error.
            assert_eq!(
                c.delete_segments("missing", "_1:0)").await.unwrap(),
                DeleteOutcome::NoFlow
            );
            let err = c.delete_segments("bad", "_1:0)").await.unwrap_err();
            let status = err.downcast_ref::<HttpStatusError>().map(|e| e.status);
            assert_eq!(status, Some(StatusCode::BAD_GATEWAY));
        }
    }
}
