//! Block builder trait for runtime GStreamer element creation.

use crate::events::EventBroadcaster;
use gstreamer as gst;
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
    ///
    /// # Returns
    /// A vector of (element_id, gst::Element) tuples where element_id is already namespaced.
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
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
