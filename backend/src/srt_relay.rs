//! SRT relay (srt-splice) configuration and status.
//!
//! Strom's SRT blocks can dial out to an SRT relay instead of listening on a
//! public port (`srt+relay://<id>`, resolved in `strom_types::srt_relay`). This
//! module holds the relay settings and answers `GET /api/srt-relay`: the
//! relay's public address, asked from the relay's own HTTP API on demand.
//!
//! Nothing here touches the network at startup. The relay may come up after
//! Strom, so its public address is fetched lazily and cached.

use serde::Deserialize;
use std::time::{Duration, Instant};
use strom_types::srt_relay::SrtRelayInfo;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// How long a fetched public address is reused.
const SUCCESS_TTL: Duration = Duration::from_secs(30);
/// How long a failed fetch is remembered before the relay is asked again.
const FAILURE_TTL: Duration = Duration::from_secs(5);
/// Time allowed for one request to the relay's HTTP API.
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
/// Port of the relay's HTTP API when `STROM_SRT_RELAY_API` is not set.
const DEFAULT_API_PORT: u16 = 9001;

/// SRT relay settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtRelayConfig {
    /// `host:port` of the relay's SRT UDP port as Strom reaches it.
    pub relay: String,
    /// Base URL of the relay's HTTP API.
    pub api: String,
    /// Public `host:port` override; when set, the relay is not asked.
    pub public: Option<String>,
}

impl SrtRelayConfig {
    /// Build the settings from the configured values. `relay` unset or blank
    /// switches the feature off. Only the format is checked: the relay need
    /// not be reachable.
    pub fn from_settings(
        relay: Option<String>,
        api: Option<String>,
        public: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(relay) = strom_types::env::non_blank(relay).map(|r| r.trim().to_string()) else {
            return Ok(None);
        };
        let host = relay_host(&relay).ok_or_else(|| {
            anyhow::anyhow!("srt_relay.relay / STROM_SRT_RELAY must be host:port, got {relay}")
        })?;
        let api = strom_types::env::non_blank(api)
            .map(|a| a.trim().trim_end_matches('/').to_string())
            .unwrap_or_else(|| format!("http://{host}:{DEFAULT_API_PORT}"));
        let public = strom_types::env::non_blank(public).map(|p| p.trim().to_string());
        Ok(Some(Self { relay, api, public }))
    }
}

/// The host part of `host:port` (IPv6 hosts keep their brackets), or `None`
/// if the value is not `host:port` with a valid port.
fn relay_host(relay: &str) -> Option<&str> {
    let (host, port) = relay.rsplit_once(':')?;
    port.parse::<u16>().ok().filter(|p| *p != 0)?;
    let bare_ipv6 = host.contains(':') && !(host.starts_with('[') && host.ends_with(']'));
    if host.is_empty() || bare_ipv6 || host.contains('/') {
        return None;
    }
    Some(host)
}

/// The relay's `GET /v1/info` answer. Only `public` is used.
#[derive(Debug, Deserialize)]
struct RelayInfoResponse {
    public: Option<String>,
}

/// Outcome of one fetch: the public address (or `None` when the relay does
/// not know it), or why the fetch failed.
type FetchResult = Result<Option<String>, String>;

/// Relay settings plus the cached answer from the relay's HTTP API.
pub struct SrtRelayService {
    config: SrtRelayConfig,
    client: reqwest::Client,
    cache: Mutex<Option<(Instant, FetchResult)>>,
}

impl SrtRelayService {
    pub fn new(config: SrtRelayConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            config,
            client,
            cache: Mutex::new(None),
        }
    }

    /// The relay's SRT `host:port`, which `srt+relay://` URIs resolve to.
    pub fn relay_address(&self) -> &str {
        &self.config.relay
    }

    /// Current relay status for `GET /api/srt-relay`.
    pub async fn info(&self) -> SrtRelayInfo {
        let relay = Some(self.config.relay.clone());
        if let Some(public) = &self.config.public {
            return SrtRelayInfo {
                configured: true,
                relay,
                public: Some(public.clone()),
                error: None,
            };
        }
        let (public, error) = match self.public_from_relay().await {
            Ok(Some(public)) => (Some(public), None),
            Ok(None) => (
                None,
                Some(
                    "the relay does not know its public address: configure it on the relay, \
                     or set STROM_SRT_RELAY_PUBLIC"
                        .to_string(),
                ),
            ),
            Err(e) => (None, Some(e)),
        };
        SrtRelayInfo {
            configured: true,
            relay,
            public,
            error,
        }
    }

    /// The public address the relay reports, cached for [`SUCCESS_TTL`]
    /// (failures for [`FAILURE_TTL`]).
    async fn public_from_relay(&self) -> FetchResult {
        let mut cache = self.cache.lock().await;
        if let Some((at, result)) = cache.as_ref() {
            let ttl = if result.is_ok() {
                SUCCESS_TTL
            } else {
                FAILURE_TTL
            };
            if at.elapsed() < ttl {
                return result.clone();
            }
        }
        let result = self.fetch().await;
        match &result {
            Ok(public) => debug!("SRT relay reports public address {:?}", public),
            Err(e) => warn!("SRT relay info unavailable: {}", e),
        }
        *cache = Some((Instant::now(), result.clone()));
        result
    }

    async fn fetch(&self) -> FetchResult {
        let url = format!("{}/v1/info", self.config.api);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("could not reach the SRT relay API at {url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the SRT relay API at {url} answered HTTP {}",
                response.status().as_u16()
            ));
        }
        let info: RelayInfoResponse = response
            .json()
            .await
            .map_err(|e| format!("unexpected answer from the SRT relay API at {url}: {e}"))?;
        Ok(strom_types::env::non_blank(info.public))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn config(relay: &str) -> anyhow::Result<Option<SrtRelayConfig>> {
        SrtRelayConfig::from_settings(Some(relay.to_string()), None, None)
    }

    #[test]
    fn unset_or_blank_relay_is_off() {
        assert_eq!(
            SrtRelayConfig::from_settings(None, None, None).unwrap(),
            None
        );
        assert_eq!(config("  ").unwrap(), None);
    }

    #[test]
    fn api_defaults_to_the_relay_host_on_9001() {
        let c = config("srt-splice:9000").unwrap().unwrap();
        assert_eq!(c.relay, "srt-splice:9000");
        assert_eq!(c.api, "http://srt-splice:9001");
        assert_eq!(c.public, None);
        let c = config("[2001:db8::1]:9000").unwrap().unwrap();
        assert_eq!(c.api, "http://[2001:db8::1]:9001");
    }

    #[test]
    fn explicit_api_and_public_are_kept() {
        let c = SrtRelayConfig::from_settings(
            Some("127.0.0.1:9000".into()),
            Some("http://relay.example.com:8000/".into()),
            Some("srt.example.com:9000".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.api, "http://relay.example.com:8000");
        assert_eq!(c.public.as_deref(), Some("srt.example.com:9000"));
    }

    #[test]
    fn malformed_relay_is_a_config_error() {
        for bad in [
            "srt-splice",
            "srt-splice:",
            "srt-splice:0",
            "srt-splice:99999",
            ":9000",
            "2001:db8::1",
            "srt://srt-splice:9000",
        ] {
            let err = config(bad).unwrap_err().to_string();
            assert!(err.contains("STROM_SRT_RELAY"), "{bad}: {err}");
        }
    }

    /// Serve `/v1/info` on a local port; returns the API base URL and a
    /// counter of requests.
    async fn fake_relay(public: Option<&'static str>) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/v1/info",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    Json(serde_json::json!({
                        "version": "0.1.0", "public": public, "srt_port": 9000
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    fn service(api: String, public: Option<&str>) -> SrtRelayService {
        SrtRelayService::new(SrtRelayConfig {
            relay: "127.0.0.1:9000".into(),
            api,
            public: public.map(str::to_string),
        })
    }

    #[tokio::test]
    async fn public_comes_from_the_relay_and_is_cached() {
        let (api, hits) = fake_relay(Some("srt.example.com:9000")).await;
        let svc = service(api, None);
        let info = svc.info().await;
        assert_eq!(
            info,
            SrtRelayInfo {
                configured: true,
                relay: Some("127.0.0.1:9000".into()),
                public: Some("srt.example.com:9000".into()),
                error: None,
            }
        );
        svc.info().await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn relay_without_public_address_explains() {
        let (api, _) = fake_relay(None).await;
        let info = service(api, None).info().await;
        assert_eq!(info.public, None);
        assert!(info.error.unwrap().contains("STROM_SRT_RELAY_PUBLIC"));
    }

    #[tokio::test]
    async fn unreachable_relay_reports_an_error() {
        // Bind and drop to get a local port nobody listens on.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let info = service(format!("http://127.0.0.1:{port}"), None)
            .info()
            .await;
        assert!(info.configured);
        assert_eq!(info.public, None);
        assert!(info.error.unwrap().contains("could not reach"));
    }

    #[tokio::test]
    async fn public_override_skips_the_relay() {
        let (api, hits) = fake_relay(Some("other.example.com:9000")).await;
        let info = service(api, Some("srt.example.com:9000")).info().await;
        assert_eq!(info.public.as_deref(), Some("srt.example.com:9000"));
        assert_eq!(info.error, None);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}
