use super::*;

impl ApiClient {
    /// Get version and build information from the backend.
    pub async fn get_version(&self) -> ApiResult<strom_types::api::SystemInfo> {
        use tracing::info;

        let url = format!("{}/version", self.base_url);
        info!("Fetching version info from: {}", url);

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

        let version_info: strom_types::api::SystemInfo = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!("Successfully fetched version: v{}", version_info.version);
        Ok(version_info)
    }

    /// Get system clock synchronization state from the kernel.
    ///
    /// Returns `ApiError::Http(501, _)` on platforms where kernel clock
    /// discipline state is not exposed (macOS, Windows, etc.).
    pub async fn get_system_clock(&self) -> ApiResult<strom_types::api::SystemClockInfo> {
        let url = format!("{}/system/clock", self.base_url);
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
        response
            .json::<strom_types::api::SystemClockInfo>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))
    }

    /// Check authentication status and whether auth is required.
    pub async fn get_auth_status(&self) -> ApiResult<strom_types::api::AuthStatusResponse> {
        use tracing::info;

        let url = format!("{}/auth/status", self.base_url);
        info!("Fetching auth status from: {}", url);

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

        let auth_status: strom_types::api::AuthStatusResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!(
            "Auth status: required={}, authenticated={}",
            auth_status.auth_required, auth_status.authenticated
        );
        Ok(auth_status)
    }

    /// Logout the current session.
    pub async fn logout(&self) -> ApiResult<()> {
        use tracing::info;

        let url = format!("{}/logout", self.base_url);
        info!("Logging out");

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

        info!("Logged out successfully");
        Ok(())
    }
}
