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

/// Reserved credential key for the default (fallback) PAT, seeded from the
/// `STROM_OSC_PAT` env var. Per-flow keys are flow UUIDs, so this never collides.
const DEFAULT_KEY: &str = "__default__";

/// A non-secret fingerprint of a PAT, used as a SAT cache key. A SAT is fully
/// determined by `(PAT, service_id)`, so flows sharing a PAT share one cached
/// SAT, while different tenants (different PATs) never do. We hash rather than
/// key on the PAT itself so the secret isn't duplicated across map keys.
fn pat_fingerprint(pat: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pat.hash(&mut hasher);
    hasher.finish()
}

/// Mints and caches OSC Service Access Tokens from Personal Access Tokens.
///
/// Supports a shared, multi-tenant Strom: PATs are keyed by an opaque credential
/// key (in practice the Strom flow id), so different OSC users never reach each
/// other's services. A `default` PAT (from `STROM_OSC_PAT`) is used when no
/// per-key PAT is registered — the single-tenant case. The SAT cache is keyed by
/// `(pat_fingerprint, service_id)`: SATs are per service and follow the PAT, so
/// flows sharing a PAT (same tenant) share a SAT per service while different
/// tenants never do.
pub struct SatProvider {
    /// PAT per credential key. The reserved [`DEFAULT_KEY`] holds the fallback PAT.
    pats: RwLock<HashMap<String, String>>,
    token_url: String,
    http: reqwest::Client,
    /// SAT cache keyed by `(pat_fingerprint, service_id)` — so flows on the same
    /// PAT share a SAT and different tenants never do. The lock is released
    /// before the network mint, so a slow/hung token endpoint never stalls
    /// unrelated callers (other flows/services) or PAT pushes; a rare concurrent
    /// double-mint is harmless (both SATs are valid, last write wins).
    cache: Mutex<HashMap<(u64, String), CachedSat>>,
}

#[derive(Deserialize)]
struct ServiceTokenResp {
    token: String,
}

impl SatProvider {
    pub fn new(default_pat: Option<String>, token_url: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building OSC token HTTP client")?;
        let mut pats = HashMap::new();
        if let Some(pat) = default_pat.filter(|p| !p.is_empty()) {
            pats.insert(DEFAULT_KEY.to_string(), pat);
        }
        Ok(Self {
            pats: RwLock::new(pats),
            token_url: token_url.unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string()),
            http,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Whether a PAT usable for `credential_key` exists — its own, or the default.
    pub fn has_pat_for(&self, credential_key: &str) -> bool {
        self.pats
            .read()
            .map(|m| m.contains_key(credential_key) || m.contains_key(DEFAULT_KEY))
            .unwrap_or(false)
    }

    /// Whether the default (fallback) PAT is set.
    pub fn has_default_pat(&self) -> bool {
        self.pats
            .read()
            .map(|m| m.contains_key(DEFAULT_KEY))
            .unwrap_or(false)
    }

    /// The per-flow credential keys with a registered PAT (excludes the default).
    pub fn configured_keys(&self) -> Vec<String> {
        self.pats
            .read()
            .map(|m| m.keys().filter(|k| *k != DEFAULT_KEY).cloned().collect())
            .unwrap_or_default()
    }

    /// Register (or replace) the PAT for one credential key (the flow id). Clears
    /// the SAT cache so a rotated PAT takes effect immediately; PAT changes are
    /// rare control-plane events, so dropping the whole (cheap to refill) cache is
    /// simpler than tracking which entries a given key fed.
    pub async fn set_pat(&self, credential_key: String, pat: String) {
        if let Ok(mut m) = self.pats.write() {
            m.insert(credential_key.clone(), pat);
        }
        self.cache.lock().await.clear();
        info!("OSC: PAT set for credential {}", credential_key);
    }

    /// Forget one credential key's PAT and clear the SAT cache.
    pub async fn clear_pat(&self, credential_key: &str) {
        if let Ok(mut m) = self.pats.write() {
            m.remove(credential_key);
        }
        self.cache.lock().await.clear();
        info!("OSC: PAT cleared for credential {}", credential_key);
    }

    /// Return a valid SAT for `(credential_key, service_id)`, minting or refreshing
    /// as needed. The PAT is the one registered for `credential_key`, else the
    /// default; the SAT is cached by the PAT, so flows sharing a PAT share it.
    pub async fn token(&self, credential_key: &str, service_id: &str) -> Result<String> {
        let pat = self.resolve_pat(credential_key).ok_or_else(|| {
            anyhow!(
                "OSC PAT not configured for credential '{}' — set STROM_OSC_PAT / \
                 OSC_ACCESS_TOKEN, or push it via PUT /api/osc/pat/{}",
                credential_key,
                credential_key
            )
        })?;
        let cache_key = (pat_fingerprint(&pat), service_id.to_string());
        // Fast path: return a cached, still-fresh SAT. Scope the guard so it is
        // dropped before the (potentially slow) network mint below.
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.get(&cache_key) {
                if c.fetched_at.elapsed() < SAT_REFRESH_AFTER {
                    return Ok(c.token.clone());
                }
            }
        }
        // Miss or stale: mint WITHOUT holding the cache lock, so a slow token
        // endpoint can't stall unrelated callers. A concurrent double-mint is
        // rare and harmless.
        let token = self.mint(&pat, service_id).await?;
        self.cache.lock().await.insert(
            cache_key,
            CachedSat {
                token: token.clone(),
                fetched_at: Instant::now(),
            },
        );
        Ok(token)
    }

    /// The PAT for a credential key: its own if registered, else the default.
    fn resolve_pat(&self, credential_key: &str) -> Option<String> {
        let m = self.pats.read().ok()?;
        m.get(credential_key)
            .or_else(|| m.get(DEFAULT_KEY))
            .cloned()
    }

    /// Exchange a PAT for a fresh SAT scoped to `service_id`.
    async fn mint(&self, pat: &str, service_id: &str) -> Result<String> {
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
/// The default PAT is seeded from `STROM_OSC_PAT` (or `OSC_ACCESS_TOKEN`) if set,
/// and per-flow PATs can be supplied at runtime via [`SatProvider::set_pat`].
/// Always returns a provider; callers check [`SatProvider::has_pat_for`] to know
/// whether a PAT is available for a given credential yet.
pub fn sat_provider() -> Arc<SatProvider> {
    static PROVIDER: OnceLock<Arc<SatProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| {
            // var_opt already rejects a blank value, so the trim here is only
            // to strip padding from a real token.
            let pat = strom_types::env::var_opt("STROM_OSC_PAT")
                .or_else(|| strom_types::env::var_opt("OSC_ACCESS_TOKEN"))
                .map(|p| p.trim().to_string());
            let token_url = strom_types::env::var_opt("STROM_OSC_TOKEN_URL");
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
    fn no_pat_resolves_to_nothing() {
        let p = SatProvider::new(None, None).unwrap();
        assert!(!p.has_default_pat());
        assert!(!p.has_pat_for("flow-a"));
        assert!(p.configured_keys().is_empty());
        assert_eq!(p.resolve_pat("flow-a"), None);
    }

    #[test]
    fn per_flow_pats_isolate_and_fall_back_to_env_default() {
        // Default PAT is seeded from env (here: the constructor), per-flow PATs
        // are pushed at runtime.
        let p = SatProvider::new(Some("pat-default".to_string()), None).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        assert!(p.has_default_pat());
        // Any flow falls back to the default until it has its own PAT.
        assert!(p.has_pat_for("flow-a"));
        assert_eq!(p.resolve_pat("flow-a").as_deref(), Some("pat-default"));

        // A per-flow PAT overrides the default for that flow only.
        rt.block_on(p.set_pat("flow-a".to_string(), "pat-a".to_string()));
        assert_eq!(p.resolve_pat("flow-a").as_deref(), Some("pat-a"));
        assert_eq!(p.resolve_pat("flow-b").as_deref(), Some("pat-default"));
        assert_eq!(p.configured_keys(), vec!["flow-a".to_string()]);

        // Clearing flow-a falls back to the default again.
        rt.block_on(p.clear_pat("flow-a"));
        assert_eq!(p.resolve_pat("flow-a").as_deref(), Some("pat-default"));
        assert!(p.configured_keys().is_empty());
    }

    #[test]
    fn same_pat_shares_a_cache_key_distinct_pats_do_not() {
        // The SAT cache keys by PAT fingerprint, so two flows on the same PAT
        // collapse to one cache entry while different PATs stay separate.
        assert_eq!(pat_fingerprint("pat-a"), pat_fingerprint("pat-a"));
        assert_ne!(pat_fingerprint("pat-a"), pat_fingerprint("pat-b"));
    }
}
