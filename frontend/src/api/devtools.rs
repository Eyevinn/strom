//! Remote control links for HTML sources.

use super::{ApiClient, ApiError, ApiResult};
use strom_types::devtools::DevToolsLink;

impl ApiClient {
    /// Ask the server for a link that drives one HTML source remotely.
    ///
    /// The server decides which browser the block is rendering and hands back
    /// an opaque path; the caller never sees a Chromium target id.
    pub async fn create_block_devtools_link(
        &self,
        flow_id: &str,
        block_id: &str,
    ) -> ApiResult<DevToolsLink> {
        let url = format!(
            "{}/flows/{}/blocks/{}/devtools/link",
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

        response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))
    }
}
