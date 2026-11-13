//! Loading `GET /api/capabilities` (#1257): what the UI knows about this
//! Strom's network, and how a failed load is retried.

use std::time::Duration;
use strom_types::capabilities::Capabilities;

/// What the UI knows about this Strom's capabilities.
#[derive(Debug, Clone, PartialEq)]
pub enum CapabilitiesState {
    /// Not loaded yet.
    Loading,
    /// The Strom has no capabilities endpoint (an older release): nothing to
    /// warn about.
    Unsupported,
    /// The last attempt failed and no earlier one succeeded. Retried.
    Failed(String),
    /// Loaded.
    Loaded(Capabilities),
}

/// The outcome of one attempt to load the capabilities.
#[derive(Debug)]
pub enum CapabilitiesUpdate {
    Loaded(Capabilities),
    /// 404: the endpoint does not exist on this Strom.
    Unsupported,
    Failed(String),
}

impl CapabilitiesState {
    /// Apply the outcome of an attempt. A failure does not replace loaded
    /// capabilities: they follow the server configuration, and a reload that
    /// fails (the server restarting) keeps what is known until a retry
    /// succeeds.
    pub fn apply(&mut self, update: CapabilitiesUpdate) {
        *self = match (update, &*self) {
            (CapabilitiesUpdate::Loaded(caps), _) => Self::Loaded(caps),
            (CapabilitiesUpdate::Unsupported, _) => Self::Unsupported,
            (CapabilitiesUpdate::Failed(_), Self::Loaded(_)) => return,
            (CapabilitiesUpdate::Failed(error), _) => Self::Failed(error),
        };
    }
}

/// First delay before retrying a failed load.
pub const RETRY_INITIAL: Duration = Duration::from_secs(1);
/// Longest delay between retries.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// The delay after `delay`: doubled, at most [`RETRY_MAX`].
pub fn next_retry_delay(delay: Duration) -> Duration {
    (delay * 2).min(RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::capabilities::{
        NetworkCapabilities, ProtocolCapabilities, SrtCapabilities, SrtModes, WebRtcCapabilities,
    };

    fn caps() -> Capabilities {
        Capabilities {
            network: NetworkCapabilities {
                inbound_reachable: false,
                ports: None,
            },
            protocols: ProtocolCapabilities {
                srt: SrtCapabilities {
                    modes: SrtModes {
                        listener: false,
                        caller: true,
                        rendezvous: false,
                    },
                    relays: Vec::new(),
                },
                webrtc: WebRtcCapabilities {
                    ice_servers: false,
                    ice_transport_policy: "all".to_string(),
                },
            },
        }
    }

    #[test]
    fn failure_then_success() {
        let mut state = CapabilitiesState::Loading;
        state.apply(CapabilitiesUpdate::Failed("HTTP 502".to_string()));
        assert_eq!(state, CapabilitiesState::Failed("HTTP 502".to_string()));
        state.apply(CapabilitiesUpdate::Loaded(caps()));
        assert_eq!(state, CapabilitiesState::Loaded(caps()));
    }

    #[test]
    fn a_failed_reload_keeps_loaded_capabilities() {
        let mut state = CapabilitiesState::Loaded(caps());
        state.apply(CapabilitiesUpdate::Failed("network".to_string()));
        assert_eq!(state, CapabilitiesState::Loaded(caps()));
    }

    #[test]
    fn unsupported() {
        let mut state = CapabilitiesState::Loading;
        state.apply(CapabilitiesUpdate::Unsupported);
        assert_eq!(state, CapabilitiesState::Unsupported);
    }

    #[test]
    fn retry_delay_doubles_up_to_the_max() {
        let delays: Vec<u64> =
            std::iter::successors(Some(RETRY_INITIAL), |d| Some(next_retry_delay(*d)))
                .take(8)
                .map(|d| d.as_secs())
                .collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
