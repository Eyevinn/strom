//! Cross-pipeline input sharing using GStreamer inter elements.
//!
//! This module provides mechanisms for flows to publish outputs that other
//! flows can subscribe to, enabling shared inputs across multiple pipelines.

pub mod channel_registry;

pub use channel_registry::{ChannelInfo, ChannelRegistry};
