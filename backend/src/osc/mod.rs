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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Default OSC token-exchange endpoint. Overridable via `STROM_OSC_TOKEN_URL`.
const DEFAULT_TOKEN_URL: &str = "https://token.svc.prod.osaas.io/servicetoken";

/// SATs live ~1 hour; refresh well before then so an in-flight request never
/// races expiry. The OSC docs recommend refreshing every ~50 minutes.
const SAT_REFRESH_AFTER: Duration = Duration::from_secs(50 * 60);

/// When the SAT carries an `exp` claim, refresh this long before it.
const SAT_EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);

/// A cached SAT. Times are wall-clock (`SystemTime`), not `Instant`: on macOS
/// the monotonic clock stops while the machine sleeps, so an `Instant`-based
/// age would keep a SAT "fresh" for hours after it really expired.
struct CachedSat {
    token: String,
    fetched_at: SystemTime,
    refresh_at: SystemTime,
}

/// The claims of a JWT, if it is one. The signature is not checked: this only
/// decides when to refresh our own token, never whether to trust one.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A numeric date claim (seconds since the Unix epoch) of a JWT.
fn jwt_time(token: &str, claim: &str) -> Option<SystemTime> {
    let secs = jwt_claims(token)?.get(claim)?.as_u64()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// The `exp` claim of a JWT, if it has one.
#[cfg(test)]
fn jwt_exp(token: &str) -> Option<SystemTime> {
    jwt_time(token, "exp")
}

/// Never schedule a refresh sooner than this after a fetch. A host clock that
/// is ahead of the token server's would otherwise put `exp` in the past, and
/// every request would mint a new SAT. A SAT that really is rejected is
/// dropped on the 401 (see [`SatProvider::invalidate`]).
const SAT_MIN_REFRESH: Duration = Duration::from_secs(60);

/// When a SAT fetched at `fetched_at` should be refreshed: after
/// [`SAT_REFRESH_AFTER`], or [`SAT_EXPIRY_MARGIN`] before its own expiry,
/// whichever comes first, but no sooner than [`SAT_MIN_REFRESH`].
///
/// `exp` is on the token server's clock and `fetched_at` on ours, so the two
/// are not compared directly. With an `iat` claim the token's lifetime
/// (`exp - iat`, both on the server's clock) is applied to `fetched_at`. Without
/// one, `exp` is compared with our clock, and the floor bounds what a skewed
/// clock can do.
fn sat_refresh_at(token: &str, fetched_at: SystemTime) -> SystemTime {
    let by_age = fetched_at + SAT_REFRESH_AFTER;
    let floor = fetched_at + SAT_MIN_REFRESH;
    let Some(exp) = jwt_time(token, "exp") else {
        return by_age;
    };
    // The claims come from the token server: an absurd lifetime that does not
    // fit in a `SystemTime` refreshes by age instead of panicking.
    let expires_at = match jwt_time(token, "iat").and_then(|iat| exp.duration_since(iat).ok()) {
        Some(lifetime) => match fetched_at.checked_add(lifetime) {
            Some(t) => t,
            None => return by_age,
        },
        None => exp,
    };
    match expires_at.checked_sub(SAT_EXPIRY_MARGIN) {
        Some(by_exp) => by_age.min(by_exp).max(floor),
        None => floor,
    }
}

/// Whether a cached SAT is still usable at wall-clock `now`. A clock that
/// went backwards past `fetched_at` counts as stale: the age is unknown.
fn sat_is_fresh(sat: &CachedSat, now: SystemTime) -> bool {
    now >= sat.fetched_at && now < sat.refresh_at
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
                if sat_is_fresh(c, SystemTime::now()) {
                    return Ok(c.token.clone());
                }
            }
        }
        // Miss or stale: mint WITHOUT holding the cache lock, so a slow token
        // endpoint can't stall unrelated callers. A concurrent double-mint is
        // rare and harmless.
        let token = self.mint(&pat, service_id).await?;
        let fetched_at = SystemTime::now();
        self.cache.lock().await.insert(
            cache_key,
            CachedSat {
                refresh_at: sat_refresh_at(&token, fetched_at),
                token: token.clone(),
                fetched_at,
            },
        );
        Ok(token)
    }

    /// Drop the cached SAT for `(credential_key, service_id)`, so the next
    /// [`Self::token`] mints a new one. Called when a service rejects the SAT
    /// (HTTP 401) although the cache still held it as fresh. Returns whether a
    /// PAT exists to mint a new SAT from, i.e. whether a retry could help.
    pub async fn invalidate(&self, credential_key: &str, service_id: &str) -> bool {
        let Some(pat) = self.resolve_pat(credential_key) else {
            return false;
        };
        let cache_key = (pat_fingerprint(&pat), service_id.to_string());
        if self.cache.lock().await.remove(&cache_key).is_some() {
            info!(
                "OSC: dropped rejected SAT for service {} (credential {})",
                service_id, credential_key
            );
        }
        true
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
            let text = bounded_error_body(resp.text().await.unwrap_or_default());
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

/// Longest token-endpoint error body kept in an error. Errors are logged, and
/// an endpoint (possibly a misconfigured `STROM_OSC_TOKEN_URL`) can send
/// anything. Same limit as the TAMS client's.
const MAX_ERROR_BODY_CHARS: usize = 300;

/// `body` cut to [`MAX_ERROR_BODY_CHARS`] characters.
fn bounded_error_body(mut body: String) -> String {
    if let Some((cut, _)) = body.char_indices().nth(MAX_ERROR_BODY_CHARS) {
        body.truncate(cut);
        body.push_str("...");
    }
    body
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
/// that are not OSC-hosted (e.g. a self-hosted gateway) or do not parse, so the
/// caller can ask for an explicit service id instead.
///
/// The host is taken from the parsed URL, never by splitting the string:
/// `https://a.svc.auto.prod.osaas.io:pw@evil.example/` has user info that
/// *looks* like an OSC host, but the request goes to `evil.example`.
pub fn derive_service_id(url: &str) -> Option<String> {
    service_id_of_host(reqwest::Url::parse(url).ok()?.host_str()?)
}

/// The OSC service type of a host name `<tenant>-<instance>.<service-type>.…osaas.io`.
fn service_id_of_host(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let prefix = host.strip_suffix(".osaas.io")?;
    let mut labels = prefix.split('.');
    let tenant_instance = labels.next()?; // <tenant>-<instance>
    let service_type = labels.next()?; // <service-type>
    if tenant_instance.is_empty() || service_type.is_empty() || labels.any(str::is_empty) {
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
        // Not a URL, or no host.
        assert_eq!(
            derive_service_id("a-b.eyevinn-tams-gateway.auto.prod.osaas.io"),
            None
        );
        assert_eq!(derive_service_id("data:text/plain,osaas.io"), None);
        assert_eq!(derive_service_id("https://osaas.io"), None);
        assert_eq!(derive_service_id("https://x.osaas.io"), None);
        assert_eq!(derive_service_id("https://a..osaas.io"), None);
    }

    #[test]
    fn user_info_port_query_and_suffixes_do_not_fake_an_osc_host() {
        for url in [
            // The review's example: user info that looks like an OSC host.
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io:pw@evil.example/",
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io@evil.example/",
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io:443@evil.example:443/",
            // An OSC-looking host in the path, query or fragment.
            "https://evil.example/x.eyevinn-tams-gateway.auto.prod.osaas.io",
            "https://evil.example?h=x.eyevinn-tams-gateway.auto.prod.osaas.io",
            "https://evil.example#x.eyevinn-tams-gateway.auto.prod.osaas.io",
            "https://evil.example\\x.eyevinn-tams-gateway.auto.prod.osaas.io",
            // Lookalike hosts.
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io.evil.example/",
            "https://x.eyevinn-tams-gateway.auto.prod.evilosaas.io/",
            "https://x.eyevinn-tams-gateway.auto.prod.osaas.io./",
        ] {
            assert_eq!(derive_service_id(url), None, "{url}");
        }
        // User info on a real OSC host does not change the host.
        assert_eq!(
            derive_service_id("https://u:p@a-b.eyevinn-tams-gateway.auto.prod.osaas.io:8443/x"),
            Some("eyevinn-tams-gateway".to_string())
        );
        // Host names are case-insensitive.
        assert_eq!(
            derive_service_id("https://A-B.Eyevinn-Tams-Gateway.AUTO.PROD.OSAAS.IO"),
            Some("eyevinn-tams-gateway".to_string())
        );
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

    /// A JWT-shaped token with the given claims (unsigned; only `exp` is read).
    fn jwt(claims: serde_json::Value) -> String {
        use base64::Engine;
        let enc = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.sig",
            enc(br#"{"alg":"none"}"#),
            enc(claims.to_string().as_bytes())
        )
    }

    fn cached(token: &str, fetched_at: SystemTime) -> CachedSat {
        CachedSat {
            token: token.to_string(),
            fetched_at,
            refresh_at: sat_refresh_at(token, fetched_at),
        }
    }

    #[test]
    fn sat_freshness_follows_the_wall_clock() {
        let t0 = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let sat = cached("opaque", t0);
        assert!(sat_is_fresh(&sat, t0));
        assert!(sat_is_fresh(&sat, t0 + Duration::from_secs(49 * 60)));
        assert!(!sat_is_fresh(&sat, t0 + SAT_REFRESH_AFTER));
        // A night asleep: wall time moved on, so the SAT is stale.
        assert!(!sat_is_fresh(&sat, t0 + Duration::from_secs(10 * 3600)));
        // A clock set backwards past the fetch counts as stale.
        assert!(!sat_is_fresh(&sat, t0 - Duration::from_secs(1)));
    }

    #[test]
    fn sat_refresh_honours_an_earlier_exp_claim() {
        let t0 = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        // exp in 20 minutes: refresh 5 minutes before it, not after 50.
        let short = jwt(serde_json::json!({ "exp": 1_800_000_000u64 + 20 * 60 }));
        let sat = cached(&short, t0);
        assert_eq!(sat.refresh_at, t0 + Duration::from_secs(15 * 60));
        assert!(sat_is_fresh(&sat, t0 + Duration::from_secs(14 * 60)));
        assert!(!sat_is_fresh(&sat, t0 + Duration::from_secs(16 * 60)));
        // exp further out than the age limit: the age limit wins.
        let long = jwt(serde_json::json!({ "exp": 1_800_000_000u64 + 3 * 3600 }));
        assert_eq!(cached(&long, t0).refresh_at, t0 + SAT_REFRESH_AFTER);
        // No exp, or not a JWT: the age limit.
        assert_eq!(jwt_exp(&jwt(serde_json::json!({ "sub": "x" }))), None);
        assert_eq!(jwt_exp("not-a-jwt"), None);
    }

    #[test]
    fn a_skewed_host_clock_does_not_mint_per_request() {
        let t0 = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        // Host clock 2 h ahead of the token server: by our clock the SAT
        // (iat = server now, exp = +1 h) has already expired. With `iat` the
        // lifetime is used instead: 1 h - 5 min margin, capped at 50 min.
        let server_now = 1_800_000_000u64 - 2 * 3600;
        let tok = jwt(serde_json::json!({ "iat": server_now, "exp": server_now + 3600 }));
        let sat = cached(&tok, t0);
        assert_eq!(sat.refresh_at, t0 + SAT_REFRESH_AFTER);
        assert!(sat_is_fresh(&sat, t0 + Duration::from_secs(10)));
        // A short-lived SAT with iat: refresh 5 minutes before its lifetime ends.
        let tok = jwt(serde_json::json!({ "iat": server_now, "exp": server_now + 20 * 60 }));
        assert_eq!(
            cached(&tok, t0).refresh_at,
            t0 + Duration::from_secs(15 * 60)
        );
        // No iat, exp in the past by our clock: the floor, not "always stale".
        let tok = jwt(serde_json::json!({ "exp": server_now + 3600 }));
        let sat = cached(&tok, t0);
        assert_eq!(sat.refresh_at, t0 + SAT_MIN_REFRESH);
        assert!(sat_is_fresh(&sat, t0 + Duration::from_secs(30)));
    }

    #[test]
    fn an_absurd_lifetime_does_not_overflow() {
        let t0 = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        // exp - iat does not fit after `fetched_at`: refresh by age.
        let tok = jwt(serde_json::json!({ "iat": 0u64, "exp": i64::MAX as u64 }));
        assert_eq!(cached(&tok, t0).refresh_at, t0 + SAT_REFRESH_AFTER);
    }

    #[test]
    fn token_endpoint_error_body_is_bounded() {
        let long = "x".repeat(10_000);
        let cut = bounded_error_body(long);
        assert_eq!(cut.len(), MAX_ERROR_BODY_CHARS + 3);
        assert!(cut.ends_with("..."));
        // Cut on a character boundary, not a byte.
        let cut = bounded_error_body("å".repeat(400));
        assert_eq!(cut.chars().count(), MAX_ERROR_BODY_CHARS + 3);
        assert_eq!(bounded_error_body("short".into()), "short");
    }

    #[test]
    fn same_pat_shares_a_cache_key_distinct_pats_do_not() {
        // The SAT cache keys by PAT fingerprint, so two flows on the same PAT
        // collapse to one cache entry while different PATs stay separate.
        assert_eq!(pat_fingerprint("pat-a"), pat_fingerprint("pat-a"));
        assert_ne!(pat_fingerprint("pat-a"), pat_fingerprint("pat-b"));
    }
}
