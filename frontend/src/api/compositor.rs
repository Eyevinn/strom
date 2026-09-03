use super::*;

impl ApiClient {
    /// Get pad properties from a running element in a flow.
    pub async fn get_pad_properties(
        &self,
        flow_id: &str,
        element_id: &str,
        pad_name: &str,
    ) -> ApiResult<std::collections::HashMap<String, strom_types::PropertyValue>> {
        use strom_types::api::PadPropertiesResponse;
        use tracing::info;

        let url = format!(
            "{}/flows/{}/elements/{}/pads/{}/properties",
            self.base_url, flow_id, element_id, pad_name
        );
        info!("Fetching pad properties from: {}", url);

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

        let pad_props_response: PadPropertiesResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!(
            "Successfully fetched {} pad properties",
            pad_props_response.properties.len()
        );
        Ok(pad_props_response.properties)
    }

    /// Update a pad property on a running element in a flow.
    pub async fn update_pad_property(
        &self,
        flow_id: &str,
        element_id: &str,
        pad_name: &str,
        property_name: &str,
        value: strom_types::PropertyValue,
    ) -> ApiResult<()> {
        use strom_types::api::UpdatePadPropertyRequest;
        use tracing::info;

        let url = format!(
            "{}/flows/{}/elements/{}/pads/{}/properties",
            self.base_url, flow_id, element_id, pad_name
        );
        info!(
            "Updating pad property: {} on {}:{} in flow {}",
            property_name, element_id, pad_name, flow_id
        );

        let request = UpdatePadPropertyRequest {
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

        info!("Successfully updated pad property");
        Ok(())
    }

    /// Trigger a transition on a compositor block.
    pub async fn trigger_transition(
        &self,
        flow_id: &str,
        block_id: &str,
        from_input: usize,
        to_input: usize,
        transition_type: &str,
        duration_ms: u64,
    ) -> ApiResult<()> {
        use strom_types::api::TriggerTransitionRequest;
        use tracing::info;

        let url = format!(
            "{}/flows/{}/blocks/{}/transition",
            self.base_url, flow_id, block_id
        );
        info!(
            "Triggering {} transition on {} ({} -> {}, {}ms): {}",
            transition_type, block_id, from_input, to_input, duration_ms, url
        );

        let request = TriggerTransitionRequest {
            from_input,
            to_input,
            transition_type: transition_type.to_string(),
            duration_ms,
            // Stingers are driven from the vision mixer operator page, not
            // from this editor, which serves the first-generation compositor
            // blocks — they have no keyed inputs to play a clip on.
            stinger_source: None,
            stinger_cut_point_ms: None,
            stinger_under_transition: None,
        };

        let response = self
            .with_auth(self.client.post(&url))
            .json(&request)
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

        info!("Successfully triggered transition");
        Ok(())
    }

    /// Animate a single input's position/size on a compositor block.
    #[allow(clippy::too_many_arguments)]
    pub async fn animate_input(
        &self,
        flow_id: &str,
        block_id: &str,
        input: usize,
        xpos: Option<i32>,
        ypos: Option<i32>,
        width: Option<i32>,
        height: Option<i32>,
        duration_ms: u64,
    ) -> ApiResult<()> {
        use strom_types::api::AnimateInputRequest;
        use tracing::info;

        let url = format!(
            "{}/flows/{}/blocks/{}/animate",
            self.base_url, flow_id, block_id
        );
        info!(
            "Animating input {} on {} to ({:?}, {:?}, {:?}, {:?}) over {}ms: {}",
            input, block_id, xpos, ypos, width, height, duration_ms, url
        );

        let request = AnimateInputRequest {
            input,
            xpos,
            ypos,
            width,
            height,
            duration_ms,
        };

        let response = self
            .with_auth(self.client.post(&url))
            .json(&request)
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

        info!("Successfully started animation");
        Ok(())
    }
}
