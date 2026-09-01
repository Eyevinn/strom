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

/// Individually owned variable names that carry no Strom prefix.
///
/// Deliberately narrow: `OSC_ACCESS_TOKEN` is the only variable from another
/// project's namespace that Strom reads, so the rest of `OSC_` is left alone.
const OWNED_NAMES: &[&str] = &["OSC_ACCESS_TOKEN"];

/// Returns true if `key` names a variable Strom owns and may therefore remove.
fn is_owned(key: &str) -> bool {
    OWNED_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) || OWNED_NAMES.contains(&key)
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
    let keys = blank_owned_keys(std::env::vars_os());
    for key in &keys {
        std::env::remove_var(key);
    }
    keys
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
