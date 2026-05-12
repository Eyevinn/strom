use super::*;

impl ApiClient {
    /// List available network interfaces.
    pub async fn list_network_interfaces(
        &self,
    ) -> ApiResult<strom_types::NetworkInterfacesResponse> {
        use tracing::info;

        let url = format!("{}/network/interfaces", self.base_url);
        info!("Fetching network interfaces from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching interfaces: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let interfaces: strom_types::NetworkInterfacesResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse network interfaces response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!(
            "Successfully loaded {} network interfaces",
            interfaces.interfaces.len()
        );
        Ok(interfaces)
    }

    /// List discovered local capture devices in a given category
    /// (e.g. `"video_source"`, `"audio_source"`). Backed by the GStreamer
    /// `DeviceMonitor` running on the backend.
    pub async fn list_devices(
        &self,
        category: &str,
    ) -> ApiResult<Vec<strom_types::discovery::DeviceResponse>> {
        use tracing::info;

        let url = format!("{}/discovery/devices?category={}", self.base_url, category);
        info!("Fetching {} devices from: {}", category, url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching devices: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError::Http(status, text));
        }

        let devices: Vec<strom_types::discovery::DeviceResponse> =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse device list: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Loaded {} {} device(s)", devices.len(), category);
        Ok(devices)
    }

    /// Trigger a re-scan of local capture devices on the backend
    /// (`POST /api/discovery/devices/refresh`).
    pub async fn refresh_devices(&self) -> ApiResult<()> {
        let url = format!("{}/discovery/devices/refresh", self.base_url);
        let response = self
            .with_auth(self.client.post(&url))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError::Http(status, text));
        }
        Ok(())
    }

    /// Get available inter-pipeline channels.
    ///
    /// Returns channels published by running flows with InterOutput blocks.
    pub async fn get_available_sources(
        &self,
    ) -> ApiResult<strom_types::api::AvailableSourcesResponse> {
        use tracing::info;

        let url = format!("{}/sources", self.base_url);
        info!("Fetching available sources from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching sources: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let sources: strom_types::api::AvailableSourcesResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse available sources response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Successfully loaded {} source flows", sources.sources.len());
        Ok(sources)
    }

    /// Get discovered SAP/AES67 streams.
    pub async fn get_discovered_streams(
        &self,
    ) -> ApiResult<Vec<crate::discovery::DiscoveredStream>> {
        let url = format!("{}/discovery/streams", self.base_url);
        tracing::debug!("Fetching discovered streams from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching discovered streams: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let streams: Vec<crate::discovery::DiscoveredStream> =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse discovered streams response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        tracing::debug!("Successfully loaded {} discovered streams", streams.len());
        Ok(streams)
    }

    /// Get streams we are announcing via SAP.
    pub async fn get_announced_streams(&self) -> ApiResult<Vec<crate::discovery::AnnouncedStream>> {
        let url = format!("{}/discovery/announced", self.base_url);
        tracing::debug!("Fetching announced streams from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching announced streams: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let streams: Vec<crate::discovery::AnnouncedStream> =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse announced streams response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        tracing::debug!("Successfully loaded {} announced streams", streams.len());
        Ok(streams)
    }

    /// Get the SDP for a specific discovered stream.
    pub async fn get_stream_sdp(&self, stream_id: &str) -> ApiResult<String> {
        use tracing::info;

        let url = format!("{}/discovery/streams/{}/sdp", self.base_url, stream_id);
        info!("Fetching SDP for stream: {}", stream_id);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching stream SDP: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let sdp = response.text().await.map_err(|e| {
            tracing::error!("Failed to read SDP response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully loaded SDP for stream: {}", stream_id);
        Ok(sdp)
    }

    /// Get discovered NDI sources.
    /// Returns (available, sources) where available indicates if NDI plugin is installed.
    pub async fn get_ndi_sources(&self) -> ApiResult<(bool, Vec<crate::discovery::NdiSource>)> {
        // First check status
        let status_url = format!("{}/discovery/ndi/status", self.base_url);
        tracing::debug!("Fetching NDI status from: {}", status_url);

        let status_response = self
            .with_auth(self.client.get(&status_url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching NDI status: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !status_response.status().is_success() {
            // NDI not available, return empty
            return Ok((false, Vec::new()));
        }

        #[derive(serde::Deserialize)]
        struct NdiStatus {
            available: bool,
        }

        let status: NdiStatus = status_response.json().await.map_err(|e| {
            tracing::error!("Failed to parse NDI status response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        if !status.available {
            return Ok((false, Vec::new()));
        }

        // Fetch sources
        let sources_url = format!("{}/discovery/ndi/sources", self.base_url);
        tracing::debug!("Fetching NDI sources from: {}", sources_url);

        let sources_response = self
            .with_auth(self.client.get(&sources_url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching NDI sources: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !sources_response.status().is_success() {
            let status = sources_response.status().as_u16();
            let text = sources_response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let sources: Vec<crate::discovery::NdiSource> =
            sources_response.json().await.map_err(|e| {
                tracing::error!("Failed to parse NDI sources response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        tracing::debug!("Successfully loaded {} NDI sources", sources.len());
        Ok((true, sources))
    }
}
