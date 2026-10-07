use crate::json_rejection::JsonBody;
use axum::{
    extract::Request,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use cookie::{Cookie, CookieJar, Key, SameSite};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
pub use strom_types::api::AuthStatusResponse;
pub use strom_types::auth::{LoginRequest, LoginResponse};
use tracing::{info, warn};

/// A login lapses after this long without a request.
const SESSION_IDLE_SECS: u64 = 24 * 60 * 60;
/// A login lapses this long after the password was entered, however active.
/// Without a cap a copied cookie that is used daily would never expire.
const SESSION_MAX_SECS: u64 = 30 * 24 * 60 * 60;
/// A cookie older than this is re-issued, which slides the idle window
/// forward. Re-issuing on every request would add a `Set-Cookie` to each
/// API poll for no gain.
const SESSION_REFRESH_SECS: u64 = 60 * 60;
/// File in the data directory holding the generated signing key.
const SESSION_KEY_FILE: &str = "session.key";
/// Shortest `STROM_SESSION_SECRET` accepted.
const MIN_SESSION_SECRET_LEN: usize = 32;

/// Key that signs the login cookie.
///
/// The cookie carries the login itself, so the server keeps no session
/// state, and a login stays valid across a restart as long as the key does.
/// Anyone holding the key can mint a login, so it must be secret and must
/// differ between installations.
#[derive(Clone)]
pub struct SessionKey(Key);

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(..)")
    }
}

impl SessionKey {
    /// A key that lives only as long as the process.
    pub fn random() -> Self {
        Self(Key::generate())
    }

    /// Derive the key from an operator-supplied secret.
    pub fn from_secret(secret: &str) -> anyhow::Result<Self> {
        if secret.len() < MIN_SESSION_SECRET_LEN {
            anyhow::bail!(
                "STROM_SESSION_SECRET must be at least {MIN_SESSION_SECRET_LEN} bytes, got {}. \
                 Generate one with 'openssl rand -base64 32'.",
                secret.len()
            );
        }
        Ok(Self(Key::derive_from(secret.as_bytes())))
    }

    /// The key to sign logins with: `STROM_SESSION_SECRET` when set,
    /// otherwise a key kept in `data_dir`, created on first start.
    ///
    /// If the key file cannot be written the key is kept in memory only, so
    /// logins work but do not survive a restart.
    pub fn load(data_dir: &Path) -> anyhow::Result<Self> {
        if let Some(secret) = strom_types::env::var_opt("STROM_SESSION_SECRET") {
            info!("Session cookies signed with STROM_SESSION_SECRET");
            return Self::from_secret(&secret);
        }

        let path = data_dir.join(SESSION_KEY_FILE);
        if let Some(key) = Self::read_file(&path)? {
            info!("Session cookies signed with the key in {}", path.display());
            return Ok(key);
        }

        let key = Self::random();
        match create_private(&path, &key.encoded()) {
            Ok(()) => {
                info!("Generated session key in {}", path.display());
                Ok(key)
            }
            // Another process created it first. Use its key, so both agree.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Self::read_file(&path)?
                .ok_or_else(|| anyhow::anyhow!("{} vanished while starting", path.display())),
            Err(e) => {
                warn!(
                    "Could not write session key to {}: {e}. Logins will not survive a \
                     restart; set STROM_SESSION_SECRET to keep them.",
                    path.display()
                );
                Ok(key)
            }
        }
    }

    /// The key in `path`, or `None` if there is no such file.
    fn read_file(path: &Path) -> anyhow::Result<Option<Self>> {
        use base64::Engine;
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => anyhow::bail!("Could not read session key {}: {e}", path.display()),
        };
        let key = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .ok()
            .and_then(|bytes| Key::try_from(bytes.as_slice()).ok());
        match key {
            Some(key) => Ok(Some(Self(key))),
            None => anyhow::bail!(
                "{} is not a valid session key. Delete it to generate a new one \
                 (everyone will have to log in again).",
                path.display()
            ),
        }
    }

    fn encoded(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.0.master())
    }
}

/// Create `path` holding `contents`, readable only by the owner. Fails with
/// `AlreadyExists` if the file is there.
///
/// The contents go to a temporary file first and are linked into place
/// whole, so a crash mid-write cannot leave a truncated key behind.
fn create_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut file| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    });
    let linked = written.and_then(|()| std::fs::hard_link(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    linked
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The signed login cookie.
///
/// Its value is when the password was entered and when the cookie was last
/// issued, signed with a key derived from the [`SessionKey`] and the admin
/// credentials. Changing the username or password hash therefore logs
/// everyone out, as does changing the key.
///
/// Logout clears the cookie in the browser; it cannot revoke a copy taken
/// elsewhere.
#[derive(Clone)]
pub struct SessionCookie {
    name: String,
    key: Key,
    secure: bool,
}

/// The two times a login cookie carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Login {
    /// When the password was entered.
    logged_in: u64,
    /// When this cookie was issued.
    issued: u64,
}

impl SessionCookie {
    /// The cookie name includes the port so multiple instances on the same
    /// host don't collide.
    pub fn new(port: u16, config: &AuthConfig) -> Self {
        // The master key is 64 bytes, so the input always meets
        // `derive_from`'s 32-byte minimum.
        let mut material = config.session_key.0.master().to_vec();
        for part in [&config.admin_user, &config.admin_password_hash] {
            material.push(0);
            material.extend_from_slice(part.as_deref().unwrap_or_default().as_bytes());
        }
        Self {
            name: format!("strom_session_{port}"),
            key: Key::derive_from(&material),
            secure: config.session_cookie_secure,
        }
    }

    /// The login in `headers`, if it is validly signed and unexpired.
    fn login_in(&self, headers: &HeaderMap) -> Option<Login> {
        let now = unix_now();
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(Cookie::split_parse)
            .filter_map(Result::ok)
            .filter(|c| c.name() == self.name)
            .find_map(|c| {
                let mut jar = CookieJar::new();
                jar.add_original(c.into_owned());
                let verified = jar.signed(&self.key).get(&self.name)?;
                let (logged_in, issued) = verified.value().split_once('.')?;
                let login = Login {
                    logged_in: logged_in.parse().ok()?,
                    issued: issued.parse().ok()?,
                };
                // Times in the future are not trusted to be fresh.
                let valid = login.logged_in <= login.issued
                    && login.issued <= now
                    && now - login.issued < SESSION_IDLE_SECS
                    && now - login.logged_in < SESSION_MAX_SECS;
                valid.then_some(login)
            })
    }

    /// Whether `headers` carry a valid login cookie.
    pub fn is_authenticated(&self, headers: &HeaderMap) -> bool {
        self.login_in(headers).is_some()
    }

    fn build(&self, value: String, max_age: u64) -> Cookie<'static> {
        Cookie::build((self.name.clone(), value))
            .path("/")
            .http_only(true)
            .secure(self.secure)
            .same_site(SameSite::Strict)
            .max_age(cookie::time::Duration::seconds(max_age as i64))
            .build()
    }

    /// A `Set-Cookie` value for a login made at `logged_in`, issued at `now`.
    fn issue(&self, logged_in: u64, now: u64) -> HeaderValue {
        let remaining = SESSION_MAX_SECS.saturating_sub(now.saturating_sub(logged_in));
        let mut jar = CookieJar::new();
        jar.signed_mut(&self.key).add(self.build(
            format!("{logged_in}.{now}"),
            SESSION_IDLE_SECS.min(remaining),
        ));
        let cookie = jar.get(&self.name).expect("cookie was just added");
        HeaderValue::from_str(&cookie.to_string()).expect("cookie is a valid header value")
    }

    /// A `Set-Cookie` value for a password login made now.
    fn login(&self) -> HeaderValue {
        let now = unix_now();
        self.issue(now, now)
    }

    /// A `Set-Cookie` value removing the login from the browser.
    fn logout(&self) -> HeaderValue {
        HeaderValue::from_str(&self.build(String::new(), 0).to_string())
            .expect("cookie is a valid header value")
    }
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
    /// Key that signs the login cookie
    pub session_key: SessionKey,
    /// Whether the login cookie is marked `Secure`. Set when Strom serves
    /// TLS itself, where the browser only ever reaches it over HTTPS.
    pub session_cookie_secure: bool,
}

impl AuthConfig {
    /// Whether any authentication method is configured, without building the
    /// configuration or warning about anything.
    ///
    /// The remote control interlock has to know this before the real
    /// configuration is built, and [`AuthConfig::from_env`] warns as it goes,
    /// so the rule for "authentication is on" lives here and both use it.
    pub fn is_configured_in_env() -> bool {
        strom_types::env::var_opt("STROM_ADMIN_USER").is_some()
            || strom_types::env::var_opt("STROM_API_KEY").is_some()
    }

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
        let enabled = Self::is_configured_in_env();

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
            // Replaced by `SessionKey::load` at startup; this one does not
            // survive a restart.
            session_key: SessionKey::random(),
            session_cookie_secure: false,
        }
    }

    /// [`Self::from_env`], with the login cookie key from [`SessionKey::load`]
    /// when login is configured. `tls` is whether Strom serves HTTPS itself.
    pub fn load(data_dir: &Path, tls: bool) -> anyhow::Result<Self> {
        let mut config = Self::from_env();
        config.session_cookie_secure = tls;
        if config.has_session_auth() {
            config.session_key = SessionKey::load(data_dir)?;
        }
        Ok(config)
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
    Extension(session): Extension<Arc<SessionCookie>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // If authentication is disabled, allow all requests
    if !config.enabled {
        return Ok(next.run(request).await);
    }

    // Check the login cookie, and slide its idle window forward
    if let Some(login) = session.login_in(request.headers()) {
        let mut response = next.run(request).await;
        let now = unix_now();
        if now - login.issued >= SESSION_REFRESH_SECS {
            response
                .headers_mut()
                .append(header::SET_COOKIE, session.issue(login.logged_in, now));
        }
        return Ok(response);
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
        (status = 200, description = "Login attempt result", body = LoginResponse)
    )
)]
pub async fn login_handler(
    Extension(config): Extension<Arc<AuthConfig>>,
    Extension(session): Extension<Arc<SessionCookie>>,
    JsonBody(payload): JsonBody<LoginRequest>,
) -> Response {
    if !config.has_session_auth() {
        return Json(LoginResponse {
            success: false,
            message: "Session authentication not configured".to_string(),
        })
        .into_response();
    }

    if config.verify_credentials(&payload.username, &payload.password) {
        (
            [(header::SET_COOKIE, session.login())],
            Json(LoginResponse {
                success: true,
                message: "Login successful".to_string(),
            }),
        )
            .into_response()
    } else {
        Json(LoginResponse {
            success: false,
            message: "Invalid username or password".to_string(),
        })
        .into_response()
    }
}

/// Logout handler
#[utoipa::path(
    post,
    path = "/api/logout",
    tag = "auth",
    responses(
        (status = 200, description = "Logout successful", body = LoginResponse)
    )
)]
pub async fn logout_handler(Extension(session): Extension<Arc<SessionCookie>>) -> Response {
    (
        [(header::SET_COOKIE, session.logout())],
        Json(LoginResponse {
            success: true,
            message: "Logged out successfully".to_string(),
        }),
    )
        .into_response()
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
    Extension(session): Extension<Arc<SessionCookie>>,
    headers: HeaderMap,
) -> Json<AuthStatusResponse> {
    let authenticated = if !config.enabled {
        // If auth is disabled, consider everyone authenticated
        true
    } else {
        // Check if authenticated via the login cookie
        session.is_authenticated(&headers)
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
        };
        assert!(config_with_session.has_session_auth());

        let config_without_hash = AuthConfig {
            admin_user: Some("admin".to_string()),
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: true,
            session_key: SessionKey::random(),
            session_cookie_secure: false,
        };
        assert!(!config_without_hash.has_session_auth());

        let config_without_user = AuthConfig {
            admin_user: None,
            admin_password_hash: Some("hash".to_string()),
            api_key: None,
            native_gui_token: None,
            enabled: true,
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
        };
        assert!(config_with_key.has_api_key_auth());

        let config_without_key = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: false,
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
        }
    }

    fn router(config: AuthConfig) -> Router {
        let session = Arc::new(SessionCookie::new(0, &config));
        let protected = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(middleware::from_fn(auth_middleware));
        Router::new()
            .route("/login", post(login_handler))
            .route("/logout", post(logout_handler))
            .merge(protected)
            .layer(Extension(Arc::new(config)))
            .layer(Extension(session))
    }

    fn login_req(password: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "username": ADMIN_USER, "password": password }).to_string(),
            ))
            .unwrap()
    }

    /// The `name=value` part of the response's `Set-Cookie`, if any.
    fn cookie_of(response: &Response) -> Option<String> {
        response.headers().get(header::SET_COOKIE).map(|value| {
            value
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_string()
        })
    }

    fn with_cookie(cookie: &str) -> Request<Body> {
        Request::builder()
            .uri("/protected")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    async fn login_cookie(app: &Router) -> String {
        let ok = app
            .clone()
            .oneshot(login_req(ADMIN_PASSWORD))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        cookie_of(&ok).expect("a successful login sets a session cookie")
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
            session_key: SessionKey::random(),
            session_cookie_secure: false,
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

        // A failed login sets no cookie.
        let failed = app.clone().oneshot(login_req("wrong")).await.unwrap();
        assert_eq!(cookie_of(&failed), None);

        let cookie = login_cookie(&app).await;
        assert_eq!(status(&app, with_cookie(&cookie)).await, StatusCode::OK);

        // A tampered cookie is not authenticated.
        assert_eq!(
            status(&app, with_cookie(&format!("{cookie}0"))).await,
            StatusCode::UNAUTHORIZED
        );
        // Nor is the bare issue time without its signature.
        assert_eq!(
            status(
                &app,
                with_cookie(&format!("strom_session_0={}", unix_now()))
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The point of signing the cookie: a new process with the same key
    /// accepts a login made before the restart, and one with another key
    /// does not.
    #[tokio::test]
    async fn login_survives_restart_with_same_key() {
        let config = enabled_config();
        let cookie = login_cookie(&router(config.clone())).await;

        let restarted = router(config.clone());
        assert_eq!(
            status(&restarted, with_cookie(&cookie)).await,
            StatusCode::OK
        );

        let other_key = router(AuthConfig {
            session_key: SessionKey::random(),
            session_cookie_secure: false,
            ..config
        });
        assert_eq!(
            status(&other_key, with_cookie(&cookie)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The `name=value` part of a `Set-Cookie` value.
    fn pair(value: &HeaderValue) -> String {
        value
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    fn cookie_headers(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, cookie.parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn idle_login_expires_and_active_login_is_refreshed() {
        let config = enabled_config();
        let session = SessionCookie::new(0, &config);
        let app = router(config);
        let now = unix_now();
        // A cookie for a login made `logged_in_ago`, issued `issued_ago`.
        let cookie = |logged_in_ago: u64, issued_ago: u64| {
            pair(&session.issue(now - logged_in_ago, now - issued_ago))
        };

        let idle = cookie(SESSION_IDLE_SECS + 1, SESSION_IDLE_SECS + 1);
        assert_eq!(
            status(&app, with_cookie(&idle)).await,
            StatusCode::UNAUTHORIZED
        );

        // A fresh cookie is accepted without being re-issued.
        let fresh = app
            .clone()
            .oneshot(with_cookie(&cookie(0, 0)))
            .await
            .unwrap();
        assert_eq!(fresh.status(), StatusCode::OK);
        assert_eq!(cookie_of(&fresh), None);

        // An older one is re-issued. The new cookie is a valid login that
        // keeps the original login time.
        let logged_in_ago = 3 * SESSION_IDLE_SECS;
        let old = app
            .clone()
            .oneshot(with_cookie(&cookie(
                logged_in_ago,
                SESSION_REFRESH_SECS + 1,
            )))
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::OK);
        let refreshed = cookie_of(&old).expect("an old cookie is re-issued");
        let login = session.login_in(&cookie_headers(&refreshed)).unwrap();
        assert_eq!(login.logged_in, now - logged_in_ago);
        assert!(login.issued >= now);
    }

    /// Activity does not extend a login past its maximum age, so a copied
    /// cookie cannot be kept alive forever by using it.
    #[tokio::test]
    async fn active_login_still_ends_at_max_age() {
        let config = enabled_config();
        let session = SessionCookie::new(0, &config);
        let app = router(config);
        let now = unix_now();

        let near_end = pair(&session.issue(now - (SESSION_MAX_SECS - 60), now));
        assert_eq!(status(&app, with_cookie(&near_end)).await, StatusCode::OK);
        // The cookie itself says when it ends.
        assert!(session
            .issue(now - (SESSION_MAX_SECS - 60), now)
            .to_str()
            .unwrap()
            .contains("Max-Age=60"));

        let past_end = pair(&session.issue(now - SESSION_MAX_SECS - 1, now));
        assert_eq!(
            status(&app, with_cookie(&past_end)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Changing the password, for instance because it leaked, must end every
    /// login made with the old one.
    #[tokio::test]
    async fn changing_credentials_ends_existing_logins() {
        let config = enabled_config();
        let cookie = login_cookie(&router(config.clone())).await;

        let new_password = router(AuthConfig {
            admin_password_hash: Some(bcrypt::hash("another password", 4).unwrap()),
            ..config.clone()
        });
        assert_eq!(
            status(&new_password, with_cookie(&cookie)).await,
            StatusCode::UNAUTHORIZED
        );

        let new_user = router(AuthConfig {
            admin_user: Some("someone-else".to_string()),
            ..config
        });
        assert_eq!(
            status(&new_user, with_cookie(&cookie)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn cookie_is_secure_only_when_serving_tls() {
        for secure in [false, true] {
            let app = router(AuthConfig {
                session_cookie_secure: secure,
                ..enabled_config()
            });
            let ok = app
                .clone()
                .oneshot(login_req(ADMIN_PASSWORD))
                .await
                .unwrap();
            let set_cookie = ok.headers()[header::SET_COOKIE].to_str().unwrap();
            assert_eq!(
                set_cookie.split("; ").any(|attr| attr == "Secure"),
                secure,
                "{set_cookie}"
            );
        }
    }

    #[tokio::test]
    async fn logout_clears_the_cookie() {
        let app = router(enabled_config());
        let logout = Request::builder()
            .method("POST")
            .uri("/logout")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(logout).await.unwrap();
        let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.starts_with("strom_session_0=;"), "{set_cookie}");
        assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");
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

#[cfg(test)]
mod session_key_tests {
    use super::*;
    use serial_test::serial;

    /// Run `f` with `STROM_SESSION_SECRET` set to `value`, then restore it.
    fn with_secret<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let saved = std::env::var("STROM_SESSION_SECRET").ok();
        match value {
            Some(v) => std::env::set_var("STROM_SESSION_SECRET", v),
            None => std::env::remove_var("STROM_SESSION_SECRET"),
        }
        let result = f();
        match saved {
            Some(v) => std::env::set_var("STROM_SESSION_SECRET", v),
            None => std::env::remove_var("STROM_SESSION_SECRET"),
        }
        result
    }

    fn cookie_for(key: &SessionKey) -> SessionCookie {
        SessionCookie::new(
            0,
            &AuthConfig {
                admin_user: Some("admin".to_string()),
                admin_password_hash: Some("hash".to_string()),
                api_key: None,
                native_gui_token: None,
                enabled: true,
                session_key: key.clone(),
                session_cookie_secure: false,
            },
        )
    }

    /// Whether a login signed with `a` is accepted under `b`.
    fn same_key(a: &SessionKey, b: &SessionKey) -> bool {
        let login = cookie_for(a).login();
        let mut headers = HeaderMap::new();
        let pair = login.to_str().unwrap().split(';').next().unwrap();
        headers.insert(header::COOKIE, pair.parse().unwrap());
        cookie_for(b).is_authenticated(&headers)
    }

    #[test]
    #[serial]
    fn generated_key_is_kept_in_the_data_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let (first, second) = with_secret(None, || {
            (
                SessionKey::load(dir.path()).unwrap(),
                SessionKey::load(dir.path()).unwrap(),
            )
        });
        assert!(same_key(&first, &second));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(SESSION_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Another data dir gets another key.
        let other = tempfile::TempDir::new().unwrap();
        let third = with_secret(None, || SessionKey::load(other.path()).unwrap());
        assert!(!same_key(&first, &third));
    }

    #[test]
    #[serial]
    fn corrupt_key_file_is_an_error_not_a_silent_replacement() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(SESSION_KEY_FILE);
        std::fs::write(&path, "not base64 at all").unwrap();
        assert!(with_secret(None, || SessionKey::load(dir.path())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not base64 at all");
    }

    #[test]
    #[serial]
    fn secret_from_env_wins_and_is_stable() {
        let dir = tempfile::TempDir::new().unwrap();
        let secret = "0123456789abcdef0123456789abcdef";
        let (a, b) = with_secret(Some(secret), || {
            (
                SessionKey::load(dir.path()).unwrap(),
                SessionKey::load(dir.path()).unwrap(),
            )
        });
        assert!(same_key(&a, &b));
        // No key file is written when the secret comes from the environment.
        assert!(!dir.path().join(SESSION_KEY_FILE).exists());
    }

    #[test]
    #[serial]
    fn short_secret_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(with_secret(Some("too-short"), || SessionKey::load(dir.path())).is_err());
    }

    /// A second process starting at the same time must not overwrite the
    /// first one's key, and a failed attempt leaves no temporary file.
    #[test]
    fn key_file_is_never_replaced() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(SESSION_KEY_FILE);
        create_private(&path, "first").unwrap();
        let err = create_private(&path, "second").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [SESSION_KEY_FILE]);
    }
}
