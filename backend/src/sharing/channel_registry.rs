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

    /// List all active channels.
    pub async fn list_all(&self) -> Vec<ChannelInfo> {
        let channels = self.channels.read().await;
        channels.values().cloned().collect()
    }
}
