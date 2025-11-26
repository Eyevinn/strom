use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_sessions::Session;

const SESSION_USER_KEY: &str = "user_authenticated";

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
        let admin_user = std::env::var("STROM_ADMIN_USER").ok();
        let admin_password_hash = std::env::var("STROM_ADMIN_PASSWORD_HASH").ok();
        let api_key = std::env::var("STROM_API_KEY").ok();

        // Authentication is enabled if any method is configured
        let enabled = admin_user.is_some() || api_key.is_some();

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
        if !self.has_api_key_auth() {
            return false;
        }

        self.api_key.as_ref().map(|k| k == key).unwrap_or(false)
    }
}

/// Login request payload
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub success: bool,
    pub message: String,
}

/// Authentication status response
#[derive(Debug, Serialize)]
pub struct AuthStatusResponse {
    pub authenticated: bool,
    pub auth_required: bool,
    pub methods: Vec<String>,
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
    if let Ok(Some(true)) = session.get::<bool>(SESSION_USER_KEY).await {
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
            if let Some(token) = param.strip_prefix("auth_token=") {
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

    // No valid authentication found
    Err(StatusCode::UNAUTHORIZED)
}

/// Login handler
pub async fn login_handler(
    Extension(config): Extension<Arc<AuthConfig>>,
    session: Session,
    Json(payload): Json<LoginRequest>,
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
pub async fn auth_status_handler(
    Extension(config): Extension<Arc<AuthConfig>>,
    session: Session,
) -> Json<AuthStatusResponse> {
    let authenticated = if !config.enabled {
        // If auth is disabled, consider everyone authenticated
        true
    } else {
        // Check if authenticated via session
        session
            .get::<bool>(SESSION_USER_KEY)
            .await
            .ok()
            .flatten()
            .unwrap_or(false)
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
}
