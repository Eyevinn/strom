//! Outbound HTTP authentication methods — how Strom authenticates to *external*
//! services it calls (e.g. a TAMS gateway).
//!
//! This is the opposite direction from [`crate::auth`], which authenticates
//! *inbound* requests to Strom's own server. Keep the two from being confused.
//!
//! The TAMS gateway client is the first user. The enum is deliberately general
//! so other outbound integrations can share it and so new schemes (HTTP Basic,
//! OIDC client-credentials, ...) slot in as additional variants without touching
//! the call sites — each just signs a `reqwest::RequestBuilder` in [`AuthMethod::apply`].

use crate::osc::SatProvider;
use anyhow::Result;
use reqwest::header::AUTHORIZATION;
use std::sync::Arc;

/// How an outbound HTTP request authenticates to a remote service.
///
/// A block's user-facing "Authentication" choice is a projection of this: the UI
/// picks a scheme and supplies its inputs, and the backend resolves it to one of
/// these variants (e.g. an empty static token collapses to [`AuthMethod::None`]).
#[derive(Clone)]
pub enum AuthMethod {
    /// No `Authorization` header — the service is open or behind an external gate.
    None,
    /// Static bearer token (a long-lived, pre-shared token).
    Bearer(String),
    /// OSC PAT/SAT: mint a short-lived Service Access Token from the OSC Personal
    /// Access Token registered under `credential_key` (in practice the flow id),
    /// falling back to the instance default PAT. Refreshed per service id.
    Osc {
        provider: Arc<SatProvider>,
        service_id: String,
        /// Credential key the PAT is looked up under — isolates OSC tenants on a
        /// shared Strom instance.
        credential_key: String,
    },
    // Future schemes slot in here as new variants, e.g.
    //   Basic { username: String, password: String },
    //   Oidc(Arc<OidcClient>),
    // Add the matching arm in `apply` and they work everywhere AuthMethod is used.
}

impl AuthMethod {
    /// Attach the appropriate credentials to an outgoing request. Schemes that
    /// resolve a token over the network (OSC) make this async and fallible.
    pub async fn apply(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(match self {
            AuthMethod::None => req,
            AuthMethod::Bearer(token) => req.header(AUTHORIZATION, format!("Bearer {}", token)),
            AuthMethod::Osc {
                provider,
                service_id,
                credential_key,
            } => {
                let sat = provider.token(credential_key, service_id).await?;
                req.header(AUTHORIZATION, format!("Bearer {}", sat))
            }
        })
    }

    /// The service rejected the credentials `apply` attached (HTTP 401). Drop
    /// any cached token so the next `apply` gets a new one, and return whether
    /// retrying the request once could help. Only schemes with a cached,
    /// refreshable token (OSC) say yes; a static token would fail the same way.
    pub async fn invalidate_after_rejection(&self) -> bool {
        match self {
            AuthMethod::None | AuthMethod::Bearer(_) => false,
            AuthMethod::Osc {
                provider,
                service_id,
                credential_key,
            } => provider.invalidate(credential_key, service_id).await,
        }
    }
}

/// Whether two URLs have the same origin (scheme, host and port).
pub fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.origin() == b.origin()
}

/// Redirect policy for requests that carry credentials: follow a redirect only
/// within the origin of the first request (at most 10 hops). reqwest strips the
/// standard `Authorization`/`Cookie` headers on a cross-origin redirect, but not
/// a custom header a future scheme may use, and the redirect target still sees
/// the request; refusing to leave the origin keeps credentials with the service
/// they were meant for. A refused redirect comes back as the 3xx response.
pub fn same_origin_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let leaves_origin = attempt
            .previous()
            .first()
            .is_some_and(|first| !same_origin(first, attempt.url()));
        if leaves_origin || attempt.previous().len() >= 10 {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

/// A URL for logs: scheme, host, port and path, without the query string or
/// user info. Presigned object URLs carry their signature in the query.
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => url.split(['?', '#']).next().unwrap_or_default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn origins() {
        assert!(same_origin(
            &url("https://gw.example.com/a"),
            &url("https://gw.example.com:443/b?x=1")
        ));
        assert!(!same_origin(
            &url("https://gw.example.com/a"),
            &url("http://gw.example.com/a")
        ));
        assert!(!same_origin(
            &url("https://gw.example.com/a"),
            &url("https://gw.example.com:8443/a")
        ));
        assert!(!same_origin(
            &url("https://gw.example.com/a"),
            &url("https://evil.example.net/a")
        ));
    }

    #[test]
    fn redaction_drops_query_and_user_info() {
        assert_eq!(
            redact_url("https://s3.example.com/b/o.ts?X-Amz-Signature=abc&X-Amz-Credential=k"),
            "https://s3.example.com/b/o.ts"
        );
        assert_eq!(
            redact_url("https://user:pw@s3.example.com/b/o.ts#f"),
            "https://s3.example.com/b/o.ts"
        );
        assert_eq!(redact_url("not a url?secret=1"), "not a url");
    }
}
