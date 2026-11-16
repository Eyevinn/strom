//! Environment variable hygiene for the variables Strom owns.
//!
//! A set-but-empty environment variable means "unset", never "the empty value".
//! Orchestrators routinely forward blanks for settings the operator left
//! unconfigured — `STROM_API_KEY=${SOME_SECRET}` in a compose file or k8s
//! manifest becomes an empty string when `SOME_SECRET` is not set, and
//! Eyevinn Open Source Cloud forwards an empty `STROM_DATABASE_URL` for
//! instances with no database attached.
//!
//! Nothing downstream defends against that on its own: clap treats a
//! set-but-empty `env =` value as a real value, so `Option<String>` becomes
//! `Some("")` and `Option<u16>` fails to parse. An empty database URL used to
//! panic at startup, and an empty API key used to enable authentication while
//! accepting the empty bearer token.
//!
//! Two layers handle it. [`remove_blank_owned_vars`] scrubs the process
//! environment once in `main`, before anything reads it, which covers every
//! consumer including clap. [`var_opt`] is the value-level guard for direct
//! reads, and stays useful for values that reach us from a config file rather
//! than the environment.
//!
//! Because of this, no Strom variable may use presence-only semantics
//! (`env::var(..).is_ok()` as an on/off flag) — setting one to the empty string
//! is indistinguishable from not setting it at all.

use std::ffi::OsString;

/// Variable name prefixes owned by Strom.
const OWNED_PREFIXES: &[&str] = &["STROM_"];

/// Variables that switch authentication on.
///
/// Dropping one as blank leaves the instance with authentication disabled
/// unless another method is configured, which deserves a warning rather than
/// an info line: `lib.rs` already warns that all endpoints are public, and
/// this is what connects that warning to its cause.
const CREDENTIAL_NAMES: &[&str] = &[
    "STROM_ADMIN_USER",
    "STROM_ADMIN_PASSWORD_HASH",
    "STROM_API_KEY",
];

/// Individually owned variable names that carry no Strom prefix.
///
/// Deliberately narrow: `OSC_ACCESS_TOKEN` is the only variable from another
/// project's namespace that Strom reads, so the rest of `OSC_` is left alone.
const OWNED_NAMES: &[&str] = &["OSC_ACCESS_TOKEN"];

/// Returns true if `key` names a variable Strom owns and may therefore remove.
fn is_owned(key: &str) -> bool {
    OWNED_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) || OWNED_NAMES.contains(&key)
}

/// Returns true if `key` names a variable that configures authentication.
pub fn is_credential(key: &str) -> bool {
    CREDENTIAL_NAMES.contains(&key)
}

/// Reads an environment variable, treating a blank value as unset.
///
/// Prefer this over `env::var(key).ok()` for every Strom variable.
pub fn var_opt(key: &str) -> Option<String> {
    non_blank(std::env::var(key).ok())
}

/// Treats a blank string as absent, leaving any other value untouched.
///
/// Only fully blank values are dropped — a value is never trimmed, so
/// `" secret "` keeps its spaces.
pub fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Picks out the Strom-owned variables in `vars` whose value is blank.
///
/// Pure so it can be tested without mutating the process environment. A
/// variable with a non-UTF-8 name or value is left alone: it cannot be blank,
/// and `env::vars` would panic on it where `env::vars_os` does not.
pub fn blank_owned_keys<I>(vars: I) -> Vec<String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            let value = value.to_str()?;
            (is_owned(key) && value.trim().is_empty()).then(|| key.to_string())
        })
        .collect()
}

/// Removes every Strom-owned environment variable that is set but blank, and
/// returns the names removed so the caller can log them once logging is up.
///
/// # Safety and placement
///
/// This mutates the process environment, which races with `getenv` in any other
/// thread (the reason `remove_var` became `unsafe` in edition 2024). Call it
/// only from the single-threaded prologue of `main`: after `dotenvy`, so blanks
/// coming from a local `.env` file are caught too, and before argument parsing
/// or any thread, runtime, or GStreamer initialization.
pub fn remove_blank_owned_vars() -> Vec<String> {
    let mut keys = blank_owned_keys(std::env::vars_os());
    keys.sort();
    for key in &keys {
        std::env::remove_var(key);
    }
    keys
}

/// Strom variables that hold a port number, and whose name Kubernetes can
/// also produce for a Service.
///
/// With `enableServiceLinks` on (the default), Kubernetes gives every pod
/// Docker-link style variables for each Service in its namespace. A Service
/// named `strom` sets `STROM_PORT=tcp://<cluster ip>:<port>`, one named
/// `strom-server` sets `STROM_SERVER_PORT=tcp://...`. Strom reads those names
/// as its own port settings, and failed to start on the URL (#1266).
const PORT_VARS: &[&str] = &["STROM_PORT", "STROM_SERVER_PORT", "STROM_CEF_DEBUG_PORT"];

/// A Strom port variable that was dropped because it held a Kubernetes
/// service link value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLinkVar {
    /// The variable name, e.g. `STROM_PORT`.
    pub key: String,
    /// The value it held, e.g. `tcp://192.0.2.10:8080`.
    pub value: String,
}

impl ServiceLinkVar {
    /// The name of the Kubernetes Service that injects this variable: the
    /// variable name without `_PORT`, lowercase, with `-` for `_`.
    pub fn service_name(&self) -> String {
        self.key
            .strip_suffix("_PORT")
            .unwrap_or(&self.key)
            .to_lowercase()
            .replace('_', "-")
    }
}

/// Returns true if `value` has the form Kubernetes uses for a service link:
/// `tcp://<host>:<port>`, `udp://...` or `sctp://...`.
///
/// Anything else, `abc` for instance, is not a service link and is left for
/// the normal parser to accept or refuse.
pub fn is_service_link_value(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "tcp" | "udp" | "sctp") {
        return false;
    }
    let Some((host, port)) = rest.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && port.parse::<u16>().is_ok()
}

/// Returns true if `key` has the form of a Kubernetes service link variable
/// other than the bare `<NAME>_PORT`: `<NAME>_SERVICE_HOST`,
/// `<NAME>_SERVICE_PORT[_<port name>]` or `<NAME>_PORT_<n>_<PROTO>[_ADDR|_PORT|_PROTO]`.
///
/// Strom must never read a variable with such a name: a Service named after
/// the prefix would set it. Guard tests check every variable Strom reads.
pub fn is_service_link_name(key: &str) -> bool {
    if key.ends_with("_SERVICE_HOST")
        || key.ends_with("_SERVICE_PORT")
        || key.contains("_SERVICE_PORT_")
    {
        return true;
    }
    key.match_indices("_PORT_").any(|(i, m)| {
        key[i + m.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

/// Returns true if `key` is one of the port variables that
/// [`remove_service_link_port_vars`] checks. Every `STROM_..._PORT` name Strom
/// reads must be one, or a Service named after it stops Strom at startup.
pub fn is_port_var(key: &str) -> bool {
    PORT_VARS.contains(&key)
}

/// Picks out the Strom port variables in `vars` that hold a Kubernetes
/// service link value. Pure so it can be tested without mutating the process
/// environment.
pub fn service_link_port_vars<I>(vars: I) -> Vec<ServiceLinkVar>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut found: Vec<ServiceLinkVar> = vars
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            let value = value.to_str()?;
            (is_port_var(key) && is_service_link_value(value)).then(|| ServiceLinkVar {
                key: key.to_string(),
                value: value.to_string(),
            })
        })
        .collect();
    found.sort_by(|a, b| a.key.cmp(&b.key));
    found
}

/// Removes every Strom port variable that holds a Kubernetes service link
/// value, so the port falls back to the next source (`STROM_SERVER_PORT`, the
/// config file, the default). Returns what it removed, for the caller to log
/// once logging is up.
///
/// Same placement rules as [`remove_blank_owned_vars`]: only from the
/// single-threaded prologue of `main`, before argument parsing. Like the
/// blank sweep, call it both before `dotenvy` (which skips a key that is
/// already set, so an injected value would shadow `.env` and then be deleted,
/// losing both) and after it.
pub fn remove_service_link_port_vars() -> Vec<ServiceLinkVar> {
    let found = service_link_port_vars(std::env::vars_os());
    for var in &found {
        std::env::remove_var(&var.key);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
            .collect()
    }

    #[test]
    fn blank_strom_variables_are_selected() {
        let keys = blank_owned_keys(vars(&[
            ("STROM_DATABASE_URL", ""),
            ("STROM_API_KEY", "   "),
            ("STROM_PORT", "\t\n"),
        ]));

        assert_eq!(
            keys,
            vec!["STROM_DATABASE_URL", "STROM_API_KEY", "STROM_PORT"]
        );
    }

    #[test]
    fn variables_with_a_value_are_kept() {
        let keys = blank_owned_keys(vars(&[
            ("STROM_DATABASE_URL", "postgresql://localhost/strom"),
            ("STROM_API_KEY", " secret "),
            ("STROM_PORT", "0"),
        ]));

        assert!(keys.is_empty());
    }

    #[test]
    fn foreign_variables_are_never_touched() {
        let keys = blank_owned_keys(vars(&[
            ("RUST_LOG", ""),
            ("GST_DEBUG", ""),
            ("WAYLAND_DISPLAY", ""),
            ("PATH", ""),
            ("OSC_ENVIRONMENT", ""),
            ("NOT_STROM_PORT", ""),
        ]));

        assert!(keys.is_empty(), "unexpectedly claimed {:?}", keys);
    }

    #[test]
    fn the_one_borrowed_name_is_owned() {
        let keys = blank_owned_keys(vars(&[("OSC_ACCESS_TOKEN", "")]));

        assert_eq!(keys, vec!["OSC_ACCESS_TOKEN"]);
    }

    #[test]
    fn credential_variables_are_recognised() {
        assert!(is_credential("STROM_API_KEY"));
        assert!(is_credential("STROM_ADMIN_USER"));
        assert!(is_credential("STROM_ADMIN_PASSWORD_HASH"));
        assert!(!is_credential("STROM_DATABASE_URL"));
        assert!(!is_credential("STROM_PORT"));
    }

    #[test]
    fn non_utf8_values_are_left_alone() {
        let keys = blank_owned_keys(vec![(
            OsString::from("STROM_DATA_DIR"),
            non_utf8_os_string(),
        )]);

        assert!(keys.is_empty());
    }

    #[test]
    fn var_opt_drops_blanks_but_preserves_padding() {
        assert_eq!(non_blank(Some(String::new())), None);
        assert_eq!(non_blank(Some("  \t ".to_string())), None);
        assert_eq!(non_blank(None), None);
        assert_eq!(
            non_blank(Some(" secret ".to_string())),
            Some(" secret ".to_string())
        );
    }

    #[test]
    fn kubernetes_service_link_ports_are_selected() {
        let found = service_link_port_vars(vars(&[
            ("STROM_PORT", "tcp://192.0.2.10:8080"),
            ("STROM_SERVER_PORT", "udp://192.0.2.11:5000"),
            ("STROM_PORT_8080_TCP", "tcp://192.0.2.10:8080"),
            ("OTHER_PORT", "tcp://192.0.2.12:80"),
        ]));

        assert_eq!(
            found,
            vec![
                ServiceLinkVar {
                    key: "STROM_PORT".to_string(),
                    value: "tcp://192.0.2.10:8080".to_string(),
                },
                ServiceLinkVar {
                    key: "STROM_SERVER_PORT".to_string(),
                    value: "udp://192.0.2.11:5000".to_string(),
                },
            ]
        );
        assert_eq!(found[0].service_name(), "strom");
        assert_eq!(found[1].service_name(), "strom-server");
    }

    #[test]
    fn real_and_invalid_port_values_are_kept() {
        let found = service_link_port_vars(vars(&[
            ("STROM_PORT", "9000"),
            ("STROM_SERVER_PORT", "abc"),
            ("STROM_CEF_DEBUG_PORT", "http://192.0.2.10:9222"),
        ]));
        assert!(found.is_empty(), "unexpectedly dropped {:?}", found);

        for value in [
            "abc",
            "9000",
            "tcp://",
            "tcp://192.0.2.10",
            "tcp://:80",
            "tcp://h:x",
        ] {
            assert!(!is_service_link_value(value), "{value}");
        }
        for value in ["tcp://192.0.2.10:8080", "udp://192.0.2.10:53", "sctp://h:1"] {
            assert!(is_service_link_value(value), "{value}");
        }
    }

    #[test]
    fn service_link_names_are_recognised() {
        for key in [
            "STROM_SERVICE_HOST",
            "STROM_SERVICE_PORT",
            "STROM_SERVICE_PORT_HTTP",
            "STROM_PORT_8080_TCP",
            "STROM_PORT_8080_TCP_ADDR",
            "STROM_PORT_8080_TCP_PORT",
            "STROM_PORT_8080_TCP_PROTO",
        ] {
            assert!(is_service_link_name(key), "{key}");
        }
        for key in [
            "STROM_PORT",
            "STROM_PORTS",
            "STROM_SERVER_PORT",
            "STROM_PORT_POOL",
            "STROM_SERVICE_PORTAL_URL",
        ] {
            assert!(!is_service_link_name(key), "{key}");
        }
        assert!(is_port_var("STROM_PORT"));
        assert!(!is_port_var("STROM_PORTS"));
    }

    #[cfg(unix)]
    fn non_utf8_os_string() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0x66, 0x80, 0x6f])
    }

    #[cfg(windows)]
    fn non_utf8_os_string() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0x66, 0xD800, 0x6f])
    }
}
