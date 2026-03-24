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
    pub async fn update_element_property(
        &self,
        flow_id: &FlowId,
        element_id: &str,
        property_name: &str,
        value: strom_types::PropertyValue,
    ) -> ApiResult<()> {
        use strom_types::api::UpdatePropertyRequest;
        use tracing::info;

        let url = format!(
            "{}/flows/{}/elements/{}/properties",
            self.base_url, flow_id, element_id
        );
        info!(
            "Updating element property: {} on {} in flow {}",
            property_name, element_id, flow_id
        );

        let request = UpdatePropertyRequest {
            property_name: property_name.to_string(),
            value,
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

    /// Get the vision mixer control page URL for a given flow ID.
    pub fn get_vision_mixer_url(&self, flow_id: &strom_types::FlowId) -> String {
        let server_base = self.base_url.trim_end_matches("/api");
        format!("{}/player/vision-mixer/{}", server_base, flow_id)
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
