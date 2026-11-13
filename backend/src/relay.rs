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
//! `name=public[|internal]` entries separated by commas. An environment entry
//! replaces the file's entry of the same name as a whole. Everything here is
//! keyed by [`RelayProtocol`], so a new protocol only adds a variant there.

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
        for &protocol in RelayProtocol::ALL {
            let relays = protocol_relays(
                protocol,
                section.get(protocol.key()).cloned().unwrap_or_default(),
                &env,
            )?;
            if !relays.is_empty() {
                by_protocol.insert(protocol, relays);
            }
        }
        Ok(Self { by_protocol })
    }

    /// Relays from a list, for tests and embedders.
    pub fn from_targets(protocol: RelayProtocol, mut relays: Vec<RelayTarget>) -> Self {
        sort_relays(&mut relays);
        let mut by_protocol = BTreeMap::new();
        if !relays.is_empty() {
            by_protocol.insert(protocol, relays);
        }
        Self { by_protocol }
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

/// The checked relays of one protocol: the file's entries, with the
/// environment's entries replacing those of the same name.
fn protocol_relays(
    protocol: RelayProtocol,
    mut entries: BTreeMap<String, RelayEntry>,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Vec<RelayTarget>> {
    let mut sources: BTreeMap<String, String> = entries
        .keys()
        .map(|name| {
            (
                name.clone(),
                format!("[relays.{}.{}]", protocol.key(), name),
            )
        })
        .collect();

    let named_var = protocol.relays_env_var();
    if let Some(raw) = strom_types::env::non_blank(env(&named_var)) {
        for (name, entry) in parse_relays_env(&named_var, &raw)? {
            sources.insert(name.clone(), named_var.clone());
            entries.insert(name, entry);
        }
    }

    let (public_var, internal_var) = (protocol.public_env_var(), protocol.internal_env_var());
    let public = strom_types::env::non_blank(env(&public_var));
    let internal = strom_types::env::non_blank(env(&internal_var));
    if public.is_some() || internal.is_some() {
        sources.insert(
            DEFAULT_RELAY_NAME.to_string(),
            format!("{public_var} / {internal_var}"),
        );
        entries.insert(
            DEFAULT_RELAY_NAME.to_string(),
            RelayEntry { public, internal },
        );
    }

    let mut relays = Vec::new();
    for (name, entry) in entries {
        let source = &sources[&name];
        if !is_valid_relay_name(&name) {
            anyhow::bail!(
                "{source}: invalid relay name '{name}': use 1 to {RELAY_NAME_MAX_LEN} characters from a-z, 0-9 and '-'"
            );
        }
        let internal = strom_types::env::non_blank(entry.internal)
            .map(|a| check_address(a.trim(), source, "internal"))
            .transpose()?;
        let Some(public) = strom_types::env::non_blank(entry.public) else {
            if internal.is_some() {
                anyhow::bail!("{source}: relay '{name}' has an internal address but no public one");
            }
            // Nothing set: no relay of this name.
            continue;
        };
        let public = check_address(public.trim(), source, "public")?;
        relays.push(RelayTarget {
            name,
            public,
            internal,
        });
    }
    sort_relays(&mut relays);
    Ok(relays)
}

/// Parse `name=public[|internal]` entries separated by commas.
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
            Some((public, internal)) => (public, Some(internal)),
            None => (addresses, None),
        };
        out.push((
            name,
            RelayEntry {
                public: Some(public.trim().to_string()),
                internal: internal.map(|i| i.trim().to_string()),
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
    fn env_overrides_the_file_per_relay_name() {
        let section = file(&[
            (
                "default",
                Some("srt.example.com:9000"),
                Some("srt-splice:9000"),
            ),
            ("eu-north", Some("srt-eu.example.com:9000"), None),
            ("us", Some("srt-us.example.com:9000"), None),
        ]);
        let relays = Relays::from_settings(
            &section,
            env(&[
                ("STROM_SRT_RELAY_PUBLIC", "srt2.example.com:9000"),
                (
                    "STROM_SRT_RELAYS",
                    "eu-north=srt-eu2.example.com:9000|splice-eu:9000",
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            relays.for_protocol(SRT),
            [
                // The env entry replaces the file entry as a whole: the
                // file's internal address of `default` is gone.
                target("default", "srt2.example.com:9000", None),
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
        for raw in ["eu-north", "eu=h:1,eu=h:2"] {
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
