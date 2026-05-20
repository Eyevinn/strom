use strom_types::element::ElementInfo;
use strom_types::FlowId;

use super::*;

impl ApiClient {
    /// List available GStreamer elements.
    pub async fn list_elements(&self) -> ApiResult<Vec<ElementInfo>> {
        use strom_types::api::ElementListResponse;
        use tracing::info;

        let url = format!("{}/elements", self.base_url);
        info!("Fetching elements from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching elements: {}", e);
                ApiError::Network(e.to_string())
            })?;

        info!("Elements response status: {}", response.status());

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let element_list: ElementListResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse element list response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!(
            "Successfully loaded {} elements",
            element_list.elements.len()
        );
        Ok(element_list.elements)
    }

    /// Get details about a specific element type.
    pub async fn get_element_info(&self, name: &str) -> ApiResult<ElementInfo> {
        use strom_types::api::ElementInfoResponse;
        use tracing::info;

        let url = format!("{}/elements/{}", self.base_url, name);
        info!("Fetching element info from: {}", url);

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

        let element_response: ElementInfoResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!("Successfully loaded element info for: {}", name);
        Ok(element_response.element)
    }

    /// Get pad properties for a specific element type (on-demand introspection).
    pub async fn get_element_pad_properties(&self, name: &str) -> ApiResult<ElementInfo> {
        use strom_types::api::ElementInfoResponse;
        use tracing::info;

        let url = format!("{}/elements/{}/pads", self.base_url, name);
        info!("Fetching element pad properties from: {}", url);

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

        let element_response: ElementInfoResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!("Successfully loaded pad properties for: {}", name);
        Ok(element_response.element)
    }

    /// Update a property on a live element in a running flow.
    ///
    /// `ramp_ms` is honored by the backend only for properties that support
    /// smooth interpolation (currently audio `volume`-element `volume`).
    /// Other properties ignore it; callers can safely pass `None` or always
    /// pass their preferred fade duration.
    pub async fn update_element_property(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        property_name: &str,
        value: strom_types::PropertyValue,
        ramp_ms: Option<u32>,
    ) -> ApiResult<()> {
        use strom_types::api::UpdatePropertyRequest;
        use tracing::info;

        let url = format!(
            "{}/flows/{}/elements/{}/properties",
            self.base_url, flow_id, element_id
        );
        info!(
            "Updating element property: {} on {} in flow {} (ramp_ms={:?})",
            property_name, element_id, flow_id, ramp_ms
        );

        let request = UpdatePropertyRequest {
            property_name: property_name.to_string(),
            value,
            ramp_ms,
        };

        let response = self
            .with_auth(self.client.patch(&url).json(&request))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network request failed: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        info!("Successfully updated element property");
        Ok(())
    }

    /// Update one or more exposed properties on a block instance live.
    ///
    /// Values are expressed in block-level (user-facing) units (e.g. a bool for
    /// `ch{N}_pfl`, dB for `fader_db`). The backend resolves each to the underlying
    /// element via the block's PropertyMapping and applies the declared transform.
    pub async fn update_block_property(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        property_name: &str,
        value: strom_types::PropertyValue,
        ramp_ms: Option<u32>,
    ) -> ApiResult<()> {
        use std::collections::HashMap;
        let mut properties = HashMap::new();
        properties.insert(property_name.to_string(), value);
        self.update_block_properties(flow_id, block_id, properties, ramp_ms)
            .await
    }

    /// Batched variant of [`Self::update_block_property`]. All writes in `properties`
    /// arrive at the backend in a single PATCH and are applied atomically relative to
    /// any block-level derived state (e.g. the mixer monitor-gate refresh fires once
    /// per batch, not once per property).
    pub async fn update_block_properties(
        &self,
        flow_id: &FlowId,
        block_id: &str,
        properties: std::collections::HashMap<String, strom_types::PropertyValue>,
        ramp_ms: Option<u32>,
    ) -> ApiResult<()> {
        use strom_types::api::UpdateBlockPropertiesRequest;

        let url = format!(
            "{}/flows/{}/blocks/{}/properties",
            self.base_url, flow_id, block_id
        );

        let request = UpdateBlockPropertiesRequest {
            properties,
            ramp_ms,
        };

        let response = self
            .with_auth(self.client.patch(&url).json(&request))
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

    /// Get the debug graph URL for a flow.
    /// Returns the full URL that can be opened in a new tab.
    pub fn get_debug_graph_url(&self, id: FlowId) -> String {
        format!("{}/flows/{}/debug-graph", self.base_url, id)
    }

    /// Get the WHEP player URL for a given endpoint ID.
    /// Returns the full URL that can be opened in a new tab.
    pub fn get_whep_player_url(&self, endpoint_id: &str) -> String {
        // base_url is like "http://localhost:8080/api", we need "http://localhost:8080"
        let server_base = self.base_url.trim_end_matches("/api");
        // WHEP endpoint path (proxy at /whep/{endpoint_id})
        let whep_endpoint = format!("/whep/{}", endpoint_id);
        format!(
            "{}/player/whep?endpoint={}",
            server_base,
            urlencoding::encode(&whep_endpoint)
        )
    }

    /// Get the vision mixer control page URL for a given flow ID and block ID.
    pub fn get_vision_mixer_url(&self, flow_id: &strom_types::FlowId, block_id: &str) -> String {
        let server_base = self.base_url.trim_end_matches("/api");
        format!(
            "{}/player/vision-mixer/{}/{}",
            server_base, flow_id, block_id
        )
    }

    /// Get the WHIP ingest URL for a given endpoint ID.
    /// Returns the full URL that can be opened in a new tab.
    pub fn get_whip_ingest_url(&self, endpoint_id: &str) -> String {
        let server_base = self.base_url.trim_end_matches("/api");
        let whip_endpoint = format!("/whip/{}", endpoint_id);
        format!(
            "{}/player/whip-ingest?endpoint={}",
            server_base,
            urlencoding::encode(&whip_endpoint)
        )
    }
}
