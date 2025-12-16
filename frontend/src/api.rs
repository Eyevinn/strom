//! API client for communicating with the Strom backend.

use serde::{Deserialize, Serialize};
use strom_types::element::ElementInfo;
use strom_types::{Flow, FlowId};

/// Version information from the backend
#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub git_hash: String,
    pub git_tag: String,
    pub git_branch: String,
    pub git_dirty: bool,
    pub build_timestamp: String,
}

/// Result type for API operations.
pub type ApiResult<T> = Result<T, ApiError>;

/// API client errors.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// Network error
    Network(String),
    /// HTTP error with status code
    Http(u16, String),
    /// Deserialization error
    Decode(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Network(msg) => write!(f, "Network error: {}", msg),
            ApiError::Http(code, msg) => write!(f, "HTTP {} error: {}", code, msg),
            ApiError::Decode(msg) => write!(f, "Decode error: {}", msg),
        }
    }
}

/// Client for the Strom REST API.
#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    client: reqwest::Client,
    /// Optional auth token for Bearer authentication (used by native GUI)
    auth_token: Option<String>,
}

impl ApiClient {
    /// Create a new API client with the given base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
            auth_token: None,
        }
    }

    /// Create a new API client with authentication token.
    pub fn new_with_auth(base_url: impl Into<String>, auth_token: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
            auth_token,
        }
    }

    /// Get the auth token (for WebSocket connections)
    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
    }

    /// Helper to add auth header to a request builder
    fn with_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref token) = self.auth_token {
            builder.header("Authorization", format!("Bearer {}", token))
        } else {
            builder
        }
    }

    /// Get the base URL for the API.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

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
        use strom_types::api::{CreateFlowRequest, FlowResponse};
        use tracing::info;

        let url = format!("{}/flows", self.base_url);
        info!("Creating flow via API: POST {}", url);
        info!("Flow data: name='{}'", flow.name);

        let request = CreateFlowRequest {
            name: flow.name.clone(),
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
    pub async fn get_flow_latency(&self, id: FlowId) -> ApiResult<LatencyInfo> {
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

        let latency_info: LatencyInfo = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        Ok(latency_info)
    }

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

    /// Get the debug graph URL for a flow.
    /// Returns the full URL that can be opened in a new tab.
    pub fn get_debug_graph_url(&self, id: FlowId) -> String {
        format!("{}/flows/{}/debug-graph", self.base_url, id)
    }

    /// List all block definitions (built-in + user-defined).
    pub async fn list_blocks(&self) -> ApiResult<Vec<strom_types::BlockDefinition>> {
        use strom_types::block::BlockListResponse;
        use tracing::info;

        let url = format!("{}/blocks", self.base_url);
        info!("Fetching blocks from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching blocks: {}", e);
                ApiError::Network(e.to_string())
            })?;

        info!("Blocks response status: {}", response.status());

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let block_list: BlockListResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse block list response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully loaded {} blocks", block_list.blocks.len());
        Ok(block_list.blocks)
    }

    /// Get a specific block definition by ID.
    pub async fn get_block(&self, id: &str) -> ApiResult<strom_types::BlockDefinition> {
        use strom_types::block::BlockResponse;
        use tracing::info;

        let url = format!("{}/blocks/{}", self.base_url, id);
        info!("Fetching block from: {}", url);

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

        let block_response: BlockResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!("Successfully loaded block: {}", id);
        Ok(block_response.block)
    }

    /// Create a new user-defined block.
    pub async fn create_block(
        &self,
        block: &strom_types::block::CreateBlockRequest,
    ) -> ApiResult<strom_types::BlockDefinition> {
        use strom_types::block::BlockResponse;
        use tracing::info;

        let url = format!("{}/blocks", self.base_url);
        info!("Creating block via API: POST {}", url);

        let response = self
            .with_auth(self.client.post(&url).json(block))
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

        let block_response: BlockResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse block response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully created block: {}", block_response.block.name);
        Ok(block_response.block)
    }

    /// Update an existing user-defined block.
    pub async fn update_block(
        &self,
        block: &strom_types::BlockDefinition,
    ) -> ApiResult<strom_types::BlockDefinition> {
        use strom_types::block::BlockResponse;
        use tracing::info;

        let url = format!("{}/blocks/{}", self.base_url, block.id);
        info!("Updating block via API: PUT {}", url);

        let response = self
            .with_auth(self.client.put(&url).json(block))
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

        let block_response: BlockResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse block response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully updated block: {}", block_response.block.name);
        Ok(block_response.block)
    }

    /// Delete a user-defined block.
    pub async fn delete_block(&self, id: &str) -> ApiResult<()> {
        let url = format!("{}/blocks/{}", self.base_url, id);
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

    /// Get all block categories.
    pub async fn get_block_categories(&self) -> ApiResult<Vec<String>> {
        use strom_types::block::BlockCategoriesResponse;
        use tracing::info;

        let url = format!("{}/blocks/categories", self.base_url);
        info!("Fetching block categories from: {}", url);

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

        let categories_response: BlockCategoriesResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!(
            "Successfully loaded {} categories",
            categories_response.categories.len()
        );
        Ok(categories_response.categories)
    }

    /// Get version and build information from the backend.
    pub async fn get_version(&self) -> ApiResult<VersionInfo> {
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

        let version_info: VersionInfo = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!("Successfully fetched version: v{}", version_info.version);
        Ok(version_info)
    }

    /// Check authentication status and whether auth is required.
    pub async fn get_auth_status(&self) -> ApiResult<AuthStatusResponse> {
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

        let auth_status: AuthStatusResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        info!(
            "Auth status: required={}, authenticated={}",
            auth_status.auth_required, auth_status.authenticated
        );
        Ok(auth_status)
    }

    /// Login with username and password.
    pub async fn login(&self, username: String, password: String) -> ApiResult<LoginResponse> {
        use tracing::info;

        let url = format!("{}/login", self.base_url);
        info!("Attempting login for user: {}", username);

        let request = LoginRequest { username, password };

        let response = self
            .with_auth(self.client.post(&url).json(&request))
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

        let login_response: LoginResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse login response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Login response: success={}", login_response.success);
        Ok(login_response)
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

    /// Get WebRTC statistics from a running flow.
    pub async fn get_webrtc_stats(&self, id: FlowId) -> ApiResult<strom_types::api::WebRtcStats> {
        use strom_types::api::WebRtcStatsResponse;
        use tracing::trace;

        let url = format!("{}/flows/{}/webrtc-stats", self.base_url, id);
        trace!("Fetching WebRTC stats from: {}", url);

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

        let stats_response: WebRtcStatsResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        trace!(
            "Successfully fetched WebRTC stats: {} connections",
            stats_response.stats.connections.len()
        );
        Ok(stats_response.stats)
    }

    /// Get statistics for a running flow.
    pub async fn get_flow_stats(&self, id: FlowId) -> ApiResult<FlowStatsInfo> {
        let url = format!("{}/flows/{}/stats", self.base_url, id);
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

        let stats_info: FlowStatsInfo = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        Ok(stats_info)
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

        #[derive(Deserialize)]
        struct DynamicPadsResponse {
            pads: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
        }

        let response: DynamicPadsResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;

        Ok(response.pads)
    }
}

/// Login request payload
#[derive(Debug, Serialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Login response
#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub success: bool,
    pub message: String,
}

/// Authentication status response
#[derive(Debug, Clone, Deserialize)]
pub struct AuthStatusResponse {
    pub authenticated: bool,
    pub auth_required: bool,
    pub methods: Vec<String>,
}

/// Pipeline latency information
#[derive(Debug, Clone, Deserialize)]
pub struct LatencyInfo {
    /// Minimum latency in nanoseconds
    pub min_latency_ns: u64,
    /// Maximum latency in nanoseconds
    pub max_latency_ns: u64,
    /// Whether the pipeline is a live pipeline
    pub live: bool,
    /// Minimum latency formatted as human-readable string
    pub min_latency_formatted: String,
    /// Maximum latency formatted as human-readable string
    pub max_latency_formatted: String,
}

/// Flow statistics information
#[derive(Debug, Clone, Deserialize)]
pub struct FlowStatsInfo {
    /// The flow ID
    pub flow_id: FlowId,
    /// The flow name
    pub flow_name: String,
    /// Statistics for each block in the flow
    pub blocks: Vec<BlockStatsInfo>,
    /// Timestamp when stats were collected (nanoseconds since UNIX epoch)
    pub collected_at: u64,
}

/// Statistics for a single block instance
#[derive(Debug, Clone, Deserialize)]
pub struct BlockStatsInfo {
    /// The block instance ID
    pub block_instance_id: String,
    /// The block definition ID (e.g., "builtin.aes67_input")
    pub block_definition_id: String,
    /// Human-readable block name
    pub block_name: String,
    /// Collection of statistics for this block
    pub stats: Vec<StatisticInfo>,
    /// Timestamp when these stats were collected
    pub collected_at: u64,
}

/// A single statistic with its value and metadata
#[derive(Debug, Clone, Deserialize)]
pub struct StatisticInfo {
    /// Unique identifier for this statistic within the block
    pub id: String,
    /// Current value
    pub value: StatValueInfo,
    /// Metadata about this statistic
    pub metadata: StatMetadataInfo,
}

/// A statistic value
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum StatValueInfo {
    /// Counter - monotonically increasing value (e.g., packets received)
    Counter(u64),
    /// Gauge - value that can go up or down (e.g., buffer level)
    Gauge(i64),
    /// Float value (e.g., average jitter)
    Float(f64),
    /// Boolean flag (e.g., is_synced)
    Bool(bool),
    /// String value (e.g., current SSRC)
    String(String),
    /// Duration in nanoseconds
    DurationNs(u64),
    /// Timestamp in nanoseconds since epoch
    TimestampNs(u64),
}

/// Metadata about a statistic
#[derive(Debug, Clone, Deserialize)]
pub struct StatMetadataInfo {
    /// Human-readable name for display
    pub display_name: String,
    /// Description of what this statistic measures
    pub description: String,
    /// Unit of measurement (e.g., "packets", "ms", "bytes")
    pub unit: Option<String>,
    /// Category for grouping in UI (e.g., "RTP", "Buffer", "Network")
    pub category: Option<String>,
}

impl StatValueInfo {
    /// Format the value for display
    pub fn format(&self) -> String {
        match self {
            StatValueInfo::Counter(v) => format!("{}", v),
            StatValueInfo::Gauge(v) => format!("{}", v),
            StatValueInfo::Float(v) => format!("{:.2}", v),
            StatValueInfo::Bool(v) => if *v { "Yes" } else { "No" }.to_string(),
            StatValueInfo::String(v) => v.clone(),
            StatValueInfo::DurationNs(v) => {
                if *v < 1_000 {
                    format!("{} ns", v)
                } else if *v < 1_000_000 {
                    format!("{:.2} us", *v as f64 / 1_000.0)
                } else if *v < 1_000_000_000 {
                    format!("{:.2} ms", *v as f64 / 1_000_000.0)
                } else {
                    format!("{:.2} s", *v as f64 / 1_000_000_000.0)
                }
            }
            StatValueInfo::TimestampNs(v) => format!("{}", v),
        }
    }
}

// ============================================================================
// gst-launch-1.0 Import/Export Types and Methods
// ============================================================================

/// Response from parsing a gst-launch pipeline
#[derive(Debug, Clone, Deserialize)]
pub struct ParseGstLaunchResponse {
    /// Elements extracted from the parsed pipeline
    pub elements: Vec<strom_types::Element>,
    /// Links between elements
    pub links: Vec<strom_types::element::Link>,
}

/// Response from exporting to gst-launch syntax
#[derive(Debug, Clone, Deserialize)]
pub struct ExportGstLaunchResponse {
    /// The generated gst-launch-1.0 pipeline string
    pub pipeline: String,
}

/// Response with the current player state.
#[derive(Debug, Clone, Deserialize)]
pub struct PlayerStateResponse {
    /// Current playback state: "playing", "paused", "stopped"
    pub state: String,
    /// Current position in nanoseconds
    pub position_ns: u64,
    /// Total duration in nanoseconds
    pub duration_ns: u64,
    /// Current file index (0-based)
    pub current_file_index: usize,
    /// Total number of files in playlist
    pub total_files: usize,
    /// Current file path/URI
    pub current_file: Option<String>,
    /// Full playlist
    pub playlist: Vec<String>,
    /// Whether playlist loops
    pub loop_playlist: bool,
}

impl ApiClient {
    /// Parse a gst-launch-1.0 pipeline string and return elements and links.
    ///
    /// This uses the backend's GStreamer parser to ensure complete compatibility
    /// with the gst-launch-1.0 syntax.
    pub async fn parse_gst_launch(&self, pipeline: &str) -> ApiResult<ParseGstLaunchResponse> {
        use tracing::info;

        let url = format!("{}/gst-launch/parse", self.base_url);
        info!("Parsing gst-launch pipeline via API: POST {}", url);

        #[derive(Serialize)]
        struct ParseRequest<'a> {
            pipeline: &'a str,
        }

        let request = ParseRequest { pipeline };

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

        let parse_response: ParseGstLaunchResponse = response.json().await.map_err(|e| {
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

        #[derive(Serialize)]
        struct ExportRequest<'a> {
            elements: &'a [strom_types::Element],
            links: &'a [strom_types::element::Link],
        }

        let request = ExportRequest { elements, links };

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

        let export_response: ExportGstLaunchResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully exported pipeline");
        Ok(export_response.pipeline)
    }

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

    /// List available network interfaces.
    pub async fn list_network_interfaces(
        &self,
    ) -> ApiResult<strom_types::NetworkInterfacesResponse> {
        use tracing::info;

        let url = format!("{}/network/interfaces", self.base_url);
        info!("Fetching network interfaces from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching interfaces: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let interfaces: strom_types::NetworkInterfacesResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse network interfaces response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!(
            "Successfully loaded {} network interfaces",
            interfaces.interfaces.len()
        );
        Ok(interfaces)
    }

    /// Get available inter-pipeline channels.
    ///
    /// Returns channels published by running flows with InterOutput blocks.
    pub async fn get_available_sources(
        &self,
    ) -> ApiResult<strom_types::api::AvailableSourcesResponse> {
        use tracing::info;

        let url = format!("{}/sources", self.base_url);
        info!("Fetching available sources from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching sources: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let sources: strom_types::api::AvailableSourcesResponse =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse available sources response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        info!("Successfully loaded {} source flows", sources.sources.len());
        Ok(sources)
    }

    /// Get discovered SAP/AES67 streams.
    pub async fn get_discovered_streams(
        &self,
    ) -> ApiResult<Vec<crate::discovery::DiscoveredStream>> {
        let url = format!("{}/discovery/streams", self.base_url);
        tracing::debug!("Fetching discovered streams from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching discovered streams: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let streams: Vec<crate::discovery::DiscoveredStream> =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse discovered streams response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        tracing::debug!("Successfully loaded {} discovered streams", streams.len());
        Ok(streams)
    }

    /// Get streams we are announcing via SAP.
    pub async fn get_announced_streams(&self) -> ApiResult<Vec<crate::discovery::AnnouncedStream>> {
        let url = format!("{}/discovery/announced", self.base_url);
        tracing::debug!("Fetching announced streams from: {}", url);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching announced streams: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let streams: Vec<crate::discovery::AnnouncedStream> =
            response.json().await.map_err(|e| {
                tracing::error!("Failed to parse announced streams response: {}", e);
                ApiError::Decode(e.to_string())
            })?;

        tracing::debug!("Successfully loaded {} announced streams", streams.len());
        Ok(streams)
    }

    /// Get the SDP for a specific discovered stream.
    pub async fn get_stream_sdp(&self, stream_id: &str) -> ApiResult<String> {
        use tracing::info;

        let url = format!("{}/discovery/streams/{}/sdp", self.base_url, stream_id);
        info!("Fetching SDP for stream: {}", stream_id);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error fetching stream SDP: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let sdp = response.text().await.map_err(|e| {
            tracing::error!("Failed to read SDP response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!("Successfully loaded SDP for stream: {}", stream_id);
        Ok(sdp)
    }

    // ========================================================================
    // Media File Management
    // ========================================================================

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

    /// Upload a file to the media directory.
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

    // ========================================================================
    // Media Player API
    // ========================================================================

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

    /// Get the current state of a media player, including playlist.
    pub async fn get_player_state(
        &self,
        flow_id: FlowId,
        block_id: &str,
    ) -> ApiResult<PlayerStateResponse> {
        use tracing::info;

        let url = format!(
            "{}/flows/{}/blocks/{}/player/state",
            self.base_url, flow_id, block_id
        );
        info!("Getting player state for {}", block_id);

        let response = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Network error getting player state: {}", e);
                ApiError::Network(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP error {}: {}", status, text);
            return Err(ApiError::Http(status, text));
        }

        let state: PlayerStateResponse = response.json().await.map_err(|e| {
            tracing::error!("Failed to parse player state response: {}", e);
            ApiError::Decode(e.to_string())
        })?;

        info!(
            "Player {} state: {}, {} files in playlist",
            block_id,
            state.state,
            state.playlist.len()
        );
        Ok(state)
    }
}
