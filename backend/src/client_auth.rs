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
}
