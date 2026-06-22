//! Eyevinn Open Source Cloud (OSC) service authentication.
//!
//! OSC uses a two-token model instead of plain OIDC:
//! - a long-lived **Personal Access Token (PAT)**, configured per Strom instance
//!   via the `STROM_OSC_PAT` env var (the OSC SDK's `OSC_ACCESS_TOKEN` is accepted
//!   as a fallback), or pushed at runtime over the API — the latter lets a control
//!   plane like Open Live hand its PAT to a freshly provisioned Strom VM;
//! - short-lived **Service Access Tokens (SATs)**, minted from the PAT and scoped
//!   to a single OSC service type. A SAT lives ~1 hour.
//!
//! This module exchanges the PAT for SATs on demand and caches them per service
//! id, refreshing before expiry. It is OSC-wide, not TAMS-specific, so any
//! OSC-backed block can reuse it (the TAMS Output block is the first user).
//!
//! See <https://github.com/EyevinnOSC/community/wiki> ("Service Access Tokens").

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Default OSC token-exchange endpoint. Overridable via `STROM_OSC_TOKEN_URL`.
const DEFAULT_TOKEN_URL: &str = "https://token.svc.prod.osaas.io/servicetoken";

/// SATs live ~1 hour; refresh well before then so an in-flight request never
/// races expiry. The OSC docs recommend refreshing every ~50 minutes.
const SAT_REFRESH_AFTER: Duration = Duration::from_secs(50 * 60);

struct CachedSat {
    token: String,
    fetched_at: Instant,
}

/// Mints and caches OSC Service Access Tokens from a single Personal Access Token.
///
/// The PAT is mutable at runtime ([`set_pat`](Self::set_pat)); replacing it clears
/// the SAT cache so subsequent tokens are minted from the new PAT.
pub struct SatProvider {
    /// The PAT, or `None` until configured (via env or the API).
    pat: RwLock<Option<String>>,
    token_url: String,
    http: reqwest::Client,
    /// Per-service-id SAT cache. A `tokio::Mutex` held across the mint serializes
    /// concurrent callers for the same service so we never mint duplicates.
    cache: Mutex<HashMap<String, CachedSat>>,
}

#[derive(Deserialize)]
struct ServiceTokenResp {
    token: String,
}

impl SatProvider {
    pub fn new(pat: Option<String>, token_url: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building OSC token HTTP client")?;
        Ok(Self {
            pat: RwLock::new(pat.filter(|p| !p.is_empty())),
            token_url: token_url.unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string()),
            http,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Whether a PAT is currently configured.
    pub fn has_pat(&self) -> bool {
        self.pat.read().map(|p| p.is_some()).unwrap_or(false)
    }

    /// Set (or replace) the PAT at runtime and clear cached SATs, so the next
    /// `token` call mints from the new PAT.
    pub async fn set_pat(&self, pat: String) {
        if let Ok(mut guard) = self.pat.write() {
            *guard = Some(pat);
        }
        self.cache.lock().await.clear();
        info!("OSC: PAT set; SAT cache cleared");
    }

    /// Forget the PAT and all cached SATs.
    pub async fn clear_pat(&self) {
        if let Ok(mut guard) = self.pat.write() {
            *guard = None;
        }
        self.cache.lock().await.clear();
        info!("OSC: PAT cleared");
    }

    /// Return a valid SAT for `service_id`, minting or refreshing as needed.
    pub async fn token(&self, service_id: &str) -> Result<String> {
        let mut cache = self.cache.lock().await;
        if let Some(c) = cache.get(service_id) {
            if c.fetched_at.elapsed() < SAT_REFRESH_AFTER {
                return Ok(c.token.clone());
            }
        }
        let token = self.mint(service_id).await?;
        cache.insert(
            service_id.to_string(),
            CachedSat {
                token: token.clone(),
                fetched_at: Instant::now(),
            },
        );
        Ok(token)
    }

    /// Exchange the PAT for a fresh SAT scoped to `service_id`.
    async fn mint(&self, service_id: &str) -> Result<String> {
        let pat = self
            .pat
            .read()
            .ok()
            .and_then(|p| p.clone())
            .ok_or_else(|| {
                anyhow!(
                    "OSC PAT not configured — set STROM_OSC_PAT / OSC_ACCESS_TOKEN, \
                     or push it via PUT /api/osc/pat"
                )
            })?;

        debug!("OSC: minting SAT for service {}", service_id);
        let resp = self
            .http
            .post(&self.token_url)
            .header("x-pat-jwt", format!("Bearer {}", pat))
            .json(&serde_json::json!({ "serviceId": service_id }))
            .send()
            .await
            .with_context(|| format!("POST {}", self.token_url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OSC servicetoken {} -> {}: {}",
                self.token_url,
                status,
                text
            ));
        }
        let parsed: ServiceTokenResp = resp
            .json()
            .await
            .context("parsing OSC servicetoken response")?;
        info!("OSC: minted SAT for service {}", service_id);
        Ok(parsed.token)
    }
}

/// The process-wide OSC SAT provider, built on first use.
///
/// The PAT is seeded from `STROM_OSC_PAT` (or `OSC_ACCESS_TOKEN`) if set, and can
/// be supplied or replaced later at runtime via [`SatProvider::set_pat`]. Always
/// returns a provider; callers check [`SatProvider::has_pat`] to know whether a
/// PAT is available yet.
pub fn sat_provider() -> Arc<SatProvider> {
    static PROVIDER: OnceLock<Arc<SatProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| {
            let pat = std::env::var("STROM_OSC_PAT")
                .ok()
                .or_else(|| std::env::var("OSC_ACCESS_TOKEN").ok())
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
            let token_url = std::env::var("STROM_OSC_TOKEN_URL")
                .ok()
                .filter(|u| !u.is_empty());
            Arc::new(
                SatProvider::new(pat, token_url).expect("building OSC SAT provider (HTTP client)"),
            )
        })
        .clone()
}

/// Derive the OSC service id (the service-type slug) from a service URL.
///
/// OSC service URLs look like
/// `https://<tenant>-<instance>.<service-type>.auto.prod.osaas.io`; a SAT is
/// scoped to `<service-type>` (the second DNS label). Returns `None` for URLs
/// that are not OSC-hosted (e.g. a self-hosted gateway), so the caller can ask
/// for an explicit service id instead.
pub fn derive_service_id(url: &str) -> Option<String> {
    let host = url.split("://").nth(1).unwrap_or(url);
    let host = host.split('/').next()?; // strip path
    let host = host.split(':').next()?; // strip port
    if !host.ends_with(".osaas.io") {
        return None;
    }
    let mut labels = host.split('.');
    let _tenant_instance = labels.next()?; // <tenant>-<instance>
    let service_type = labels.next()?; // <service-type>
    if service_type.is_empty() {
        None
    } else {
        Some(service_type.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_service_type_from_osc_url() {
        assert_eq!(
            derive_service_id(
                "https://mytenant-myinstance.eyevinn-tams-gateway.auto.prod.osaas.io"
            ),
            Some("eyevinn-tams-gateway".to_string())
        );
        // With a path and port.
        assert_eq!(
            derive_service_id("https://a-b.eyevinn-test-adserver.auto.prod.osaas.io:443/flows"),
            Some("eyevinn-test-adserver".to_string())
        );
    }

    #[test]
    fn returns_none_for_non_osc_urls() {
        assert_eq!(derive_service_id("http://localhost:8000"), None);
        assert_eq!(derive_service_id("https://tams.example.com/flows"), None);
    }

    #[test]
    fn pat_lifecycle() {
        let p = SatProvider::new(None, None).unwrap();
        assert!(!p.has_pat());
        // set_pat / clear_pat need a runtime for the cache lock.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(p.set_pat("secret".to_string()));
        assert!(p.has_pat());
        rt.block_on(p.clear_pat());
        assert!(!p.has_pat());
    }
}
