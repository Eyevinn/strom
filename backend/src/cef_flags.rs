//! How Strom sets up CEF before the first `cefsrc` starts it.
//!
//! gstcefsrc reads its switches from the environment once, when the first
//! `cefsrc` in the process initializes CEF, so everything here runs early in
//! `main`, before any thread is spawned and before GStreamer is initialized:
//! that is what makes `set_var` safe.

use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

const EXTRA_FLAGS: &str = "GST_CEF_CHROME_EXTRA_FLAGS";
const CACHE_LOCATION: &str = "GST_CEF_CACHE_LOCATION";
const LOG_SEVERITY: &str = "GST_CEF_LOG_SEVERITY";

/// What the caller decided about CEF, and what comes back from setting it up.
pub struct CefSetup<'a> {
    /// This instance's own cache directory, used unless the environment
    /// already names one.
    pub cache_path: &'a Path,
    /// The configured remote debugging port.
    pub debug_port: Option<u16>,
    /// Whether a link carries Chromium's DevTools application unfiltered.
    pub full_devtools: bool,
    /// Whether Strom's authentication is configured.
    pub auth_configured: bool,
}

/// Set CEF up, and return Chromium's debug port, if it is opened.
///
/// The port is how Strom guards every page (see [`crate::cef_pages`]) and what
/// remote control links go through, so it is opened whenever authentication
/// is configured, on a free loopback port unless one is configured. Nothing
/// outside Strom connects to it, so its number does not matter. Whether a
/// block hands out links is the block's Remote Control switch.
pub fn configure(setup: CefSetup<'_>) -> Option<u16> {
    configure_cache(setup.cache_path);
    quiet_page_console();

    let existing = std::env::var(EXTRA_FLAGS).unwrap_or_default();
    let mut flags = network_checks(&existing);
    flags.extend(fake_media_switches());

    let port = debug_port(&mut flags, setup.debug_port, setup.auth_configured);
    std::env::set_var(EXTRA_FLAGS, flags.join(","));

    if let Some(port) = port {
        crate::cef_pages::set_debug_port(port);
        if setup.full_devtools {
            warn!(
                "CEF remote debugging on 127.0.0.1:{} with full DevTools - a remote control \
                 link is full control of the browser. One browser process serves every HTML \
                 source in this instance, so a session opened against one of them reaches all \
                 of them, every page they are logged in to, and the files on this host. To \
                 keep customers apart, run a Strom process per customer",
                port
            );
        } else {
            info!(
                "CEF debug port on 127.0.0.1:{}, for guarding HTML sources and remote control \
                 links, which carry the page's picture and input only. The port itself is full \
                 control of the browser to anything on this host that can reach loopback; \
                 never publish it",
                port
            );
        }
    }
    port
}

/// Keep pages' console output out of Strom's log, unless the operator asked
/// for CEF's log at a lower severity.
///
/// Chromium logs every console message, `console.error` included, at INFO,
/// and gstcefsrc defaults CEF to INFO, so a page logging in a loop filled the
/// log and the disk under it. Measured on upstream gstcefsrc: 1200 lines per
/// console level from a page logging for three seconds at INFO, none at
/// warning. The strom-full entrypoint sets the same default; this gives it to
/// a native run too.
fn quiet_page_console() {
    if std::env::var_os(LOG_SEVERITY).is_none() {
        std::env::set_var(LOG_SEVERITY, "warning");
    }
}

/// Give CEF a per-instance cache directory.
///
/// Native runs otherwise get Chromium's default, which warns
///
///   Please customize CefSettings.root_cache_path for your application. Use
///   of the default value may lead to unintended process singleton behavior.
///
/// and leaves Chromium's process singleton shared: a second Strom instance on
/// the same machine cannot start any cefsrc element, failing in
/// gst_base_src_start() so the flow start returns 500 "Element failed to
/// change its state". The resolved path is per data directory, so instances
/// stay isolated while keeping a warm profile across restarts, which also
/// cuts repeat flow-start latency. GST_CEF_CACHE_LOCATION always wins if it is
/// already set — the strom-full Docker image sets it in its entrypoint.
fn configure_cache(cache_path: &Path) {
    match std::env::var_os(CACHE_LOCATION) {
        Some(existing) => info!(
            "CEF cache directory: {} (from {})",
            PathBuf::from(existing).display(),
            CACHE_LOCATION
        ),
        None => {
            if let Err(e) = std::fs::create_dir_all(cache_path) {
                warn!(
                    "Could not create CEF cache directory {}: {}",
                    cache_path.display(),
                    e
                );
            }
            std::env::set_var(CACHE_LOCATION, cache_path);
            info!("CEF cache directory: {}", cache_path.display());
        }
    }
    #[cfg(unix)]
    if let Some(root) = crate::cef_profiles::cache_root() {
        crate::cef_profiles::clear_stale_singleton_lock(&root);
    }
}

/// The checks Chromium makes of what a page reaches on this machine and its
/// network, ahead of the flags the operator set.
///
/// Local Network Access is on by default, but in this Chromium it leaves
/// navigations, WebSocket and WebTransport unchecked. These only switch the
/// checks on; whether a page is refused is each browser's own answer to the
/// prompt, which gstcefsrc's strict-network gives.
///
/// gstcefsrc splits the list on commas, so a feature list cannot be one value.
/// Strom's build merges every `enable-features` it is given; upstream keeps
/// the last one only. So Strom's go first, the most important of them last,
/// and an `enable-features` the operator set comes after all of them: on
/// upstream it is the operator's that is kept, as they asked.
fn network_checks(existing: &str) -> Vec<String> {
    let mut flags: Vec<String> = [
        "LocalNetworkAccessChecksWebTransport",
        "LocalNetworkAccessChecksWebSockets",
        "LocalNetworkAccessForNavigations",
    ]
    .iter()
    .map(|feature| format!("enable-features={}", feature))
    .collect();
    flags.extend(split_flags(existing));
    flags
}

/// A page's camera and microphone: a silent one and a black one, never the
/// server's own devices.
///
/// Upstream gstcefsrc grants every media request, so the fake-device switch is
/// what keeps the server's capture cards out of reach. It goes in even when
/// Strom cannot write its silence and black frame: Chromium's own test tone
/// and card are then what a page gets, which beat a real device.
fn fake_media_switches() -> Vec<String> {
    let Some(cache) = std::env::var_os(CACHE_LOCATION) else {
        return vec![crate::cef_media::FAKE_DEVICE_SWITCH.to_string()];
    };
    let dir = PathBuf::from(cache).join("strom-fake-media");
    match crate::cef_media::fake_device_switches(&dir) {
        Ok(switches) => switches,
        Err(e) => {
            warn!(
                "Could not write silent media devices to {}: {} - pages get Chromium's test \
                 tone and test card as their microphone and camera",
                dir.display(),
                e
            );
            vec![crate::cef_media::FAKE_DEVICE_SWITCH.to_string()]
        }
    }
}

fn split_flags(flags: &str) -> Vec<String> {
    flags
        .split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// A flag's value: `Some(None)` for the bare flag, `Some(Some(v))` for `name=v`.
fn flag_value(flags: &[String], name: &str) -> Option<Option<String>> {
    flags.iter().find_map(|f| {
        if f == name {
            Some(None)
        } else {
            f.strip_prefix(&format!("{}=", name))
                .map(|v| Some(v.to_string()))
        }
    })
}

/// Open Chromium's remote debugging port, and return the port in force.
///
/// This is what lets Strom guard every page, and an operator drive an HTML
/// source: log in to a page, click through a consent dialog, dismiss a cookie
/// banner. The port speaks the Chrome DevTools Protocol, which is total
/// control of the browser process, and Chromium binds it to loopback. Reach it
/// through the authenticated API, never by publishing the port.
///
/// `persist-session-cookies` rides along: without it a login lands in a
/// session cookie that Chromium keeps in memory only, so the next flow start
/// is logged out again even with a warm profile. Chromium writes the cookie
/// store on a timer, so a login survives a graceful restart but not a kill in
/// the first half minute after it.
fn debug_port(
    flags: &mut Vec<String>,
    configured: Option<u16>,
    auth_configured: bool,
) -> Option<u16> {
    // Minting a link is reached through the authenticated API, so with no
    // authentication configured there is no door in front of it at all:
    // anyone who can reach the HTTP port could mint one. Rather than open the
    // debug port and rely on a lock that is not fitted, do not open it.
    if !auth_configured {
        if configured.is_some() {
            error!(
                "A CEF debug port is configured but authentication is not, so minting a \
                 remote control link would take no credentials at all. The port stays shut. \
                 Set STROM_ADMIN_USER together with STROM_ADMIN_PASSWORD_HASH, or \
                 STROM_API_KEY, and start again to use it"
            );
        }
        return None;
    }

    // Whatever port Chromium ends up listening on is the one the proxy has to
    // dial. An operator who put the flag in GST_CEF_CHROME_EXTRA_FLAGS
    // themselves keeps it - but then a configured value is not where the
    // browser is, and a proxy pointed at it would answer "no pages" forever
    // with nothing to say why.
    let effective = match flag_value(flags, "remote-debugging-port") {
        Some(Some(existing)) => match existing.parse::<u16>() {
            Ok(port) if port != 0 => {
                if configured.is_some_and(|c| c != port) {
                    warn!(
                        "CEF debug port: {} already sets remote-debugging-port={}, so that is \
                         the port in use and the configured {} is ignored",
                        EXTRA_FLAGS,
                        port,
                        configured.unwrap_or_default()
                    );
                }
                port
            }
            _ => {
                warn!(
                    "CEF debug port: {} sets remote-debugging-port={}, which is not a port \
                     Strom can reach. HTML sources are not guarded and remote control is \
                     off - fix the flag or remove it",
                    EXTRA_FLAGS, existing
                );
                return None;
            }
        },
        // The bare flag without a value leaves Chromium to pick a port, and it
        // never tells us which. Nothing can be proxied to that.
        Some(None) => {
            warn!(
                "CEF debug port: {} sets remote-debugging-port with no port, so the port \
                 Chromium picks is unknown. HTML sources are not guarded and remote control \
                 is off - give the flag a port, or remove it",
                EXTRA_FLAGS
            );
            return None;
        }
        None => {
            // Chromium binds it when the first cefsrc starts, so another
            // process could take a free port in between; then pages go
            // unguarded and each says so when it starts.
            let port = match configured {
                Some(port) => port,
                None => std::net::TcpListener::bind(("127.0.0.1", 0))
                    .and_then(|listener| listener.local_addr())
                    .map(|addr| addr.port())
                    .ok()?,
            };
            flags.push(format!("remote-debugging-port={}", port));
            port
        }
    };

    if flag_value(flags, "persist-session-cookies").is_none() {
        flags.push("persist-session-cookies".to_string());
    }

    Some(effective)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(flags: &[String]) -> Vec<&str> {
        flags
            .iter()
            .filter_map(|f| f.strip_prefix("enable-features="))
            .collect()
    }

    #[test]
    fn an_operators_feature_comes_last_so_upstream_keeps_it() {
        let flags = network_checks("show-fps-counter,enable-features=Vulkan");
        let features = features(&flags);
        assert_eq!(features.last(), Some(&"Vulkan"));
        assert!(flags.contains(&"show-fps-counter".to_string()));
    }

    #[test]
    fn without_one_upstream_keeps_the_navigation_check() {
        // Upstream keeps only the last enable-features; navigations to the
        // server are the check that matters most.
        let flags = network_checks("");
        assert_eq!(
            features(&flags).last(),
            Some(&"LocalNetworkAccessForNavigations")
        );
    }

    #[test]
    fn with_authentication_a_free_port_is_opened_unasked() {
        let mut flags = Vec::new();
        let port = debug_port(&mut flags, None, true).expect("a free loopback port");
        assert_ne!(port, 0);
        assert!(flags.contains(&format!("remote-debugging-port={}", port)));
    }

    #[test]
    fn the_debug_port_stays_shut_without_authentication() {
        let mut flags = Vec::new();
        assert_eq!(debug_port(&mut flags, Some(9222), false), None);
        assert_eq!(debug_port(&mut flags, None, false), None);
        assert!(flags.is_empty());
    }

    #[test]
    fn the_port_already_in_the_flags_is_the_one_in_force() {
        let mut flags = split_flags("remote-debugging-port=9333");
        assert_eq!(debug_port(&mut flags, Some(9222), true), Some(9333));
        let mut flags = split_flags("remote-debugging-port");
        assert_eq!(debug_port(&mut flags, Some(9222), true), None);
        let mut flags = Vec::new();
        assert_eq!(debug_port(&mut flags, Some(9222), true), Some(9222));
        assert!(flags.contains(&"remote-debugging-port=9222".to_string()));
        assert!(flags.contains(&"persist-session-cookies".to_string()));
    }
}
