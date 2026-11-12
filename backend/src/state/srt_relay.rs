//! SRT relay configuration and status.

use super::AppState;
use crate::srt_relay::SrtRelayConfig;
use strom_types::srt_relay::SrtRelayInfo;
use tracing::info;

impl AppState {
    /// Configure the SRT relay. Called once from main after the
    /// configuration is loaded. The relay is not contacted: it may start
    /// after Strom.
    pub fn configure_srt_relay(&self, config: Option<SrtRelayConfig>) {
        if let Some(config) = &config {
            match &config.internal {
                Some(internal) => info!(
                    "SRT relay configured: public {}, dialed at {}",
                    config.public, internal
                ),
                None => info!("SRT relay configured: {}", config.public),
            }
        }
        *self.inner.srt_relay.write() = config;
    }

    /// The relay `host:port` that `srt+relay://` URIs resolve to, if configured.
    pub fn srt_relay_address(&self) -> Option<String> {
        self.inner
            .srt_relay
            .read()
            .as_ref()
            .map(|c| c.dial_address().to_string())
    }

    /// SRT relay status for `GET /api/srt-relay`.
    pub fn srt_relay_info(&self) -> SrtRelayInfo {
        self.inner
            .srt_relay
            .read()
            .as_ref()
            .map(SrtRelayConfig::info)
            .unwrap_or_default()
    }
}
