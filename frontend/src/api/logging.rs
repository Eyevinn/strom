//! Log level API client methods.

use super::{ApiClient, ApiError, ApiResult};
use strom_types::api::{LogLevelResponse, SetLogLevelRequest};

impl ApiClient {
    /// Get the current log level from the backend.
    pub async fn get_log_level(&self) -> ApiResult<LogLevelResponse> {
        let url = format!("{}/log-level", self.base_url);
        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(ApiError::Http(
                response.status().as_u16(),
                response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string()),
            ));
        }

        response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))
    }

    /// Set the log level on the backend.
    pub async fn set_log_level(&self, filter: &str) -> ApiResult<LogLevelResponse> {
        let url = format!("{}/log-level", self.base_url);
        let req = SetLogLevelRequest {
            filter: filter.to_string(),
        };
        let response = self
            .with_auth(self.client.put(&url).json(&req))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(ApiError::Http(
                response.status().as_u16(),
                response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string()),
            ));
        }

        response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))
    }
}
