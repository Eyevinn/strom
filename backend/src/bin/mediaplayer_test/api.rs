//! HTTP client for Strom API
//!
//! Provides a type-safe interface for interacting with the Strom server API.

#![allow(dead_code)] // Some methods reserved for future tests

use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use strom_types::{Flow, FlowId};
use uuid::Uuid;

/// Client for the Strom API
pub struct StromClient {
    client: Client,
    base_url: String,
}

/// Response from creating a flow
#[derive(Debug, Deserialize)]
struct FlowResponse {
    flow: FlowData,
}

#[derive(Debug, Deserialize)]
struct FlowData {
    id: Uuid,
    name: String,
}

/// Response from listing flows
#[derive(Debug, Deserialize)]
struct FlowListResponse {
    flows: Vec<FlowData>,
}

/// Player state response
#[derive(Debug, Deserialize)]
pub struct PlayerStateResponse {
    pub state: String,
    pub position_ns: u64,
    pub duration_ns: u64,
    pub current_file_index: usize,
    pub total_files: usize,
    pub current_file: Option<String>,
    pub playlist: Vec<String>,
    pub loop_playlist: bool,
}

/// Player control request
#[derive(Debug, Serialize)]
struct PlayerControlRequest {
    action: String,
}

/// Seek request
#[derive(Debug, Serialize)]
struct SeekRequest {
    position_ns: u64,
}

/// Set playlist request
#[derive(Debug, Serialize)]
struct SetPlaylistRequest {
    files: Vec<String>,
}

/// Goto file request
#[derive(Debug, Serialize)]
struct GotoFileRequest {
    index: usize,
}

impl StromClient {
    /// Create a new client for the given port
    pub fn new(port: u16) -> Self {
        Self {
            client: Client::new(),
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    /// Wait for the server to be ready
    pub async fn wait_for_ready(&self, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        let mut dots = 0;

        while start.elapsed() < timeout {
            match self
                .client
                .get(format!("{}/api/version", self.base_url))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    println!(); // New line after dots
                    return Ok(());
                }
                _ => {
                    // Print progress dots
                    dots += 1;
                    if dots % 2 == 0 {
                        eprint!(".");
                        let _ = std::io::stderr().flush();
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }

        println!(); // New line after dots
        Err(anyhow!("Server did not become ready within timeout"))
    }

    /// Create a new flow
    pub async fn create_flow(&self, flow: &Flow) -> Result<FlowId> {
        // First create an empty flow
        let resp = self
            .client
            .post(format!("{}/api/flows", self.base_url))
            .json(&serde_json::json!({ "name": &flow.name }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to create flow: {}", text));
        }

        let flow_resp: FlowResponse = resp.json().await?;
        let flow_id = flow_resp.flow.id;

        // Then update it with the full content
        let mut full_flow = flow.clone();
        full_flow.id = flow_id;

        let resp = self
            .client
            .post(format!("{}/api/flows/{}", self.base_url, flow_id))
            .json(&full_flow)
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to update flow: {}", text));
        }

        Ok(flow_id)
    }

    /// Start a flow
    pub async fn start_flow(&self, flow_id: FlowId) -> Result<()> {
        let resp = self
            .client
            .post(format!("{}/api/flows/{}/start", self.base_url, flow_id))
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to start flow: {}", text));
        }

        Ok(())
    }

    /// Stop a flow
    pub async fn stop_flow(&self, flow_id: FlowId) -> Result<()> {
        let resp = self
            .client
            .post(format!("{}/api/flows/{}/stop", self.base_url, flow_id))
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to stop flow: {}", text));
        }

        Ok(())
    }

    /// Delete a flow
    pub async fn delete_flow(&self, flow_id: FlowId) -> Result<()> {
        let resp = self
            .client
            .delete(format!("{}/api/flows/{}", self.base_url, flow_id))
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to delete flow: {}", text));
        }

        Ok(())
    }

    /// List all flows
    pub async fn list_flows(&self) -> Result<Vec<(FlowId, String)>> {
        let resp = self
            .client
            .get(format!("{}/api/flows", self.base_url))
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to list flows: {}", text));
        }

        let list: FlowListResponse = resp.json().await?;
        Ok(list.flows.into_iter().map(|f| (f.id, f.name)).collect())
    }

    /// Delete all flows with names starting with a prefix
    pub async fn delete_flows_by_prefix(&self, prefix: &str) -> Result<()> {
        let flows = self.list_flows().await?;

        for (id, name) in flows {
            if name.starts_with(prefix) {
                // Stop flow first (ignore errors - it may not be running)
                let _ = self.stop_flow(id).await;
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Delete flow
                if let Err(e) = self.delete_flow(id).await {
                    eprintln!("Warning: Failed to delete flow {}: {}", name, e);
                }
            }
        }

        Ok(())
    }

    /// Get player state
    pub async fn get_player_state(
        &self,
        flow_id: FlowId,
        block_id: &str,
    ) -> Result<PlayerStateResponse> {
        let resp = self
            .client
            .get(format!(
                "{}/api/flows/{}/blocks/{}/player/state",
                self.base_url, flow_id, block_id
            ))
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to get player state: {}", text));
        }

        Ok(resp.json().await?)
    }

    /// Control player (play, pause, stop, next, previous)
    pub async fn player_control(
        &self,
        flow_id: FlowId,
        block_id: &str,
        action: &str,
    ) -> Result<()> {
        let resp = self
            .client
            .post(format!(
                "{}/api/flows/{}/blocks/{}/player/control",
                self.base_url, flow_id, block_id
            ))
            .json(&PlayerControlRequest {
                action: action.to_string(),
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to control player: {}", text));
        }

        Ok(())
    }

    /// Seek to position (in nanoseconds)
    pub async fn seek(&self, flow_id: FlowId, block_id: &str, position_ns: u64) -> Result<()> {
        let resp = self
            .client
            .post(format!(
                "{}/api/flows/{}/blocks/{}/player/seek",
                self.base_url, flow_id, block_id
            ))
            .json(&SeekRequest { position_ns })
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to seek: {}", text));
        }

        Ok(())
    }

    /// Set playlist
    pub async fn set_playlist(
        &self,
        flow_id: FlowId,
        block_id: &str,
        files: Vec<String>,
    ) -> Result<()> {
        let resp = self
            .client
            .post(format!(
                "{}/api/flows/{}/blocks/{}/player/playlist",
                self.base_url, flow_id, block_id
            ))
            .json(&SetPlaylistRequest { files })
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to set playlist: {}", text));
        }

        Ok(())
    }

    /// Go to specific file in playlist
    pub async fn goto_file(
        &self,
        flow_id: FlowId,
        block_id: &str,
        file_index: usize,
    ) -> Result<()> {
        let resp = self
            .client
            .post(format!(
                "{}/api/flows/{}/blocks/{}/player/goto",
                self.base_url, flow_id, block_id
            ))
            .json(&GotoFileRequest { index: file_index })
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("Failed to goto file: {}", text));
        }

        Ok(())
    }
}
