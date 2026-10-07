use super::*;

impl ApiClient {
    /// List contents of a media directory.
    pub async fn list_media(&self, path: &str) -> ApiResult<strom_types::api::ListMediaResponse> {
        let url = format!("{}/media?path={}", self.base_url, urlencoding::encode(path));
        tracing::debug!("Fetching media listing from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching media listing: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let listing: strom_types::api::ListMediaResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse media listing response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        tracing::debug!("Successfully loaded {} entries", listing.entries.len());
        Ok(listing)
    }

    /// Get the download URL for a media file.
    pub fn get_media_download_url(&self, path: &str) -> String {
        format!("{}/media/file/{}", self.base_url, urlencoding::encode(path))
    }

    /// Upload a file to the media directory (WASM only).
    #[cfg(target_arch = "wasm32")]
    pub async fn upload_media(
        &self,
        target_path: &str,
        filename: &str,
        data: Vec<u8>,
    ) -> ApiResult<strom_types::api::MediaOperationResponse> {
        use tracing::info;

        let url = format!(
            "{}/media/upload?path={}",
            self.base_url,
            urlencoding::encode(target_path)
        );
        info!("Uploading file {} to: {}", filename, url);

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| ApiError::Network(e.to_string()))?;

        let form = reqwest::multipart::Form::new().part("file", part);

        let response = self
            .with_auth(self.client.post(&url).multipart(form))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error uploading file: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let result: strom_types::api::MediaOperationResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse upload response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Upload result: {}", result.message);
        Ok(result)
    }

    /// Rename a file or directory.
    pub async fn rename_media(
        &self,
        old_path: &str,
        new_name: &str,
    ) -> ApiResult<strom_types::api::MediaOperationResponse> {
        use tracing::info;

        let url = format!("{}/media/rename", self.base_url);
        info!("Renaming {} to {}", old_path, new_name);

        let request = strom_types::api::RenameMediaRequest {
            old_path: old_path.to_string(),
            new_name: new_name.to_string(),
        };

        let response = self
            .with_auth(self.client.post(&url).json(&request))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error renaming media: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let result: strom_types::api::MediaOperationResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse rename response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Rename result: {}", result.message);
        Ok(result)
    }

    /// Delete a file.
    pub async fn delete_media_file(
        &self,
        path: &str,
    ) -> ApiResult<strom_types::api::MediaOperationResponse> {
        use tracing::info;

        let url = format!("{}/media/file/{}", self.base_url, urlencoding::encode(path));
        info!("Deleting file: {}", path);

        let response = self
            .with_auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error deleting file: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let result: strom_types::api::MediaOperationResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse delete response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Delete result: {}", result.message);
        Ok(result)
    }

    /// Create a directory.
    pub async fn create_media_directory(
        &self,
        path: &str,
    ) -> ApiResult<strom_types::api::MediaOperationResponse> {
        use tracing::info;

        let url = format!("{}/media/directory", self.base_url);
        info!("Creating directory: {}", path);

        let request = strom_types::api::CreateDirectoryRequest {
            path: path.to_string(),
        };

        let response = self
            .with_auth(self.client.post(&url).json(&request))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error creating directory: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let result: strom_types::api::MediaOperationResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse create directory response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Create directory result: {}", result.message);
        Ok(result)
    }

    /// Delete a directory (must be empty).
    pub async fn delete_media_directory(
        &self,
        path: &str,
    ) -> ApiResult<strom_types::api::MediaOperationResponse> {
        use tracing::info;

        let url = format!(
            "{}/media/directory/{}",
            self.base_url,
            urlencoding::encode(path)
        );
        info!("Deleting directory: {}", path);

        let response = self
            .with_auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error deleting directory: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let result: strom_types::api::MediaOperationResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse delete directory response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Delete directory result: {}", result.message);
        Ok(result)
    }

    /// Start downloading a URL into a media directory. Returns once the
    /// server has the response headers; progress arrives over the WebSocket.
    pub async fn download_media_url(
        &self,
        url: &str,
        target_path: &str,
        overwrite: bool,
    ) -> ApiResult<strom_types::media_download::MediaDownloadJob> {
        let endpoint = format!("{}/media/download", self.base_url);
        tracing::info!("Requesting media download into '{}'", target_path);

        let request = strom_types::media_download::MediaDownloadRequest {
            url: url.to_string(),
            path: target_path.to_string(),
            filename: None,
            overwrite,
        };

        let response = self
            .with_auth(self.client.post(&endpoint).json(&request))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error starting media download: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::warn!("Media download refused ({}): {}", status, text);
            // Show the server's own explanation rather than raw JSON.
            let message = serde_json::from_str::<strom_types::api::ErrorResponse>(&text)
                .map(|e| e.error)
                .unwrap_or(text);
            return Err(ApiError::Http(status, message));
        }

        response.json().await.map_err(|e| {
            tracing::error!("Failed to parse media download response: {}", e);
            ApiError::Decode(e.to_string())
        })
    }

    /// List active and recently finished URL downloads.
    pub async fn list_media_downloads(
        &self,
    ) -> ApiResult<strom_types::media_download::MediaDownloadListResponse> {
        let url = format!("{}/media/downloads", self.base_url);

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
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))
    }

    /// Cancel a running URL download.
    pub async fn cancel_media_download(
        &self,
        job_id: &str,
    ) -> ApiResult<strom_types::api::MediaOperationResponse> {
        let url = format!(
            "{}/media/downloads/{}",
            self.base_url,
            urlencoding::encode(job_id)
        );

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

        response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))
    }
}
