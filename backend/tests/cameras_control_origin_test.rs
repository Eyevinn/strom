//! The camera controller WebSocket refuses a browser upgrade from another
//! origin. Browsers make no CORS check for WebSockets, and the session cookie
//! still goes to another port of the same host, so without the check a page
//! there could drive the cameras through the operator's browser.

use std::net::SocketAddr;

use strom::auth::AuthConfig;
use strom::create_app_with_state_and_auth;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Error;

async fn serve(dir: &TempDir) -> SocketAddr {
    gstreamer::init().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(dir.path().join("flows.json")),
        dir.path().join("blocks.json"),
        dir.path(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    // Unauthenticated, so the origin check is what answers.
    let auth = AuthConfig {
        admin_user: None,
        admin_password_hash: None,
        api_key: None,
        native_gui_token: None,
        enabled: false,
        session_key: strom::auth::SessionKey::random(),
        session_cookie_secure: false,
    };
    let app = create_app_with_state_and_auth(state, auth).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// The upgrade's HTTP status, or 101 when it was accepted.
async fn upgrade(addr: SocketAddr, origin: Option<String>) -> u16 {
    let mut request = format!("ws://{addr}/api/cameras/control")
        .into_client_request()
        .unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((_socket, response)) => response.status().as_u16(),
        Err(Error::Http(response)) => response.status().as_u16(),
        Err(e) => panic!("upgrade failed: {e}"),
    }
}

#[tokio::test]
async fn controller_upgrade_from_another_origin_is_refused() {
    let dir = TempDir::new().unwrap();
    let addr = serve(&dir).await;
    let other_port = addr.port().wrapping_add(1).max(1);

    assert_eq!(
        upgrade(addr, Some(format!("http://127.0.0.1:{other_port}"))).await,
        403,
        "another port of the same host"
    );
    assert_eq!(
        upgrade(addr, Some("https://evil.example.com".into())).await,
        403,
        "another site"
    );
    assert_eq!(
        upgrade(addr, Some(format!("http://{addr}"))).await,
        101,
        "the page Strom serves"
    );
    assert_eq!(upgrade(addr, None).await, 101, "not a browser");
}
