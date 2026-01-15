//! Block builder trait for runtime GStreamer element creation.

use crate::events::EventBroadcaster;
use gstreamer as gst;
use std::cell::RefCell;
use std::collections::HashMap;
use strom_types::{block::ExternalPads, element::ElementPadRef, FlowId, PropertyValue};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BlockBuildError {
    #[error("GStreamer error: {0}")]
    GStreamer(#[from] gst::glib::Error),

    #[error("GStreamer boolean error: {0}")]
    BoolError(#[from] gst::glib::BoolError),

    #[error("Failed to create element: {0}")]
    ElementCreation(String),

    #[error("Failed to link elements: {0}")]
    LinkError(String),

    #[error("Invalid property: {0}")]
    InvalidProperty(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),
}

/// Function type for connecting a block-specific bus message handler.
///
/// Takes the GStreamer bus, flow ID, and event broadcaster.
/// Returns a SignalHandlerId that identifies the connected handler.
/// Uses `connect_message` which allows multiple handlers (unlike `add_watch`).
pub type BusMessageConnectFn = Box<
    dyn FnOnce(&gst::Bus, FlowId, EventBroadcaster) -> gst::glib::SignalHandlerId + Send + Sync,
>;

/// Legacy type alias for backward compatibility
#[deprecated(note = "Use BusMessageConnectFn instead")]
pub type BusWatchSetupFn = BusMessageConnectFn;

/// Stream mode for WHEP endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhepStreamMode {
    /// Audio only
    #[default]
    Audio,
    /// Video only
    Video,
    /// Both audio and video
    AudioVideo,
}

impl WhepStreamMode {
    pub fn has_audio(&self) -> bool {
        matches!(self, WhepStreamMode::Audio | WhepStreamMode::AudioVideo)
    }

    pub fn has_video(&self) -> bool {
        matches!(self, WhepStreamMode::Video | WhepStreamMode::AudioVideo)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            WhepStreamMode::Audio => "audio",
            WhepStreamMode::Video => "video",
            WhepStreamMode::AudioVideo => "audio_video",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "video" => WhepStreamMode::Video,
            "audio_video" => WhepStreamMode::AudioVideo,
            _ => WhepStreamMode::Audio,
        }
    }
}

/// WHEP endpoint registration info.
#[derive(Debug, Clone)]
pub struct WhepEndpointInfo {
    /// The block instance ID that owns this endpoint
    pub block_id: String,
    /// The endpoint ID (user-configurable or auto-generated UUID)
    pub endpoint_id: String,
    /// The internal localhost port where whepserversink is listening
    pub internal_port: u16,
    /// Stream mode (audio, video, or both)
    pub mode: WhepStreamMode,
}

/// Context provided to block builders during build.
///
/// Contains methods for blocks to register services, endpoints, or other
/// resources that need to be set up after the pipeline is created.
/// This allows blocks to interact with the broader system without
/// coupling BlockBuildResult to specific block types.
pub struct BlockBuildContext {
    /// WHEP endpoints queued for registration
    whep_endpoints: RefCell<Vec<WhepEndpointInfo>>,
    /// ICE servers for WebRTC NAT traversal (STUN/TURN URLs)
    ice_servers: Vec<String>,
}

impl BlockBuildContext {
    /// Create a new build context with the given ICE servers.
    pub fn new(ice_servers: Vec<String>) -> Self {
        Self {
            whep_endpoints: RefCell::new(Vec::new()),
            ice_servers,
        }
    }

    /// Get the configured ICE servers.
    pub fn ice_servers(&self) -> &[String] {
        &self.ice_servers
    }

    /// Get the first STUN server URL (for GStreamer elements).
    /// Returns the default Google STUN server if no STUN server configured.
    pub fn stun_server(&self) -> Option<&str> {
        self.ice_servers
            .iter()
            .find(|s| s.starts_with("stun:"))
            .map(|s| s.as_str())
    }

    /// Get the first TURN server URL (for GStreamer elements).
    /// Returns None if no TURN server configured.
    pub fn turn_server(&self) -> Option<&str> {
        self.ice_servers
            .iter()
            .find(|s| s.starts_with("turn:") || s.starts_with("turns:"))
            .map(|s| s.as_str())
    }

    /// Register a WHEP endpoint (called by WHEP Output blocks during build).
    ///
    /// The endpoint will be registered with the WhepRegistry after the pipeline starts.
    pub fn register_whep_endpoint(
        &self,
        block_id: &str,
        endpoint_id: &str,
        port: u16,
        mode: WhepStreamMode,
    ) {
        self.whep_endpoints.borrow_mut().push(WhepEndpointInfo {
            block_id: block_id.to_string(),
            endpoint_id: endpoint_id.to_string(),
            internal_port: port,
            mode,
        });
    }

    /// Take all queued WHEP endpoint registrations.
    ///
    /// Called after block expansion to process the registrations.
    pub fn take_whep_endpoints(&self) -> Vec<WhepEndpointInfo> {
        self.whep_endpoints.borrow_mut().drain(..).collect()
    }
}

/// Result of building a block - contains GStreamer elements with namespaced IDs and link specifications.
pub struct BlockBuildResult {
    /// GStreamer elements with their namespaced IDs (format: "block_instance_id:internal_element_id")
    pub elements: Vec<(String, gst::Element)>,

    /// Internal links between elements using structured ElementPadRef (type-safe, no string parsing)
    pub internal_links: Vec<(ElementPadRef, ElementPadRef)>,

    /// Optional bus message handler connection function.
    /// If provided, this will be called when the pipeline starts to allow the block
    /// to register its own bus message handlers using `connect_message`.
    /// Multiple blocks can register handlers since `connect_message` allows multiple handlers.
    pub bus_message_handler: Option<BusMessageConnectFn>,

    /// Pad properties to apply after linking (element_id -> pad_name -> property_name -> value).
    /// Used for properties on request pads that are created during linking (e.g., mixer sink pads).
    pub pad_properties: HashMap<String, HashMap<String, HashMap<String, PropertyValue>>>,
}

/// Trait for building GStreamer elements from block instances.
///
/// Implementors create actual GStreamer elements at runtime based on block properties.
/// Elements are namespaced with the block instance ID to avoid conflicts.
pub trait BlockBuilder: Send + Sync {
    /// Build GStreamer elements for this block instance.
    ///
    /// # Arguments
    /// * `instance_id` - Unique ID for this block instance (used for namespacing)
    /// * `properties` - Property values from the block instance
    /// * `ctx` - Build context for registering services (WHEP endpoints, etc.)
    ///
    /// # Returns
    /// A vector of (element_id, gst::Element) tuples where element_id is already namespaced.
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError>;

    /// Compute the external pads for this block instance based on its properties.
    ///
    /// This allows blocks to have dynamic pads based on their configuration.
    /// If None is returned, the block's static definition pads will be used.
    ///
    /// # Arguments
    /// * `properties` - Property values from the block instance
    ///
    /// # Returns
    /// Optional ExternalPads if this block has dynamic pads, None to use static definition pads.
    fn get_external_pads(
        &self,
        _properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        None
    }
}
