use anyhow::{Context, Result};
use reqwest::{header::HeaderValue, Client, RequestBuilder};
use serde::Deserialize;
use std::collections::HashMap;
use strom_types::{
    api::{
        CreateFlowRequest, ElementPropertiesResponse, FlowListResponse, FlowResponse,
        PadPropertiesResponse, UpdateFlowPropertiesRequest, UpdatePadPropertyRequest,
        UpdatePropertyRequest,
    },
    element::{ElementInfo, PropertyValue},
    flow::{Flow, FlowProperties},
};

#[derive(Deserialize)]
struct ElementListResponse {
    elements: Vec<ElementInfo>,
}

#[derive(Deserialize)]
struct ElementResponse {
    element: ElementInfo,
}

/// HTTP client for Strom REST API
#[derive(Clone, Debug)]
pub struct StromClient {
    base_url: String,
    client: Client,
    api_key: Option<String>,
}

impl StromClient {
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        Self {
            base_url,
            client: Client::new(),
            api_key,
        }
    }

    /// Add authentication header if API key is configured
    fn with_auth(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(key) => {
                let header_value = HeaderValue::from_str(key)
                    .unwrap_or_else(|_| HeaderValue::from_static("invalid-api-key"));
                request.header("X-API-Key", header_value)
            }
            None => request,
        }
    }

    /// List all flows
    pub async fn list_flows(&self) -> Result<FlowListResponse> {
        let url = format!("{}/api/flows", self.base_url);
        self.with_auth(self.client.get(&url))
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")
    }

    /// Get a specific flow
    pub async fn get_flow(&self, flow_id: &str) -> Result<FlowResponse> {
        let url = format!("{}/api/flows/{}", self.base_url, flow_id);
        self.with_auth(self.client.get(&url))
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")
    }

    /// Create a new flow
    pub async fn create_flow(&self, request: CreateFlowRequest) -> Result<FlowResponse> {
        let url = format!("{}/api/flows", self.base_url);
        self.with_auth(self.client.post(&url))
            .json(&request)
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")
    }

    /// Update a flow
    pub async fn update_flow(&self, flow_id: &str, flow: Flow) -> Result<FlowResponse> {
        let url = format!("{}/api/flows/{}", self.base_url, flow_id);
        self.with_auth(self.client.post(&url))
            .json(&flow)
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")
    }

    /// Delete a flow
    pub async fn delete_flow(&self, flow_id: &str) -> Result<()> {
        let url = format!("{}/api/flows/{}", self.base_url, flow_id);
        self.with_auth(self.client.delete(&url))
            .send()
            .await
            .context("Failed to send request")?;
        Ok(())
    }

    /// Start a flow
    pub async fn start_flow(&self, flow_id: &str) -> Result<()> {
        let url = format!("{}/api/flows/{}/start", self.base_url, flow_id);
        self.with_auth(self.client.post(&url))
            .send()
            .await
            .context("Failed to send request")?;
        Ok(())
    }

    /// Stop a flow
    pub async fn stop_flow(&self, flow_id: &str) -> Result<()> {
        let url = format!("{}/api/flows/{}/stop", self.base_url, flow_id);
        self.with_auth(self.client.post(&url))
            .send()
            .await
            .context("Failed to send request")?;
        Ok(())
    }

    /// List available GStreamer elements
    pub async fn list_elements(&self) -> Result<Vec<ElementInfo>> {
        let url = format!("{}/api/elements", self.base_url);
        let response: ElementListResponse = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")?;
        Ok(response.elements)
    }

    /// Get element information
    pub async fn get_element_info(&self, element_name: &str) -> Result<ElementInfo> {
        let url = format!("{}/api/elements/{}", self.base_url, element_name);
        let response: ElementResponse = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")?;
        Ok(response.element)
    }

    /// Get current property values from a running element
    pub async fn get_element_properties(
        &self,
        flow_id: &str,
        element_id: &str,
    ) -> Result<HashMap<String, PropertyValue>> {
        let url = format!(
            "{}/api/flows/{}/elements/{}/properties",
            self.base_url, flow_id, element_id
        );
        let response: ElementPropertiesResponse = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")?;
        Ok(response.properties)
    }

    /// Update a property on a running element
    pub async fn update_element_property(
        &self,
        flow_id: &str,
        element_id: &str,
        property_name: &str,
        value: PropertyValue,
    ) -> Result<HashMap<String, PropertyValue>> {
        let url = format!(
            "{}/api/flows/{}/elements/{}/properties",
            self.base_url, flow_id, element_id
        );
        let request = UpdatePropertyRequest {
            property_name: property_name.to_string(),
            value,
        };
        let response: ElementPropertiesResponse = self
            .with_auth(self.client.patch(&url))
            .json(&request)
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")?;
        Ok(response.properties)
    }

    /// Update flow properties (description, clock type, etc.)
    pub async fn update_flow_properties(
        &self,
        flow_id: &str,
        properties: FlowProperties,
    ) -> Result<FlowResponse> {
        let url = format!("{}/api/flows/{}/properties", self.base_url, flow_id);
        let request = UpdateFlowPropertiesRequest { properties };
        self.with_auth(self.client.patch(&url))
            .json(&request)
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")
    }

    /// Get current property values from a pad
    #[allow(dead_code)]
    pub async fn get_pad_properties(
        &self,
        flow_id: &str,
        element_id: &str,
        pad_name: &str,
    ) -> Result<HashMap<String, PropertyValue>> {
        let url = format!(
            "{}/api/flows/{}/elements/{}/pads/{}/properties",
            self.base_url, flow_id, element_id, pad_name
        );
        let response: PadPropertiesResponse = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")?;
        Ok(response.properties)
    }

    /// Update a property on a pad
    #[allow(dead_code)]
    pub async fn update_pad_property(
        &self,
        flow_id: &str,
        element_id: &str,
        pad_name: &str,
        property_name: &str,
        value: PropertyValue,
    ) -> Result<HashMap<String, PropertyValue>> {
        let url = format!(
            "{}/api/flows/{}/elements/{}/pads/{}/properties",
            self.base_url, flow_id, element_id, pad_name
        );
        let request = UpdatePadPropertyRequest {
            property_name: property_name.to_string(),
            value,
        };
        let response: PadPropertiesResponse = self
            .with_auth(self.client.patch(&url))
            .json(&request)
            .send()
            .await
            .context("Failed to send request")?
            .json()
            .await
            .context("Failed to parse response")?;
        Ok(response.properties)
    }
}
