//! Relay configuration.

use super::AppState;
use crate::relay::Relays;
use strom_types::relay::{RelayProtocol, RelayTarget};
use tracing::info;

impl AppState {
    /// Configure the relays. Called once from main after the configuration
    /// is loaded. No relay is contacted: they may start after Strom.
    pub fn configure_relays(&self, relays: Relays) {
        for (protocol, relay) in relays.iter() {
            match &relay.internal {
                Some(internal) => info!(
                    "{} relay '{}' configured: public {}, dialed at {}",
                    protocol.env_name(),
                    relay.name,
                    relay.public,
                    internal
                ),
                None => info!(
                    "{} relay '{}' configured: {}",
                    protocol.env_name(),
                    relay.name,
                    relay.public
                ),
            }
        }
        *self.inner.relays.write() = relays;
    }

    /// The configured relays of `protocol`, `default` first, with their
    /// internal addresses. Never hand these to a client.
    pub fn relay_targets(&self, protocol: RelayProtocol) -> Vec<RelayTarget> {
        self.inner.relays.read().for_protocol(protocol).to_vec()
    }
}
