use serde::Serialize;
use strom_types::FlowId;

use super::*;

impl ApiClient {
    /// Set the playlist for a media player block.
    pub async fn set_player_playlist(
        &self,
        flow_id: FlowId,
        block_id: &str,
        files: Vec<String>,
    ) -> ApiResult<()> {
        use tracing::info;

        let url = format!(
            "{}/flows/{}/blocks/{}/player/playlist",
            self.base_url, flow_id, block_id
        );
        info!(
            "Setting playlist for player {}: {} files",
            block_id,
            files.len()
        );

        #[derive(Serialize)]
        struct SetPlaylistRequest {
            files: Vec<String>,
        }

        let response = self
            .with_auth(self.client.post(&url))
            .json(&SetPlaylistRequest { files })
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error setting playlist: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        info!("Successfully set playlist for player {}", block_id);
        Ok(())
    }

    /// Control a media player block (play, pause, next, prev).
    pub async fn control_player(
        &self,
        flow_id: FlowId,
        block_id: &str,
        action: &str,
    ) -> ApiResult<()> {
        use tracing::info;

        let url = format!(
            "{}/flows/{}/blocks/{}/player/control",
            self.base_url, flow_id, block_id
        );
        info!("Controlling player {}: {}", block_id, action);

        #[derive(Serialize)]
        struct ControlRequest {
            action: String,
        }

        let response = self
            .with_auth(self.client.post(&url))
            .json(&ControlRequest {
                action: action.to_string(),
            })
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error controlling player: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        info!("Successfully sent {} to player {}", action, block_id);
        Ok(())
    }

    /// Jump a media player to playlist entry `index` and play it.
    pub async fn goto_player(
        &self,
        flow_id: FlowId,
        block_id: &str,
        index: usize,
    ) -> ApiResult<()> {
        let url = format!(
            "{}/flows/{}/blocks/{}/player/goto",
            self.base_url, flow_id, block_id
        );

        #[derive(Serialize)]
        struct GotoRequest {
            index: usize,
        }

        let response = self
            .with_auth(self.client.post(&url))
            .json(&GotoRequest { index })
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error jumping to playlist entry: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }
        Ok(())
    }

    /// Seek a media player to a specific position.
    pub async fn seek_player(
        &self,
        flow_id: FlowId,
        block_id: &str,
        position_ns: u64,
    ) -> ApiResult<()> {
        use tracing::info;

        let url = format!(
            "{}/flows/{}/blocks/{}/player/seek",
            self.base_url, flow_id, block_id
        );
        info!("Seeking player {} to {} ns", block_id, position_ns);

        #[derive(Serialize)]
        struct SeekRequest {
            position_ns: u64,
        }

        let response = self
            .with_auth(self.client.post(&url))
            .json(&SeekRequest { position_ns })
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error seeking player: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        info!("Successfully seeked player {}", block_id);
        Ok(())
    }
}
