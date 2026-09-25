use crate::json_rejection::JsonBody;
use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
    Extension, Json,
};
use std::sync::Arc;
pub use strom_types::api::AuthStatusResponse;
pub use strom_types::auth::{LoginRequest, LoginResponse};
use tower_sessions::Session;
use tracing::warn;

pub(crate) const SESSION_USER_KEY: &str = "user_authenticated";

/// Whether this cookie session has completed a successful login.
///
/// Route handlers outside the `auth_middleware` chain (the MCP endpoint, which
/// needs its own error shape) use this to accept the same session cookie the
/// middleware would have accepted.
pub async fn session_is_authenticated(session: &Session) -> bool {
    matches!(session.get::<bool>(SESSION_USER_KEY).await, Ok(Some(true)))
}

/// Authentication configuration loaded from environment variables
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// Admin username (from STROM_ADMIN_USER env var)
    pub admin_user: Option<String>,
    /// Admin password hash (from STROM_ADMIN_PASSWORD_HASH env var)
    pub admin_password_hash: Option<String>,
    /// API key for bearer token auth (from STROM_API_KEY env var)
    pub api_key: Option<String>,
    /// Native GUI token (auto-generated for embedded GUI authentication)
    pub native_gui_token: Option<String>,
    /// Whether authentication is enabled
    pub enabled: bool,
}

impl AuthConfig {
    pub fn from_env() -> Self {
        // A blank value is not a credential. Without this an empty
        // STROM_API_KEY enables authentication and then accepts the empty
        // bearer token, and an empty STROM_ADMIN_USER enables it with no
        // working login at all. strom_types::env scrubs blanks in main, so
        // this is the second layer for anything that arrives another way.
        let admin_user = strom_types::env::var_opt("STROM_ADMIN_USER");
        let admin_password_hash = strom_types::env::var_opt("STROM_ADMIN_PASSWORD_HASH");
        let api_key = strom_types::env::var_opt("STROM_API_KEY");

        // Authentication is enabled if any method is configured
        let enabled = admin_user.is_some() || api_key.is_some();

        if admin_user.is_some() && admin_password_hash.is_none() {
            if api_key.is_some() {
                warn!(
                    "STROM_ADMIN_USER is set without STROM_ADMIN_PASSWORD_HASH - session login \
                     is impossible, only the API key works. Generate a hash with 'strom \
                     hash-password'."
                );
            } else {
                warn!(
                    "STROM_ADMIN_USER is set without STROM_ADMIN_PASSWORD_HASH and no \
                     STROM_API_KEY is set - authentication is enabled with no way to pass it, so \
                     every request will be rejected. Generate a hash with 'strom hash-password'."
                );
            }
        }

        Self {
            admin_user,
            admin_password_hash,
            api_key,
            native_gui_token: None,
            enabled,
        }
    }

    /// Generate a native GUI token for embedded GUI authentication.
    /// Returns the token that should be passed to the GUI.
    pub fn generate_native_gui_token(&mut self) -> String {
        use uuid::Uuid;
        let token = format!("native-gui-{}", Uuid::new_v4());
        self.native_gui_token = Some(token.clone());
        token
    }

    /// Verify a native GUI token
    pub fn verify_native_gui_token(&self, token: &str) -> bool {
        self.native_gui_token
            .as_ref()
            .map(|t| t == token)
            .unwrap_or(false)
    }

    /// Check if session-based authentication is configured
    pub fn has_session_auth(&self) -> bool {
        self.admin_user.is_some() && self.admin_password_hash.is_some()
    }

    /// Check if API key authentication is configured
    pub fn has_api_key_auth(&self) -> bool {
        self.api_key.is_some()
    }

    /// Verify username and password against configured credentials
    pub fn verify_credentials(&self, username: &str, password: &str) -> bool {
        if !self.has_session_auth() {
            return false;
        }

        let admin_user = self.admin_user.as_ref().unwrap();
        let admin_hash = self.admin_password_hash.as_ref().unwrap();

        // Check username matches
        if username != admin_user {
            return false;
        }

        // Verify password against bcrypt hash
        bcrypt::verify(password, admin_hash).unwrap_or(false)
    }

    /// Verify API key
    pub fn verify_api_key(&self, key: &str) -> bool {
        // An empty presented token never authenticates, whatever is configured.
        // `Authorization: Bearer ` and `?auth_token=` both yield an empty token
        // after the prefix is stripped, so a blank configured key would
        // otherwise match every unauthenticated request.
        if key.is_empty() {
            return false;
        }

        if !self.has_api_key_auth() {
            return false;
        }

        self.api_key.as_ref().map(|k| k == key).unwrap_or(false)
    }
}

/// Authentication middleware that checks session, API key, native GUI token, and query param
pub async fn auth_middleware(
    Extension(config): Extension<Arc<AuthConfig>>,
    session: Session,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // If authentication is disabled, allow all requests
    if !config.enabled {
        return Ok(next.run(request).await);
    }

    // Check session authentication
    if session_is_authenticated(&session).await {
        return Ok(next.run(request).await);
    }

    // Check Bearer token authentication (API key or native GUI token)
    if let Some(auth_header) = request.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                // Check API key
                if config.verify_api_key(token) {
                    return Ok(next.run(request).await);
                }
                // Check native GUI token
                if config.verify_native_gui_token(token) {
                    return Ok(next.run(request).await);
                }
            }
        }
    }

    // Check auth_token query parameter (for WebSocket connections)
    if let Some(query) = request.uri().query() {
        for param in query.split('&') {
            if let Some(raw) = param.strip_prefix("auth_token=") {
                // A client that builds its URL properly percent-encodes the
                // token, and a base64 API key has `+`, `/` and `=` in it.
                // Try the decoded form as well as the raw one, so a key
                // pasted into the URL unencoded keeps working. `+` is left
                // as it is: in a key it is a plus, never a space.
                let decoded = urlencoding::decode(raw).ok();
                let candidates =
                    std::iter::once(raw).chain(decoded.as_deref().filter(|d| *d != raw));
                for token in candidates {
                    if config.verify_api_key(token) || config.verify_native_gui_token(token) {
                        return Ok(next.run(request).await);
                    }
                }
            }
        }
    }

    // No valid authentication found
    Err(StatusCode::UNAUTHORIZED)
}

/// Login handler
#[utoipa::path(
    post,
    path = "/api/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Login attempt result", body = LoginResponse),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn login_handler(
    Extension(config): Extension<Arc<AuthConfig>>,
    session: Session,
    JsonBody(payload): JsonBody<LoginRequest>,
) -> Result<Json<LoginResponse>, StatusCode> {
    if !config.has_session_auth() {
        return Ok(Json(LoginResponse {
            success: false,
            message: "Session authentication not configured".to_string(),
        }));
    }

    if config.verify_credentials(&payload.username, &payload.password) {
        session
            .insert(SESSION_USER_KEY, true)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        Ok(Json(LoginResponse {
            success: true,
            message: "Login successful".to_string(),
        }))
    } else {
        Ok(Json(LoginResponse {
            success: false,
            message: "Invalid username or password".to_string(),
        }))
    }
}

/// Logout handler
#[utoipa::path(
    post,
    path = "/api/logout",
    tag = "auth",
    responses(
        (status = 200, description = "Logout successful", body = LoginResponse),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn logout_handler(session: Session) -> Result<Json<LoginResponse>, StatusCode> {
    session
        .delete()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(LoginResponse {
        success: true,
        message: "Logged out successfully".to_string(),
    }))
}

/// Get authentication status
#[utoipa::path(
    get,
    path = "/api/auth/status",
    tag = "auth",
    responses(
        (status = 200, description = "Current authentication status", body = AuthStatusResponse)
    )
)]
pub async fn auth_status_handler(
    Extension(config): Extension<Arc<AuthConfig>>,
    session: Session,
) -> Json<AuthStatusResponse> {
    let authenticated = if !config.enabled {
        // If auth is disabled, consider everyone authenticated
        true
    } else {
        // Check if authenticated via session
        session_is_authenticated(&session).await
    };

    let mut methods = Vec::new();
    if config.has_session_auth() {
        methods.push("session".to_string());
    }
    if config.has_api_key_auth() {
        methods.push("api_key".to_string());
    }

    Json(AuthStatusResponse {
        authenticated,
        auth_required: config.enabled,
        methods,
    })
}

/// Helper function to generate password hash for setup
/// Usage: echo "password" | strom hash-password
pub fn hash_password(password: &str) -> Result<String, bcrypt::BcryptError> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_password_hashing() {
        let password = "test_password_123";
        let hash = hash_password(password).unwrap();

        // Verify correct password
        assert!(bcrypt::verify(password, &hash).unwrap());

        // Verify incorrect password fails
        assert!(!bcrypt::verify("wrong_password", &hash).unwrap());
    }

    #[test]
    fn test_auth_config_disabled() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: false,
        };

        assert!(!config.has_session_auth());
        assert!(!config.has_api_key_auth());
        assert!(!config.enabled);
    }

    #[test]
    fn test_verify_api_key_valid() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: Some("secret-api-key".to_string()),
            native_gui_token: None,
            enabled: true,
        };

        assert!(config.verify_api_key("secret-api-key"));
    }

    #[test]
    fn test_verify_api_key_invalid() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: Some("secret-api-key".to_string()),
            native_gui_token: None,
            enabled: true,
        };

        assert!(!config.verify_api_key("wrong-key"));
    }

    #[test]
    fn test_verify_api_key_not_configured() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: false,
        };

        assert!(!config.verify_api_key("any-key"));
    }

    /// A blank configured key used to authenticate every request: the middleware
    /// strips `Bearer ` and hands on an empty token, which compared equal.
    #[test]
    fn test_blank_api_key_never_authenticates() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: Some(String::new()),
            native_gui_token: None,
            enabled: true,
        };

        assert!(!config.verify_api_key(""));
        assert!(!config.verify_api_key("anything"));
    }

    #[test]
    #[serial]
    fn test_from_env_ignores_blank_credentials() {
        let restore = [
            ("STROM_ADMIN_USER", std::env::var("STROM_ADMIN_USER").ok()),
            (
                "STROM_ADMIN_PASSWORD_HASH",
                std::env::var("STROM_ADMIN_PASSWORD_HASH").ok(),
            ),
            ("STROM_API_KEY", std::env::var("STROM_API_KEY").ok()),
        ];

        std::env::set_var("STROM_ADMIN_USER", "   ");
        std::env::set_var("STROM_ADMIN_PASSWORD_HASH", "");
        std::env::set_var("STROM_API_KEY", "");

        let config = AuthConfig::from_env();

        for (key, value) in restore {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        assert_eq!(config.admin_user, None);
        assert_eq!(config.admin_password_hash, None);
        assert_eq!(config.api_key, None);
        assert!(
            !config.enabled,
            "blank credentials must not enable authentication"
        );
        assert!(!config.has_api_key_auth());
        assert!(!config.has_session_auth());
    }

    #[test]
    fn test_native_gui_token_generate_and_verify() {
        let mut config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };

        let token = config.generate_native_gui_token();
        assert!(token.starts_with("native-gui-"));
        assert!(config.verify_native_gui_token(&token));
    }

    #[test]
    fn test_native_gui_token_verify_wrong_token() {
        let mut config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };

        let _token = config.generate_native_gui_token();
        assert!(!config.verify_native_gui_token("wrong-token"));
    }

    #[test]
    fn test_native_gui_token_verify_not_generated() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };

        assert!(!config.verify_native_gui_token("any-token"));
    }

    #[test]
    fn test_verify_credentials_valid() {
        let password = "correct_password";
        let hash = hash_password(password).unwrap();

        let config = AuthConfig {
            admin_user: Some("admin".to_string()),
            admin_password_hash: Some(hash),
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };

        assert!(config.verify_credentials("admin", password));
    }

    #[test]
    fn test_verify_credentials_wrong_password() {
        let password = "correct_password";
        let hash = hash_password(password).unwrap();

        let config = AuthConfig {
            admin_user: Some("admin".to_string()),
            admin_password_hash: Some(hash),
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };

        assert!(!config.verify_credentials("admin", "wrong_password"));
    }

    #[test]
    fn test_verify_credentials_wrong_username() {
        let password = "correct_password";
        let hash = hash_password(password).unwrap();

        let config = AuthConfig {
            admin_user: Some("admin".to_string()),
            admin_password_hash: Some(hash),
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };

        assert!(!config.verify_credentials("wrong_user", password));
    }

    #[test]
    fn test_verify_credentials_not_configured() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: false,
        };

        assert!(!config.verify_credentials("admin", "password"));
    }

    #[test]
    fn test_has_session_auth() {
        let hash = hash_password("password").unwrap();

        let config_with_session = AuthConfig {
            admin_user: Some("admin".to_string()),
            admin_password_hash: Some(hash),
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };
        assert!(config_with_session.has_session_auth());

        let config_without_hash = AuthConfig {
            admin_user: Some("admin".to_string()),
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };
        assert!(!config_without_hash.has_session_auth());

        let config_without_user = AuthConfig {
            admin_user: None,
            admin_password_hash: Some("hash".to_string()),
            api_key: None,
            native_gui_token: None,
            enabled: true,
        };
        assert!(!config_without_user.has_session_auth());
    }

    #[test]
    fn test_has_api_key_auth() {
        let config_with_key = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: Some("key".to_string()),
            native_gui_token: None,
            enabled: true,
        };
        assert!(config_with_key.has_api_key_auth());

        let config_without_key = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: false,
        };
        assert!(!config_without_key.has_api_key_auth());
    }
}

/// Tests that drive `auth_middleware` through a router, the way requests
/// actually reach it: session layer and config extension outside, the
/// middleware in front of a protected route.
#[cfg(test)]
mod middleware_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::{get, post},
        Router,
    };
    use tower::ServiceExt;
    use tower_sessions::{MemoryStore, SessionManagerLayer};

    const API_KEY: &str = "test-api-key";
    const NATIVE_TOKEN: &str = "native-gui-00000000-0000-0000-0000-000000000000";
    const ADMIN_USER: &str = "admin";
    const ADMIN_PASSWORD: &str = "correct horse";

    fn enabled_config() -> AuthConfig {
        AuthConfig {
            admin_user: Some(ADMIN_USER.to_string()),
            // Minimum bcrypt cost keeps the login test fast; the verify path is
            // the same one production uses.
            admin_password_hash: Some(bcrypt::hash(ADMIN_PASSWORD, 4).unwrap()),
            api_key: Some(API_KEY.to_string()),
            native_gui_token: Some(NATIVE_TOKEN.to_string()),
            enabled: true,
        }
    }

    fn router(config: AuthConfig) -> Router {
        let protected = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(middleware::from_fn(auth_middleware));
        Router::new()
            .route("/login", post(login_handler))
            .merge(protected)
            .layer(Extension(Arc::new(config)))
            .layer(SessionManagerLayer::new(MemoryStore::default()).with_secure(false))
    }

    async fn status(app: &Router, request: Request<Body>) -> StatusCode {
        app.clone().oneshot(request).await.unwrap().status()
    }

    fn get_req(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn with_auth_header(value: &str) -> Request<Body> {
        Request::builder()
            .uri("/protected")
            .header(header::AUTHORIZATION, value)
            .body(Body::empty())
            .unwrap()
    }

    fn bearer(token: &str) -> Request<Body> {
        with_auth_header(&format!("Bearer {token}"))
    }

    #[tokio::test]
    async fn no_credentials_is_unauthorized() {
        let app = router(enabled_config());
        assert_eq!(
            status(&app, get_req("/protected")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn auth_disabled_lets_everything_through() {
        let app = router(AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: false,
        });
        assert_eq!(status(&app, get_req("/protected")).await, StatusCode::OK);
        assert_eq!(status(&app, bearer("whatever")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn valid_bearer_api_key_is_accepted() {
        let app = router(enabled_config());
        assert_eq!(status(&app, bearer(API_KEY)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn valid_bearer_native_gui_token_is_accepted() {
        let app = router(enabled_config());
        assert_eq!(status(&app, bearer(NATIVE_TOKEN)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_or_malformed_bearer_is_unauthorized() {
        let app = router(enabled_config());
        for value in [
            "Bearer wrong-key".to_string(),
            "Bearer ".to_string(),
            // The right key under the wrong scheme, or with no scheme.
            format!("Basic {API_KEY}"),
            API_KEY.to_string(),
        ] {
            assert_eq!(
                status(&app, with_auth_header(&value)).await,
                StatusCode::UNAUTHORIZED,
                "{value}"
            );
        }
    }

    #[tokio::test]
    async fn valid_query_token_is_accepted() {
        let app = router(enabled_config());
        for uri in [
            format!("/protected?auth_token={API_KEY}"),
            format!("/protected?auth_token={NATIVE_TOKEN}"),
            // Not the first parameter.
            format!("/protected?foo=bar&auth_token={API_KEY}"),
        ] {
            assert_eq!(status(&app, get_req(&uri)).await, StatusCode::OK, "{uri}");
        }
    }

    /// `openssl rand -base64 32`, the documented way to make an API key,
    /// yields `+`, `/` and `=`. A client that builds the query properly
    /// percent-encodes them, and the key must still match. A client that
    /// pastes the key in raw must keep working too.
    #[tokio::test]
    async fn percent_encoded_query_token_is_accepted() {
        let key = "ab+cd/ef==";
        let app = router(AuthConfig {
            api_key: Some(key.to_string()),
            ..enabled_config()
        });
        for uri in [
            "/protected?auth_token=ab%2Bcd%2Fef%3D%3D",
            "/protected?auth_token=ab%2bcd%2fef%3d%3d",
            "/protected?auth_token=ab+cd/ef==",
        ] {
            assert_eq!(status(&app, get_req(uri)).await, StatusCode::OK, "{uri}");
        }
        // Decoding must not turn a wrong token into a right one.
        assert_eq!(
            status(&app, get_req("/protected?auth_token=ab%2Bcd%2Fef%3D")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn invalid_query_token_is_unauthorized() {
        let app = router(enabled_config());
        for uri in [
            "/protected?auth_token=wrong-key".to_string(),
            "/protected?auth_token=".to_string(),
            // The key under another parameter name does not count.
            format!("/protected?token={API_KEY}"),
            format!("/protected?xauth_token={API_KEY}"),
        ] {
            assert_eq!(
                status(&app, get_req(&uri)).await,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn logged_in_session_is_accepted() {
        let app = router(enabled_config());

        let login = |password: &str| {
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "username": ADMIN_USER, "password": password }).to_string(),
                ))
                .unwrap()
        };
        let cookie_of = |response: &Response| {
            response.headers().get(header::SET_COOKIE).map(|value| {
                value
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_string()
            })
        };
        let with_cookie = |cookie: &str| {
            Request::builder()
                .uri("/protected")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap()
        };

        // A failed login yields no authenticated session. An empty session
        // is not stored, so there may be no cookie at all.
        let failed = app.clone().oneshot(login("wrong")).await.unwrap();
        if let Some(cookie) = cookie_of(&failed) {
            assert_eq!(
                status(&app, with_cookie(&cookie)).await,
                StatusCode::UNAUTHORIZED
            );
        }

        let ok = app.clone().oneshot(login(ADMIN_PASSWORD)).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let cookie = cookie_of(&ok).expect("a successful login sets a session cookie");
        assert_eq!(status(&app, with_cookie(&cookie)).await, StatusCode::OK);

        // An unknown session id is not authenticated.
        assert_eq!(
            status(&app, with_cookie(&format!("{cookie}x"))).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The middleware has no exempt paths of its own: exemption is where the
    /// app router applies it. Check that placement on the real router.
    #[tokio::test]
    async fn app_router_exempts_only_public_routes() {
        gstreamer::init().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let state = crate::state::AppState::with_json_storage(
            dir.path().join("flows.json"),
            dir.path().join("blocks.json"),
            dir.path().join("media"),
            vec![],
            "all".to_string(),
            vec![],
            false,
            false,
        );
        let app = crate::create_app_with_state_and_auth(state, enabled_config()).await;

        for uri in ["/health", "/api/auth/status"] {
            assert_eq!(status(&app, get_req(uri)).await, StatusCode::OK, "{uri}");
        }
        for uri in ["/api/flows", "/api/ws", "/swagger-ui/"] {
            assert_eq!(
                status(&app, get_req(uri)).await,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
        }
        let authed = Request::builder()
            .uri("/api/flows")
            .header(header::AUTHORIZATION, format!("Bearer {API_KEY}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(&app, authed).await, StatusCode::OK);
    }
}
