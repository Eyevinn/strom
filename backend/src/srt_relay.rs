//! SRT relay (srt-splice) configuration.
//!
//! Strom's SRT blocks can dial out to an SRT relay instead of listening on a
//! public port (`srt+relay://<id>`, resolved in `strom_types::srt_relay`).
//! Like the TURN servers, the relay is configured by address: Strom never asks
//! the relay anything, so the relay may start after Strom and need not expose
//! an HTTP API to it.

use strom_types::srt_relay::SrtRelayInfo;

/// SRT relay settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtRelayConfig {
    /// The relay's public SRT `host:port`, which the outside party dials
    /// (`STROM_SRT_RELAY`). Strom dials it too unless `internal` is set.
    pub public: String,
    /// The `host:port` Strom dials when it sits next to the relay
    /// (`STROM_SRT_RELAY_INTERNAL`), to avoid hairpin NAT.
    pub internal: Option<String>,
}

impl SrtRelayConfig {
    /// Build the settings from the configured values. `public` unset or blank
    /// switches the feature off. Only the format is checked: the relay need
    /// not be reachable.
    pub fn from_settings(
        public: Option<String>,
        internal: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        let internal = strom_types::env::non_blank(internal)
            .map(|a| check_address(a.trim(), "srt_relay.internal / STROM_SRT_RELAY_INTERNAL"))
            .transpose()?;
        let Some(public) = strom_types::env::non_blank(public) else {
            return Ok(None);
        };
        let public = check_address(public.trim(), "srt_relay.relay / STROM_SRT_RELAY")?;
        Ok(Some(Self { public, internal }))
    }

    /// The SRT `host:port` Strom dials, which `srt+relay://` URIs resolve to:
    /// the internal address if set, else the public one.
    pub fn dial_address(&self) -> &str {
        self.internal.as_deref().unwrap_or(&self.public)
    }

    /// The status `GET /api/srt-relay` reports.
    pub fn info(&self) -> SrtRelayInfo {
        SrtRelayInfo {
            configured: true,
            relay: Some(self.dial_address().to_string()),
            public: Some(self.public.clone()),
            error: None,
        }
    }
}

/// `address` as an owned string if it is `host:port`, else a config error
/// naming `setting`.
fn check_address(address: &str, setting: &str) -> anyhow::Result<String> {
    if is_host_port(address) {
        Ok(address.to_string())
    } else {
        anyhow::bail!("{setting} must be host:port, got {address}")
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

    fn config(public: &str, internal: Option<&str>) -> anyhow::Result<Option<SrtRelayConfig>> {
        SrtRelayConfig::from_settings(Some(public.to_string()), internal.map(str::to_string))
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
    fn unset_or_blank_relay_is_off() {
        assert_eq!(SrtRelayConfig::from_settings(None, None).unwrap(), None);
        assert_eq!(config("  ", None).unwrap(), None);
    }

    #[test]
    fn public_only_is_dialed_and_shown() {
        let c = config(" srt.example.com:9000 ", None).unwrap().unwrap();
        assert_eq!(c.dial_address(), "srt.example.com:9000");
        assert_eq!(
            c.info(),
            SrtRelayInfo {
                configured: true,
                relay: Some("srt.example.com:9000".into()),
                public: Some("srt.example.com:9000".into()),
                error: None,
            }
        );
        let c = config("[2001:db8::1]:9000", None).unwrap().unwrap();
        assert_eq!(c.dial_address(), "[2001:db8::1]:9000");
    }

    #[test]
    fn internal_is_dialed_and_public_is_shown() {
        let c = config("srt.example.com:9000", Some("srt-splice:9000"))
            .unwrap()
            .unwrap();
        assert_eq!(c.dial_address(), "srt-splice:9000");
        assert_eq!(
            c.info(),
            SrtRelayInfo {
                configured: true,
                relay: Some("srt-splice:9000".into()),
                public: Some("srt.example.com:9000".into()),
                error: None,
            }
        );
        // A blank internal address is unset.
        let c = config("srt.example.com:9000", Some(" ")).unwrap().unwrap();
        assert_eq!(c.internal, None);
    }

    #[test]
    fn malformed_relay_is_a_config_error() {
        for bad in BAD {
            let err = config(bad, None).unwrap_err().to_string();
            assert!(err.contains("STROM_SRT_RELAY"), "{bad}: {err}");
        }
    }

    #[test]
    fn malformed_internal_is_a_config_error() {
        for bad in BAD {
            let err = config("srt.example.com:9000", Some(bad))
                .unwrap_err()
                .to_string();
            assert!(err.contains("STROM_SRT_RELAY_INTERNAL"), "{bad}: {err}");
        }
    }
}
