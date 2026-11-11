//! SRT relay configuration and status.

use super::AppState;
use crate::srt_relay::{SrtRelayConfig, SrtRelayService};
use std::sync::Arc;
use strom_types::srt_relay::SrtRelayInfo;
use tracing::info;

impl AppState {
    /// Configure the SRT relay. Called once from main after the
    /// configuration is loaded. The relay is not contacted here: it may start
    /// after Strom.
    pub fn configure_srt_relay(&self, config: Option<SrtRelayConfig>) {
        if let Some(config) = &config {
            info!(
                "SRT relay configured: {} (API {}{})",
                config.relay,
                config.api,
                config
                    .public
                    .as_ref()
                    .map(|p| format!(", public {p}"))
                    .unwrap_or_default()
            );
        }
        *self.inner.srt_relay.write() = config.map(|c| Arc::new(SrtRelayService::new(c)));
    }

    /// The relay `host:port` that `srt+relay://` URIs resolve to, if configured.
    pub fn srt_relay_address(&self) -> Option<String> {
        self.inner
            .srt_relay
            .read()
            .as_ref()
            .map(|s| s.relay_address().to_string())
    }

    /// SRT relay status for `GET /api/srt-relay`.
    pub async fn srt_relay_info(&self) -> SrtRelayInfo {
        let service = self.inner.srt_relay.read().clone();
        match service {
            Some(service) => service.info().await,
            None => SrtRelayInfo::default(),
        }
    }
}
