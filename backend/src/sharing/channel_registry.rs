//! Channel registry for tracking active inter-pipeline channels.
//!
//! The registry keeps track of which flows are publishing which outputs,
//! allowing consumers to discover and subscribe to available sources.

use std::collections::HashMap;
use std::sync::Arc;
use strom_types::{FlowId, MediaType};
use tokio::sync::RwLock;

/// Information about an active inter-pipeline channel.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    /// The flow that publishes this output
    pub source_flow_id: FlowId,
    /// Name of the published output
    pub output_name: String,
    /// Generated channel name for inter elements
    pub channel_name: String,
    /// Media type of the output
    pub media_type: MediaType,
}

/// Registry of active inter-pipeline channels.
///
/// Tracks which flows are publishing outputs and allows consumers
/// to discover available sources for subscription.
#[derive(Debug)]
pub struct ChannelRegistry {
    /// Active channels: channel_name -> ChannelInfo
    channels: Arc<RwLock<HashMap<String, ChannelInfo>>>,
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelRegistry {
    /// Create a new empty channel registry.
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Generate a channel name from flow ID and output name.
    ///
    /// Channel names are formatted as `strom_{flow_id}_{output_name}`
    /// with spaces replaced by underscores.
    pub fn generate_channel_name(flow_id: &FlowId, output_name: &str) -> String {
        format!("strom_{}_{}", flow_id, output_name.replace([' ', ':'], "_"))
    }

    /// Register a published output channel.
    ///
    /// Called when a flow with published outputs starts.
    pub async fn register(&self, info: ChannelInfo) {
        let mut channels = self.channels.write().await;
        tracing::info!(
            channel_name = %info.channel_name,
            source_flow_id = %info.source_flow_id,
            output_name = %info.output_name,
            "Registering inter-pipeline channel"
        );
        channels.insert(info.channel_name.clone(), info);
    }

    /// Unregister a channel.
    ///
    /// Called when a flow with published outputs stops.
    pub async fn unregister(&self, channel_name: &str) {
        let mut channels = self.channels.write().await;
        if channels.remove(channel_name).is_some() {
            tracing::info!(
                channel_name = %channel_name,
                "Unregistered inter-pipeline channel"
            );
        }
    }

    /// Unregister all channels for a flow.
    ///
    /// Called when a flow stops to clean up all its published outputs.
    pub async fn unregister_flow(&self, flow_id: &FlowId) {
        let mut channels = self.channels.write().await;
        let to_remove: Vec<String> = channels
            .iter()
            .filter(|(_, info)| &info.source_flow_id == flow_id)
            .map(|(name, _)| name.clone())
            .collect();

        for name in to_remove {
            tracing::info!(
                channel_name = %name,
                flow_id = %flow_id,
                "Unregistered inter-pipeline channel (flow stopped)"
            );
            channels.remove(&name);
        }
    }

    /// Get information about a specific channel.
    pub async fn get(&self, channel_name: &str) -> Option<ChannelInfo> {
        let channels = self.channels.read().await;
        channels.get(channel_name).cloned()
    }

    /// List all active channels.
    pub async fn list_all(&self) -> Vec<ChannelInfo> {
        let channels = self.channels.read().await;
        channels.values().cloned().collect()
    }

    /// List all channels published by a specific flow.
    pub async fn list_by_flow(&self, flow_id: &FlowId) -> Vec<ChannelInfo> {
        let channels = self.channels.read().await;
        channels
            .values()
            .filter(|info| &info.source_flow_id == flow_id)
            .cloned()
            .collect()
    }

    /// Check if a channel exists and is active.
    pub async fn is_active(&self, channel_name: &str) -> bool {
        let channels = self.channels.read().await;
        channels.contains_key(channel_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_generate_channel_name() {
        let flow_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let name = ChannelRegistry::generate_channel_name(&flow_id, "main video");
        assert_eq!(
            name,
            "strom_550e8400-e29b-41d4-a716-446655440000_main_video"
        );
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let registry = ChannelRegistry::new();
        let flow_id = Uuid::new_v4();
        let channel_name = ChannelRegistry::generate_channel_name(&flow_id, "video");

        registry
            .register(ChannelInfo {
                source_flow_id: flow_id,
                output_name: "video".to_string(),
                channel_name: channel_name.clone(),
                media_type: MediaType::Video,
            })
            .await;

        let info = registry.get(&channel_name).await;
        assert!(info.is_some());
        assert_eq!(info.unwrap().output_name, "video");
    }

    #[tokio::test]
    async fn test_unregister_flow() {
        let registry = ChannelRegistry::new();
        let flow_id = Uuid::new_v4();

        // Register two channels for the same flow
        for name in ["video", "audio"] {
            let channel_name = ChannelRegistry::generate_channel_name(&flow_id, name);
            registry
                .register(ChannelInfo {
                    source_flow_id: flow_id,
                    output_name: name.to_string(),
                    channel_name,
                    media_type: MediaType::Generic,
                })
                .await;
        }

        assert_eq!(registry.list_all().await.len(), 2);

        registry.unregister_flow(&flow_id).await;

        assert_eq!(registry.list_all().await.len(), 0);
    }
}
