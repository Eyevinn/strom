use super::*;

fn state() -> DevToolsState {
    DevToolsState::new(DevToolsConfig {
        debug_port: Some(9222),
        tls: false,
        full_devtools: false,
    })
}

fn link_source(url: &str) -> LinkSource {
    LinkSource {
        flow_id: FlowId::new_v4(),
        block_id: "html".to_string(),
        home_url: url.to_string(),
        flow_name: "Flow".to_string(),
        block_name: "Scoreboard".to_string(),
        strict: true,
    }
}

#[test]
fn target_ids_are_hex_only() {
    assert!(valid_target_id("4A8CD2F2840F8591A6277A7FFFDAB3A9"));
    assert!(valid_target_id("cf033111-6dfb-4027-909c-a39d9ba6f825"));
    assert!(!valid_target_id(""));
    assert!(!valid_target_id("../../json/version"));
    assert!(!valid_target_id("page/ABC?x=1"));
    assert!(!valid_target_id(&"a".repeat(65)));
}

#[test]
fn asset_paths_stay_inside_the_application() {
    assert!(safe_asset_path("inspector.html"));
    assert!(safe_asset_path("entrypoints/inspector/inspector.js"));
    assert!(safe_asset_path("core/common/common.js"));
    assert!(!safe_asset_path("../json/version"));
    assert!(!safe_asset_path("/etc/passwd"));
    assert!(!safe_asset_path("a/../../b"));
    assert!(!safe_asset_path(""));
}

#[test]
fn forwarded_proto_outranks_our_own_socket() {
    let plain = DevToolsConfig {
        debug_port: Some(9222),
        tls: false,
        full_devtools: false,
    };
    let mut headers = HeaderMap::new();
    assert!(!client_is_secure(&headers, &plain));

    headers.insert("x-forwarded-proto", "https".parse().unwrap());
    assert!(client_is_secure(&headers, &plain));

    // A proxy chain lists the client's protocol first.
    headers.insert("x-forwarded-proto", "https, http".parse().unwrap());
    assert!(client_is_secure(&headers, &plain));

    headers.insert("x-forwarded-proto", "http".parse().unwrap());
    let tls = DevToolsConfig {
        debug_port: Some(9222),
        tls: true,
        full_devtools: false,
    };
    assert!(!client_is_secure(&headers, &tls));
}

#[test]
fn a_key_is_the_link_and_spells_out_nothing() {
    let s = state();
    let minted = s.mint(link_source("https://example.com/"));
    // The key is what a client sees, so it must not spell out the source.
    assert!(valid_key(&minted.key));
    assert!(!minted.key.to_lowercase().contains("scoreboard"));
    // The id is a name for the link, not a second copy of the credential.
    assert!(valid_link_id(&minted.id));
    assert_ne!(minted.id, minted.key);
    assert!(!minted.key.contains(&minted.id));
    assert!(s.touch(&minted.key));
}

#[test]
fn keys_are_not_guessable_from_each_other() {
    let s = state();
    let a = s.mint(link_source("https://a.example"));
    let b = s.mint(link_source("https://a.example"));
    assert_ne!(a.key, b.key);
    assert_ne!(a.id, b.id);
}

#[test]
fn an_unknown_key_resolves_to_nothing() {
    let s = state();
    assert!(!s.touch(&"f".repeat(64)));
    assert!(!valid_key("short"));
    assert!(!valid_key(&"z".repeat(64)));
}

#[test]
fn a_revoked_key_stops_working() {
    let s = state();
    let minted = s.mint(link_source("https://a.example"));
    assert!(s.revoke_by_id(&minted.id));
    assert!(!s.touch(&minted.key));
    // Revoking twice is not an error the caller can act on differently.
    assert!(!s.revoke_by_id(&minted.id));
}

#[test]
fn an_expired_key_is_gone_even_before_anyone_asks() {
    let s = state();
    let minted = s.mint(link_source("https://a.example"));
    {
        let mut links = s.links.lock().unwrap();
        links.get_mut(&minted.key).unwrap().expires = Instant::now() - Duration::from_secs(1);
    }
    assert!(!s.touch(&minted.key));
    assert!(s.links.lock().unwrap().is_empty());
}

#[tokio::test]
async fn working_in_a_session_keeps_its_link_alive() {
    let s = state();
    let minted = s.mint(link_source("https://a.example"));
    let session = s.open_session(&minted.key).expect("session opens");
    let expire_soon = || {
        s.links
            .lock()
            .unwrap()
            .get_mut(&minted.key)
            .unwrap()
            .expires = Instant::now() + Duration::from_millis(20);
    };

    // Input from the operator starts the time over.
    expire_soon();
    session.hold.used();
    let left = s.links.lock().unwrap()[&minted.key]
        .expires
        .saturating_duration_since(Instant::now());
    assert!(left > LINK_TTL - Duration::from_secs(5));

    // Left alone, the session is ended by the expiry without anything
    // else touching the table.
    expire_soon();
    let mut cancelled = session.cancelled;
    tokio::time::sleep(Duration::from_millis(30)).await;
    session.hold.idle();
    let _ = tokio::time::timeout(Duration::from_secs(1), cancelled.recv())
        .await
        .expect("an idle session must be ended by its link's expiry");
}

#[test]
fn using_a_key_pushes_its_expiry_out() {
    let s = state();
    let minted = s.mint(link_source("https://a.example"));
    let first = s.links.lock().unwrap().get(&minted.key).unwrap().expires;
    std::thread::sleep(Duration::from_millis(5));
    assert!(s.touch(&minted.key));
    let second = s.links.lock().unwrap().get(&minted.key).unwrap().expires;
    assert!(second > first, "a session in use must not expire under it");
}

// --- Revocation has to reach sessions that are already open ---

#[tokio::test]
async fn revoking_a_link_ends_the_session_already_open_on_it() {
    // Without this, revocation only closes the door to *new* sessions and
    // whoever already holds the socket keeps control of the browser.
    let s = state();
    let minted = s.mint(link_source("https://a.example"));
    let mut cancelled = s
        .open_session(&minted.key)
        .expect("session opens")
        .cancelled;

    assert!(s.revoke_by_id(&minted.id));

    // The signal arrives; a session selecting on it stops.
    let _ = tokio::time::timeout(Duration::from_secs(1), cancelled.recv())
        .await
        .expect("the live session must be told within the second");
}

fn app_state() -> AppState {
    let storage_file = tempfile::NamedTempFile::new().unwrap();
    let blocks_file = tempfile::NamedTempFile::new().unwrap();
    AppState::new(
        crate::storage::JsonFileStorage::new(storage_file.path()),
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    )
}

fn html_flow() -> strom_types::Flow {
    let mut flow = strom_types::Flow::new("remote-control");
    flow.blocks.push(
        serde_json::from_value(serde_json::json!({
            "id": "html1",
            "block_definition_id": crate::blocks::builtin::html_input::BLOCK_ID,
            "properties": { "url": "https://a.example", "remote_control": true },
            "position": { "x": 0.0, "y": 0.0 }
        }))
        .expect("a valid block"),
    );
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_remote_control_off_ends_the_sessions_already_open() {
    // The switch is read when a link is minted. Without the watch, an
    // operator switching it off would stop only new links, and the one
    // already handed out would keep control of the page on air.
    gstreamer::init().unwrap();
    let app = app_state();
    let flow = html_flow();
    let flow_id = flow.id;
    app.upsert_flow(flow).await.expect("upsert_flow");

    let s = state();
    s.watch_flows(app.clone());
    let mut source = link_source("https://a.example");
    source.flow_id = flow_id;
    source.block_id = "html1".to_string();
    let minted = s.mint(source);
    let mut cancelled = s
        .open_session(&minted.key)
        .expect("session opens")
        .cancelled;
    assert_eq!(s.revoke_disallowed(&app).await, 0, "it is still allowed");

    let (_, rejected) = app
        .update_block_properties(
            &flow_id,
            "html1",
            HashMap::from([(
                crate::blocks::builtin::html_input::REMOTE_CONTROL_PROPERTY.to_string(),
                strom_types::PropertyValue::Bool(false),
            )]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");
    assert!(rejected.is_empty(), "{:?}", rejected);

    let _ = tokio::time::timeout(Duration::from_secs(1), cancelled.recv())
        .await
        .expect("the open session must be told within the second");
    assert!(
        !s.touch(&minted.key),
        "the key must not open a new session either"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_whose_block_is_gone_is_revoked() {
    gstreamer::init().unwrap();
    let app = app_state();
    let s = state();
    let minted = s.mint(link_source("https://a.example"));
    assert_eq!(s.revoke_disallowed(&app).await, 1);
    assert!(!s.touch(&minted.key));
}

#[tokio::test]
async fn revoking_everything_ends_every_open_session() {
    let s = state();
    let a = s.mint(link_source("https://a.example"));
    let b = s.mint(link_source("https://b.example"));
    let mut first = s.open_session(&a.key).expect("session opens").cancelled;
    let mut second = s.open_session(&b.key).expect("session opens").cancelled;

    assert_eq!(s.revoke_all(), 2);

    let _ = tokio::time::timeout(Duration::from_secs(1), first.recv())
        .await
        .expect("first session told");
    let _ = tokio::time::timeout(Duration::from_secs(1), second.recv())
        .await
        .expect("second session told");
    assert!(s.list().is_empty());
}

#[tokio::test]
async fn an_expiring_link_ends_the_session_on_it() {
    // The TTL has to bite on a socket that is already open, the same way
    // revocation does - otherwise an established session never expires.
    let s = state();
    let minted = s.mint(link_source("https://a.example"));
    let mut cancelled = s
        .open_session(&minted.key)
        .expect("session opens")
        .cancelled;
    {
        let mut links = s.links.lock().unwrap();
        links.get_mut(&minted.key).unwrap().expires = Instant::now() - Duration::from_secs(1);
    }
    // Any call that touches the table sweeps it.
    assert!(s.list().is_empty());
    let _ = tokio::time::timeout(Duration::from_secs(1), cancelled.recv())
        .await
        .expect("the expired session must be told");
}

#[test]
fn a_listing_names_links_without_handing_the_key_back() {
    let s = state();
    let minted = s.mint(link_source("https://a.example/login"));
    let listed = s.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, minted.id);
    // Named by the source it controls, which is what the operator knows.
    assert_eq!(listed[0].block_name, "Scoreboard");
    assert_eq!(listed[0].flow_name, "Flow");
    assert!(listed[0].expires_in_seconds > 0);
    // Whatever else a listing carries, it is not the credential.
    let rendered = serde_json::to_string(&listed[0]).unwrap();
    assert!(
        !rendered.contains(&minted.key),
        "a listing must never carry the key"
    );
}

// --- Keys must not reach the log ---

#[test]
fn a_key_in_a_path_is_redacted_before_it_is_logged() {
    let key = "a".repeat(64);
    let path = format!("/devtools/{}/ui/core/common/common.js", key);
    let redacted = redact_path(&path);
    assert!(!redacted.contains(&key), "got {}", redacted);
    assert_eq!(redacted, "/devtools/<redacted>/ui/core/common/common.js");

    // The websocket path carries it too.
    assert_eq!(
        redact_path(&format!("/devtools/{}/ws", key)),
        "/devtools/<redacted>/ws"
    );
}

#[test]
fn a_path_without_a_key_is_left_exactly_as_it_was() {
    for path in [
        "/api/flows",
        "/devtools/targets",
        // A link id is not a credential and an operator needs to see it.
        "/api/devtools/links/0123456789abcdef0123456789abcdef",
        "/",
    ] {
        assert_eq!(redact_path(path), path, "path {} was rewritten", path);
    }
}

// --- The host that ends up in the redirect ---

#[test]
fn only_a_bare_host_and_port_are_accepted() {
    assert!(valid_host("example.com"));
    assert!(valid_host("example.com:8080"));
    assert!(valid_host("192.0.2.10:8080"));
    assert!(valid_host("[2001:db8::1]"));
    assert!(valid_host("[2001:db8::1]:8080"));
    assert!(valid_host("localhost"));
}

#[test]
fn a_host_carrying_query_syntax_is_refused() {
    // These land inside the inspector URL's query string, where an `&`
    // would append parameters of the sender's choosing and could point the
    // DevTools websocket at a host they control.
    for host in [
        "example.com&ws=attacker.example/x",
        "example.com?x=1",
        "example.com#frag",
        "example.com/path",
        "user@example.com",
        "example.com:notaport",
        "",
        " ",
    ] {
        assert!(!valid_host(host), "{} should be refused", host);
    }
}

#[test]
fn a_forwarded_host_is_filtered_the_same_way_as_host() {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "strom.example:8080".parse().unwrap());
    assert_eq!(client_host(&headers).as_deref(), Some("strom.example:8080"));

    // A crafted forwarded host does not win by being crafted - it is
    // dropped, and the Host header is not consulted as a fallback because
    // the client chose to send a forwarded host at all.
    headers.insert(
        "x-forwarded-host",
        "evil.example&ws=evil.example/x".parse().unwrap(),
    );
    assert!(client_host(&headers).is_none());
}

#[test]
fn the_remote_control_page_is_in_the_binary() {
    // open_link serves this; without it a link opens on an error page and
    // the whole feature is dead in a release build.
    assert!(crate::assets::RemoteControlAssets::get("index.html").is_some());
}

#[test]
fn the_warning_says_which_of_the_two_modes_this_is() {
    let filtered = DevToolsConfig {
        debug_port: Some(9222),
        tls: false,
        full_devtools: false,
    };
    let unfiltered = DevToolsConfig {
        full_devtools: true,
        ..filtered
    };
    for isolated in [true, false] {
        assert!(warning_for(&filtered, isolated).starts_with(SCREENCAST_CONTROL_WARNING));
        assert_eq!(warning_for(&unfiltered, isolated), REMOTE_CONTROL_WARNING);
    }
}

#[test]
fn without_isolation_the_warning_says_a_login_reaches_every_source() {
    let filtered = DevToolsConfig {
        debug_port: Some(9222),
        tls: false,
        full_devtools: false,
    };
    assert!(warning_for(&filtered, false).contains(SHARED_CONTEXT_WARNING));
    assert!(!warning_for(&filtered, true).contains(SHARED_CONTEXT_WARNING));
}
