//! Utility functions for local storage, file downloads, VLC playlist generation, and task spawning.

// Local storage helpers (WASM only)
#[cfg(target_arch = "wasm32")]
pub fn set_local_storage(key: &str, value: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(key, value);
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub fn get_local_storage(key: &str) -> Option<String> {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            return storage.get_item(key).ok().flatten();
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
pub fn remove_local_storage(key: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(key);
        }
    }
}

// Stubs for native mode (use in-memory HashMap)
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
static LOCAL_STORAGE: Mutex<Option<std::collections::HashMap<String, String>>> = Mutex::new(None);

#[cfg(not(target_arch = "wasm32"))]
pub fn set_local_storage(key: &str, value: &str) {
    let mut storage = LOCAL_STORAGE.lock().unwrap();
    if storage.is_none() {
        *storage = Some(std::collections::HashMap::new());
    }
    storage
        .as_mut()
        .unwrap()
        .insert(key.to_string(), value.to_string());
}

#[cfg(not(target_arch = "wasm32"))]
pub fn get_local_storage(key: &str) -> Option<String> {
    let storage = LOCAL_STORAGE.lock().unwrap();
    storage.as_ref()?.get(key).cloned()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn remove_local_storage(key: &str) {
    let mut storage = LOCAL_STORAGE.lock().unwrap();
    if let Some(ref mut map) = *storage {
        map.remove(key);
    }
}

/// Trigger a file download in the browser with the given content.
#[cfg(target_arch = "wasm32")]
pub fn download_file(filename: &str, content: &str, mime_type: &str) {
    use wasm_bindgen::JsCast;

    let window = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let document = match window.document() {
        Some(d) => d,
        None => return,
    };

    // Create a blob with the content
    let blob_parts = js_sys::Array::new();
    blob_parts.push(&wasm_bindgen::JsValue::from_str(content));

    let blob_options = web_sys::BlobPropertyBag::new();
    blob_options.set_type(mime_type);

    let blob = match web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_options) {
        Ok(b) => b,
        Err(_) => return,
    };

    // Create object URL
    let url = match web_sys::Url::create_object_url_with_blob(&blob) {
        Ok(u) => u,
        Err(_) => return,
    };

    // Create a temporary anchor element and click it
    let anchor = match document.create_element("a") {
        Ok(el) => el,
        Err(_) => return,
    };

    let _ = anchor.set_attribute("href", &url);
    let _ = anchor.set_attribute("download", filename);

    if let Some(html_anchor) = anchor.dyn_ref::<web_sys::HtmlElement>() {
        html_anchor.click();
    }

    // Clean up the object URL
    let _ = web_sys::Url::revoke_object_url(&url);
}

/// Native mode - save file to temp directory and open with default application.
#[cfg(not(target_arch = "wasm32"))]
pub fn download_file(filename: &str, content: &str, _mime_type: &str) {
    // Save to temp directory so it doesn't clutter working directory
    let path = std::env::temp_dir().join(filename);

    match std::fs::write(&path, content) {
        Ok(_) => {
            tracing::info!("Saved file to: {}", path.display());

            // Open the file with the default application (VLC for .xspf)
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                    tracing::error!("Failed to open file with xdg-open: {}", e);
                }
            }

            #[cfg(target_os = "macos")]
            {
                if let Err(e) = std::process::Command::new("open").arg(&path).spawn() {
                    tracing::error!("Failed to open file: {}", e);
                }
            }

            #[cfg(target_os = "windows")]
            {
                if let Err(e) = std::process::Command::new("cmd")
                    .args(["/C", "start", "", &path.to_string_lossy()])
                    .spawn()
                {
                    tracing::error!("Failed to open file: {}", e);
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to save file {}: {}", path.display(), e);
        }
    }
}

/// Generate XSPF playlist content for VLC to play an SRT stream.
///
/// If the block is in listener mode (e.g., `srt://:5000?mode=listener`), VLC needs to
/// connect as a caller. We transform the URI to use the server's hostname from the
/// current browser URL.
pub fn generate_vlc_playlist(srt_uri: &str, latency_ms: i32, stream_name: &str) -> String {
    // Transform URI if it's in listener mode - VLC needs to connect as caller
    let vlc_uri = transform_srt_uri_for_vlc(srt_uri);

    // Escape XML special characters in the URI
    let escaped_uri = vlc_uri
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");

    let escaped_name = stream_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist xmlns="http://xspf.org/ns/0/" xmlns:vlc="http://www.videolan.org/vlc/playlist/ns/0/" version="1">
  <title>Strom SRT Stream</title>
  <trackList>
    <track>
      <location>{}</location>
      <title>{}</title>
      <extension application="http://www.videolan.org/vlc/playlist/0">
        <vlc:option>network-caching={}</vlc:option>
      </extension>
    </track>
  </trackList>
</playlist>
"#,
        escaped_uri, escaped_name, latency_ms
    )
}

/// Transform SRT URI for VLC playback.
///
/// When the MPEG-TS/SRT block is in listener mode (server waiting for connections),
/// VLC needs to connect as a caller. This function:
/// 1. Detects listener mode URIs (e.g., `srt://:5000?mode=listener`)
/// 2. Replaces empty host with the Strom server's hostname
/// 3. Changes mode from listener to caller
fn transform_srt_uri_for_vlc(srt_uri: &str) -> String {
    // Check if this is a listener mode URI (empty host or mode=listener)
    let is_listener = srt_uri.contains("mode=listener");
    let has_empty_host = srt_uri.starts_with("srt://:") || srt_uri.starts_with("srt://:");

    if !is_listener && !has_empty_host {
        // Already in caller mode with a host, use as-is
        return srt_uri.to_string();
    }

    // Get the current hostname from the browser (WASM) or use localhost (native)
    let hostname = get_current_hostname();

    // Parse the URI to extract port and other parameters
    // URI format: srt://[host]:port[?params]
    let uri_without_scheme = srt_uri.strip_prefix("srt://").unwrap_or(srt_uri);

    // Find the port - it's between : and ? (or end of string)
    let (host_port, params) = if let Some(q_pos) = uri_without_scheme.find('?') {
        (
            &uri_without_scheme[..q_pos],
            Some(&uri_without_scheme[q_pos + 1..]),
        )
    } else {
        (uri_without_scheme, None)
    };

    // Extract just the port (after the last colon)
    let port = if let Some(colon_pos) = host_port.rfind(':') {
        &host_port[colon_pos + 1..]
    } else {
        host_port
    };

    // Build the new URI with caller mode
    let mut new_uri = format!("srt://{}:{}", hostname, port);

    // Add parameters, but change mode to caller
    if let Some(params) = params {
        let new_params: Vec<&str> = params
            .split('&')
            .filter(|p| !p.starts_with("mode="))
            .collect();

        if new_params.is_empty() {
            new_uri.push_str("?mode=caller");
        } else {
            new_uri.push('?');
            new_uri.push_str(&new_params.join("&"));
            new_uri.push_str("&mode=caller");
        }
    } else {
        new_uri.push_str("?mode=caller");
    }

    new_uri
}

/// Get the hostname of the current server.
/// Returns "127.0.0.1" instead of "localhost" because VLC doesn't work well with localhost.
#[cfg(target_arch = "wasm32")]
fn get_current_hostname() -> String {
    let hostname = web_sys::window()
        .and_then(|w| w.location().hostname().ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // VLC doesn't work well with "localhost", use 127.0.0.1 instead
    if hostname == "localhost" {
        "127.0.0.1".to_string()
    } else {
        hostname
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn get_current_hostname() -> String {
    // VLC doesn't work well with "localhost", use 127.0.0.1 instead
    "127.0.0.1".to_string()
}

// Cross-platform task spawning
#[cfg(target_arch = "wasm32")]
pub fn spawn_task<F>(future: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_task<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(future);
}
