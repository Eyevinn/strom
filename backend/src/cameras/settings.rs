//! Where phones reach this Strom: the `[cameras]` configuration section.

use serde::{Deserialize, Serialize};

/// `[cameras]` section of the configuration file
/// (`STROM_CAMERAS_PUBLIC_HOST`, `STROM_CAMERAS_DEVICE_URL`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CamerasConfig {
    /// Host name or address phones use for this Strom, in the SRT URLs they
    /// are told to send to and in invites. Unset: the host the phone (or the
    /// operator's browser, for invites) used, or the first LAN address when
    /// that is loopback.
    #[serde(default)]
    pub public_host: Option<String>,
    /// Full device WebSocket URL for invites when a proxy sits in front, e.g.
    /// `wss://strom.example.com/api/cameras/device`.
    #[serde(default)]
    pub device_url: Option<String>,
}

impl CamerasConfig {
    /// A blank value (an empty environment variable) means unset.
    pub fn normalized(self) -> Self {
        Self {
            public_host: strom_types::env::non_blank(self.public_host),
            device_url: strom_types::env::non_blank(self.device_url),
        }
    }

    /// The settings the camera hub runs with, on Strom's HTTP port.
    pub fn settings(&self, http_port: u16, tls: bool) -> CameraSettings {
        CameraSettings {
            public_host: self.public_host.clone(),
            device_url: self.device_url.clone(),
            http_port,
            tls,
        }
    }
}

/// Where phones reach this Strom, as the camera hub uses it.
#[derive(Debug, Clone, Default)]
pub struct CameraSettings {
    /// See [`CamerasConfig::public_host`].
    pub public_host: Option<String>,
    /// See [`CamerasConfig::device_url`].
    pub device_url: Option<String>,
    /// Strom's HTTP port, for invite URLs built from a host.
    pub http_port: u16,
    /// Strom serves HTTPS, so invites use `wss://`.
    pub tls: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::{Format, Toml};

    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        cameras: CamerasConfig,
    }

    fn parse(toml: &str) -> CamerasConfig {
        figment::Figment::from(Toml::string(toml))
            .extract::<File>()
            .unwrap()
            .cameras
            .normalized()
    }

    #[test]
    fn section_parses_and_blank_values_are_unset() {
        let config = parse(
            r#"
[cameras]
public_host = "strom.example.com"
device_url = "  "
"#,
        );
        assert_eq!(config.public_host.as_deref(), Some("strom.example.com"));
        assert_eq!(config.device_url, None);
        assert_eq!(parse(""), CamerasConfig::default());

        let settings = config.settings(8443, true);
        assert_eq!(settings.public_host.as_deref(), Some("strom.example.com"));
        assert_eq!(settings.http_port, 8443);
        assert!(settings.tls);
    }
}
