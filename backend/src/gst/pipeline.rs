//! GStreamer pipeline management.

use crate::blocks::BlockRegistry;
use crate::events::EventBroadcaster;
use crate::gst::thread_priority::{self, ThreadPriorityState};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_net as gst_net;
use std::collections::HashMap;
use strom_types::flow::ThreadPriorityStatus;
use strom_types::{Element, Flow, FlowId, Link, PipelineState, PropertyValue, StromEvent};
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

/// Result of processing links with automatic tee insertion.
struct ProcessedLinks {
    /// Final list of links (including links to/from tees)
    links: Vec<Link>,
    /// Map of tee element IDs to their source spec (element:pad they're connected to)
    tees: HashMap<String, String>,
}

#[derive(Error, Debug)]
pub enum PipelineError {
    #[error("GStreamer error: {0}")]
    GStreamer(#[from] gst::glib::Error),

    #[error("GStreamer boolean error: {0}")]
    BoolError(#[from] gst::glib::BoolError),

    #[error("Element not found: {0}")]
    ElementNotFound(String),

    #[error("Failed to create element: {0}")]
    ElementCreation(String),

    #[error("Failed to link elements: {0} -> {1}")]
    LinkError(String, String),

    #[error("Invalid property value for {element}.{property}: {reason}")]
    InvalidProperty {
        element: String,
        property: String,
        reason: String,
    },

    #[error("Pipeline state change failed: {0}")]
    StateChange(String),

    #[error("Invalid flow: {0}")]
    InvalidFlow(String),

    #[error("Property {property} on element {element} cannot be changed in {state:?} state")]
    PropertyNotMutable {
        element: String,
        property: String,
        state: PipelineState,
    },

    #[error("Pad not found: {element}:{pad}")]
    PadNotFound { element: String, pad: String },
}

/// Manages a single GStreamer pipeline for a flow.
pub struct PipelineManager {
    flow_id: FlowId,
    flow_name: String,
    pipeline: gst::Pipeline,
    elements: HashMap<String, gst::Element>,
    events: EventBroadcaster,
    /// Pending links that couldn't be made because source pads don't exist yet (dynamic pads)
    pending_links: Vec<Link>,
    /// Flow properties (clock configuration, etc.)
    properties: strom_types::flow::FlowProperties,
    /// Pad properties to apply after pads are created (element_id -> (pad_name -> properties))
    pad_properties: HashMap<String, HashMap<String, HashMap<String, PropertyValue>>>,
    /// Block-specific bus message handler IDs (allows blocks to register their own bus message handlers)
    block_message_handlers: Vec<gst::glib::SignalHandlerId>,
    /// Bus message handler connection functions from blocks (called when pipeline starts)
    block_message_connect_fns: Vec<crate::blocks::BusMessageConnectFn>,
    /// Thread priority state tracker (tracks whether priority was successfully set)
    thread_priority_state: Option<ThreadPriorityState>,
}

impl PipelineManager {
    /// Create a new pipeline from a flow definition.
    pub fn new(
        flow: &Flow,
        events: EventBroadcaster,
        _block_registry: &BlockRegistry,
    ) -> Result<Self, PipelineError> {
        info!("Creating pipeline for flow: {} ({})", flow.name, flow.id);
        info!(
            "Flow has {} elements, {} blocks, {} links",
            flow.elements.len(),
            flow.blocks.len(),
            flow.links.len()
        );

        let pipeline = gst::Pipeline::builder()
            .name(format!("flow-{}", flow.id))
            .build();
        info!("Created GStreamer pipeline object");

        let mut manager = Self {
            flow_id: flow.id,
            flow_name: flow.name.clone(),
            pipeline,
            elements: HashMap::new(),
            events,
            pending_links: Vec::new(),
            properties: flow.properties.clone(),
            pad_properties: HashMap::new(),
            block_message_handlers: Vec::new(),
            block_message_connect_fns: Vec::new(),
            thread_priority_state: None,
        };

        // Expand blocks into GStreamer elements
        info!("Starting block expansion (block_in_place)...");
        let expanded = tokio::task::block_in_place(|| {
            info!("Inside block_in_place, calling block_on...");
            tokio::runtime::Handle::current().block_on(async {
                info!("Inside block_on, calling expand_blocks...");
                let result = super::block_expansion::expand_blocks(&flow.blocks, &flow.links).await;
                info!("expand_blocks completed");
                result
            })
        })?;
        info!("Block expansion completed");

        // Add regular elements from flow
        info!(
            "Adding {} regular elements from flow...",
            flow.elements.len()
        );
        for (idx, element) in flow.elements.iter().enumerate() {
            info!(
                "Adding element {}/{}: {} (type: {})",
                idx + 1,
                flow.elements.len(),
                element.id,
                element.element_type
            );
            manager.add_element(element)?;
            info!("Successfully added element: {}", element.id);
        }
        info!("All regular elements added");

        // Add GStreamer elements from expanded blocks
        let block_element_count = expanded.gst_elements.len();
        info!("Adding {} block elements...", block_element_count);
        let mut idx = 0;
        for (element_id, gst_element) in expanded.gst_elements {
            idx += 1;
            info!(
                "Adding block element {}/{}: {}",
                idx, block_element_count, element_id
            );
            manager.pipeline.add(&gst_element).map_err(|e| {
                PipelineError::ElementCreation(format!(
                    "Failed to add block element {} to pipeline: {}",
                    element_id, e
                ))
            })?;
            manager.elements.insert(element_id.clone(), gst_element);
            info!("Successfully added block element: {}", element_id);
        }
        info!("All block elements added");

        // Store bus message handler connection functions from blocks
        info!(
            "Storing {} bus message handler(s) from blocks",
            expanded.bus_message_handlers.len()
        );
        manager.block_message_connect_fns = expanded.bus_message_handlers;

        // Analyze links and auto-insert tee elements where needed
        info!("Analyzing links and inserting tee elements if needed...");
        let processed_links = Self::insert_tees_if_needed(&expanded.links);
        info!(
            "Link analysis complete: {} links, {} tees",
            processed_links.links.len(),
            processed_links.tees.len()
        );

        // Create tee elements
        info!("Creating {} tee elements...", processed_links.tees.len());
        for (idx, tee_id) in processed_links.tees.keys().enumerate() {
            info!(
                "Creating tee {}/{}: {}",
                idx + 1,
                processed_links.tees.len(),
                tee_id
            );
            manager.add_tee_element(tee_id)?;
            info!("Successfully created tee: {}", tee_id);
        }
        info!("All tee elements created");

        // Link elements according to processed links
        info!("Linking {} elements...", processed_links.links.len());
        for (idx, link) in processed_links.links.iter().enumerate() {
            info!(
                "Linking {}/{}: {} -> {}",
                idx + 1,
                processed_links.links.len(),
                link.from,
                link.to
            );
            if let Err(e) = manager.try_link_elements(link) {
                info!(
                    "Could not link immediately: {} - will try when pad becomes available ({})",
                    e, link.from
                );
                // Store as pending link
                manager.pending_links.push(link.clone());
            } else {
                info!("Successfully linked: {} -> {}", link.from, link.to);
            }
        }
        info!(
            "Linking phase complete ({} pending links)",
            manager.pending_links.len()
        );

        // Set up dynamic pad handlers for all elements that might have dynamic pads
        info!("Setting up dynamic pad handlers...");
        manager.setup_dynamic_pad_handlers();
        info!("Dynamic pad handlers set up");

        // Apply pad properties now that pads have been created (during linking)
        // Note: Request pads (like audiomixer sink_%u) are created during linking
        info!("Applying pad properties...");
        manager.apply_pad_properties();
        info!("Pad properties applied");

        // Note: Bus watch is set up when pipeline starts, not here
        info!("Pipeline created successfully for flow: {}", flow.name);
        Ok(manager)
    }

    /// Set up bus message handlers to monitor pipeline messages.
    fn setup_bus_watch(&mut self) {
        // Clean up any existing message handlers first
        if !self.block_message_handlers.is_empty() {
            debug!(
                "Clearing {} existing message handlers for flow: {}",
                self.block_message_handlers.len(),
                self.flow_name
            );
            self.block_message_handlers.clear();
        }

        let Some(bus) = self.pipeline.bus() else {
            error!(
                "Pipeline '{}' does not have a bus - cannot set up message watch",
                self.flow_name
            );
            return;
        };

        // Set up block-specific message handlers using connect_message (allows multiple handlers)
        info!(
            "Connecting {} block message handler(s) for flow: {}",
            self.block_message_connect_fns.len(),
            self.flow_name
        );
        let flow_id = self.flow_id;
        let events_for_blocks = self.events.clone();

        // Take the connect functions (they're FnOnce, so we consume them)
        let connect_fns = std::mem::take(&mut self.block_message_connect_fns);
        for connect_fn in connect_fns {
            // Each block's connect_fn calls bus.add_signal_watch() and bus.connect_message()
            // add_signal_watch is ref-counted so multiple calls are safe
            let handler_id = connect_fn(&bus, flow_id, events_for_blocks.clone());
            debug!("Successfully connected block message handler");
            self.block_message_handlers.push(handler_id);
        }

        // Enable signal watch on the bus (ref-counted, safe to call multiple times)
        // This allows using connect_message for multiple handlers
        bus.add_signal_watch();

        // Set up main pipeline message handler using connect_message
        let flow_name = self.flow_name.clone();
        let events = self.events.clone();

        let main_handler_id = bus.connect_message(None, move |_bus, msg| {
            use gst::MessageView;

            // Log ALL bus messages to debug
            debug!("Bus message type: {:?}", msg.type_());

            match msg.view() {
                MessageView::Error(err) => {
                    let error_msg = err.error().to_string();
                    let debug_info = err.debug();
                    let source = err.src().map(|s| s.name().to_string());

                    error!(
                        "Pipeline error in flow '{}': {} (debug: {:?}, source: {:?})",
                        flow_name, error_msg, debug_info, source
                    );

                    events.broadcast(StromEvent::PipelineError {
                        flow_id,
                        error: error_msg,
                        source,
                    });
                }
                MessageView::Warning(warn) => {
                    let warning_msg = warn.error().to_string();
                    let debug_info = warn.debug();
                    let source = warn.src().map(|s| s.name().to_string());

                    warn!(
                        "Pipeline warning in flow '{}': {} (debug: {:?}, source: {:?})",
                        flow_name, warning_msg, debug_info, source
                    );

                    events.broadcast(StromEvent::PipelineWarning {
                        flow_id,
                        warning: warning_msg,
                        source,
                    });
                }
                MessageView::Info(inf) => {
                    let info_msg = inf.error().to_string();
                    let source = inf.src().map(|s| s.name().to_string());

                    info!(
                        "Pipeline info in flow '{}': {} (source: {:?})",
                        flow_name, info_msg, source
                    );

                    events.broadcast(StromEvent::PipelineInfo {
                        flow_id,
                        message: info_msg,
                        source,
                    });
                }
                MessageView::Eos(_) => {
                    info!("Pipeline '{}' reached end of stream", flow_name);
                    events.broadcast(StromEvent::PipelineEos { flow_id });
                }
                MessageView::StateChanged(state_changed) => {
                    // Log state changes from all elements to debug pausing issues
                    if let Some(source) = msg.src() {
                        let source_name = source.name();
                        let old_state = state_changed.old();
                        let new_state = state_changed.current();
                        let pending_state = state_changed.pending();

                        if source.type_() == gst::Pipeline::static_type() {
                            info!(
                                "Pipeline '{}' state changed: {:?} -> {:?} (pending: {:?})",
                                flow_name,
                                old_state,
                                new_state,
                                pending_state
                            );
                        } else {
                            // Log element state changes at debug level to avoid log spam
                            debug!(
                                "Element '{}' in pipeline '{}' state changed: {:?} -> {:?} (pending: {:?})",
                                source_name,
                                flow_name,
                                old_state,
                                new_state,
                                pending_state
                            );
                        }
                    }
                }
                _ => {
                    // Ignore other message types
                }
            }
        });

        // Store main handler ID (we'll disconnect it when stopping)
        self.block_message_handlers.push(main_handler_id);
        debug!("Bus message handlers set up for flow: {}", self.flow_name);
    }

    /// Remove the bus message handlers.
    fn remove_bus_watch(&mut self) {
        if !self.block_message_handlers.is_empty() {
            debug!(
                "Disconnecting {} message handler(s) for flow: {}",
                self.block_message_handlers.len(),
                self.flow_name
            );
            // Disconnect signal handlers from the bus
            if let Some(bus) = self.pipeline.bus() {
                for handler_id in self.block_message_handlers.drain(..) {
                    bus.disconnect(handler_id);
                }
                // Remove the signal watch (ref-counted, so this balances the add_signal_watch calls)
                bus.remove_signal_watch();
            } else {
                // Bus already gone, just clear the handlers
                self.block_message_handlers.clear();
            }
        }
    }

    /// Add an element to the pipeline.
    fn add_element(&mut self, element_def: &Element) -> Result<(), PipelineError> {
        info!(
            "add_element: Creating element {} (type: {})",
            element_def.id, element_def.element_type
        );

        // Create the element
        info!(
            "add_element: Calling ElementFactory::make for {}",
            element_def.element_type
        );
        let element = gst::ElementFactory::make(&element_def.element_type)
            .name(&element_def.id)
            .build()
            .map_err(|e| {
                error!(
                    "add_element: Failed to create element {}: {}",
                    element_def.id, e
                );
                PipelineError::ElementCreation(format!(
                    "{}: {} - {}",
                    element_def.id, element_def.element_type, e
                ))
            })?;
        info!(
            "add_element: Element {} created successfully",
            element_def.id
        );

        // Set properties
        info!(
            "add_element: Setting {} properties for element {}",
            element_def.properties.len(),
            element_def.id
        );
        for (prop_name, prop_value) in &element_def.properties {
            info!(
                "add_element: Setting property {}.{} = {:?}",
                element_def.id, prop_name, prop_value
            );
            self.set_property(&element, &element_def.id, prop_name, prop_value)?;
            info!(
                "add_element: Property {}.{} set successfully",
                element_def.id, prop_name
            );
        }
        info!(
            "add_element: All properties set for element {}",
            element_def.id
        );

        // Store pad properties for later application (after pads are created)
        if !element_def.pad_properties.is_empty() {
            info!(
                "add_element: Storing {} pad properties for element {}",
                element_def.pad_properties.len(),
                element_def.id
            );
            self.pad_properties
                .insert(element_def.id.clone(), element_def.pad_properties.clone());
        }

        // Add to pipeline
        info!(
            "add_element: Adding element {} to pipeline (this may block)...",
            element_def.id
        );
        self.pipeline.add(&element).map_err(|e| {
            error!(
                "add_element: Failed to add {} to pipeline: {}",
                element_def.id, e
            );
            PipelineError::ElementCreation(format!(
                "Failed to add {} to pipeline: {}",
                element_def.id, e
            ))
        })?;
        info!(
            "add_element: Element {} added to pipeline successfully",
            element_def.id
        );

        info!(
            "add_element: Inserting {} into elements map",
            element_def.id
        );
        self.elements.insert(element_def.id.clone(), element);
        info!(
            "add_element: Successfully completed adding element {}",
            element_def.id
        );
        Ok(())
    }

    /// Set a property on an element.
    fn set_property(
        &self,
        element: &gst::Element,
        element_id: &str,
        prop_name: &str,
        prop_value: &PropertyValue,
    ) -> Result<(), PipelineError> {
        debug!(
            "Setting property: {}.{} = {:?}",
            element_id, prop_name, prop_value
        );

        // Set property based on type
        match prop_value {
            PropertyValue::String(v) => {
                element.set_property_from_str(prop_name, v);
            }
            PropertyValue::Int(v) => {
                // Check property type to determine if we need i32, i64, or unsigned types
                if let Some(pspec) = element.find_property(prop_name) {
                    let type_name = pspec.value_type().name();
                    if type_name == "gint" || type_name == "glong" {
                        // Property expects i32
                        if let Ok(v32) = i32::try_from(*v) {
                            element.set_property(prop_name, v32);
                        } else {
                            return Err(PipelineError::InvalidProperty {
                                element: element_id.to_string(),
                                property: prop_name.to_string(),
                                reason: format!("Value {} doesn't fit in i32", v),
                            });
                        }
                    } else if type_name == "guint" || type_name == "gulong" {
                        // Property expects u32, but we got a signed int
                        // Convert if value is positive and fits in u32
                        if *v >= 0 {
                            if let Ok(v32) = u32::try_from(*v) {
                                element.set_property(prop_name, v32);
                            } else {
                                return Err(PipelineError::InvalidProperty {
                                    element: element_id.to_string(),
                                    property: prop_name.to_string(),
                                    reason: format!("Value {} doesn't fit in u32", v),
                                });
                            }
                        } else {
                            return Err(PipelineError::InvalidProperty {
                                element: element_id.to_string(),
                                property: prop_name.to_string(),
                                reason: format!(
                                    "Property expects unsigned integer, got negative value: {}",
                                    v
                                ),
                            });
                        }
                    } else if type_name == "guint64" {
                        // Property expects u64, convert if positive
                        if *v >= 0 {
                            element.set_property(prop_name, *v as u64);
                        } else {
                            return Err(PipelineError::InvalidProperty {
                                element: element_id.to_string(),
                                property: prop_name.to_string(),
                                reason: format!(
                                    "Property expects unsigned integer, got negative value: {}",
                                    v
                                ),
                            });
                        }
                    } else if type_name == "gint64" {
                        // Property expects i64
                        element.set_property(prop_name, *v);
                    } else {
                        // Try i64, might work
                        element.set_property(prop_name, *v);
                    }
                } else {
                    // Property not found, try anyway
                    element.set_property(prop_name, *v);
                }
            }
            PropertyValue::UInt(v) => {
                // Check property type to determine if we need u32 or u64
                if let Some(pspec) = element.find_property(prop_name) {
                    let type_name = pspec.value_type().name();
                    if type_name == "guint" || type_name == "gulong" {
                        // Property expects u32
                        if let Ok(v32) = u32::try_from(*v) {
                            element.set_property(prop_name, v32);
                        } else {
                            return Err(PipelineError::InvalidProperty {
                                element: element_id.to_string(),
                                property: prop_name.to_string(),
                                reason: format!("Value {} doesn't fit in u32", v),
                            });
                        }
                    } else if type_name == "guint64" {
                        // Property expects u64
                        element.set_property(prop_name, *v);
                    } else {
                        // Try u64, might work
                        element.set_property(prop_name, *v);
                    }
                } else {
                    // Property not found, try anyway
                    element.set_property(prop_name, *v);
                }
            }
            PropertyValue::Float(v) => {
                element.set_property(prop_name, *v);
            }
            PropertyValue::Bool(v) => {
                element.set_property(prop_name, *v);
            }
        }

        Ok(())
    }

    /// Try to link two elements according to a link definition.
    /// Returns Ok if successful, Err if pads don't exist yet (dynamic pads).
    fn try_link_elements(&self, link: &Link) -> Result<(), PipelineError> {
        debug!("Trying to link: {} -> {}", link.from, link.to);

        // Parse element:pad format (e.g., "src" or "src:pad_name")
        let (from_element, from_pad) = Self::parse_element_pad(&link.from);
        let (to_element, to_pad) = Self::parse_element_pad(&link.to);

        let src = self
            .elements
            .get(from_element)
            .ok_or_else(|| PipelineError::ElementNotFound(from_element.to_string()))?;

        let sink = self
            .elements
            .get(to_element)
            .ok_or_else(|| PipelineError::ElementNotFound(to_element.to_string()))?;

        // Link with or without specific pads
        if let (Some(src_pad_name), Some(sink_pad_name)) = (from_pad, to_pad) {
            // Try to get the pad - try static first, then request if not found
            let src_pad_obj = if let Some(pad) = src.static_pad(src_pad_name) {
                pad
            } else {
                // Pad not static - try to request it
                // First try request_pad_simple with the exact name
                if let Some(pad) = src.request_pad_simple(src_pad_name) {
                    pad
                } else {
                    // If that didn't work, try finding a compatible pad template
                    // This handles cases like "src_0" needing the "src_%u" template (e.g., tee)
                    // IMPORTANT: We get pad templates directly from the element, not from the factory.
                    // Accessing static_pad_templates from the factory can corrupt GStreamer state
                    // for aggregator elements like mpegtsmux (see discovery.rs:533-538).
                    let element_pad_templates = src.pad_template_list();
                    let pad_template = element_pad_templates
                        .iter()
                        .filter(|tmpl| {
                            tmpl.presence() == gst::PadPresence::Request
                                && tmpl.direction() == gst::PadDirection::Src
                        })
                        .find(|tmpl| {
                            let name_template = tmpl.name_template();
                            // Check if this template could produce the requested pad name
                            if name_template.contains("%u") || name_template.contains("%d") {
                                let prefix = name_template.split('%').next().unwrap_or("");
                                src_pad_name.starts_with(prefix)
                            } else {
                                name_template == src_pad_name
                            }
                        });

                    if let Some(pad_tmpl) = pad_template {
                        let tmpl_name = pad_tmpl.name_template();
                        debug!(
                            "Found matching pad template '{}' for pad name '{}'",
                            tmpl_name, src_pad_name
                        );
                        // Request a new pad from the template - let GStreamer auto-name it
                        if let Some(pad) = src.request_pad(pad_tmpl, None, None) {
                            debug!(
                                "Successfully requested pad '{}' for requested name '{}'",
                                pad.name(),
                                src_pad_name
                            );
                            pad
                        } else {
                            // Couldn't get pad from template
                            return Err(PipelineError::LinkError(
                                link.from.clone(),
                                format!(
                                    "Source pad {} not available (tried template '{}')",
                                    src_pad_name, tmpl_name
                                ),
                            ));
                        }
                    } else {
                        // No compatible template found - might be a dynamic pad
                        return Err(PipelineError::LinkError(
                            link.from.clone(),
                            format!(
                                "Source pad {} not available yet (dynamic pad)",
                                src_pad_name
                            ),
                        ));
                    }
                }
            };

            // Try to get sink pad - try static first, then request if not found
            info!(
                "Trying to get sink pad '{}' on element '{}'",
                sink_pad_name, to_element
            );

            // Special handling for mpegtsmux - use direct element-to-element linking to avoid request_pad() deadlock
            let element_type_name = sink
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();
            if element_type_name == "mpegtsmux" {
                info!("Detected mpegtsmux - using element-level linking to avoid request_pad deadlock");
                // For mpegtsmux, link directly at element level using the source element
                // GStreamer will internally handle pad requesting without us having to call request_pad()
                // We need to link from source element, not from the source pad object
                if let Err(e) = src.link(sink) {
                    return Err(PipelineError::LinkError(
                        link.from.clone(),
                        format!("Failed to auto-link to mpegtsmux: {}", e),
                    ));
                }
                info!(
                    "Successfully auto-linked to mpegtsmux: {} -> {}",
                    link.from, link.to
                );
                return Ok(());
            }

            let sink_pad_obj = if let Some(pad) = sink.static_pad(sink_pad_name) {
                info!("Found static sink pad: {}", sink_pad_name);
                pad
            } else {
                info!(
                    "Sink pad '{}' not static, trying to request it...",
                    sink_pad_name
                );
                // Pad not static - try to request it
                // First try request_pad_simple with the exact name
                info!(
                    "Calling request_pad_simple('{}') on {} (this may block)...",
                    sink_pad_name, to_element
                );
                if let Some(pad) = sink.request_pad_simple(sink_pad_name) {
                    info!("Successfully requested sink pad: {}", sink_pad_name);
                    pad
                } else {
                    info!("request_pad_simple returned None, trying pad template matching...");

                    // If that didn't work, try finding a compatible pad template
                    // This handles cases like "sink_0" needing the "sink_%u" template
                    // IMPORTANT: We get pad templates directly from the element, not from the factory.
                    // Accessing static_pad_templates from the factory can corrupt GStreamer state
                    // for aggregator elements like mpegtsmux (see discovery.rs:533-538).

                    info!("Trying to find matching pad template on element...");
                    // Get pad template list directly from the element (not factory)
                    let element_pad_templates = sink.pad_template_list();
                    debug!(
                        "Available pad templates from element: {:?}",
                        element_pad_templates
                            .iter()
                            .map(|t| format!(
                                "{} (direction: {:?}, presence: {:?})",
                                t.name_template(),
                                t.direction(),
                                t.presence()
                            ))
                            .collect::<Vec<_>>()
                    );

                    let pad_template = element_pad_templates
                        .iter()
                        .filter(|tmpl| {
                            tmpl.presence() == gst::PadPresence::Request
                                && tmpl.direction() == gst::PadDirection::Sink
                        })
                        .find(|tmpl| {
                            let name_template = tmpl.name_template();
                            // Check if this template could produce the requested pad name
                            // e.g., "sink_%u" can produce "sink_0", "sink_1", etc.
                            if name_template.contains("%u") || name_template.contains("%d") {
                                let prefix = name_template.split('%').next().unwrap_or("");
                                let matches = sink_pad_name.starts_with(prefix);
                                debug!(
                                    "Checking template '{}': prefix='{}', pad_name='{}', matches={}",
                                    name_template, prefix, sink_pad_name, matches
                                );
                                matches
                            } else {
                                name_template == sink_pad_name
                            }
                        });

                    if let Some(pad_tmpl) = pad_template {
                        let tmpl_name = pad_tmpl.name_template();
                        info!(
                            "Found matching pad template '{}' for pad name '{}'",
                            tmpl_name, sink_pad_name
                        );
                        // Request a new pad from the template - let GStreamer auto-name it
                        info!("Calling request_pad on element with template (this may block)...");
                        if let Some(pad) = sink.request_pad(pad_tmpl, None, None) {
                            info!(
                                "Successfully requested pad '{}' for requested name '{}'",
                                pad.name(),
                                sink_pad_name
                            );
                            pad
                        } else {
                            // Couldn't get pad from template
                            return Err(PipelineError::LinkError(
                                link.to.clone(),
                                format!(
                                    "Sink pad {} not available (tried template '{}')",
                                    sink_pad_name, tmpl_name
                                ),
                            ));
                        }
                    } else {
                        // No compatible template found - might be a dynamic pad
                        return Err(PipelineError::LinkError(
                            link.to.clone(),
                            format!("Sink pad {} not available yet (dynamic pad)", sink_pad_name),
                        ));
                    }
                }
            };

            src_pad_obj.link(&sink_pad_obj).map_err(|e| {
                PipelineError::LinkError(link.from.clone(), format!("{} - {}", link.to, e))
            })?;

            debug!("Successfully linked: {} -> {}", link.from, link.to);
        } else {
            // Simple link without pad names
            src.link(sink).map_err(|e| {
                PipelineError::LinkError(link.from.clone(), format!("{} - {}", link.to, e))
            })?;

            debug!("Successfully linked: {} -> {}", link.from, link.to);
        }

        Ok(())
    }

    /// Parse element:pad format into (element_id, optional pad_name).
    /// Handles namespaced block elements like "block_0:rtpL24pay:sink".
    /// Splits from the right to get the last colon-separated part as the pad name.
    fn parse_element_pad(spec: &str) -> (&str, Option<&str>) {
        if let Some((element, pad)) = spec.rsplit_once(':') {
            (element, Some(pad))
        } else {
            (spec, None)
        }
    }

    /// Set up pad-added signal handlers for elements with dynamic pads.
    fn setup_dynamic_pad_handlers(&mut self) {
        if self.pending_links.is_empty() {
            return;
        }

        info!(
            "Setting up dynamic pad handlers for {} pending link(s)",
            self.pending_links.len()
        );

        // For each element that might have dynamic pads, connect to pad-added signal
        let elements_map = self.elements.clone();
        let pending_links = self.pending_links.clone();

        for (element_id, element) in &self.elements {
            let element_id = element_id.clone();
            let elements_map = elements_map.clone();
            let pending_links = pending_links.clone();

            // Connect to pad-added signal
            element.connect_pad_added(move |_elem, new_pad| {
                let new_pad_name = new_pad.name();
                debug!("Pad added on element {}: {}", element_id, new_pad_name);

                // Check if any pending links match this pad
                for link in &pending_links {
                    let (from_elem, from_pad) = Self::parse_element_pad(&link.from);
                    let (to_elem, to_pad) = Self::parse_element_pad(&link.to);

                    // Check if this new pad matches a pending source pad
                    if from_elem == element_id {
                        if let Some(expected_pad_name) = from_pad {
                            if new_pad_name == expected_pad_name {
                                // This is the source pad we're waiting for
                                if let (Some(_src_elem), Some(sink_elem)) =
                                    (elements_map.get(from_elem), elements_map.get(to_elem))
                                {
                                    if let Some(sink_pad_name) = to_pad {
                                        // Get the sink pad
                                        let sink_pad = if let Some(pad) =
                                            sink_elem.static_pad(sink_pad_name)
                                        {
                                            pad
                                        } else if let Some(pad) =
                                            sink_elem.request_pad_simple(sink_pad_name)
                                        {
                                            pad
                                        } else {
                                            warn!(
                                                "Sink pad {} not found on {}",
                                                sink_pad_name, to_elem
                                            );
                                            continue;
                                        };

                                        // Try to link
                                        match new_pad.link(&sink_pad) {
                                            Ok(_) => {
                                                info!(
                                                    "Successfully linked dynamic pad: {} -> {}",
                                                    link.from, link.to
                                                );
                                            }
                                            Err(e) => {
                                                error!(
                                                    "Failed to link dynamic pad {} -> {}: {}",
                                                    link.from, link.to, e
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
    }

    /// Apply stored pad properties to pads after they've been created.
    /// This must be called after all linking is complete, since request pads
    /// (like audiomixer sink_%u) don't exist until they're requested during linking.
    fn apply_pad_properties(&self) {
        if self.pad_properties.is_empty() {
            return;
        }

        info!(
            "Applying pad properties for {} element(s)",
            self.pad_properties.len()
        );

        for (element_id, pad_props) in &self.pad_properties {
            let Some(element) = self.elements.get(element_id) else {
                warn!(
                    "Element {} not found when trying to apply pad properties",
                    element_id
                );
                continue;
            };

            for (pad_name, properties) in pad_props {
                // Try to get the pad - try static first, then request
                let pad = if let Some(p) = element.static_pad(pad_name) {
                    p
                } else if let Some(p) = element.request_pad_simple(pad_name) {
                    p
                } else {
                    warn!(
                        "Pad {}:{} not found when trying to apply pad properties",
                        element_id, pad_name
                    );
                    continue;
                };

                debug!(
                    "Applying {} properties to pad {}:{}",
                    properties.len(),
                    element_id,
                    pad_name
                );

                // Apply each property
                for (prop_name, prop_value) in properties {
                    if let Err(e) =
                        self.set_pad_property(&pad, element_id, pad_name, prop_name, prop_value)
                    {
                        error!(
                            "Failed to set pad property {}:{}:{}: {}",
                            element_id, pad_name, prop_name, e
                        );
                    } else {
                        info!(
                            "Set pad property {}:{}:{} = {:?}",
                            element_id, pad_name, prop_name, prop_value
                        );
                    }
                }
            }
        }
    }

    /// Analyze links and insert tee elements where multiple links share the same source.
    fn insert_tees_if_needed(original_links: &[Link]) -> ProcessedLinks {
        use std::collections::HashMap;

        // Count how many times each source spec appears
        let mut source_counts: HashMap<String, usize> = HashMap::new();
        for link in original_links {
            *source_counts.entry(link.from.clone()).or_insert(0) += 1;
        }

        // Find sources that need a tee (appear more than once)
        let sources_needing_tee: Vec<String> = source_counts
            .iter()
            .filter(|(_, &count)| count > 1)
            .map(|(src, _)| src.clone())
            .collect();

        if sources_needing_tee.is_empty() {
            // No tees needed, return original links
            info!("No tee elements needed");
            return ProcessedLinks {
                links: original_links.to_vec(),
                tees: HashMap::new(),
            };
        }

        info!(
            "Auto-inserting {} tee element(s) for sources with multiple outputs",
            sources_needing_tee.len()
        );

        let mut new_links = Vec::new();
        let mut tees = HashMap::new();
        let mut tee_src_counters: HashMap<String, usize> = HashMap::new();

        for src_spec in &sources_needing_tee {
            let tee_id = format!("auto_tee_{}", src_spec.replace(":", "_"));
            tees.insert(tee_id.clone(), src_spec.clone());

            // Add link from original source to tee (without explicit sink pad, tee will auto-connect)
            new_links.push(Link {
                from: src_spec.clone(),
                to: format!("{}:sink", tee_id),
            });

            info!("Created tee element '{}' for source '{}'", tee_id, src_spec);
        }

        // Process original links
        for link in original_links {
            if sources_needing_tee.contains(&link.from) {
                // This source needs a tee - link from tee to destination
                let tee_id = format!("auto_tee_{}", link.from.replace(":", "_"));

                // Get next src pad from tee (src_0, src_1, src_2, ...)
                let counter = tee_src_counters.entry(tee_id.clone()).or_insert(0);
                let tee_src_pad = format!("{}:src_{}", tee_id, counter);
                *counter += 1;

                new_links.push(Link {
                    from: tee_src_pad,
                    to: link.to.clone(),
                });
            } else {
                // No tee needed, keep original link
                new_links.push(link.clone());
            }
        }

        ProcessedLinks {
            links: new_links,
            tees,
        }
    }

    /// Add a tee element to the pipeline.
    fn add_tee_element(&mut self, tee_id: &str) -> Result<(), PipelineError> {
        debug!("Creating auto-inserted tee element: {}", tee_id);

        let tee = gst::ElementFactory::make("tee")
            .name(tee_id)
            .build()
            .map_err(|e| {
                PipelineError::ElementCreation(format!("Failed to create tee {}: {}", tee_id, e))
            })?;

        // Configure tee to allow branches to not be linked without affecting other branches
        // This prevents one branch from blocking others when it goes to PAUSED
        tee.set_property("allow-not-linked", true);

        self.pipeline.add(&tee).map_err(|e| {
            PipelineError::ElementCreation(format!(
                "Failed to add tee {} to pipeline: {}",
                tee_id, e
            ))
        })?;

        self.elements.insert(tee_id.to_string(), tee);
        Ok(())
    }

    /// Configure the pipeline clock based on flow properties.
    ///
    /// For direct media timing (AES67), we always set:
    /// - base_time = 0
    /// - start_time = None (GST_CLOCK_TIME_NONE)
    ///
    /// This ensures that RTP timestamps directly correspond to the reference clock,
    /// which is required for `a=mediaclk:direct=0` signaling in SDP (RFC 7273).
    fn configure_clock(&mut self) -> Result<(), PipelineError> {
        use strom_types::flow::GStreamerClockType;

        match self.properties.clock_type {
            GStreamerClockType::Ptp => {
                let domain = self.properties.ptp_domain.unwrap_or(0);
                info!(
                    "Configuring PTP clock for pipeline '{}' with domain {}",
                    self.flow_name, domain
                );

                // Initialize PTP clock (on all interfaces)
                // This is a global init, so it's OK if it was already initialized
                if let Err(e) = gst_net::PtpClock::init(None, &[]) {
                    warn!(
                        "PTP clock initialization warning (may already be initialized): {}",
                        e
                    );
                }

                // Create a PTP clock instance for this domain
                let ptp_clock = gst_net::PtpClock::new(None, domain as u32).map_err(|e| {
                    PipelineError::StateChange(format!("Failed to create PTP clock: {}", e))
                })?;

                // Set the PTP clock as the pipeline's clock
                self.pipeline.set_clock(Some(&ptp_clock)).map_err(|e| {
                    PipelineError::StateChange(format!("Failed to set PTP clock: {}", e))
                })?;

                info!("PTP clock configured: domain={}", domain);
            }
            GStreamerClockType::Monotonic => {
                info!("Using Monotonic clock for pipeline '{}'", self.flow_name);
                // Create a system monotonic clock
                let clock = gst::SystemClock::obtain();
                self.pipeline.set_clock(Some(&clock)).map_err(|e| {
                    PipelineError::StateChange(format!("Failed to set monotonic clock: {}", e))
                })?;
            }
            GStreamerClockType::Realtime => {
                info!("Using Realtime clock for pipeline '{}'", self.flow_name);
                // For realtime, we'd need a custom clock implementation
                // For now, use the system clock which is close to realtime
                let clock = gst::SystemClock::obtain();
                self.pipeline.set_clock(Some(&clock)).map_err(|e| {
                    PipelineError::StateChange(format!("Failed to set realtime clock: {}", e))
                })?;
            }
            GStreamerClockType::Ntp => {
                info!(
                    "NTP clock requested for pipeline '{}' - using system clock as fallback",
                    self.flow_name
                );
                // NTP clock implementation would require additional setup
                // For now, fall back to system clock
                let clock = gst::SystemClock::obtain();
                self.pipeline.set_clock(Some(&clock)).map_err(|e| {
                    PipelineError::StateChange(format!("Failed to set clock: {}", e))
                })?;
                warn!("NTP clock not yet fully implemented, using system clock");
            }
        }

        // For direct media timing (AES67 / RFC 7273):
        // Set base_time to 0 and start_time to NONE for ALL clock types.
        // This makes RTP timestamps directly correspond to the reference clock,
        // which is required for mediaclk:direct=0 signaling.
        //
        // Combined with timestamp-offset=0 on the RTP payloader (set in aes67.rs),
        // this ensures GStreamer generates RTP timestamps that directly reflect
        // the pipeline clock time.
        self.pipeline.set_base_time(gst::ClockTime::ZERO);
        self.pipeline.set_start_time(gst::ClockTime::NONE);
        info!(
            "Pipeline '{}' configured for direct media timing: base_time=0, start_time=None",
            self.flow_name
        );

        Ok(())
    }

    /// Start the pipeline (set to PLAYING state).
    pub fn start(&mut self) -> Result<PipelineState, PipelineError> {
        info!("Starting pipeline: {}", self.flow_name);
        info!("Pipeline has {} elements", self.elements.len());

        // Set up thread priority handler FIRST (before any state changes)
        // This must be done before the pipeline starts so we catch all thread enter events
        info!(
            "Setting up thread priority handler (requested: {:?})...",
            self.properties.thread_priority
        );
        let priority_state = thread_priority::setup_thread_priority_handler(
            &self.pipeline,
            self.properties.thread_priority,
        );
        self.thread_priority_state = Some(priority_state);
        info!("Thread priority handler installed");

        // Set up bus watch before starting
        info!("Setting up bus watch...");
        self.setup_bus_watch();
        info!("Bus watch set up");

        // Configure clock before starting
        info!(
            "Configuring clock (type: {:?})...",
            self.properties.clock_type
        );
        self.configure_clock()?;
        info!("Clock configured");

        info!(
            "Setting pipeline '{}' to PLAYING state (this may block)...",
            self.flow_name
        );
        let state_change_result = self.pipeline.set_state(gst::State::Playing);
        info!("set_state(Playing) call returned");

        match &state_change_result {
            Ok(gst::StateChangeSuccess::Success) => {
                info!("Pipeline '{}' set to PLAYING: Success", self.flow_name);
            }
            Ok(gst::StateChangeSuccess::Async) => {
                info!(
                    "Pipeline '{}' set to PLAYING: Async (state change in progress)",
                    self.flow_name
                );
            }
            Ok(gst::StateChangeSuccess::NoPreroll) => {
                info!(
                    "Pipeline '{}' set to PLAYING: NoPreroll (live source)",
                    self.flow_name
                );
            }
            Err(e) => {
                error!("Pipeline '{}' failed to start: {}", self.flow_name, e);
            }
        }

        state_change_result
            .map_err(|e| PipelineError::StateChange(format!("Failed to start: {}", e)))?;

        // Wait a moment to get the actual state
        info!("Sleeping 100ms to allow state change to complete...");
        std::thread::sleep(std::time::Duration::from_millis(100));
        info!("Querying pipeline state...");
        let (result, current_state, pending_state) =
            self.pipeline.state(gst::ClockTime::from_mseconds(100));
        info!(
            "Pipeline '{}' state after start: result={:?}, current={:?}, pending={:?}",
            self.flow_name, result, current_state, pending_state
        );

        // Check if we've reached the target state
        // If current_state is Playing and pending is VoidPending, that's success!
        let target_reached =
            current_state == gst::State::Playing && pending_state == gst::State::VoidPending;

        if !target_reached {
            // Only fail if we haven't reached the target state
            if let Err(e) = result {
                error!(
                    "Pipeline '{}' failed to reach PLAYING state: {:?} (current: {:?}, pending: {:?})",
                    self.flow_name, e, current_state, pending_state
                );
                return Err(PipelineError::StateChange(format!(
                    "State change failed: {:?} - current: {:?}, pending: {:?}",
                    e, current_state, pending_state
                )));
            }
        } else {
            info!(
                "Pipeline '{}' successfully reached PLAYING state",
                self.flow_name
            );
        }

        // Return the actual current state
        let actual_state = match current_state {
            gst::State::Null => PipelineState::Null,
            gst::State::Ready => PipelineState::Ready,
            gst::State::Paused => PipelineState::Paused,
            gst::State::Playing => PipelineState::Playing,
            _ => PipelineState::Null,
        };

        Ok(actual_state)
    }

    /// Stop the pipeline (set to NULL state).
    pub fn stop(&mut self) -> Result<PipelineState, PipelineError> {
        info!("Stopping pipeline: {}", self.flow_name);

        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|e| PipelineError::StateChange(format!("Failed to stop: {}", e)))?;

        // Remove bus watch when stopped to free resources
        self.remove_bus_watch();

        // Remove thread priority handler
        thread_priority::remove_thread_priority_handler(&self.pipeline);
        self.thread_priority_state = None;

        Ok(PipelineState::Null)
    }

    /// Pause the pipeline.
    pub fn pause(&self) -> Result<PipelineState, PipelineError> {
        info!("Pausing pipeline: {}", self.flow_name);

        self.pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| PipelineError::StateChange(format!("Failed to pause: {}", e)))?;

        Ok(PipelineState::Paused)
    }

    /// Get the current state of the pipeline.
    pub fn get_state(&self) -> PipelineState {
        let (_, state, _) = self.pipeline.state(gst::ClockTime::from_seconds(1));

        match state {
            gst::State::Null => PipelineState::Null,
            gst::State::Ready => PipelineState::Ready,
            gst::State::Paused => PipelineState::Paused,
            gst::State::Playing => PipelineState::Playing,
            _ => PipelineState::Null,
        }
    }

    /// Get the flow ID this pipeline manages.
    pub fn flow_id(&self) -> FlowId {
        self.flow_id
    }

    /// Get the thread priority status for this pipeline.
    /// Returns None if pipeline hasn't been started yet.
    pub fn get_thread_priority_status(&self) -> Option<ThreadPriorityStatus> {
        self.thread_priority_state
            .as_ref()
            .map(|state| state.get_status())
    }

    /// Get the clock synchronization status for this pipeline.
    pub fn get_clock_sync_status(&self) -> strom_types::flow::ClockSyncStatus {
        use strom_types::flow::{ClockSyncStatus, GStreamerClockType};

        match self.properties.clock_type {
            GStreamerClockType::Ptp => {
                // For PTP clocks, use the is_synced() method from gst::Clock trait
                // This returns true only when the clock has synchronized with a PTP master
                if let Some(clock) = self.pipeline.clock() {
                    if clock.is_synced() {
                        ClockSyncStatus::Synced
                    } else {
                        ClockSyncStatus::NotSynced
                    }
                } else {
                    // No clock set yet
                    ClockSyncStatus::NotSynced
                }
            }
            GStreamerClockType::Ntp => {
                // For NTP clocks, use the is_synced() method from gst::Clock trait
                // Note: NTP clock not fully implemented yet, so this is best-effort
                if let Some(clock) = self.pipeline.clock() {
                    if clock.is_synced() {
                        ClockSyncStatus::Synced
                    } else {
                        ClockSyncStatus::NotSynced
                    }
                } else {
                    ClockSyncStatus::NotSynced
                }
            }
            _ => {
                // For other clock types (Monotonic, Realtime, PipelineDefault),
                // sync status is not applicable - these are local clocks
                ClockSyncStatus::Unknown
            }
        }
    }

    /// Get the underlying GStreamer pipeline (for debugging).
    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    /// Generate a DOT graph of the pipeline for debugging.
    /// Returns the DOT graph content as a string.
    pub fn generate_dot_graph(&self) -> String {
        use gst::prelude::*;

        info!("Generating DOT graph for pipeline: {}", self.flow_name);

        // Use GStreamer's debug graph dump functionality
        // The details level determines how much information is included
        let dot = self
            .pipeline
            .debug_to_dot_data(gst::DebugGraphDetails::all());

        dot.to_string()
    }

    /// Update a property on a live element in the pipeline.
    /// Validates that the property can be changed in the current pipeline state.
    pub fn update_element_property(
        &self,
        element_id: &str,
        property_name: &str,
        value: &PropertyValue,
    ) -> Result<(), PipelineError> {
        debug!(
            "Updating property {}.{} to {:?} on running pipeline",
            element_id, property_name, value
        );

        // Get element reference
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        // Get current pipeline state
        let state = self.get_state();

        // Validate property is mutable in current state
        self.validate_property_mutability(element, element_id, property_name, state)?;

        // Set the property (reuse existing set_property method)
        self.set_property(element, element_id, property_name, value)?;

        info!(
            "Successfully updated property {}.{} to {:?}",
            element_id, property_name, value
        );

        Ok(())
    }

    /// Get current value of a property from a live element.
    pub fn get_element_property(
        &self,
        element_id: &str,
        property_name: &str,
    ) -> Result<PropertyValue, PipelineError> {
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        // Get property spec to determine type
        let pspec =
            element
                .find_property(property_name)
                .ok_or_else(|| PipelineError::InvalidProperty {
                    element: element_id.to_string(),
                    property: property_name.to_string(),
                    reason: "Property not found".to_string(),
                })?;

        let type_name = pspec.value_type().name();

        // Get property value based on type
        let value = match type_name.to_string().as_str() {
            "gchararray" => {
                let v = element.property::<Option<String>>(property_name);
                v.map(PropertyValue::String)
                    .unwrap_or(PropertyValue::String(String::new()))
            }
            "gboolean" => {
                let v = element.property::<bool>(property_name);
                PropertyValue::Bool(v)
            }
            "gint" | "glong" => {
                let v = element.property::<i32>(property_name);
                PropertyValue::Int(v as i64)
            }
            "gint64" => {
                let v = element.property::<i64>(property_name);
                PropertyValue::Int(v)
            }
            "guint" | "gulong" => {
                let v = element.property::<u32>(property_name);
                PropertyValue::UInt(v as u64)
            }
            "guint64" => {
                let v = element.property::<u64>(property_name);
                PropertyValue::UInt(v)
            }
            "gfloat" => {
                let v = element.property::<f32>(property_name);
                PropertyValue::Float(v as f64)
            }
            "gdouble" => {
                let v = element.property::<f64>(property_name);
                PropertyValue::Float(v)
            }
            "GEnum" => {
                // Get enum as string
                // In GStreamer 0.24.x, enum properties have stricter types and can't always be read as i32
                // We need to use the Value API and handle type conversion carefully
                if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecEnum>() {
                    let enum_class = param_spec.enum_class();

                    // Get the property as a Value, then try to extract the enum value
                    let value = element.property_value(property_name);

                    // Try to get as i32 (standard enum representation)
                    match value.get::<i32>() {
                        Ok(v) => {
                            if let Some(enum_value) = enum_class.value(v) {
                                PropertyValue::String(enum_value.name().to_string())
                            } else {
                                PropertyValue::Int(v as i64)
                            }
                        }
                        Err(_) => {
                            // Can't convert to i32, this enum type is not supported
                            return Err(PipelineError::InvalidProperty {
                                element: element_id.to_string(),
                                property: property_name.to_string(),
                                reason: format!(
                                    "Cannot read enum property of type {} (not convertible to i32)",
                                    type_name
                                ),
                            });
                        }
                    }
                } else {
                    // Fallback if we can't get the enum class
                    return Err(PipelineError::InvalidProperty {
                        element: element_id.to_string(),
                        property: property_name.to_string(),
                        reason: "Cannot read enum property spec".to_string(),
                    });
                }
            }
            _ => {
                return Err(PipelineError::InvalidProperty {
                    element: element_id.to_string(),
                    property: property_name.to_string(),
                    reason: format!("Unsupported property type: {}", type_name),
                });
            }
        };

        Ok(value)
    }

    /// Get all readable property values from a live element.
    pub fn get_element_properties(
        &self,
        element_id: &str,
    ) -> Result<HashMap<String, PropertyValue>, PipelineError> {
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        let mut properties = HashMap::new();

        // Get all properties from the element
        for pspec in element.list_properties() {
            let name = pspec.name().to_string();

            // Skip non-readable properties
            if !pspec.flags().contains(glib::ParamFlags::READABLE) {
                continue;
            }

            // Skip internal/private properties
            if name.starts_with('_') {
                continue;
            }

            // Try to get the property value
            if let Ok(value) = self.get_element_property(element_id, &name) {
                properties.insert(name, value);
            }
        }

        Ok(properties)
    }

    /// Update a property on a pad in the pipeline.
    /// Validates that the property can be changed in the current pipeline state.
    pub fn update_pad_property(
        &self,
        element_id: &str,
        pad_name: &str,
        property_name: &str,
        value: &PropertyValue,
    ) -> Result<(), PipelineError> {
        debug!(
            "Updating pad property {}:{}:{} to {:?}",
            element_id, pad_name, property_name, value
        );

        // Get element reference
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        // Get pad reference - try static pad first, then request pad
        let pad = if let Some(p) = element.static_pad(pad_name) {
            p
        } else if let Some(p) = element.request_pad_simple(pad_name) {
            p
        } else {
            return Err(PipelineError::PadNotFound {
                element: element_id.to_string(),
                pad: pad_name.to_string(),
            });
        };

        // Get current pipeline state
        let state = self.get_state();

        // Validate property is mutable in current state (using pad's property spec)
        self.validate_pad_property_mutability(&pad, element_id, pad_name, property_name, state)?;

        // Set the property on the pad
        self.set_pad_property(&pad, element_id, pad_name, property_name, value)?;

        info!(
            "Successfully updated pad property {}:{}:{} to {:?}",
            element_id, pad_name, property_name, value
        );

        Ok(())
    }

    /// Get current value of a property from a pad.
    pub fn get_pad_property(
        &self,
        element_id: &str,
        pad_name: &str,
        property_name: &str,
    ) -> Result<PropertyValue, PipelineError> {
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        // Get pad reference
        let pad = if let Some(p) = element.static_pad(pad_name) {
            p
        } else if let Some(p) = element.request_pad_simple(pad_name) {
            p
        } else {
            return Err(PipelineError::PadNotFound {
                element: element_id.to_string(),
                pad: pad_name.to_string(),
            });
        };

        // Get property spec to determine type
        let pspec =
            pad.find_property(property_name)
                .ok_or_else(|| PipelineError::InvalidProperty {
                    element: format!("{}:{}", element_id, pad_name),
                    property: property_name.to_string(),
                    reason: "Property not found on pad".to_string(),
                })?;

        let type_name = pspec.value_type().name();

        // Get property value based on type
        let value = match type_name.to_string().as_str() {
            "gchararray" => {
                let v = pad.property::<Option<String>>(property_name);
                v.map(PropertyValue::String)
                    .unwrap_or(PropertyValue::String(String::new()))
            }
            "gboolean" => {
                let v = pad.property::<bool>(property_name);
                PropertyValue::Bool(v)
            }
            "gint" | "glong" => {
                let v = pad.property::<i32>(property_name);
                PropertyValue::Int(v as i64)
            }
            "gint64" => {
                let v = pad.property::<i64>(property_name);
                PropertyValue::Int(v)
            }
            "guint" | "gulong" => {
                let v = pad.property::<u32>(property_name);
                PropertyValue::UInt(v as u64)
            }
            "guint64" => {
                let v = pad.property::<u64>(property_name);
                PropertyValue::UInt(v)
            }
            "gfloat" => {
                let v = pad.property::<f32>(property_name);
                PropertyValue::Float(v as f64)
            }
            "gdouble" => {
                let v = pad.property::<f64>(property_name);
                PropertyValue::Float(v)
            }
            _ => {
                return Err(PipelineError::InvalidProperty {
                    element: format!("{}:{}", element_id, pad_name),
                    property: property_name.to_string(),
                    reason: format!("Unsupported property type: {}", type_name),
                });
            }
        };

        Ok(value)
    }

    /// Get all readable property values from a pad.
    pub fn get_pad_properties(
        &self,
        element_id: &str,
        pad_name: &str,
    ) -> Result<HashMap<String, PropertyValue>, PipelineError> {
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        // Get pad reference
        let pad = if let Some(p) = element.static_pad(pad_name) {
            p
        } else if let Some(p) = element.request_pad_simple(pad_name) {
            p
        } else {
            return Err(PipelineError::PadNotFound {
                element: element_id.to_string(),
                pad: pad_name.to_string(),
            });
        };

        let mut properties = HashMap::new();

        // Get all properties from the pad
        for pspec in pad.list_properties() {
            let name = pspec.name().to_string();

            // Skip non-readable properties
            if !pspec.flags().contains(glib::ParamFlags::READABLE) {
                continue;
            }

            // Skip internal/private properties
            if name.starts_with('_') {
                continue;
            }

            // Try to get the property value
            if let Ok(value) = self.get_pad_property(element_id, pad_name, &name) {
                properties.insert(name, value);
            }
        }

        Ok(properties)
    }

    /// Set a property on a pad.
    fn set_pad_property(
        &self,
        pad: &gst::Pad,
        element_id: &str,
        pad_name: &str,
        prop_name: &str,
        prop_value: &PropertyValue,
    ) -> Result<(), PipelineError> {
        debug!(
            "Setting pad property: {}:{}:{} = {:?}",
            element_id, pad_name, prop_name, prop_value
        );

        // Set property based on type
        match prop_value {
            PropertyValue::String(v) => {
                pad.set_property_from_str(prop_name, v);
            }
            PropertyValue::Int(v) => {
                // Check property type to determine if we need i32 or i64
                if let Some(pspec) = pad.find_property(prop_name) {
                    let type_name = pspec.value_type().name();
                    if type_name == "gint" || type_name == "glong" {
                        if let Ok(v32) = i32::try_from(*v) {
                            pad.set_property(prop_name, v32);
                        } else {
                            return Err(PipelineError::InvalidProperty {
                                element: format!("{}:{}", element_id, pad_name),
                                property: prop_name.to_string(),
                                reason: format!("Value {} doesn't fit in i32", v),
                            });
                        }
                    } else {
                        pad.set_property(prop_name, *v);
                    }
                } else {
                    pad.set_property(prop_name, *v);
                }
            }
            PropertyValue::UInt(v) => {
                if let Some(pspec) = pad.find_property(prop_name) {
                    let type_name = pspec.value_type().name();
                    if type_name == "guint" || type_name == "gulong" {
                        if let Ok(v32) = u32::try_from(*v) {
                            pad.set_property(prop_name, v32);
                        } else {
                            return Err(PipelineError::InvalidProperty {
                                element: format!("{}:{}", element_id, pad_name),
                                property: prop_name.to_string(),
                                reason: format!("Value {} doesn't fit in u32", v),
                            });
                        }
                    } else {
                        pad.set_property(prop_name, *v);
                    }
                } else {
                    pad.set_property(prop_name, *v);
                }
            }
            PropertyValue::Float(v) => {
                pad.set_property(prop_name, *v);
            }
            PropertyValue::Bool(v) => {
                pad.set_property(prop_name, *v);
            }
        }

        Ok(())
    }

    /// Validate that a pad property can be changed in the current pipeline state.
    fn validate_pad_property_mutability(
        &self,
        pad: &gst::Pad,
        element_id: &str,
        pad_name: &str,
        property_name: &str,
        current_state: PipelineState,
    ) -> Result<(), PipelineError> {
        let pspec =
            pad.find_property(property_name)
                .ok_or_else(|| PipelineError::InvalidProperty {
                    element: format!("{}:{}", element_id, pad_name),
                    property: property_name.to_string(),
                    reason: "Property not found on pad".to_string(),
                })?;

        let flags = pspec.flags();

        // Check if property is writable
        if !flags.contains(glib::ParamFlags::WRITABLE) {
            return Err(PipelineError::InvalidProperty {
                element: format!("{}:{}", element_id, pad_name),
                property: property_name.to_string(),
                reason: "Property is not writable".to_string(),
            });
        }

        // Check if property is construct-only
        if flags.contains(glib::ParamFlags::CONSTRUCT_ONLY) {
            return Err(PipelineError::InvalidProperty {
                element: format!("{}:{}", element_id, pad_name),
                property: property_name.to_string(),
                reason: "Property is construct-only and cannot be changed after pad creation"
                    .to_string(),
            });
        }

        // Check if property can be changed in current state
        // GStreamer-specific flags (from gstreamer-sys)
        // GST_PARAM_MUTABLE_READY = 0x400
        // GST_PARAM_MUTABLE_PAUSED = 0x800
        // GST_PARAM_MUTABLE_PLAYING = 0x1000
        // GST_PARAM_CONTROLLABLE = 0x200
        let flags_bits = flags.bits();
        let mutable_in_ready = (flags_bits & 0x400) != 0;
        let mutable_in_paused = (flags_bits & 0x800) != 0;
        let mutable_in_playing = (flags_bits & 0x1000) != 0;
        let controllable = (flags_bits & 0x200) != 0;

        // Controllable properties can generally be changed at runtime
        let can_change_at_runtime = controllable;

        match current_state {
            PipelineState::Playing => {
                if !mutable_in_playing && !can_change_at_runtime {
                    return Err(PipelineError::PropertyNotMutable {
                        element: format!("{}:{}", element_id, pad_name),
                        property: property_name.to_string(),
                        state: current_state,
                    });
                }
            }
            PipelineState::Paused => {
                if !mutable_in_paused && !mutable_in_playing {
                    return Err(PipelineError::PropertyNotMutable {
                        element: format!("{}:{}", element_id, pad_name),
                        property: property_name.to_string(),
                        state: current_state,
                    });
                }
            }
            PipelineState::Ready => {
                if !mutable_in_ready && !mutable_in_paused && !mutable_in_playing {
                    return Err(PipelineError::PropertyNotMutable {
                        element: format!("{}:{}", element_id, pad_name),
                        property: property_name.to_string(),
                        state: current_state,
                    });
                }
            }
            PipelineState::Null => {
                // All writable, non-construct-only properties can be changed in NULL state
            }
        }

        Ok(())
    }

    /// Validate that a property can be changed in the current pipeline state.
    fn validate_property_mutability(
        &self,
        element: &gst::Element,
        element_id: &str,
        property_name: &str,
        current_state: PipelineState,
    ) -> Result<(), PipelineError> {
        let pspec =
            element
                .find_property(property_name)
                .ok_or_else(|| PipelineError::InvalidProperty {
                    element: element_id.to_string(),
                    property: property_name.to_string(),
                    reason: "Property not found".to_string(),
                })?;

        let flags = pspec.flags();

        // Check if property is writable
        if !flags.contains(glib::ParamFlags::WRITABLE) {
            return Err(PipelineError::InvalidProperty {
                element: element_id.to_string(),
                property: property_name.to_string(),
                reason: "Property is not writable".to_string(),
            });
        }

        // Check if property is construct-only
        if flags.contains(glib::ParamFlags::CONSTRUCT_ONLY) {
            return Err(PipelineError::InvalidProperty {
                element: element_id.to_string(),
                property: property_name.to_string(),
                reason: "Property is construct-only and cannot be changed after element creation"
                    .to_string(),
            });
        }

        // Check if property can be changed in current state
        // GStreamer-specific flags (from gstreamer-sys)
        // GST_PARAM_MUTABLE_READY = 0x400
        // GST_PARAM_MUTABLE_PAUSED = 0x800
        // GST_PARAM_MUTABLE_PLAYING = 0x1000
        // GST_PARAM_CONTROLLABLE = 0x200
        let flags_bits = flags.bits();
        let mutable_in_ready = (flags_bits & 0x400) != 0;
        let mutable_in_paused = (flags_bits & 0x800) != 0;
        let mutable_in_playing = (flags_bits & 0x1000) != 0;
        let controllable = (flags_bits & 0x200) != 0;

        // Controllable properties can generally be changed at runtime
        // They're designed for dynamic updates via GstController
        let can_change_at_runtime = controllable;

        match current_state {
            PipelineState::Playing => {
                if !mutable_in_playing && !can_change_at_runtime {
                    return Err(PipelineError::PropertyNotMutable {
                        element: element_id.to_string(),
                        property: property_name.to_string(),
                        state: current_state,
                    });
                }
            }
            PipelineState::Paused => {
                if !mutable_in_paused && !mutable_in_playing {
                    return Err(PipelineError::PropertyNotMutable {
                        element: element_id.to_string(),
                        property: property_name.to_string(),
                        state: current_state,
                    });
                }
            }
            PipelineState::Ready => {
                if !mutable_in_ready && !mutable_in_paused && !mutable_in_playing {
                    return Err(PipelineError::PropertyNotMutable {
                        element: element_id.to_string(),
                        property: property_name.to_string(),
                        state: current_state,
                    });
                }
            }
            PipelineState::Null => {
                // All writable, non-construct-only properties can be changed in NULL state
            }
        }

        Ok(())
    }

    /// Query the latency of the pipeline.
    /// Returns (min_latency_ns, max_latency_ns, live) if query succeeds.
    ///
    /// This method tries multiple approaches to get meaningful latency:
    /// 1. Pipeline-level latency query (best for live pipelines)
    /// 2. If pipeline returns 0, try querying individual sink elements
    pub fn query_latency(&self) -> Option<(u64, u64, bool)> {
        let mut query = gst::query::Latency::new();

        if self.pipeline.query(&mut query) {
            let (live, min, max) = query.result();
            let min_ns = min.nseconds();
            let max_ns = max.map_or(u64::MAX, |t| t.nseconds());
            trace!(
                "Pipeline '{}' latency query: live={}, min={}ns, max={}ns",
                self.flow_name,
                live,
                min_ns,
                max_ns
            );

            // If pipeline is live and has meaningful latency, use it
            if live && min_ns > 0 {
                return Some((min_ns, max_ns, live));
            }

            // For non-live pipelines or if latency is 0, try to get latency from sink elements
            // This gives more useful information for streaming pipelines
            let sink_latency = self.query_sink_latency();
            if let Some((sink_min, sink_max)) = sink_latency {
                if sink_min > 0 {
                    trace!(
                        "Pipeline '{}' using sink latency: min={}ns, max={}ns",
                        self.flow_name,
                        sink_min,
                        sink_max
                    );
                    return Some((sink_min, sink_max, live));
                }
            }

            // Return pipeline values even if 0 (user sees it's not live)
            Some((min_ns, max_ns, live))
        } else {
            trace!(
                "Pipeline '{}' latency query failed (may not be in playing state)",
                self.flow_name
            );
            None
        }
    }

    /// Query latency from sink elements in the pipeline.
    /// This is useful for non-live pipelines where the pipeline-level query returns 0.
    fn query_sink_latency(&self) -> Option<(u64, u64)> {
        let mut total_latency: u64 = 0;

        // Iterate over all elements and find sinks (elements with sink pads but no src pads)
        for (element_id, element) in &self.elements {
            // Check if this is a sink element by looking at pads
            let has_sink_pad = element.static_pad("sink").is_some()
                || element
                    .iterate_sink_pads()
                    .into_iter()
                    .flatten()
                    .next()
                    .is_some();
            let has_src_pad = element.static_pad("src").is_some()
                || element
                    .iterate_src_pads()
                    .into_iter()
                    .flatten()
                    .next()
                    .is_some();

            // True sinks have sink pads but no source pads
            let is_sink = has_sink_pad && !has_src_pad;

            if is_sink {
                // Try to query latency directly on the sink
                let mut sink_query = gst::query::Latency::new();
                if element.query(&mut sink_query) {
                    let (live, min, max) = sink_query.result();
                    let min_ns = min.nseconds();
                    let max_ns = max.map_or(u64::MAX, |t| t.nseconds());
                    debug!(
                        "Sink element '{}' latency: live={}, min={}ns, max={}ns",
                        element_id, live, min_ns, max_ns
                    );
                    if min_ns > 0 {
                        total_latency = total_latency.max(min_ns);
                    }
                }

                // For audio sinks, try to get the latency-time property (in microseconds)
                if element.has_property("latency-time") {
                    let latency_us = element.property::<i64>("latency-time");
                    let latency_ns = (latency_us * 1000) as u64;
                    debug!(
                        "Audio sink '{}' latency-time: {}us ({}ns)",
                        element_id, latency_us, latency_ns
                    );
                    if latency_ns > 0 {
                        total_latency = total_latency.max(latency_ns);
                    }
                }

                // Try buffer-time property as well (typically 2x latency-time)
                if element.has_property("buffer-time") {
                    let buffer_us = element.property::<i64>("buffer-time");
                    let buffer_ns = (buffer_us * 1000) as u64;
                    debug!(
                        "Audio sink '{}' buffer-time: {}us ({}ns)",
                        element_id, buffer_us, buffer_ns
                    );
                    // Don't use buffer-time directly as it's the full buffer, not latency
                }
            }

            // Also check queue elements for their current level/latency
            let factory_name = element.factory().map(|f| f.name().to_string());
            if (factory_name.as_deref() == Some("queue")
                || factory_name.as_deref() == Some("queue2"))
                && element.has_property("current-level-time")
            {
                let level_ns = element.property::<u64>("current-level-time");
                debug!("Queue '{}' current-level-time: {}ns", element_id, level_ns);
                // Queue level contributes to latency
                total_latency = total_latency.saturating_add(level_ns);
            }
        }

        if total_latency > 0 {
            Some((total_latency, total_latency))
        } else {
            None
        }
    }

    /// Get the negotiated caps for a specific pad.
    /// Returns the caps as a string, or None if caps haven't been negotiated yet.
    pub fn get_pad_caps(
        &self,
        element_id: &str,
        pad_name: &str,
    ) -> Result<Option<gst::Caps>, PipelineError> {
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        // Get pad reference
        let pad = if let Some(p) = element.static_pad(pad_name) {
            p
        } else if let Some(p) = element.request_pad_simple(pad_name) {
            p
        } else {
            return Err(PipelineError::PadNotFound {
                element: element_id.to_string(),
                pad: pad_name.to_string(),
            });
        };

        // Get current negotiated caps (not template caps)
        Ok(pad.current_caps())
    }

    /// Get WebRTC statistics from all webrtcbin elements in the pipeline.
    /// Searches for webrtcbin elements (including those nested in bins like whepclientsrc/whipclientsink)
    /// and collects their stats using the "get-stats" action signal.
    pub fn get_webrtc_stats(&self) -> strom_types::api::WebRtcStats {
        use strom_types::api::{WebRtcConnectionStats, WebRtcStats};

        let mut stats = WebRtcStats::default();

        // Find all webrtcbin elements in the pipeline
        let webrtcbins = self.find_webrtcbin_elements();
        trace!(
            "get_webrtc_stats: Found {} webrtcbin element(s)",
            webrtcbins.len()
        );

        for (name, webrtcbin) in webrtcbins {
            trace!("get_webrtc_stats: Getting stats from webrtcbin: {}", name);

            let mut conn_stats = WebRtcConnectionStats::default();

            // First check if ICE connection is established - skip if not ready
            // This avoids blocking on promise.wait() for webrtcbins that aren't connected
            let ice_state = self.get_ice_connection_state(&webrtcbin);
            trace!("get_webrtc_stats: ICE state for {}: {:?}", name, ice_state);

            // Only get detailed stats if we have a reasonable ICE state
            let should_get_stats = match ice_state.as_deref() {
                Some("connected") | Some("completed") | Some("checking") => true,
                Some("new") => {
                    // New state - webrtcbin exists but connection not started
                    // Still try to get basic stats
                    true
                }
                Some("failed") | Some("disconnected") | Some("closed") => {
                    warn!(
                        "get_webrtc_stats: Skipping stats for {} - ICE state: {:?}",
                        name, ice_state
                    );
                    false
                }
                _ => {
                    // Unknown state - try anyway but be cautious
                    true
                }
            };

            if should_get_stats {
                // Call the get-stats action signal
                // The signal takes:
                // - GstPad* (optional, NULL for all stats)
                // - GstPromise* (to receive the stats)
                // Returns void, stats come via the promise
                let pad_none: Option<&gst::Pad> = None;
                let promise = gst::Promise::new();
                trace!("get_webrtc_stats: Emitting get-stats signal...");
                webrtcbin.emit_by_name::<()>("get-stats", &[&pad_none, &promise]);

                // Wait for the promise with a timeout using interrupt from another thread
                let promise_clone = promise.clone();
                let timeout_thread = std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    promise_clone.interrupt();
                });

                trace!("get_webrtc_stats: Waiting for promise (500ms timeout)...");
                let promise_result = promise.wait();

                // Clean up timeout thread (it will either have interrupted or not)
                let _ = timeout_thread.join();

                trace!("get_webrtc_stats: Promise result: {:?}", promise_result);

                match promise_result {
                    gst::PromiseResult::Replied => {
                        if let Some(reply) = promise.get_reply() {
                            // The reply is a GstStructure containing the stats
                            trace!(
                                "get_webrtc_stats: Got reply structure with {} fields: {}",
                                reply.n_fields(),
                                reply.name()
                            );
                            // Log all field names
                            for i in 0..reply.n_fields() {
                                if let Some(field_name) = reply.nth_field_name(i) {
                                    trace!("get_webrtc_stats: Field [{}]: {}", i, field_name);
                                }
                            }
                            // Convert StructureRef to owned Structure for parsing
                            conn_stats = self.parse_webrtc_stats_structure(&reply.to_owned());
                            trace!(
                                "get_webrtc_stats: Parsed stats - ICE: {:?}, inbound_rtp: {}, outbound_rtp: {}",
                                conn_stats.ice_candidates.is_some(),
                                conn_stats.inbound_rtp.len(),
                                conn_stats.outbound_rtp.len()
                            );
                        } else {
                            trace!(
                                "get_webrtc_stats: No stats in promise reply from webrtcbin: {}",
                                name
                            );
                        }
                    }
                    gst::PromiseResult::Interrupted => {
                        debug!(
                            "get_webrtc_stats: Promise timed out (interrupted) for webrtcbin: {}",
                            name
                        );
                    }
                    gst::PromiseResult::Expired => {
                        trace!("get_webrtc_stats: Promise expired for webrtcbin: {}", name);
                    }
                    gst::PromiseResult::Pending => {
                        trace!(
                            "get_webrtc_stats: Promise still pending for webrtcbin: {}",
                            name
                        );
                    }
                    _ => {
                        info!(
                            "get_webrtc_stats: Unknown promise result for webrtcbin: {}",
                            name
                        );
                    }
                }
            }

            // Also try to get basic element properties as fallback/additional info
            if let Some(ice_state_str) = ice_state {
                if conn_stats.ice_candidates.is_none() {
                    conn_stats.ice_candidates = Some(strom_types::api::IceCandidateStats {
                        state: Some(ice_state_str),
                        ..Default::default()
                    });
                }
            }

            stats.connections.insert(name, conn_stats);
        }

        trace!(
            "get_webrtc_stats: Returning stats with {} connection(s)",
            stats.connections.len()
        );
        stats
    }

    /// Find all webrtcbin elements in the pipeline, including those nested in bins.
    fn find_webrtcbin_elements(&self) -> Vec<(String, gst::Element)> {
        let mut results = Vec::new();

        debug!(
            "find_webrtcbin_elements: Searching {} elements in elements map",
            self.elements.len()
        );

        // Check direct elements
        for (name, element) in &self.elements {
            let factory_name = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            debug!(
                "find_webrtcbin_elements: Checking element '{}' (factory: {})",
                name, factory_name
            );

            if factory_name == "webrtcbin" {
                debug!("find_webrtcbin_elements: Found direct webrtcbin: {}", name);
                results.push((name.clone(), element.clone()));
            }

            // Check if element is a bin (like whepclientsrc, whipclientsink)
            // and search inside it recursively
            if element.is::<gst::Bin>() {
                let bin = element.clone().downcast::<gst::Bin>().unwrap();
                debug!(
                    "find_webrtcbin_elements: Element '{}' is a Bin, searching children",
                    name
                );

                // Use iterate_recurse to find all nested elements
                let iter = bin.iterate_recurse();
                for child_elem in iter.into_iter().flatten() {
                    let child_factory = child_elem
                        .factory()
                        .map(|f| f.name().to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    if child_factory == "webrtcbin" {
                        let child_name = format!("{}:{}", name, child_elem.name());
                        debug!(
                            "find_webrtcbin_elements: Found nested webrtcbin: {}",
                            child_name
                        );
                        results.push((child_name, child_elem));
                    }
                }
            }
        }

        // Also search the pipeline directly in case elements were added dynamically
        debug!("find_webrtcbin_elements: Also searching pipeline directly");
        let pipeline_iter = self.pipeline.iterate_recurse();
        for elem in pipeline_iter.into_iter().flatten() {
            let factory_name = elem
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            if factory_name == "webrtcbin" {
                let elem_name = elem.name().to_string();
                // Check if we already have this element
                let already_found = results.iter().any(|(_, e)| e.name() == elem.name());
                if !already_found {
                    debug!(
                        "find_webrtcbin_elements: Found webrtcbin in pipeline: {}",
                        elem_name
                    );
                    results.push((elem_name, elem));
                }
            }
        }

        debug!(
            "find_webrtcbin_elements: Found {} webrtcbin element(s)",
            results.len()
        );
        results
    }

    /// Parse a GstStructure containing WebRTC stats into our typed format.
    fn parse_webrtc_stats_structure(
        &self,
        structure: &gst::Structure,
    ) -> strom_types::api::WebRtcConnectionStats {
        use strom_types::api::{IceCandidateStats, WebRtcConnectionStats};

        let mut conn_stats = WebRtcConnectionStats::default();

        trace!(
            "parse_webrtc_stats_structure: Parsing structure '{}' with {} fields",
            structure.name(),
            structure.n_fields()
        );

        // Log ALL field names and their types for debugging
        trace!("=== RAW WEBRTC STATS STRUCTURE ===");
        for (field_name, value) in structure.iter() {
            let type_name = value.type_().name();
            trace!("  Field: '{}' (type: {})", field_name, type_name);

            // If it's a nested structure, log its contents too
            if let Ok(nested) = value.get::<gst::Structure>() {
                trace!("    Nested structure '{}' fields:", nested.name());
                for (nested_field, nested_value) in nested.iter() {
                    let nested_type = nested_value.type_().name();
                    // Try to get the actual value for common types
                    let value_str = if let Ok(s) = nested_value.get::<String>() {
                        format!("\"{}\"", s)
                    } else if let Ok(s) = nested_value.get::<&str>() {
                        format!("\"{}\"", s)
                    } else if let Ok(n) = nested_value.get::<u64>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<i64>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<u32>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<i32>() {
                        format!("{}", n)
                    } else if let Ok(n) = nested_value.get::<f64>() {
                        format!("{:.6}", n)
                    } else if let Ok(b) = nested_value.get::<bool>() {
                        format!("{}", b)
                    } else {
                        format!("<{}>", nested_type)
                    };
                    trace!("      {}: {} = {}", nested_field, nested_type, value_str);
                }
            }
        }
        trace!("=== END RAW STATS ===");

        // WebRTC stats structure contains nested structures for each stat type
        // The field NAME indicates the type (e.g., "rtp-inbound-stream-stats_1234")

        // Iterate over all fields in the structure
        for (field_name, value) in structure.iter() {
            let field_str = field_name.to_string();

            // Try to get as nested structure
            if let Ok(nested) = value.get::<gst::Structure>() {
                // Determine type from field name prefix
                if field_str.starts_with("rtp-inbound-stream-stats") {
                    debug!(
                        "parse_webrtc_stats_structure: Found inbound RTP stats: {}",
                        field_str
                    );
                    conn_stats
                        .inbound_rtp
                        .push(self.parse_rtp_stats(&nested, true));
                } else if field_str.starts_with("rtp-outbound-stream-stats") {
                    debug!(
                        "parse_webrtc_stats_structure: Found outbound RTP stats: {}",
                        field_str
                    );
                    conn_stats
                        .outbound_rtp
                        .push(self.parse_rtp_stats(&nested, false));
                } else if field_str.starts_with("ice-candidate-local") {
                    debug!(
                        "parse_webrtc_stats_structure: Found local ICE candidate: {}",
                        field_str
                    );
                    if conn_stats.ice_candidates.is_none() {
                        conn_stats.ice_candidates = Some(IceCandidateStats::default());
                    }
                    if let Some(ref mut ice) = conn_stats.ice_candidates {
                        if let Ok(candidate_type) = nested.get::<&str>("candidate-type") {
                            ice.local_candidate_type = Some(candidate_type.to_string());
                        }
                        if let Ok(address) = nested.get::<&str>("address") {
                            ice.local_address = Some(address.to_string());
                        }
                        if let Ok(port) = nested.get::<u32>("port") {
                            ice.local_port = Some(port);
                        }
                        if let Ok(protocol) = nested.get::<&str>("protocol") {
                            ice.local_protocol = Some(protocol.to_string());
                        }
                    }
                } else if field_str.starts_with("ice-candidate-remote") {
                    debug!(
                        "parse_webrtc_stats_structure: Found remote ICE candidate: {}",
                        field_str
                    );
                    if conn_stats.ice_candidates.is_none() {
                        conn_stats.ice_candidates = Some(IceCandidateStats::default());
                    }
                    if let Some(ref mut ice) = conn_stats.ice_candidates {
                        if let Ok(candidate_type) = nested.get::<&str>("candidate-type") {
                            ice.remote_candidate_type = Some(candidate_type.to_string());
                        }
                        if let Ok(address) = nested.get::<&str>("address") {
                            ice.remote_address = Some(address.to_string());
                        }
                        if let Ok(port) = nested.get::<u32>("port") {
                            ice.remote_port = Some(port);
                        }
                        if let Ok(protocol) = nested.get::<&str>("protocol") {
                            ice.remote_protocol = Some(protocol.to_string());
                        }
                    }
                } else if field_str.starts_with("ice-candidate-pair") {
                    debug!(
                        "parse_webrtc_stats_structure: Found ICE candidate pair: {}",
                        field_str
                    );
                    // Candidate pair stats - this is where we can get connection state
                    if conn_stats.ice_candidates.is_none() {
                        conn_stats.ice_candidates = Some(IceCandidateStats::default());
                    }
                    // Note: GStreamer webrtcbin doesn't expose ICE state in stats
                    // We get it from the ice-connection-state property instead
                } else if field_str.starts_with("transport-stats") {
                    debug!(
                        "parse_webrtc_stats_structure: Found transport stats: {}",
                        field_str
                    );
                    let mut transport = strom_types::api::TransportStats::default();
                    if let Ok(bytes) = nested.get::<u64>("bytes-sent") {
                        transport.bytes_sent = Some(bytes);
                    }
                    if let Ok(bytes) = nested.get::<u64>("bytes-received") {
                        transport.bytes_received = Some(bytes);
                    }
                    if let Ok(packets) = nested.get::<u64>("packets-sent") {
                        transport.packets_sent = Some(packets);
                    }
                    if let Ok(packets) = nested.get::<u64>("packets-received") {
                        transport.packets_received = Some(packets);
                    }
                    conn_stats.transport = Some(transport);
                } else if field_str.starts_with("codec-stats") || field_str.starts_with("codec_") {
                    debug!(
                        "parse_webrtc_stats_structure: Found codec stats: {}",
                        field_str
                    );
                    let mut codec = strom_types::api::CodecStats::default();
                    if let Ok(mime) = nested.get::<&str>("mime-type") {
                        codec.mime_type = Some(mime.to_string());
                    }
                    if let Ok(clock_rate) = nested.get::<u32>("clock-rate") {
                        codec.clock_rate = Some(clock_rate);
                    }
                    if let Ok(pt) = nested.get::<u32>("payload-type") {
                        codec.payload_type = Some(pt);
                    }
                    if let Ok(channels) = nested.get::<u32>("channels") {
                        codec.channels = Some(channels);
                    }
                    conn_stats.codecs.push(codec);
                }
            }
        }

        debug!(
            "parse_webrtc_stats_structure: Done - ICE: {:?}, inbound: {}, outbound: {}, codecs: {}, transport: {:?}",
            conn_stats.ice_candidates.is_some(),
            conn_stats.inbound_rtp.len(),
            conn_stats.outbound_rtp.len(),
            conn_stats.codecs.len(),
            conn_stats.transport.is_some()
        );
        conn_stats
    }

    /// Parse RTP stream stats from a GstStructure.
    fn parse_rtp_stats(
        &self,
        structure: &gst::Structure,
        inbound: bool,
    ) -> strom_types::api::RtpStreamStats {
        use strom_types::api::RtpStreamStats;

        let mut stats = RtpStreamStats::default();

        // SSRC
        if let Ok(ssrc) = structure.get::<u32>("ssrc") {
            stats.ssrc = Some(ssrc);
        }

        // Media type
        if let Ok(media_type) = structure.get::<&str>("media-type") {
            stats.media_type = Some(media_type.to_string());
        } else if let Ok(kind) = structure.get::<&str>("kind") {
            stats.media_type = Some(kind.to_string());
        }

        // Codec
        if let Ok(codec) = structure.get::<&str>("codec-id") {
            stats.codec = Some(codec.to_string());
        }

        // Bytes
        if inbound {
            if let Ok(bytes) = structure.get::<u64>("bytes-received") {
                stats.bytes = Some(bytes);
            }
        } else if let Ok(bytes) = structure.get::<u64>("bytes-sent") {
            stats.bytes = Some(bytes);
        }

        // Packets
        if inbound {
            if let Ok(packets) = structure.get::<u64>("packets-received") {
                stats.packets = Some(packets);
            }
            // Packets lost (signed for inbound)
            if let Ok(lost) = structure.get::<i64>("packets-lost") {
                stats.packets_lost = Some(lost);
            } else if let Ok(lost) = structure.get::<i32>("packets-lost") {
                stats.packets_lost = Some(lost as i64);
            }
            // Jitter
            if let Ok(jitter) = structure.get::<f64>("jitter") {
                stats.jitter = Some(jitter);
            }
        } else if let Ok(packets) = structure.get::<u64>("packets-sent") {
            stats.packets = Some(packets);
        }

        // RTT (round-trip time) - try top level first
        if let Ok(rtt) = structure.get::<f64>("round-trip-time") {
            stats.round_trip_time = Some(rtt);
        }

        // Bitrate (if available) - try top level first
        if let Ok(bitrate) = structure.get::<u64>("bitrate") {
            stats.bitrate = Some(bitrate);
        }

        // Parse nested gst-rtpsource-stats for additional fields
        // This contains packets-lost, bitrate, and round-trip time
        if let Ok(rtp_source_stats) = structure.get::<gst::Structure>("gst-rtpsource-stats") {
            debug!(
                "parse_rtp_stats: Found nested gst-rtpsource-stats with {} fields",
                rtp_source_stats.n_fields()
            );

            // Packets lost (only for inbound)
            if inbound && stats.packets_lost.is_none() {
                // Try packets-lost first (cumulative)
                if let Ok(lost) = rtp_source_stats.get::<i32>("packets-lost") {
                    // -1 means unknown/not calculated, only set if we have a real value
                    if lost >= 0 {
                        stats.packets_lost = Some(lost as i64);
                    }
                }
                // Try sent-rb-packetslost (from receiver report we sent)
                if stats.packets_lost.is_none() {
                    if let Ok(lost) = rtp_source_stats.get::<i32>("sent-rb-packetslost") {
                        if lost >= 0 {
                            stats.packets_lost = Some(lost as i64);
                        }
                    }
                }
                // If we have sent-rb data (sent-rb=true) but packets-lost is still -1,
                // check sent-rb-fractionlost (0-255 representing 0-100% loss)
                // If fraction lost is 0, we can assume 0 packets lost
                if stats.packets_lost.is_none() {
                    if let Ok(sent_rb) = rtp_source_stats.get::<bool>("sent-rb") {
                        if sent_rb {
                            if let Ok(fraction) =
                                rtp_source_stats.get::<u32>("sent-rb-fractionlost")
                            {
                                // If fraction lost is 0, no recent packet loss
                                // Set packets_lost to 0 to indicate healthy stream
                                if fraction == 0 {
                                    stats.packets_lost = Some(0);
                                }
                            }
                        }
                    }
                }
            }

            // Bitrate from nested structure
            if stats.bitrate.is_none() {
                if let Ok(bitrate) = rtp_source_stats.get::<u64>("bitrate") {
                    if bitrate > 0 {
                        stats.bitrate = Some(bitrate);
                    }
                }
            }

            // Fraction lost (0-255 scale, convert to 0.0-1.0)
            if inbound {
                if let Ok(fraction) = rtp_source_stats.get::<u32>("sent-rb-fractionlost") {
                    // 0-255 maps to 0.0-1.0 (0-100% loss)
                    let fraction_pct = fraction as f64 / 255.0;
                    stats.fraction_lost = Some(fraction_pct);
                }
            }

            // Round-trip time from RTCP receiver reports
            if stats.round_trip_time.is_none() {
                // Try rb-round-trip first (from received receiver reports)
                if let Ok(rtt_ticks) = rtp_source_stats.get::<u32>("rb-round-trip") {
                    if rtt_ticks > 0 {
                        // rb-round-trip is in 1/65536 seconds units
                        let rtt_seconds = rtt_ticks as f64 / 65536.0;
                        stats.round_trip_time = Some(rtt_seconds);
                    }
                }
                // Also try sent-rb-dlsr and sent-rb-lsr to calculate RTT
                // RTT = now - LSR - DLSR (when we have the values)
                // For now, just use rb-round-trip if available
            }
        }

        stats
    }

    /// Get ICE connection state from webrtcbin element.
    fn get_ice_connection_state(&self, webrtcbin: &gst::Element) -> Option<String> {
        // Try to get ice-connection-state property
        if webrtcbin.has_property("ice-connection-state") {
            let state_value = webrtcbin.property_value("ice-connection-state");
            // The state is an enum, try to get it as a string
            if let Ok(state_int) = state_value.get::<i32>() {
                // Map enum values to strings
                let state_str = match state_int {
                    0 => "new",
                    1 => "checking",
                    2 => "connected",
                    3 => "completed",
                    4 => "failed",
                    5 => "disconnected",
                    6 => "closed",
                    _ => "unknown",
                };
                return Some(state_str.to_string());
            }
        }
        None
    }
}

impl Drop for PipelineManager {
    fn drop(&mut self) {
        debug!("Dropping pipeline for flow: {}", self.flow_name);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_flow() -> Flow {
        let mut flow = Flow::new("Test Pipeline");
        flow.elements = vec![
            Element {
                id: "src".to_string(),
                element_type: "videotestsrc".to_string(),
                properties: HashMap::from([("is-live".to_string(), PropertyValue::Bool(true))]),
                pad_properties: HashMap::new(),
                position: (0.0, 0.0),
            },
            Element {
                id: "sink".to_string(),
                element_type: "fakesink".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (100.0, 0.0),
            },
        ];
        flow.links = vec![Link {
            from: "src".to_string(),
            to: "sink".to_string(),
        }];
        flow
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_create_pipeline() {
        gst::init().unwrap();
        let flow = create_test_flow();
        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(&flow, events, &registry);
        assert!(manager.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_start_stop_pipeline() {
        gst::init().unwrap();
        let flow = create_test_flow();
        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let mut manager = PipelineManager::new(&flow, events, &registry).unwrap();

        // Start pipeline
        let state = manager.start();
        assert!(state.is_ok());
        assert_eq!(state.unwrap(), PipelineState::Playing);

        // Check state
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(manager.get_state(), PipelineState::Playing);

        // Stop pipeline
        let state = manager.stop();
        assert!(state.is_ok());
        assert_eq!(state.unwrap(), PipelineState::Null);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_invalid_element() {
        gst::init().unwrap();
        let mut flow = create_test_flow();
        flow.elements[0].element_type = "nonexistentelement".to_string();

        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(&flow, events, &registry);
        assert!(manager.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_auto_tee_insertion() {
        gst::init().unwrap();

        // Create a flow with one source and two sinks (should auto-insert a tee)
        let mut flow = Flow::new("Auto-Tee Test");
        flow.elements = vec![
            Element {
                id: "src".to_string(),
                element_type: "videotestsrc".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (0.0, 0.0),
            },
            Element {
                id: "sink1".to_string(),
                element_type: "fakesink".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (100.0, 0.0),
            },
            Element {
                id: "sink2".to_string(),
                element_type: "fakesink".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (100.0, 100.0),
            },
        ];
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "sink1".to_string(),
            },
            Link {
                from: "src".to_string(),
                to: "sink2".to_string(),
            },
        ];

        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(&flow, events, &registry);
        assert!(manager.is_ok());

        let manager = manager.unwrap();
        // Should have 3 original elements + 1 auto-inserted tee
        assert_eq!(manager.elements.len(), 4);
        // Check that tee element was created
        assert!(manager.elements.contains_key("auto_tee_src"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_tee_insertion_when_not_needed() {
        gst::init().unwrap();

        let flow = create_test_flow(); // Simple 1-to-1 connection

        let events = EventBroadcaster::default();
        let registry = BlockRegistry::new("test_blocks.json");
        let manager = PipelineManager::new(&flow, events, &registry).unwrap();

        // Should have only 2 original elements, no tee
        assert_eq!(manager.elements.len(), 2);
        assert!(!manager.elements.contains_key("auto_tee_src"));
    }
}
