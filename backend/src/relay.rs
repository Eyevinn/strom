//! Relay configuration: named relays per protocol.
//!
//! Strom's SRT blocks can dial out to a relay instead of listening on a
//! public port (`srt+relay://<id>[@<relay>]`, resolved in
//! `strom_types::srt_relay`). Like the TURN servers, relays are configured by
//! address: Strom never asks a relay anything, so a relay may start after
//! Strom and need not expose an HTTP API to it.
//!
//! Each protocol has its own named relays, from the config file and the
//! environment:
//!
//! ```toml
//! [relays.srt.default]
//! public   = "srt.example.com:9000"
//! internal = "srt-splice:9000"
//! [relays.srt.eu-north]
//! public   = "srt-eu.example.com:9000"
//! ```
//!
//! `STROM_<PROTO>_RELAY_PUBLIC` / `STROM_<PROTO>_RELAY_INTERNAL` set the
//! `default` relay; `STROM_<PROTO>_RELAYS` sets named relays as
//! `name=public[|internal]` entries separated by commas.
//!
//! Each field of a relay (public, internal) is merged on its own, lowest to
//! highest precedence: the file's `[relays.<proto>.<name>]`, then the
//! `STROM_<PROTO>_RELAYS` entry of that name, then
//! `STROM_<PROTO>_RELAY_PUBLIC` / `_INTERNAL` (for `default` only). A field
//! given by more than one source produces a startup warning
//! ([`Relays::warnings`]). The merged relay is checked afterwards, so an
//! internal address without a public one is still an error. Everything here
//! is keyed by [`RelayProtocol`], so a new protocol only adds a variant there.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use strom_types::capabilities::RelayEndpoint;
use strom_types::relay::{is_valid_relay_name, sort_relays, RelayProtocol, RelayTarget};
use strom_types::relay::{DEFAULT_RELAY_NAME, RELAY_NAME_MAX_LEN};

/// One relay as the config file or the environment gives it, unchecked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayEntry {
    /// The `host:port` the outside party dials.
    pub public: Option<String>,
    /// The `host:port` Strom dials instead, when it runs next to the relay.
    pub internal: Option<String>,
}

/// The `[relays]` section: protocol key, then relay name.
pub type RelaysSection = BTreeMap<String, BTreeMap<String, RelayEntry>>;

/// The configured relays of every protocol, checked. `default` comes first
/// in each list, then the others by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Relays {
    by_protocol: BTreeMap<RelayProtocol, Vec<RelayTarget>>,
    /// Fields set by more than one source, logged at startup.
    warnings: Vec<String>,
}

impl Relays {
    /// Build the relays from the `[relays]` section and the environment
    /// (read through `env`, so tests need not touch the process
    /// environment). Addresses are only checked as `host:port`: no relay is
    /// contacted.
    pub fn from_settings(
        section: &RelaysSection,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Self> {
        if let Some(key) = section
            .keys()
            .find(|k| RelayProtocol::from_key(k).is_none())
        {
            let known: Vec<_> = RelayProtocol::ALL.iter().map(|p| p.key()).collect();
            anyhow::bail!(
                "[relays.{key}]: unknown relay protocol, expected one of: {}",
                known.join(", ")
            );
        }
        let mut by_protocol = BTreeMap::new();
        let mut warnings = Vec::new();
        for &protocol in RelayProtocol::ALL {
            let (relays, protocol_warnings) = protocol_relays(
                protocol,
                section.get(protocol.key()).cloned().unwrap_or_default(),
                &env,
            )?;
            warnings.extend(protocol_warnings);
            if !relays.is_empty() {
                by_protocol.insert(protocol, relays);
            }
        }
        Ok(Self {
            by_protocol,
            warnings,
        })
    }

    /// Relays from a list, for tests and embedders.
    pub fn from_targets(protocol: RelayProtocol, mut relays: Vec<RelayTarget>) -> Self {
        sort_relays(&mut relays);
        let mut by_protocol = BTreeMap::new();
        if !relays.is_empty() {
            by_protocol.insert(protocol, relays);
        }
        Self {
            by_protocol,
            warnings: Vec::new(),
        }
    }

    /// One line per relay field that more than one source set, naming the
    /// sources and the one that wins. Logged when the relays are configured
    /// (the config is read before logging starts).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The relays of `protocol`, `default` first. Holds the internal
    /// addresses: use [`Relays::endpoints`] for anything a client sees.
    pub fn for_protocol(&self, protocol: RelayProtocol) -> &[RelayTarget] {
        self.by_protocol
            .get(&protocol)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// What a client may see of the relays of `protocol`: name and public
    /// address, never the internal address.
    pub fn endpoints(&self, protocol: RelayProtocol) -> Vec<RelayEndpoint> {
        self.for_protocol(protocol)
            .iter()
            .map(|r| RelayEndpoint {
                name: r.name.clone(),
                public: r.public.clone(),
            })
            .collect()
    }

    /// Every configured relay with its protocol.
    pub fn iter(&self) -> impl Iterator<Item = (RelayProtocol, &RelayTarget)> {
        self.by_protocol
            .iter()
            .flat_map(|(p, relays)| relays.iter().map(move |r| (*p, r)))
    }
}

/// The checked relays of one protocol, merged field by field from the file,
/// `STROM_<PROTO>_RELAYS` and `STROM_<PROTO>_RELAY_PUBLIC` / `_INTERNAL`
/// (lowest to highest precedence), plus a warning for each field given by
/// more than one source.
fn protocol_relays(
    protocol: RelayProtocol,
    file_entries: BTreeMap<String, RelayEntry>,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<(Vec<RelayTarget>, Vec<String>)> {
    let mut merged: BTreeMap<String, MergedEntry> = BTreeMap::new();
    for (name, entry) in file_entries {
        let source = format!("[relays.{}.{}]", protocol.key(), name);
        merged
            .entry(name)
            .or_insert_with(|| MergedEntry::new(&source))
            .apply(&source, &source, entry);
    }

    let named_var = protocol.relays_env_var();
    if let Some(raw) = strom_types::env::non_blank(env(&named_var)) {
        for (name, entry) in parse_relays_env(&named_var, &raw)? {
            merged
                .entry(name)
                .or_insert_with(|| MergedEntry::new(&named_var))
                .apply(&named_var, &named_var, entry);
        }
    }

    let (public_var, internal_var) = (protocol.public_env_var(), protocol.internal_env_var());
    let public = strom_types::env::non_blank(env(&public_var));
    let internal = strom_types::env::non_blank(env(&internal_var));
    if public.is_some() || internal.is_some() {
        let first_source = if public.is_some() {
            &public_var
        } else {
            &internal_var
        };
        merged
            .entry(DEFAULT_RELAY_NAME.to_string())
            .or_insert_with(|| MergedEntry::new(first_source))
            .apply(&public_var, &internal_var, RelayEntry { public, internal });
    }

    let mut relays = Vec::new();
    let mut warnings = Vec::new();
    for (name, entry) in merged {
        if !is_valid_relay_name(&name) {
            anyhow::bail!(
                "{}: invalid relay name '{name}': use 1 to {RELAY_NAME_MAX_LEN} characters from a-z, 0-9 and '-'",
                entry.name_source
            );
        }
        for (field, values) in [("public", &entry.public), ("internal", &entry.internal)] {
            if let Some(warning) = conflict_warning(protocol, &name, field, values) {
                warnings.push(warning);
            }
        }
        let internal = entry
            .internal
            .last()
            .map(|(source, a)| check_address(a, source, "internal").map(|a| (source, a)))
            .transpose()?;
        let Some((public_source, public)) = entry.public.last() else {
            if let Some((source, _)) = internal {
                anyhow::bail!("{source}: relay '{name}' has an internal address but no public one");
            }
            // Nothing set: no relay of this name.
            continue;
        };
        let public = check_address(public, public_source, "public")?;
        relays.push(RelayTarget {
            name,
            public,
            internal: internal.map(|(_, a)| a),
        });
    }
    sort_relays(&mut relays);
    Ok((relays, warnings))
}

/// One relay's fields with every source that set them, in precedence order
/// (the last one wins).
struct MergedEntry {
    /// Where the relay name was first given, for name errors.
    name_source: String,
    /// `(source, address)` for each source that set the public address.
    public: Vec<(String, String)>,
    /// `(source, address)` for each source that set the internal address.
    internal: Vec<(String, String)>,
}

impl MergedEntry {
    fn new(name_source: &str) -> Self {
        Self {
            name_source: name_source.to_string(),
            public: Vec::new(),
            internal: Vec::new(),
        }
    }

    /// Lay `entry` over the fields set so far; blank values leave a field
    /// alone.
    fn apply(&mut self, public_source: &str, internal_source: &str, entry: RelayEntry) {
        if let Some(public) = strom_types::env::non_blank(entry.public) {
            self.public
                .push((public_source.to_string(), public.trim().to_string()));
        }
        if let Some(internal) = strom_types::env::non_blank(entry.internal) {
            self.internal
                .push((internal_source.to_string(), internal.trim().to_string()));
        }
    }
}

/// A warning when more than one source set the same field of a relay,
/// naming every source and the one that wins. Relay addresses are not
/// secret, so they are included.
fn conflict_warning(
    protocol: RelayProtocol,
    name: &str,
    field: &str,
    values: &[(String, String)],
) -> Option<String> {
    let (winner, _) = values.last().filter(|_| values.len() > 1)?;
    let given: Vec<String> = values
        .iter()
        .map(|(source, address)| format!("{source} ({address})"))
        .collect();
    Some(format!(
        "{} relay '{name}': {field} address is set by {}; using {winner}",
        protocol.env_name(),
        given.join(", ")
    ))
}

/// Parse `name=public[|internal]` entries separated by commas. `name=public`
/// sets only the public address, so the internal one may come from the file.
/// An empty address on either side of `|` is an error.
fn parse_relays_env(var: &str, raw: &str) -> anyhow::Result<Vec<(String, RelayEntry)>> {
    let mut out: Vec<(String, RelayEntry)> = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((name, addresses)) = item.split_once('=') else {
            anyhow::bail!("{var}: expected name=public[|internal], got '{item}'");
        };
        let name = name.trim().to_string();
        if out.iter().any(|(n, _)| *n == name) {
            anyhow::bail!("{var}: relay '{name}' is given more than once");
        }
        let (public, internal) = match addresses.split_once('|') {
            Some((public, internal)) => (public.trim(), Some(internal.trim())),
            None => (addresses.trim(), None),
        };
        if public.is_empty() {
            anyhow::bail!(
                "{var}: relay '{name}' has no public address, expected name=public[|internal]"
            );
        }
        if internal.is_some_and(str::is_empty) {
            anyhow::bail!(
                "{var}: relay '{name}' has an empty internal address after '|': drop the '|' to set only the public address"
            );
        }
        out.push((
            name,
            RelayEntry {
                public: Some(public.to_string()),
                internal: internal.map(str::to_string),
            },
        ));
    }
    Ok(out)
}

/// `address` as an owned string if it is `host:port`, else a config error
/// naming where it came from.
fn check_address(address: &str, source: &str, field: &str) -> anyhow::Result<String> {
    if is_host_port(address) {
        Ok(address.to_string())
    } else {
        anyhow::bail!("{source}: {field} address must be host:port, got {address}")
    }
}

/// Whether `address` is `host:port` with a valid port (IPv6 hosts in
/// brackets).
fn is_host_port(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    let port_ok = port.parse::<u16>().is_ok_and(|p| p != 0);
    let bare_ipv6 = host.contains(':') && !(host.starts_with('[') && host.ends_with(']'));
    port_ok && !host.is_empty() && !bare_ipv6 && !host.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: RelayProtocol = RelayProtocol::Srt;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn file(toml_entries: &[(&str, Option<&str>, Option<&str>)]) -> RelaysSection {
        let mut srt = BTreeMap::new();
        for (name, public, internal) in toml_entries {
            srt.insert(
                name.to_string(),
                RelayEntry {
                    public: public.map(str::to_string),
                    internal: internal.map(str::to_string),
                },
            );
        }
        [("srt".to_string(), srt)].into()
    }

    fn target(name: &str, public: &str, internal: Option<&str>) -> RelayTarget {
        RelayTarget {
            name: name.into(),
            public: public.into(),
            internal: internal.map(str::to_string),
        }
    }

    const BAD: &[&str] = &[
        "srt-splice",
        "srt-splice:",
        "srt-splice:0",
        "srt-splice:99999",
        ":9000",
        "2001:db8::1",
        "srt://srt-splice:9000",
    ];

    #[test]
    fn nothing_configured_is_no_relays() {
        let relays = Relays::from_settings(&RelaysSection::new(), no_env).unwrap();
        assert!(relays.for_protocol(SRT).is_empty());
        assert!(relays.endpoints(SRT).is_empty());
        // Blank values are unset.
        let relays = Relays::from_settings(
            &RelaysSection::new(),
            env(&[("STROM_SRT_RELAY_PUBLIC", "  "), ("STROM_SRT_RELAYS", " ")]),
        )
        .unwrap();
        assert!(relays.for_protocol(SRT).is_empty());
    }

    #[test]
    fn env_configures_the_default_relay() {
        let relays = Relays::from_settings(
            &RelaysSection::new(),
            env(&[("STROM_SRT_RELAY_PUBLIC", " srt.example.com:9000 ")]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            [target("default", "srt.example.com:9000", None)]
        );
        assert_eq!(
            relays.for_protocol(SRT)[0].dial_address(),
            "srt.example.com:9000"
        );

        let relays = Relays::from_settings(
            &RelaysSection::new(),
            env(&[
                ("STROM_SRT_RELAY_PUBLIC", "srt.example.com:9000"),
                ("STROM_SRT_RELAY_INTERNAL", "srt-splice:9000"),
            ]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT)[0].dial_address(),
            "srt-splice:9000"
        );
    }

    #[test]
    fn env_configures_named_relays() {
        let relays = Relays::from_settings(
            &RelaysSection::new(),
            env(&[
                (
                    "STROM_SRT_RELAYS",
                    "us=srt-us.example.com:9000|srt-splice-us:9000, eu-north=srt-eu.example.com:9000",
                ),
                ("STROM_SRT_RELAY_PUBLIC", "srt.example.com:9000"),
            ]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            [
                target("default", "srt.example.com:9000", None),
                target("eu-north", "srt-eu.example.com:9000", None),
                target("us", "srt-us.example.com:9000", Some("srt-splice-us:9000")),
            ]
        );
    }

    #[test]
    fn file_configures_named_relays() {
        let relays = Relays::from_settings(
            &file(&[
                ("eu-north", Some("srt-eu.example.com:9000"), None),
                (
                    "default",
                    Some("srt.example.com:9000"),
                    Some("srt-splice:9000"),
                ),
            ]),
            no_env,
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            [
                target("default", "srt.example.com:9000", Some("srt-splice:9000")),
                target("eu-north", "srt-eu.example.com:9000", None),
            ]
        );
    }

    #[test]
    fn env_overrides_the_file_field_by_field() {
        let section = file(&[
            (
                "default",
                Some("srt.example.com:9000"),
                Some("srt-splice:9000"),
            ),
            (
                "eu-north",
                Some("srt-eu.example.com:9000"),
                Some("splice-eu:9000"),
            ),
            ("us", Some("srt-us.example.com:9000"), None),
        ]);
        let relays = Relays::from_settings(
            &section,
            env(&[
                ("STROM_SRT_RELAY_PUBLIC", "srt2.example.com:9000"),
                // No '|': only the public address, the file keeps internal.
                ("STROM_SRT_RELAYS", "eu-north=srt-eu2.example.com:9000"),
            ]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            [
                // The file's internal address of `default` survives.
                target("default", "srt2.example.com:9000", Some("srt-splice:9000")),
                target(
                    "eu-north",
                    "srt-eu2.example.com:9000",
                    Some("splice-eu:9000")
                ),
                target("us", "srt-us.example.com:9000", None),
            ]
        );
    }

    #[test]
    fn env_internal_completes_the_file_public() {
        // Public in .strom.toml, internal from the compose environment.
        let relays = Relays::from_settings(
            &file(&[("default", Some("srt.example.com:9000"), None)]),
            env(&[("STROM_SRT_RELAY_INTERNAL", "srt-splice:9000")]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            [target(
                "default",
                "srt.example.com:9000",
                Some("srt-splice:9000")
            )]
        );
        // Nothing was given twice.
        assert!(relays.warnings().is_empty(), "{:?}", relays.warnings());

        // The default variables complete a STROM_SRT_RELAYS entry too.
        let relays = Relays::from_settings(
            &RelaysSection::new(),
            env(&[
                ("STROM_SRT_RELAYS", "default=srt.example.com:9000"),
                ("STROM_SRT_RELAY_INTERNAL", "srt-splice:9000"),
            ]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            [target(
                "default",
                "srt.example.com:9000",
                Some("srt-splice:9000")
            )]
        );
    }

    #[test]
    fn precedence_is_file_then_relays_var_then_default_vars() {
        let section = file(&[(
            "default",
            Some("file.example.com:9000"),
            Some("file-splice:9000"),
        )]);
        let relays = Relays::from_settings(
            &section,
            env(&[
                (
                    "STROM_SRT_RELAYS",
                    "default=list.example.com:9000|list-splice:9000",
                ),
                ("STROM_SRT_RELAY_PUBLIC", "var.example.com:9000"),
            ]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            // public from STROM_SRT_RELAY_PUBLIC, internal from
            // STROM_SRT_RELAYS: each field takes its highest source.
            [target(
                "default",
                "var.example.com:9000",
                Some("list-splice:9000")
            )]
        );
    }

    #[test]
    fn a_field_set_twice_is_a_warning_naming_the_winner() {
        let section = file(&[
            (
                "default",
                Some("file.example.com:9000"),
                Some("srt-splice:9000"),
            ),
            ("eu", Some("srt-eu.example.com:9000"), None),
        ]);
        let relays = Relays::from_settings(
            &section,
            env(&[
                ("STROM_SRT_RELAYS", "default=list.example.com:9000"),
                ("STROM_SRT_RELAY_PUBLIC", "var.example.com:9000"),
            ]),
        )
        .unwrap();
        assert_eq!(
            relays.warnings(),
            ["SRT relay 'default': public address is set by \
              [relays.srt.default] (file.example.com:9000), \
              STROM_SRT_RELAYS (list.example.com:9000), \
              STROM_SRT_RELAY_PUBLIC (var.example.com:9000); \
              using STROM_SRT_RELAY_PUBLIC"]
        );

        let relays = Relays::from_settings(
            &section,
            env(&[(
                "STROM_SRT_RELAYS",
                "eu=srt-eu.example.com:9000|splice-eu:9000",
            )]),
        )
        .unwrap();
        // Same value counts as given twice too; internal is set once.
        assert_eq!(relays.warnings().len(), 1, "{:?}", relays.warnings());
        assert!(
            relays.warnings()[0].contains("'eu': public")
                && relays.warnings()[0].ends_with("using STROM_SRT_RELAYS"),
            "{:?}",
            relays.warnings()
        );

        // No overlap, no warning.
        let relays = Relays::from_settings(&section, no_env).unwrap();
        assert!(relays.warnings().is_empty(), "{:?}", relays.warnings());
    }

    #[test]
    fn bad_relay_names_are_config_errors() {
        for name in ["EU", "eu_north", "", &"a".repeat(33)] {
            let err = Relays::from_settings(
                &RelaysSection::new(),
                env(&[("STROM_SRT_RELAYS", &format!("{name}=srt.example.com:9000"))]),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("invalid relay name"), "{name}: {err}");
            assert!(err.contains("STROM_SRT_RELAYS"), "{name}: {err}");
        }
        let err = Relays::from_settings(&file(&[("Bad", Some("h:1"), None)]), no_env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[relays.srt.Bad]"), "{err}");
    }

    #[test]
    fn malformed_env_entries_are_config_errors() {
        for raw in [
            "eu-north",
            "eu=h:1,eu=h:2",
            // An empty internal after '|' is an error, not "clear internal".
            "eu=srt-eu.example.com:9000|",
            "eu=srt-eu.example.com:9000| ",
            "eu=|splice-eu:9000",
            "eu=",
        ] {
            let err =
                Relays::from_settings(&RelaysSection::new(), env(&[("STROM_SRT_RELAYS", raw)]))
                    .unwrap_err()
                    .to_string();
            assert!(err.contains("STROM_SRT_RELAYS"), "{raw}: {err}");
        }
    }

    #[test]
    fn internal_without_public_is_a_config_error() {
        let err = Relays::from_settings(
            &RelaysSection::new(),
            env(&[("STROM_SRT_RELAY_INTERNAL", "srt-splice:9000")]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no public"), "{err}");
        assert!(err.contains("STROM_SRT_RELAY_INTERNAL"), "{err}");

        // Still an error after merging when no source gives a public one.
        let err = Relays::from_settings(
            &file(&[("default", None, Some("srt-splice:9000"))]),
            env(&[("STROM_SRT_RELAY_INTERNAL", "srt-splice2:9000")]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no public"), "{err}");

        let err = Relays::from_settings(&file(&[("eu", None, Some("splice:9000"))]), no_env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[relays.srt.eu]"), "{err}");
        assert!(err.contains("no public"), "{err}");
    }

    #[test]
    fn malformed_addresses_are_config_errors() {
        for bad in BAD {
            let err = Relays::from_settings(
                &RelaysSection::new(),
                env(&[("STROM_SRT_RELAY_PUBLIC", bad)]),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("STROM_SRT_RELAY_PUBLIC"), "{bad}: {err}");
            assert!(err.contains("host:port"), "{bad}: {err}");

            let err = Relays::from_settings(
                &file(&[("default", Some("srt.example.com:9000"), Some(bad))]),
                no_env,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("internal"), "{bad}: {err}");
        }
        // Bracketed IPv6 is fine.
        let relays = Relays::from_settings(
            &RelaysSection::new(),
            env(&[("STROM_SRT_RELAY_PUBLIC", "[2001:db8::1]:9000")]),
        )
        .unwrap();
        assert_eq!(relays.for_protocol(SRT)[0].public, "[2001:db8::1]:9000");
    }

    #[test]
    fn unknown_protocol_is_a_config_error() {
        let section: RelaysSection = [("rist".to_string(), BTreeMap::new())].into();
        let err = Relays::from_settings(&section, no_env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[relays.rist]"), "{err}");
    }

    #[test]
    fn endpoints_never_carry_the_internal_address() {
        let relays = Relays::from_settings(
            &file(&[(
                "default",
                Some("srt.example.com:9000"),
                Some("srt-splice:9000"),
            )]),
            no_env,
        )
        .unwrap();
        assert_eq!(
            relays.endpoints(SRT),
            [RelayEndpoint {
                name: "default".into(),
                public: "srt.example.com:9000".into(),
            }]
        );
        let json = serde_json::to_string(&relays.endpoints(SRT)).unwrap();
        assert!(!json.contains("srt-splice"), "{json}");
    }
}
