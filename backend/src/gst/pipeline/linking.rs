use super::{PipelineError, PipelineManager, ProcessedLinks};
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::element::ElementPadRef;
use strom_types::Link;
use tracing::{debug, error, info, warn};

impl PipelineManager {
    /// Try to link two elements according to a link definition.
    /// Returns Ok if successful, Err if pads don't exist yet (dynamic pads).
    pub(super) fn try_link_elements(&self, link: &Link) -> Result<(), PipelineError> {
        // Convert to structured ElementPadRef for type-safe handling
        let (from_ref, to_ref) = link.to_pad_refs();

        debug!(
            "Trying to link: {} -> {}",
            from_ref.element_id, to_ref.element_id
        );

        self.try_link_elements_refs(&from_ref, &to_ref, link)
    }

    /// Type-safe link implementation using ElementPadRef structs.
    fn try_link_elements_refs(
        &self,
        from_ref: &ElementPadRef,
        to_ref: &ElementPadRef,
        link: &Link,
    ) -> Result<(), PipelineError> {
        let from_element = &from_ref.element_id;
        let from_pad = from_ref.pad_name.as_deref();
        let to_element = &to_ref.element_id;
        let to_pad = to_ref.pad_name.as_deref();

        let src = self
            .elements
            .get(from_element)
            .ok_or_else(|| PipelineError::ElementNotFound(from_element.to_string()))?;

        let sink = self
            .elements
            .get(to_element)
            .ok_or_else(|| PipelineError::ElementNotFound(to_element.to_string()))?;

        // Link with or without specific pads
        if let (Some(src_pad_name), None) = (from_pad, to_pad) {
            // Source pad specified, sink pad auto-requested (for aggregators)
            let element_type_name = sink
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();

            if element_type_name == "mpegtsmux" || element_type_name == "glvideomixerelement" {
                debug!(
                    "Linking {}:{} -> {} (aggregator, auto-request sink pad)",
                    from_element, src_pad_name, element_type_name
                );
                // Use link_pads with source pad specified and sink pad as None
                // This lets GStreamer automatically request and create a new sink pad on the aggregator
                if let Err(e) = src.link_pads(Some(src_pad_name), sink, None::<&str>) {
                    return Err(PipelineError::LinkError(
                        link.from.clone(),
                        format!("Failed to auto-link to {}: {}", element_type_name, e),
                    ));
                }
                debug!("Successfully linked: {} -> {}", link.from, link.to);
                return Ok(());
            } else {
                // Not an aggregator - try simple link_pads
                if let Err(e) = src.link_pads(Some(src_pad_name), sink, None::<&str>) {
                    return Err(PipelineError::LinkError(
                        link.from.clone(),
                        format!("Failed to link: {}", e),
                    ));
                }
                debug!("Successfully linked: {} -> {}", link.from, link.to);
                return Ok(());
            }
        } else if let (Some(src_pad_name), Some(sink_pad_name)) = (from_pad, to_pad) {
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
            debug!(
                "Getting sink pad '{}' on element '{}'",
                sink_pad_name, to_element
            );

            // Special handling for aggregator elements (mpegtsmux, glvideomixerelement) -
            // use link_pads with specified source pad and None for sink to let GStreamer auto-create request pads.
            // Aggregators need to be in READY state before requesting pads, but link_pads handles this automatically.
            let element_type_name = sink
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();
            if element_type_name == "mpegtsmux" || element_type_name == "glvideomixerelement" {
                debug!(
                    "Using link_pads(Some({}), None) for aggregator: {}",
                    src_pad_name, element_type_name
                );
                // For aggregator elements, use link_pads with source pad specified and sink pad as None
                // This lets GStreamer automatically request and create a new sink pad on the aggregator
                // This avoids NULL pointer crashes and pad name collisions
                if let Err(e) = src.link_pads(Some(src_pad_name), sink, None::<&str>) {
                    return Err(PipelineError::LinkError(
                        link.from.clone(),
                        format!("Failed to auto-link to {}: {}", element_type_name, e),
                    ));
                }
                debug!(
                    "Auto-linked to {}: {}:{} -> {}:auto",
                    element_type_name, from_element, src_pad_name, to_element
                );
                return Ok(());
            }

            let sink_pad_obj = if let Some(pad) = sink.static_pad(sink_pad_name) {
                debug!("Found static sink pad: {}", sink_pad_name);
                pad
            } else {
                debug!("Requesting dynamic sink pad: {}", sink_pad_name);
                // Pad not static - try to request it
                // First try request_pad_simple with the exact name
                if let Some(pad) = sink.request_pad_simple(sink_pad_name) {
                    debug!("Requested sink pad: {}", sink_pad_name);
                    pad
                } else {
                    debug!("Trying pad template matching...");

                    // If that didn't work, try finding a compatible pad template
                    // This handles cases like "sink_0" needing the "sink_%u" template
                    // IMPORTANT: We get pad templates directly from the element, not from the factory.
                    // Accessing static_pad_templates from the factory can corrupt GStreamer state
                    // for aggregator elements like mpegtsmux (see discovery.rs:533-538).
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
        } else if let Some(sink_pad_name) = to_pad {
            // Source is element-level, destination names a pad: let GStreamer
            // pick a compatible source pad but honour the sink pad the caller
            // asked for. link() would ignore it and pick both ends itself.
            src.link_pads(None::<&str>, sink, Some(sink_pad_name))
                .map_err(|e| {
                    PipelineError::LinkError(link.from.clone(), format!("{} - {}", link.to, e))
                })?;

            debug!("Successfully linked: {} -> {}", link.from, link.to);
        } else {
            // Simple link without pad names - check if sink is an aggregator
            let element_type_name = sink
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();

            if element_type_name == "mpegtsmux" || element_type_name == "glvideomixerelement" {
                warn!(
                    "Aggregator {} linked without pad names - this may cause issues. Consider using explicit pad names.",
                    element_type_name
                );
            }

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
    /// This handles two cases:
    /// 1. Dynamic pads with pending links - link them when they appear
    /// 2. Dynamic pads WITHOUT links - auto-attach a tee with allow-not-linked=true
    ///    so unlinked streams don't block the pipeline
    pub(super) fn setup_dynamic_pad_handlers(&mut self) {
        info!(
            "Setting up dynamic pad handlers ({} pending link(s))",
            self.pending_links.len()
        );

        // Build weak ref maps for the closures to avoid circular references.
        // Strong refs to pipeline/elements inside pad-added closures would prevent
        // the pipeline from ever being finalized (elements own the closures via
        // signal handlers, and the closures would own the pipeline).
        let elements_weak: std::collections::HashMap<String, gst::glib::WeakRef<gst::Element>> =
            self.elements
                .iter()
                .map(|(name, elem)| (name.clone(), elem.downgrade()))
                .collect();
        let pending_links = self.pending_links.clone();
        let pipeline_weak: gst::glib::WeakRef<gst::Pipeline> = self.pipeline.downgrade();
        let dynamic_pad_tees = self.dynamic_pad_tees.clone();

        for (element_id, element) in &self.elements {
            let element_id = element_id.clone();
            let elements_weak = elements_weak.clone();
            let pending_links = pending_links.clone();
            let pipeline_weak = pipeline_weak.clone();
            let dynamic_pad_tees = dynamic_pad_tees.clone();

            // Connect to pad-added signal
            element.connect_pad_added(move |_elem, new_pad| {
                let new_pad_name = new_pad.name();

                // Only handle src pads (output pads)
                if new_pad.direction() != gst::PadDirection::Src {
                    return;
                }

                debug!("Dynamic src pad added on element {}: {}", element_id, new_pad_name);

                // Check if any pending links match this pad
                let mut found_link = false;
                for link in &pending_links {
                    let (from_elem, from_pad) = Self::parse_element_pad(&link.from);
                    let (to_elem, to_pad) = Self::parse_element_pad(&link.to);

                    // Check if this new pad matches a pending source pad
                    if from_elem == element_id {
                        if let Some(expected_pad_name) = from_pad {
                            // Smart pad matching for dynamic pads:
                            // - Exact match: "src_0" == "src_0"
                            // - Pattern match: "src" matches "src_0", "src_1", etc.
                            //   (for elements with Sometimes pads like decodebin's src_%u template)
                            let pad_matches = new_pad_name == expected_pad_name
                                || (new_pad_name.starts_with(expected_pad_name)
                                    && new_pad_name
                                        .strip_prefix(expected_pad_name)
                                        .is_some_and(|suffix| {
                                            suffix.starts_with('_')
                                                && suffix[1..].chars().all(|c| c.is_ascii_digit())
                                        }));

                            if pad_matches {
                                if new_pad_name != expected_pad_name {
                                    debug!(
                                        "Dynamic pad pattern match: expected '{}', got '{}' on element {}",
                                        expected_pad_name, new_pad_name, element_id
                                    );
                                }
                                // Upgrade weak refs to get the elements
                                let sink_elem: Option<gst::Element> = elements_weak
                                    .get(to_elem)
                                    .and_then(gst::glib::WeakRef::upgrade);
                                let src_elem: Option<gst::Element> = elements_weak
                                    .get(from_elem)
                                    .and_then(gst::glib::WeakRef::upgrade);

                                if let (Some(_src_elem), Some(sink_elem)) =
                                    (src_elem, sink_elem)
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
                                                found_link = true;
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

                // If no pending link matched this pad, auto-attach a tee with allow-not-linked=true
                // This prevents unlinked dynamic pads from blocking the pipeline.
                // Skip elements that already have allow-not-linked=true (e.g. thumbnail tees) —
                // they handle unlinked pads themselves and may get pads requested later at runtime.
                let Some(pipeline) = pipeline_weak.upgrade() else {
                    return;
                };
                let is_allow_not_linked = {
                    if let Some(elem) = pipeline.by_name(&element_id) {
                        elem.has_property("allow-not-linked")
                            && elem.property::<bool>("allow-not-linked")
                    } else {
                        false
                    }
                };
                if !found_link && new_pad.peer().is_none() && !is_allow_not_linked {
                    let tee_name = format!("{}_{}_autotee", element_id, new_pad_name);
                    info!(
                        "Auto-creating tee '{}' for unlinked dynamic pad {}:{}",
                        tee_name, element_id, new_pad_name
                    );

                    // Create a tee with allow-not-linked=true
                    let tee = match gst::ElementFactory::make("tee")
                        .name(&tee_name)
                        .property("allow-not-linked", true)
                        .build()
                    {
                        Ok(t) => t,
                        Err(e) => {
                            error!("Failed to create auto-tee for {}:{}: {}", element_id, new_pad_name, e);
                            return;
                        }
                    };

                    // Add tee to pipeline
                    if let Err(e) = pipeline.add(&tee) {
                        error!("Failed to add auto-tee to pipeline: {}", e);
                        return;
                    }

                    // Sync tee state with pipeline
                    if let Err(e) = tee.sync_state_with_parent() {
                        error!("Failed to sync auto-tee state: {}", e);
                        return;
                    }

                    // Link dynamic pad to tee sink
                    let tee_sink = match tee.static_pad("sink") {
                        Some(pad) => pad,
                        None => {
                            error!("Auto-tee has no sink pad");
                            return;
                        }
                    };

                    match new_pad.link(&tee_sink) {
                        Ok(_) => {
                            info!(
                                "Successfully auto-linked dynamic pad {}:{} -> {}",
                                element_id, new_pad_name, tee_name
                            );

                            // Record this auto-tee for the API/frontend
                            if let Ok(mut tees) = dynamic_pad_tees.write() {
                                tees.entry(element_id.clone())
                                    .or_default()
                                    .insert(new_pad_name.to_string(), tee_name);
                            }
                        }
                        Err(e) => {
                            error!(
                                "Failed to link dynamic pad {}:{} to auto-tee: {}",
                                element_id, new_pad_name, e
                            );
                            // Clean up the tee we added
                            let _ = pipeline.remove(&tee);
                        }
                    }
                }
            });
        }
    }

    /// Enable QoS on all pads of all elements (for buffer drop monitoring).
    /// This should be called after all linking is complete.
    pub(super) fn enable_qos_on_all_pads(&self) {
        info!("Enabling QoS on all pads that support it");

        for (element_id, element) in &self.elements {
            // Iterate over all pads (both src and sink)
            for pad in element.pads() {
                if pad.has_property("qos") {
                    pad.set_property("qos", true);
                    debug!("Enabled QoS on pad {}:{}", element_id, pad.name());
                }
            }
        }
    }

    /// Apply stored pad properties to pads after they've been created.
    /// This must be called after all linking is complete, since request pads
    /// (like audiomixer sink_%u) don't exist until they're requested during linking.
    pub(super) fn apply_pad_properties(&self) {
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

            // Debug: List all pads on this element (with error handling for corrupted pad names)
            let all_pads: Vec<String> = element
                .pads()
                .iter()
                .filter_map(|p| {
                    // Safely try to get pad name - if it fails (corrupted memory), skip it
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.name().to_string()))
                        .ok()
                })
                .collect();
            info!(
                "Element {} has {} pad(s): {:?}",
                element_id,
                all_pads.len(),
                all_pads
            );

            for (pad_name, properties) in pad_props {
                // Get the pad - it should already exist from linking
                // Try static pad first, then iterate through all pads (for request pads)
                let pad = if let Some(p) = element.static_pad(pad_name) {
                    p
                } else {
                    // For request pads (like mixer sink_0, sink_1), they were created during linking
                    // Iterate through existing pads instead of calling request_pad_simple (which can crash)
                    match element
                        .pads()
                        .into_iter()
                        .find(|p| p.name().as_str() == pad_name.as_str())
                    {
                        Some(p) => p,
                        None => {
                            warn!(
                                "Pad {}:{} not found when trying to apply pad properties. Available pads: {:?}",
                                element_id, pad_name, all_pads
                            );
                            continue;
                        }
                    }
                };

                // Enable QoS by default if the pad supports it (for buffer drop monitoring)
                if pad.has_property("qos") {
                    pad.set_property("qos", true);
                    debug!("Enabled QoS on pad {}:{}", element_id, pad_name);
                }

                info!(
                    "Applying {} properties to pad {}:{}: {:?}",
                    properties.len(),
                    element_id,
                    pad_name,
                    properties.keys().collect::<Vec<_>>()
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
    pub(super) fn insert_tees_if_needed(original_links: &[strom_types::Link]) -> ProcessedLinks {
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
            new_links.push(strom_types::Link {
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

                new_links.push(strom_types::Link {
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
}
