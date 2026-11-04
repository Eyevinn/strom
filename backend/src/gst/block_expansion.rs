//! Block expansion logic - converts block instances to GStreamer elements using BlockBuilder trait.

use crate::blocks::builtin;
use crate::blocks::{
    BlockBuildContext, BusMessageConnectFn, DynamicWebrtcbinStore, ElementSetupFn,
    WhepEndpointInfo, WhipEndpointInfo,
};
use crate::gst::SessionThreadConfig;
use crate::whip_registry::WhipRegistry;
use crate::whip_session_manager::WhipEndpointConfig;
use gstreamer as gst;
use strom_types::{BlockInstance, Link};
use tracing::{debug, info};

use super::PipelineError;

use std::collections::HashMap;
use strom_types::PropertyValue;

/// Result of expanding blocks into elements and links.
pub struct ExpandedPipeline {
    /// GStreamer elements (from both regular elements and expanded blocks)
    pub gst_elements: Vec<(String, gst::Element)>,
    /// Links between all elements
    pub links: Vec<Link>,
    /// Bus message handler connection functions from blocks
    pub bus_message_handlers: Vec<BusMessageConnectFn>,
    /// Element signal setup functions from blocks
    pub element_setups: Vec<ElementSetupFn>,
    /// Pad properties from blocks (element_id -> pad_name -> property_name -> value)
    pub pad_properties: HashMap<String, HashMap<String, HashMap<String, PropertyValue>>>,
    /// WHEP endpoints registered by blocks
    pub whep_endpoints: Vec<WhepEndpointInfo>,
    /// WHIP endpoints registered by blocks
    pub whip_endpoints: Vec<WhipEndpointInfo>,
    /// WHIP endpoint configs for session manager registration
    pub whip_endpoint_configs: Vec<(String, WhipEndpointConfig)>,
}

/// Expand block instances into GStreamer elements using BlockBuilder trait.
///
/// This function:
/// 1. Calls BlockBuilder.build() for each block instance to get GStreamer elements
/// 2. Adds internal links from the blocks
/// 3. Resolves external links through block external pads
///
/// The `flow_id` is injected as a special `_flow_id` property for blocks that need it
/// (e.g., InterOutput blocks use it to generate unique channel names).
/// The `media_path` is injected as `_media_path` for blocks that resolve media files
/// (e.g., MediaPlayer block).
#[allow(clippy::too_many_arguments)]
pub async fn expand_blocks(
    blocks: &[BlockInstance],
    regular_links: &[Link],
    flow_id: &strom_types::FlowId,
    media_path: &std::path::Path,
    ice_servers: Vec<String>,
    ice_transport_policy: String,
    dynamic_webrtcbins: DynamicWebrtcbinStore,
    whip_registry: Option<WhipRegistry>,
    session_thread_config: SessionThreadConfig,
    local_devices: crate::discovery::device::GstDeviceMap,
) -> Result<ExpandedPipeline, PipelineError> {
    let mut gst_elements = Vec::new();
    let mut all_links = Vec::new();
    let mut bus_message_handlers = Vec::new();
    let mut all_pad_properties: HashMap<String, HashMap<String, HashMap<String, PropertyValue>>> =
        HashMap::new();

    // Create build context for blocks to register services (with shared webrtcbin store)
    let ctx = BlockBuildContext::new_with_webrtcbin_store(
        ice_servers,
        ice_transport_policy,
        dynamic_webrtcbins,
        whip_registry,
        session_thread_config,
        local_devices,
    );

    debug!("Expanding {} block instance(s)", blocks.len());

    // Canonicalize media_path once so all blocks get an absolute path,
    // even if the config uses a relative path like "./strom-data/media".
    // `dunce` keeps Windows paths out of the verbatim `\\?\` form, where `/`
    // is not a separator: blocks join file names onto this with `/`, and
    // filesink then refuses the location (#835).
    let canonical_media =
        dunce::canonicalize(media_path).unwrap_or_else(|_| media_path.to_path_buf());
    let canonical_media_str = canonical_media.to_string_lossy().to_string();

    let definitions = if blocks.is_empty() {
        Vec::new()
    } else {
        builtin::get_all_builtin_blocks()
    };

    for block_instance in blocks {
        // Get builder for this block type
        let builder =
            builtin::get_builder(&block_instance.block_definition_id).ok_or_else(|| {
                PipelineError::InvalidFlow(format!(
                    "No builder found for block definition: {}",
                    block_instance.block_definition_id
                ))
            })?;

        debug!(
            "Building block instance '{}' (definition: {})",
            block_instance.id, block_instance.block_definition_id
        );

        // Drop properties whose `visible_when` is false; refuse enum values
        // that do not apply to the current settings.
        let mut properties = visible_properties(block_instance, &definitions)?;

        // Inject _flow_id, _block_id, and _media_path into properties for blocks that need them
        properties.insert(
            "_flow_id".to_string(),
            PropertyValue::String(flow_id.to_string()),
        );
        properties.insert(
            "_block_id".to_string(),
            PropertyValue::String(block_instance.id.clone()),
        );
        properties.insert(
            "_media_path".to_string(),
            PropertyValue::String(canonical_media_str.clone()),
        );

        // Call the builder to create GStreamer elements
        let build_result = builder
            .build(&block_instance.id, &properties, &ctx)
            .map_err(|e| {
                PipelineError::InvalidFlow(format!(
                    "Failed to build block {}: {}",
                    block_instance.id, e
                ))
            })?;

        debug!(
            "Block {} created {} element(s) and {} internal link(s)",
            block_instance.id,
            build_result.elements.len(),
            build_result.internal_links.len()
        );

        // Add the created elements
        gst_elements.extend(build_result.elements);

        // Add internal links (convert from ElementPadRef to Link string format)
        for (from_ref, to_ref) in build_result.internal_links {
            all_links.push(Link {
                from: from_ref.to_string_format(),
                to: to_ref.to_string_format(),
            });
        }

        // Collect bus message handler connection function if provided
        if let Some(bus_message_handler) = build_result.bus_message_handler {
            debug!("Block {} provided a bus message handler", block_instance.id);
            bus_message_handlers.push(bus_message_handler);
        }

        // Merge pad properties from this block
        if !build_result.pad_properties.is_empty() {
            debug!(
                "Block {} provided pad properties for {} element(s)",
                block_instance.id,
                build_result.pad_properties.len()
            );
            all_pad_properties.extend(build_result.pad_properties);
        }
    }

    // Resolve and add external links (between elements and/or blocks)
    for link in regular_links {
        info!("Resolving link: {} -> {}", link.from, link.to);
        let from = resolve_pad(link.from.as_str(), blocks).await?;
        let to = resolve_pad(link.to.as_str(), blocks).await?;

        info!("Resolved external link: {} -> {}", from, to);
        let link = Link { from, to };
        ctx.flow_link_targets()
            .insert(link.to_pad_refs().1.element_id);
        all_links.push(link);
    }

    // Collect element signal setup functions from context
    let element_setups = ctx.take_element_setups();
    if !element_setups.is_empty() {
        debug!(
            "Collected {} element signal setup(s) from blocks",
            element_setups.len()
        );
    }

    // Collect WHEP endpoints from context
    let whep_endpoints = ctx.take_whep_endpoints();
    if !whep_endpoints.is_empty() {
        for ep in &whep_endpoints {
            info!(
                "Block {} registered WHEP endpoint: endpoint_id='{}', port={}",
                ep.block_id, ep.endpoint_id, ep.internal_port
            );
        }
    }

    // Collect WHIP endpoints from context
    let whip_endpoints = ctx.take_whip_endpoints();
    if !whip_endpoints.is_empty() {
        for ep in &whip_endpoints {
            info!(
                "Block {} registered WHIP endpoint: endpoint_id='{}' (port assigned per-session)",
                ep.block_id, ep.endpoint_id
            );
        }
    }

    // Collect WHIP endpoint configs for session manager
    let whip_endpoint_configs = ctx.take_whip_endpoint_configs();
    if !whip_endpoint_configs.is_empty() {
        debug!(
            "Collected {} WHIP endpoint config(s) for session manager",
            whip_endpoint_configs.len()
        );
    }

    debug!(
        "Block expansion complete: {} GStreamer elements, {} links, {} bus message handlers, {} element setups, {} elements with pad properties, {} WHEP endpoints, {} WHIP endpoints",
        gst_elements.len(),
        all_links.len(),
        bus_message_handlers.len(),
        element_setups.len(),
        all_pad_properties.len(),
        whep_endpoints.len(),
        whip_endpoints.len()
    );

    Ok(ExpandedPipeline {
        gst_elements,
        links: all_links,
        bus_message_handlers,
        element_setups,
        pad_properties: all_pad_properties,
        whep_endpoints,
        whip_endpoints,
        whip_endpoint_configs,
    })
}

/// Resolve a pad reference, handling block external pads.
///
/// If the pad reference is to a block's external pad, resolve it to the
/// internal element:pad that it maps to. Otherwise, return as-is.
///
/// For blocks with computed_external_pads (dynamic pads based on properties),
/// uses the computed pads. Otherwise falls back to the static block definition.
async fn resolve_pad(pad_ref: &str, blocks: &[BlockInstance]) -> Result<String, PipelineError> {
    // Check if this references a block's external pad
    // Format: "block_id:external_pad_name"
    for block_instance in blocks {
        let block_prefix = format!("{}:", block_instance.id);
        if pad_ref.starts_with(&block_prefix) {
            // Extract external pad name
            let external_pad_name = &pad_ref[block_prefix.len()..];

            // Find and resolve the external pad - prefer computed_external_pads if available
            if let Some(ref computed) = block_instance.computed_external_pads {
                // Use computed pads for blocks with dynamic pads
                debug!(
                    "Using computed external pads for block {} ({})",
                    block_instance.id, block_instance.block_definition_id
                );

                if let Some(external_pad) = computed
                    .inputs
                    .iter()
                    .chain(computed.outputs.iter())
                    .find(|p| p.name == external_pad_name)
                {
                    // Resolve to namespaced internal element:pad
                    let resolved = format!(
                        "{}:{}:{}",
                        block_instance.id,
                        external_pad.internal_element_id,
                        external_pad.internal_pad_name
                    );

                    info!(
                        "Resolved block external pad '{}' -> '{}' (internal_element_id='{}', internal_pad_name='{}')",
                        pad_ref, resolved, external_pad.internal_element_id, external_pad.internal_pad_name
                    );

                    return Ok(resolved);
                } else {
                    return Err(PipelineError::InvalidFlow(format!(
                        "External pad '{}' not found in block '{}' (computed pads)",
                        external_pad_name, block_instance.id
                    )));
                }
            } else {
                // Fall back to static definition for blocks without computed pads
                debug!(
                    "Using static external pads for block {} ({})",
                    block_instance.id, block_instance.block_definition_id
                );
                let definition = crate::blocks::builtin::get_all_builtin_blocks()
                    .into_iter()
                    .find(|b| b.id == block_instance.block_definition_id)
                    .ok_or_else(|| {
                        PipelineError::InvalidFlow(format!(
                            "Block definition not found: {}",
                            block_instance.block_definition_id
                        ))
                    })?;

                if let Some(external_pad) = definition
                    .external_pads
                    .inputs
                    .iter()
                    .chain(definition.external_pads.outputs.iter())
                    .find(|p| p.name == external_pad_name)
                {
                    // Resolve to namespaced internal element:pad
                    let resolved = format!(
                        "{}:{}:{}",
                        block_instance.id,
                        external_pad.internal_element_id,
                        external_pad.internal_pad_name
                    );

                    debug!(
                        "Resolved block external pad '{}' -> '{}'",
                        pad_ref, resolved
                    );

                    return Ok(resolved);
                } else {
                    return Err(PipelineError::InvalidFlow(format!(
                        "External pad '{}' not found in block '{}' (static definition)",
                        external_pad_name, block_instance.id
                    )));
                }
            }
        }
    }

    // Not a block reference, return as-is (regular element:pad)
    Ok(pad_ref.to_string())
}

/// The block's stored properties without the ones its definition hides for
/// the current settings (see `strom_types::property_condition`). The stored
/// values stay in the flow; the block is only built as if they were unset.
/// An enum value that does not apply to the current settings is an error.
fn visible_properties(
    block: &BlockInstance,
    definitions: &[strom_types::BlockDefinition],
) -> Result<HashMap<String, PropertyValue>, PipelineError> {
    let Some(definition) = definitions
        .iter()
        .find(|d| d.id == block.block_definition_id)
    else {
        return Ok(block.properties.clone());
    };
    strom_types::block::effective_properties(&definition.exposed_properties, &block.properties)
        .map_err(|e| {
            PipelineError::InvalidFlow(format!("Block {} ({}): {}", block.id, definition.name, e))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::FlowId;

    #[tokio::test]
    async fn test_expand_no_blocks() {
        let flow_id = FlowId::new_v4();
        let ice_servers = vec!["stun:stun.l.google.com:19302".to_string()];
        let ice_transport_policy = "all".to_string();
        let dynamic_webrtcbins = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let result = expand_blocks(
            &[],
            &[],
            &flow_id,
            std::path::Path::new("./media"),
            ice_servers,
            ice_transport_policy,
            dynamic_webrtcbins,
            None,
            crate::gst::SessionThreadConfig::new(),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .await;
        assert!(result.is_ok());

        let expanded = result.unwrap();
        assert_eq!(expanded.gst_elements.len(), 0);
        assert_eq!(expanded.links.len(), 0);
    }

    fn block(definition: &str, props: &[(&str, PropertyValue)]) -> BlockInstance {
        BlockInstance {
            id: "b1".to_string(),
            block_definition_id: definition.to_string(),
            name: None,
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            position: strom_types::block::Position { x: 0.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: None,
        }
    }

    fn s(v: &str) -> PropertyValue {
        PropertyValue::String(v.to_string())
    }

    /// A custom-only Audio Processor setting is hidden under a named preset:
    /// the block is built without it, and it comes back with Custom.
    #[test]
    fn hidden_property_is_not_passed_to_the_builder() {
        use strom_types::audioprocessor::BLOCK_ID;
        gst::init().expect("gst init");
        let definitions = builtin::get_all_builtin_blocks();
        let loudness = ("target_loudness", PropertyValue::Float(-16.0));

        let named = block(BLOCK_ID, &[("preset", s("ebu-r128")), loudness.clone()]);
        let props = visible_properties(&named, &definitions).unwrap();
        assert!(!props.contains_key("target_loudness"));
        assert!(named.properties.contains_key("target_loudness"));

        let custom = block(BLOCK_ID, &[("preset", s("custom")), loudness]);
        let props = visible_properties(&custom, &definitions).unwrap();
        assert!(props.contains_key("target_loudness"));
    }

    /// An H.264 profile on an H.265 encoder fails the flow with a message
    /// naming the property, the value and the condition, before any element
    /// is made.
    #[tokio::test]
    async fn enum_value_hidden_for_the_current_settings_fails_the_build() {
        gst::init().expect("gst init");
        let blocks = [block(
            "builtin.videoenc",
            &[("codec", s("h265")), ("profile", s("high"))],
        )];
        let result = expand_blocks(
            &blocks,
            &[],
            &FlowId::new_v4(),
            std::path::Path::new("./media"),
            Vec::new(),
            "all".to_string(),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            None,
            crate::gst::SessionThreadConfig::new(),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .await;
        let Err(PipelineError::InvalidFlow(msg)) = result else {
            panic!("expected InvalidFlow");
        };
        assert!(msg.contains("'profile'"), "{msg}");
        assert!(msg.contains("'high'"), "{msg}");
        assert!(msg.contains("codec is one of h264"), "{msg}");
    }
}
