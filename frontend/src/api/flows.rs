use strom_types::{Flow, FlowId};

use super::*;

impl ApiClient {
    /// List all flows.
    pub async fn list_flows(&self) -> ApiResult<Vec<Flow>> {
        use strom_types::api::FlowListResponse;
        use tracing::info;

        let url = format!("{}/flows", self.base_url);
        info!("Fetching flows from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching flows: {}", e);
                ApiError::Network(e.to_string())
            })?;

        info!("Flows response status: {}", response.status());

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let flow_list: FlowListResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse flow list response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully loaded {} flows", flow_list.flows.len());
        Ok(flow_list.flows)
    }

    /// Get a specific flow by ID.
    pub async fn get_flow(&self, id: FlowId) -> ApiResult<Flow> {
        use strom_types::api::FlowResponse;
        use tracing::info;

        let url = format!("{}/flows/{}", self.base_url, id);
        info!("Fetching flow from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        info!("Flow response status: {}", response.status());

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(ApiError::Http(status, text));
        }

        let flow_response: FlowResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse flow response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully fetched flow: {}", flow_response.flow.name);
        Ok(flow_response.flow)
    }

    /// Create a new flow.
    pub async fn create_flow(&self, flow: &Flow) -> ApiResult<Flow> {
        use strom_types::api::FlowResponse;
        use tracing::info;

        let url = format!("{}/flows", self.base_url);
        info!("Creating flow via API: POST {}", url);
        info!("Flow data: name='{}'", flow.name);

        let response = self
            .with_auth(self.client.post(&url).json(flow))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network request failed: {}", e);
                ApiError::Network(e.to_string())
            })?;

        info!("Response status: {}", response.status());

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let flow_response: FlowResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse flow response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully created flow: {}", flow_response.flow.name);
        Ok(flow_response.flow)
    }

    /// Update an existing flow.
    pub async fn update_flow(&self, flow: &Flow) -> ApiResult<Flow> {
        use strom_types::api::FlowResponse;
        use tracing::info;

        let url = format!("{}/flows/{}", self.base_url, flow.id);
        info!("Updating flow via API: POST {}", url);
        info!(
            "Flow data: id={}, name='{}', elements={}, links={}",
            flow.id,
            flow.name,
            flow.elements.len(),
            flow.links.len()
        );

        let response = self
            .with_auth(self.client.post(&url).json(flow))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network request failed: {}", e);
                ApiError::Network(e.to_string())
            })?;

        info!("Response status: {}", response.status());

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let flow_response: FlowResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse flow response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully updated flow: {}", flow_response.flow.name);
        Ok(flow_response.flow)
    }

    /// Delete a flow.
    pub async fn delete_flow(&self, id: FlowId) -> ApiResult<()> {
        let url = format!("{}/flows/{}", self.base_url, id);
        let response = self
            .with_auth(self.client.delete(&url))
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

    /// Start a flow.
    pub async fn start_flow(&self, id: FlowId) -> ApiResult<()> {
        let url = format!("{}/flows/{}/start", self.base_url, id);
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

    /// Stop a flow.
    pub async fn stop_flow(&self, id: FlowId) -> ApiResult<()> {
        let url = format!("{}/flows/{}/stop", self.base_url, id);
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

    /// Get latency information for a running flow.
    pub async fn get_flow_latency(
        &self,
        id: FlowId,
    ) -> ApiResult<strom_types::api::LatencyResponse> {
        let url = format!("{}/flows/{}/latency", self.base_url, id);
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

        let latency_info: strom_types::api::LatencyResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        Ok(latency_info)
    }

    /// Get dynamic pads for a running flow (pads created at runtime by elements like decodebin).
    pub async fn get_dynamic_pads(
        &self,
        id: FlowId,
    ) -> ApiResult<std::collections::HashMap<String, std::collections::HashMap<String, String>>>
    {
        let url = format!("{}/flows/{}/dynamic-pads", self.base_url, id);
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

        let response: strom_types::api::DynamicPadsResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        Ok(response.pads)
    }

    /// Reset accumulated loudness measurements on an EBU R128 meter block.
    pub async fn reset_loudness(&self, flow_id: &FlowId, block_id: &str) -> ApiResult<()> {
        let url = format!(
            "{}/flows/{}/blocks/{}/loudness/reset",
            self.base_url, flow_id, block_id
        );
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

    /// Trigger an immediate file split on a recorder block.
    pub async fn recorder_split_now(&self, flow_id: &FlowId, block_id: &str) -> ApiResult<()> {
        let url = format!(
            "{}/flows/{}/blocks/{}/recorder/split",
            self.base_url, flow_id, block_id
        );
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
}
