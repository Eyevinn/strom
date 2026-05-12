use strom_types::FlowId;

use super::*;

impl ApiClient {
    /// Get WebRTC statistics from a running flow.
    pub async fn get_webrtc_stats(&self, id: FlowId) -> ApiResult<strom_types::api::WebRtcStats> {
        use strom_types::api::WebRtcStatsResponse;
        use tracing::trace;

        let url = format!("{}/flows/{}/webrtc-stats", self.base_url, id);
        trace!("Fetching WebRTC stats from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError::Http(status, text));
        }

        let stats_response: WebRtcStatsResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        trace!(
            "Successfully fetched WebRTC stats: {} connections",
            stats_response.stats.connections.len()
        );
        Ok(stats_response.stats)
    }

    /// Get SRT statistics from a running flow.
    pub async fn get_srt_stats(&self, id: FlowId) -> ApiResult<strom_types::api::SrtStats> {
        use strom_types::api::SrtStatsResponse;
        use tracing::trace;

        let url = format!("{}/flows/{}/srt-stats", self.base_url, id);
        trace!("Fetching SRT stats from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError::Http(status, text));
        }

        let stats_response: SrtStatsResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        trace!(
            "Successfully fetched SRT stats: {} connections",
            stats_response.stats.connections.len()
        );
        Ok(stats_response.stats)
    }

    /// Get RTP statistics for a running flow (jitterbuffer stats from AES67 Input blocks).
    pub async fn get_flow_rtp_stats(
        &self,
        id: FlowId,
    ) -> ApiResult<strom_types::api::FlowStatsResponse> {
        let url = format!("{}/flows/{}/rtp-stats", self.base_url, id);
        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError::Http(status, text));
        }

        let rtp_stats_info: strom_types::api::FlowStatsResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        Ok(rtp_stats_info)
    }
}
