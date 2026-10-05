use super::*;

impl ApiClient {
    /// Parse a gst-launch-1.0 pipeline string and return elements and links.
    ///
    /// This uses the backend's GStreamer parser to ensure complete compatibility
    /// with the gst-launch-1.0 syntax.
    pub async fn parse_gst_launch(
        &self,
        pipeline: &str,
    ) -> ApiResult<strom_types::api::ParseGstLaunchResponse> {
        use tracing::info;

        let url = format!("{}/gst-launch/parse", self.base_url);
        info!("Parsing gst-launch pipeline via API: POST {}", url);

        let request = strom_types::api::ParseGstLaunchRequest {
            pipeline: pipeline.to_string(),
        };

        let response = self
            .with_auth(self.client.post(&url).json(&request))
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

        let parse_response: strom_types::api::ParseGstLaunchResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!(
            "Successfully parsed pipeline: {} elements, {} links",
            parse_response.elements.len(),
            parse_response.links.len()
        );
        Ok(parse_response)
    }

    /// Export elements and links to gst-launch-1.0 syntax.
    pub async fn export_gst_launch(
        &self,
        elements: &[strom_types::Element],
        links: &[strom_types::element::Link],
    ) -> ApiResult<String> {
        use tracing::info;

        let url = format!("{}/gst-launch/export", self.base_url);
        info!("Exporting to gst-launch syntax via API: POST {}", url);

        let request = strom_types::api::ExportGstLaunchRequest {
            elements: elements.to_vec(),
            links: links.to_vec(),
        };

        let response = self
            .with_auth(self.client.post(&url).json(&request))
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

        let export_response: strom_types::api::ExportGstLaunchResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Successfully exported pipeline");
        Ok(export_response.pipeline)
    }
}
