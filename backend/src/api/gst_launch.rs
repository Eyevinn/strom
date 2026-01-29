//! gst-launch-1.0 parsing and export API handlers.

use axum::{extract::State, http::StatusCode, Json};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use strom_types::{
    api::{
        ErrorResponse, ExportGstLaunchRequest, ExportGstLaunchResponse, ParseGstLaunchRequest,
        ParseGstLaunchResponse,
    },
    element::{Element, Link, PropertyValue},
};
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Preprocess a pipeline string to handle gst-launch command syntax.
///
/// This function:
/// - Strips "gst-launch-1.0" prefix and common flags (-v, -e, etc.)
/// - Handles line continuations (backslash followed by newline)
/// - Normalizes whitespace
fn preprocess_pipeline_string(input: &str) -> String {
    // Handle line continuations first (backslash followed by newline)
    let without_continuations = input.replace("\\\n", " ").replace("\\\r\n", " ");

    // Trim and split into tokens
    let trimmed = without_continuations.trim();

    // Check if it starts with gst-launch command (Linux/macOS or Windows .exe)
    if trimmed.starts_with("gst-launch-1.0.exe")
        || trimmed.starts_with("gst-launch.exe")
        || trimmed.starts_with("gst-launch-1.0")
        || trimmed.starts_with("gst-launch")
    {
        // Split by whitespace to remove command and flags
        let mut tokens: Vec<&str> = trimmed.split_whitespace().collect();

        // Remove the first token (gst-launch, gst-launch-1.0, gst-launch.exe, or gst-launch-1.0.exe)
        if !tokens.is_empty() {
            tokens.remove(0);
        }

        // Remove common flags (-v, -e, -m, -t, -q, -f, etc.)
        tokens.retain(|token| !token.starts_with('-'));

        // Rejoin the remaining tokens
        tokens.join(" ")
    } else {
        // No gst-launch prefix, but still need to strip any leading flags and normalize whitespace
        let mut tokens: Vec<&str> = trimmed.split_whitespace().collect();

        // Remove leading flags (tokens starting with -)
        while !tokens.is_empty() && tokens[0].starts_with('-') {
            tokens.remove(0);
        }

        tokens.join(" ")
    }
}

/// Parse a gst-launch-1.0 pipeline string and extract elements and links.
///
/// This uses GStreamer's native pipeline parser to ensure complete compatibility
/// with the gst-launch-1.0 syntax.
#[utoipa::path(
    post,
    path = "/api/gst-launch/parse",
    tag = "gst-launch",
    request_body = ParseGstLaunchRequest,
    responses(
        (status = 200, description = "Pipeline parsed successfully", body = ParseGstLaunchResponse),
        (status = 400, description = "Invalid pipeline syntax", body = ErrorResponse)
    )
)]
pub async fn parse_gst_launch(
    State(_state): State<AppState>,
    Json(req): Json<ParseGstLaunchRequest>,
) -> Result<Json<ParseGstLaunchResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!("Parsing gst-launch pipeline: {}", req.pipeline);

    // Preprocess the pipeline string (strip gst-launch-1.0, handle line continuations)
    let cleaned_pipeline = preprocess_pipeline_string(&req.pipeline);
    debug!("Cleaned pipeline: {}", cleaned_pipeline);

    // Parse the pipeline using GStreamer's native parser
    let pipeline = match gst::parse::launch(&cleaned_pipeline) {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to parse pipeline: {}", e);
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Invalid pipeline syntax",
                    e.to_string(),
                )),
            ));
        }
    };

    // The parsed pipeline is a GstBin (or GstPipeline which extends GstBin)
    let bin = match pipeline.clone().downcast::<gst::Bin>() {
        Ok(b) => b,
        Err(_) => {
            // If it's a single element, wrap it in our response
            let element = pipeline.downcast::<gst::Element>().map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new("Failed to process parsed pipeline")),
                )
            })?;

            let elem = extract_element_info(&element, 0)?;
            return Ok(Json(ParseGstLaunchResponse {
                elements: vec![elem],
                links: vec![],
            }));
        }
    };

    // Extract all elements from the bin
    let mut elements = Vec::new();
    let mut element_id_map: HashMap<String, String> = HashMap::new(); // gst name -> our id

    let gst_elements: Vec<gst::Element> = bin.iterate_elements().into_iter().flatten().collect();
    let num_elements = gst_elements.len();

    for (idx, gst_elem) in gst_elements.into_iter().enumerate() {
        let gst_name = gst_elem.name().to_string();

        match extract_element_info(&gst_elem, idx) {
            Ok(elem) => {
                element_id_map.insert(gst_name, elem.id.clone());
                elements.push(elem);
            }
            Err(e) => {
                warn!("Failed to extract element info for {}: {:?}", gst_name, e);
            }
        }
    }

    // Extract links by iterating through pads
    let mut links = Vec::new();
    let mut seen_links: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    debug!("Extracting links from {} elements", num_elements);

    // Re-iterate to find links (need to get elements from bin again)
    for gst_elem in bin.iterate_elements().into_iter().flatten() {
        let gst_name = gst_elem.name().to_string();
        let Some(our_id) = element_id_map.get(&gst_name) else {
            continue;
        };

        let src_pads: Vec<_> = gst_elem.src_pads();
        debug!(
            "Element '{}' ({}) has {} src pads",
            gst_name,
            our_id,
            src_pads.len()
        );

        // Check source pads for outgoing links
        for pad in src_pads {
            debug!("  Pad '{}' on element '{}'", pad.name(), gst_name);
            if let Some(peer) = pad.peer() {
                debug!("    Has peer: '{}'", peer.name());
                if let Some(peer_elem) = peer.parent_element() {
                    let peer_gst_name = peer_elem.name().to_string();
                    debug!("      Peer element: '{}'", peer_gst_name);
                    if let Some(peer_our_id) = element_id_map.get(&peer_gst_name) {
                        let link_key = (our_id.clone(), peer_our_id.clone());
                        if !seen_links.contains(&link_key) {
                            seen_links.insert(link_key);

                            // Always include pad names for frontend compatibility
                            let from = format!("{}:{}", our_id, pad.name());
                            let to = format!("{}:{}", peer_our_id, peer.name());

                            debug!("      Creating link: {} -> {}", from, to);
                            links.push(Link {
                                from: from.clone(),
                                to: to.clone(),
                            });
                        }
                    } else {
                        debug!(
                            "      Peer element '{}' not in element_id_map",
                            peer_gst_name
                        );
                    }
                } else {
                    debug!("    Peer has no parent element");
                }
            } else {
                debug!("    No peer");
            }
        }
    }

    // Reposition elements based on topological order (left to right flow)
    reposition_elements_topologically(&mut elements, &links);

    info!(
        "Parsed {} elements and {} links from pipeline",
        num_elements,
        links.len()
    );

    Ok(Json(ParseGstLaunchResponse { elements, links }))
}

/// Extract element information from a GStreamer element.
fn extract_element_info(
    gst_elem: &gst::Element,
    position_idx: usize,
) -> Result<Element, (StatusCode, Json<ErrorResponse>)> {
    let factory = gst_elem.factory().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("Element has no factory")),
        )
    })?;

    let element_type = factory.name().to_string();
    let gst_name = gst_elem.name().to_string();

    // Generate a unique ID - use the GStreamer element name if it's not auto-generated
    let id = if gst_name.starts_with(&element_type) && gst_name.len() > element_type.len() {
        // Auto-generated name like "videotestsrc0" - create a more readable ID
        format!("{}_{}", element_type, position_idx)
    } else {
        // User-specified name like "mysource" - preserve it
        gst_name.clone()
    };

    // Extract non-default properties
    let properties = extract_non_default_properties(gst_elem);

    // Calculate position - arrange in a horizontal line with some spacing
    let x = 100.0 + (position_idx as f32 * 250.0);
    let y = 200.0;

    Ok(Element {
        id,
        element_type,
        properties,
        pad_properties: HashMap::new(),
        position: (x, y),
    })
}

/// Reposition elements based on topological order (left to right data flow).
///
/// Elements are arranged in layers based on their distance from source elements,
/// with sources on the left and sinks on the right.
fn reposition_elements_topologically(elements: &mut [Element], links: &[Link]) {
    if elements.is_empty() {
        return;
    }

    // Build adjacency information (element_id -> downstream element_ids)
    let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();
    let mut incoming: HashMap<String, HashSet<String>> = HashMap::new();

    for link in links {
        // Extract element IDs from "element_id:pad_name" format
        let from_id = link
            .from
            .split(':')
            .next()
            .unwrap_or(&link.from)
            .to_string();
        let to_id = link.to.split(':').next().unwrap_or(&link.to).to_string();

        outgoing
            .entry(from_id.clone())
            .or_default()
            .push(to_id.clone());
        incoming.entry(to_id).or_default().insert(from_id);
    }

    // Calculate depth for each element (maximum distance from a source)
    let mut depth_map: HashMap<String, usize> = HashMap::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    // Find all source elements (no incoming links) and start BFS from them
    for elem in elements.iter() {
        if !incoming.contains_key(&elem.id) {
            depth_map.insert(elem.id.clone(), 0);
            queue.push_back((elem.id.clone(), 0));
        }
    }

    // BFS to calculate depths
    while let Some((elem_id, depth)) = queue.pop_front() {
        if let Some(downstream) = outgoing.get(&elem_id) {
            for next_id in downstream {
                let new_depth = depth + 1;
                let updated = depth_map
                    .get(next_id)
                    .is_none_or(|&existing| new_depth > existing);

                if updated {
                    depth_map.insert(next_id.clone(), new_depth);
                    queue.push_back((next_id.clone(), new_depth));
                }
            }
        }
    }

    // Assign depth 0 to any elements not reached (disconnected)
    for elem in elements.iter() {
        depth_map.entry(elem.id.clone()).or_insert(0);
    }

    // Group elements by depth
    let mut depth_groups: HashMap<usize, Vec<String>> = HashMap::new();
    for (elem_id, &depth) in &depth_map {
        depth_groups.entry(depth).or_default().push(elem_id.clone());
    }

    // Update positions: each depth level gets its own column
    const HORIZONTAL_SPACING: f32 = 250.0;
    const VERTICAL_SPACING: f32 = 150.0;
    const START_X: f32 = 100.0;
    const START_Y: f32 = 200.0;

    for elem in elements.iter_mut() {
        let depth = depth_map.get(&elem.id).copied().unwrap_or(0);
        let group = depth_groups.get(&depth).unwrap();

        // Find this element's index within its depth group
        let index_in_group = group.iter().position(|id| id == &elem.id).unwrap_or(0);

        // Position: x based on depth, y based on index within depth group
        elem.position.0 = START_X + (depth as f32 * HORIZONTAL_SPACING);
        elem.position.1 = START_Y + (index_in_group as f32 * VERTICAL_SPACING);
    }

    debug!(
        "Repositioned {} elements across {} depth levels",
        elements.len(),
        depth_groups.len()
    );
}

/// Extract properties that differ from their default values.
fn extract_non_default_properties(gst_elem: &gst::Element) -> HashMap<String, PropertyValue> {
    let mut properties = HashMap::new();

    // Get the element's class properties
    let obj_class = gst_elem.class();

    // Create a fresh element of the same type to get actual default values
    // (pspec.default_value() doesn't always match the real defaults!)
    let factory = gst_elem.factory().expect("Element should have a factory");
    let fresh_elem = match factory.create().build() {
        Ok(elem) => Some(elem),
        Err(_) => {
            warn!(
                "Failed to create fresh element for {}, falling back to pspec defaults",
                factory.name()
            );
            None
        }
    };

    for pspec in obj_class.list_properties() {
        let prop_name = pspec.name();

        // Skip read-only and construct-only properties
        if !pspec
            .flags()
            .contains(gstreamer::glib::ParamFlags::WRITABLE)
        {
            continue;
        }

        // Skip the "name" property - we handle it separately
        if prop_name == "name" || prop_name == "parent" {
            continue;
        }

        // Try to get the current value
        let current_value = gst_elem.property_value(prop_name);

        // Get the default value from a fresh element if possible
        let default_value = if let Some(ref fresh) = fresh_elem {
            fresh.property_value(prop_name)
        } else {
            pspec.default_value().clone()
        };

        // Debug logging for specific properties we care about
        if prop_name == "pattern" || prop_name == "qos" {
            debug!(
                "Element {}: Checking property '{}' - current type: {:?}, default type: {:?}",
                gst_elem.name(),
                prop_name,
                current_value.type_(),
                default_value.type_()
            );
            debug!("  Current value: {:?}", current_value);
            debug!("  Default value: {:?}", default_value);
        }

        // Compare and only include if different from default
        let is_different = !values_equal(&current_value, &default_value);

        if is_different {
            if let Some(prop_value) = gvalue_to_property_value(&current_value) {
                debug!(
                    "Element {}: Property '{}' differs from default -> {:?}",
                    gst_elem.name(),
                    prop_name,
                    prop_value
                );
                properties.insert(prop_name.to_string(), prop_value);
            } else {
                debug!(
                    "Element {}: Property '{}' differs but couldn't convert value type: {:?}",
                    gst_elem.name(),
                    prop_name,
                    current_value.type_()
                );
            }
        } else if prop_name == "pattern" || prop_name == "qos" {
            debug!(
                "Element {}: Property '{}' equals default (not including)",
                gst_elem.name(),
                prop_name
            );
        }
    }

    properties
}

/// Check if two GValues are equal.
fn values_equal(a: &gstreamer::glib::Value, b: &gstreamer::glib::Value) -> bool {
    // Handle enums first - they need special handling with transform()
    if a.type_().is_a(gstreamer::glib::Type::ENUM) && b.type_().is_a(gstreamer::glib::Type::ENUM) {
        // Both are enums - compare the underlying integer values using transform()
        if let (Ok(a_transformed), Ok(b_transformed)) = (a.transform::<i32>(), b.transform::<i32>())
        {
            if let (Ok(av), Ok(bv)) = (a_transformed.get::<i32>(), b_transformed.get::<i32>()) {
                return av == bv;
            }
        }
        // If transform fails, assume different
        return false;
    }

    // Try to compare based on type for non-enums
    if a.type_() != b.type_() {
        return false;
    }

    // Try common types
    if let (Ok(av), Ok(bv)) = (a.get::<i32>(), b.get::<i32>()) {
        return av == bv;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<i64>(), b.get::<i64>()) {
        return av == bv;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<u32>(), b.get::<u32>()) {
        return av == bv;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<u64>(), b.get::<u64>()) {
        return av == bv;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<f32>(), b.get::<f32>()) {
        return (av - bv).abs() < f32::EPSILON;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<f64>(), b.get::<f64>()) {
        return (av - bv).abs() < f64::EPSILON;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<bool>(), b.get::<bool>()) {
        return av == bv;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<String>(), b.get::<String>()) {
        return av == bv;
    }
    if let (Ok(av), Ok(bv)) = (a.get::<Option<String>>(), b.get::<Option<String>>()) {
        return av == bv;
    }

    // For unknown types, assume they're different to be safe
    false
}

/// Convert a GValue to our PropertyValue type.
fn gvalue_to_property_value(value: &gstreamer::glib::Value) -> Option<PropertyValue> {
    // Handle enums FIRST - convert to their string nick (e.g., "ball" instead of 18)
    if value.type_().is_a(gstreamer::glib::Type::ENUM) {
        debug!("Value is enum type: {:?}", value.type_());

        // Get the enum class to look up the nick
        if let Some(enum_class) = gstreamer::glib::EnumClass::with_type(value.type_()) {
            debug!("Got enum class");

            // Get the integer value - MUST use transform() for enums, not get()!
            if let Ok(transformed) = value.transform::<i32>() {
                if let Ok(int_val) = transformed.get::<i32>() {
                    debug!("Got int value via transform: {}", int_val);
                    // Look up the enum value entry
                    if let Some(enum_value) = enum_class.value(int_val) {
                        let nick = enum_value.nick().to_string();
                        debug!("Got enum nick: {}", nick);
                        // Return the nick as a string (e.g., "ball" for pattern=18)
                        return Some(PropertyValue::String(nick));
                    } else {
                        debug!("Failed to look up enum value for {}", int_val);
                    }
                } else {
                    debug!("Failed to get i32 from transformed value");
                }
            } else {
                debug!("Failed to transform enum to i32");
            }
        } else {
            debug!("Failed to get enum class for type {:?}", value.type_());
        }

        // Fallback: if we can't get the nick, try to use the integer
        debug!(
            "Failed to get enum nick for type {:?}, falling back to integer",
            value.type_()
        );
        if let Ok(transformed) = value.transform::<i32>() {
            if let Ok(v) = transformed.get::<i32>() {
                return Some(PropertyValue::Int(v as i64));
            }
        }
        return None;
    }

    // Try different types
    if let Ok(v) = value.get::<i32>() {
        return Some(PropertyValue::Int(v as i64));
    }
    if let Ok(v) = value.get::<i64>() {
        return Some(PropertyValue::Int(v));
    }
    if let Ok(v) = value.get::<u32>() {
        return Some(PropertyValue::UInt(v as u64));
    }
    if let Ok(v) = value.get::<u64>() {
        return Some(PropertyValue::UInt(v));
    }
    if let Ok(v) = value.get::<f32>() {
        return Some(PropertyValue::Float(v as f64));
    }
    if let Ok(v) = value.get::<f64>() {
        return Some(PropertyValue::Float(v));
    }
    if let Ok(v) = value.get::<bool>() {
        return Some(PropertyValue::Bool(v));
    }
    if let Ok(v) = value.get::<String>() {
        return Some(PropertyValue::String(v));
    }
    if let Ok(Some(v)) = value.get::<Option<String>>() {
        return Some(PropertyValue::String(v));
    }

    None
}

/// Export elements and links to gst-launch-1.0 syntax.
#[utoipa::path(
    post,
    path = "/api/gst-launch/export",
    tag = "gst-launch",
    request_body = ExportGstLaunchRequest,
    responses(
        (status = 200, description = "Pipeline exported successfully", body = ExportGstLaunchResponse),
        (status = 400, description = "Cannot export pipeline", body = ErrorResponse)
    )
)]
pub async fn export_gst_launch(
    State(_state): State<AppState>,
    Json(req): Json<ExportGstLaunchRequest>,
) -> Result<Json<ExportGstLaunchResponse>, (StatusCode, Json<ErrorResponse>)> {
    info!(
        "Exporting {} elements and {} links to gst-launch syntax",
        req.elements.len(),
        req.links.len()
    );

    if req.elements.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("No elements to export")),
        ));
    }

    let pipeline = elements_to_gst_launch(&req.elements, &req.links);

    Ok(Json(ExportGstLaunchResponse { pipeline }))
}

/// Convert elements and links to a gst-launch-1.0 pipeline string.
fn elements_to_gst_launch(elements: &[Element], links: &[Link]) -> String {
    if elements.is_empty() {
        return String::new();
    }

    // Build adjacency information
    let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut incoming: HashMap<&str, Vec<&str>> = HashMap::new();

    for link in links {
        let from_id = link.from.split(':').next().unwrap_or(&link.from);
        let to_id = link.to.split(':').next().unwrap_or(&link.to);

        outgoing.entry(from_id).or_default().push(to_id);
        incoming.entry(to_id).or_default().push(from_id);
    }

    // Find source elements (no incoming links)
    let sources: Vec<&Element> = elements
        .iter()
        .filter(|e| !incoming.contains_key(e.id.as_str()))
        .collect();

    // Build element lookup
    let element_map: HashMap<&str, &Element> =
        elements.iter().map(|e| (e.id.as_str(), e)).collect();

    // Track which elements we've already output
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // Elements that need to be named (have multiple outgoing or incoming connections)
    let mut needs_name: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for elem in elements {
        let out_count = outgoing.get(elem.id.as_str()).map_or(0, |v| v.len());
        let in_count = incoming.get(elem.id.as_str()).map_or(0, |v| v.len());
        if out_count > 1 || in_count > 1 {
            needs_name.insert(elem.id.as_str());
        }
    }

    let mut result = String::new();
    let mut pending_refs: Vec<(&str, &str)> = Vec::new(); // (named element, target)

    // Process each chain starting from sources
    for source in &sources {
        if visited.contains(source.id.as_str()) {
            continue;
        }

        if !result.is_empty() {
            result.push_str(" \\\n  ");
        }

        // Follow the chain
        let mut current = *source;
        loop {
            visited.insert(current.id.as_str());

            // Output element
            result.push_str(&format_element(
                current,
                needs_name.contains(current.id.as_str()),
            ));

            // Get outgoing connections
            let targets = outgoing.get(current.id.as_str());

            match targets {
                None => {
                    // End of chain
                    break;
                }
                Some(targets) if targets.is_empty() => {
                    // End of chain
                    break;
                }
                Some(targets) if targets.len() == 1 => {
                    let target_id = targets[0];
                    if visited.contains(target_id) {
                        // Already visited - this is a reference
                        result.push_str(&format!(" ! {}. ", target_id));
                        break;
                    }
                    // Continue chain
                    result.push_str(" ! ");
                    current = element_map.get(target_id).unwrap();
                }
                Some(targets) => {
                    // Multiple targets - need to use tee pattern
                    // First target continues the chain
                    let first_target = targets[0];

                    // Other targets become pending references
                    for &target_id in &targets[1..] {
                        pending_refs.push((current.id.as_str(), target_id));
                    }

                    if visited.contains(first_target) {
                        result.push_str(&format!(" ! {}. ", first_target));
                        break;
                    }

                    result.push_str(" ! ");
                    current = element_map.get(first_target).unwrap();
                }
            }
        }
    }

    // Handle pending references (branches)
    for (from_name, target_id) in pending_refs {
        if !visited.contains(target_id) {
            result.push_str(&format!(" \\\n  {from_name}. ! "));

            let mut current = element_map.get(target_id).unwrap();
            loop {
                visited.insert(current.id.as_str());
                result.push_str(&format_element(
                    current,
                    needs_name.contains(current.id.as_str()),
                ));

                let targets = outgoing.get(current.id.as_str());
                match targets {
                    None => break,
                    Some(targets) if targets.is_empty() => break,
                    Some(targets) => {
                        let target_id = targets[0];
                        if visited.contains(target_id) {
                            result.push_str(&format!(" ! {}. ", target_id));
                            break;
                        }
                        result.push_str(" ! ");
                        current = element_map.get(target_id).unwrap();
                    }
                }
            }
        }
    }

    // Handle any remaining unvisited elements (disconnected)
    for elem in elements {
        if !visited.contains(elem.id.as_str()) {
            if !result.is_empty() {
                result.push_str(" \\\n  ");
            }
            result.push_str(&format_element(elem, false));
        }
    }

    result
}

/// Format a single element with its properties.
fn format_element(elem: &Element, include_name: bool) -> String {
    let mut parts = vec![elem.element_type.clone()];

    // Add name if needed
    if include_name {
        parts.push(format!("name={}", elem.id));
    }

    // Add properties
    for (key, value) in &elem.properties {
        let value_str = match value {
            PropertyValue::String(s) => {
                // Quote strings that contain spaces or special characters
                if s.contains(' ') || s.contains('!') || s.contains('=') {
                    format!("\"{}\"", s.replace('"', "\\\""))
                } else {
                    s.clone()
                }
            }
            PropertyValue::Int(i) => i.to_string(),
            PropertyValue::UInt(u) => u.to_string(),
            PropertyValue::Float(f) => f.to_string(),
            PropertyValue::Bool(b) => b.to_string(),
        };
        parts.push(format!("{}={}", key, value_str));
    }

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use strom_types::element::{Element, Link, PropertyValue};

    fn init_gst() {
        let _ = gstreamer::init();
    }

    // ========================================================================
    // Export Tests (elements_to_gst_launch, format_element)
    // ========================================================================

    #[test]
    fn test_format_element_simple() {
        let elem = Element {
            id: "src".to_string(),
            element_type: "videotestsrc".to_string(),
            properties: HashMap::new(),
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        };

        let result = format_element(&elem, false);
        assert_eq!(result, "videotestsrc");
    }

    #[test]
    fn test_format_element_with_name() {
        let elem = Element {
            id: "mysource".to_string(),
            element_type: "videotestsrc".to_string(),
            properties: HashMap::new(),
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        };

        let result = format_element(&elem, true);
        assert_eq!(result, "videotestsrc name=mysource");
    }

    #[test]
    fn test_format_element_with_properties() {
        let mut properties = HashMap::new();
        properties.insert("pattern".to_string(), PropertyValue::Int(18));
        properties.insert("is-live".to_string(), PropertyValue::Bool(true));

        let elem = Element {
            id: "src".to_string(),
            element_type: "videotestsrc".to_string(),
            properties,
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        };

        let result = format_element(&elem, false);
        // Properties order is not guaranteed, so check contains
        assert!(result.starts_with("videotestsrc"));
        assert!(result.contains("pattern=18"));
        assert!(result.contains("is-live=true"));
    }

    #[test]
    fn test_format_element_string_with_spaces() {
        let mut properties = HashMap::new();
        properties.insert(
            "location".to_string(),
            PropertyValue::String("my file.mp4".to_string()),
        );

        let elem = Element {
            id: "sink".to_string(),
            element_type: "filesink".to_string(),
            properties,
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        };

        let result = format_element(&elem, false);
        assert!(result.contains("location=\"my file.mp4\""));
    }

    #[test]
    fn test_elements_to_gst_launch_simple_chain() {
        let elements = vec![
            Element {
                id: "src".to_string(),
                element_type: "videotestsrc".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (0.0, 0.0),
            },
            Element {
                id: "conv".to_string(),
                element_type: "videoconvert".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (100.0, 0.0),
            },
            Element {
                id: "sink".to_string(),
                element_type: "fakesink".to_string(),
                properties: HashMap::new(),
                pad_properties: HashMap::new(),
                position: (200.0, 0.0),
            },
        ];

        let links = vec![
            Link {
                from: "src".to_string(),
                to: "conv".to_string(),
            },
            Link {
                from: "conv".to_string(),
                to: "sink".to_string(),
            },
        ];

        let result = elements_to_gst_launch(&elements, &links);
        assert_eq!(result, "videotestsrc ! videoconvert ! fakesink");
    }

    #[test]
    fn test_elements_to_gst_launch_empty() {
        let result = elements_to_gst_launch(&[], &[]);
        assert_eq!(result, "");
    }

    #[test]
    fn test_elements_to_gst_launch_single_element() {
        let elements = vec![Element {
            id: "src".to_string(),
            element_type: "videotestsrc".to_string(),
            properties: HashMap::new(),
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        }];

        let result = elements_to_gst_launch(&elements, &[]);
        assert_eq!(result, "videotestsrc");
    }

    #[test]
    fn test_elements_to_gst_launch_with_properties() {
        let mut props = HashMap::new();
        props.insert("pattern".to_string(), PropertyValue::Int(18));

        let elements = vec![
            Element {
                id: "src".to_string(),
                element_type: "videotestsrc".to_string(),
                properties: props,
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

        let links = vec![Link {
            from: "src".to_string(),
            to: "sink".to_string(),
        }];

        let result = elements_to_gst_launch(&elements, &links);
        assert_eq!(result, "videotestsrc pattern=18 ! fakesink");
    }

    // ========================================================================
    // Preprocessing Tests
    // ========================================================================

    #[test]
    fn test_preprocess_simple_pipeline() {
        let input = "videotestsrc ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! fakesink");
    }

    #[test]
    fn test_preprocess_with_gst_launch_prefix() {
        let input = "gst-launch-1.0 videotestsrc ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! fakesink");
    }

    #[test]
    fn test_preprocess_with_windows_exe() {
        let input = "gst-launch-1.0.exe videotestsrc ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! fakesink");
    }

    #[test]
    fn test_preprocess_with_windows_exe_and_flags() {
        let input = "gst-launch-1.0.exe -v -e videotestsrc ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! fakesink");
    }

    #[test]
    fn test_preprocess_with_flags() {
        let input = "gst-launch-1.0 -v -e videotestsrc ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! fakesink");
    }

    #[test]
    fn test_preprocess_line_continuation() {
        let input = "videotestsrc \\\n  ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! fakesink");
    }

    #[test]
    fn test_preprocess_multiline_with_command() {
        let input = "gst-launch-1.0 -v -e videotestsrc \\\n  ! x264enc \\\n  ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! x264enc ! fakesink");
    }

    #[test]
    fn test_preprocess_leading_flags_without_command() {
        // User pasted flags but forgot the gst-launch-1.0 command
        let input = "-v -e videotestsrc ! fakesink";
        let result = preprocess_pipeline_string(input);
        assert_eq!(result, "videotestsrc ! fakesink");
    }

    #[test]
    fn test_preprocess_complex_multiline() {
        let input = r#"gst-launch-1.0 -v -e videotestsrc \
  ! x264enc \
  ! mp4mux name=mux \
  ! filesink location="bla.mp4" \
  audiotestsrc ! lamemp3enc ! mux."#;
        let result = preprocess_pipeline_string(input);
        // Should strip gst-launch-1.0, -v, -e, and handle line continuations
        assert!(result.starts_with("videotestsrc"));
        assert!(result.contains("x264enc"));
        assert!(result.contains("mp4mux"));
        assert!(result.contains("name=mux"));
        assert!(result.contains("mux."));
        assert!(!result.contains("gst-launch"));
        assert!(!result.contains("-v"));
        assert!(!result.contains("-e"));
    }

    // ========================================================================
    // Parse Tests (require GStreamer initialization)
    // ========================================================================

    #[test]
    fn test_parse_simple_pipeline() {
        init_gst();

        let pipeline = gst::parse::launch("videotestsrc ! fakesink").unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        let elements: Vec<_> = bin.iterate_elements().into_iter().flatten().collect();
        assert_eq!(elements.len(), 2);

        // Check element types
        let types: Vec<_> = elements
            .iter()
            .map(|e| e.factory().unwrap().name().to_string())
            .collect();
        assert!(types.contains(&"videotestsrc".to_string()));
        assert!(types.contains(&"fakesink".to_string()));
    }

    #[test]
    fn test_parse_pipeline_with_properties() {
        init_gst();

        let pipeline = gst::parse::launch("videotestsrc pattern=ball ! fakesink").unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Find videotestsrc and check that we correctly extract its pattern property as a string
        for (idx, gst_elem) in bin.iterate_elements().into_iter().flatten().enumerate() {
            if gst_elem.factory().unwrap().name() == "videotestsrc" {
                // Extract element info which should include pattern as a string
                let elem = extract_element_info(&gst_elem, idx).unwrap();

                // Verify pattern is extracted as "ball" (string), not 18 (int)
                assert!(elem.properties.contains_key("pattern"));
                match elem.properties.get("pattern") {
                    Some(PropertyValue::String(s)) => {
                        assert_eq!(s, "ball", "Expected pattern='ball', got pattern='{}'", s);
                    }
                    other => panic!("Expected pattern as String('ball'), got {:?}", other),
                }
            }
        }
    }

    #[test]
    fn test_parse_invalid_pipeline() {
        init_gst();

        let result = gst::parse::launch("this_element_does_not_exist ! fakesink");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_non_default_properties() {
        init_gst();

        // Create element with default properties
        let elem = gst::ElementFactory::make("videotestsrc").build().unwrap();
        let props = extract_non_default_properties(&elem);
        // Should be empty - default values should match defaults
        assert!(
            props.is_empty(),
            "Expected no non-default properties, but got: {:?}",
            props
        );

        // Create element with non-default pattern using property_from_str for enum
        let elem2 = gst::ElementFactory::make("videotestsrc")
            .property_from_str("pattern", "ball")
            .build()
            .unwrap();
        let props2 = extract_non_default_properties(&elem2);
        assert!(
            props2.contains_key("pattern"),
            "Expected 'pattern' in properties, got: {:?}",
            props2
        );
        // Should be a string "ball"
        assert!(
            matches!(props2.get("pattern"), Some(PropertyValue::String(s)) if s == "ball"),
            "Expected pattern='ball', got {:?}",
            props2.get("pattern")
        );
    }

    #[test]
    fn test_values_equal_integers() {
        let a = 42i32.to_value();
        let b = 42i32.to_value();
        let c = 43i32.to_value();

        assert!(values_equal(&a, &b));
        assert!(!values_equal(&a, &c));
    }

    #[test]
    fn test_values_equal_strings() {
        let a = "hello".to_value();
        let b = "hello".to_value();
        let c = "world".to_value();

        assert!(values_equal(&a, &b));
        assert!(!values_equal(&a, &c));
    }

    #[test]
    fn test_values_equal_booleans() {
        let a = true.to_value();
        let b = true.to_value();
        let c = false.to_value();

        assert!(values_equal(&a, &b));
        assert!(!values_equal(&a, &c));
    }

    #[test]
    fn test_qos_default_comparison() {
        init_gst();

        // Verify that fresh element comparison works correctly for qos
        let elem = gst::ElementFactory::make("videoconvert").build().unwrap();
        let fresh_elem = gst::ElementFactory::make("videoconvert").build().unwrap();

        let qos_current = elem.property_value("qos");
        let qos_fresh = fresh_elem.property_value("qos");

        println!(
            "Current qos: {:?} (type: {:?})",
            qos_current,
            qos_current.type_()
        );
        println!("Fresh qos: {:?} (type: {:?})", qos_fresh, qos_fresh.type_());

        if let Ok(cv) = qos_current.get::<bool>() {
            println!("Current as bool: {}", cv);
        }
        if let Ok(fv) = qos_fresh.get::<bool>() {
            println!("Fresh as bool: {}", fv);
        }

        println!(
            "values_equal result: {}",
            values_equal(&qos_current, &qos_fresh)
        );

        // They should be equal (both true)
        assert!(
            values_equal(&qos_current, &qos_fresh),
            "qos current and fresh should be equal"
        );
    }

    #[test]
    fn test_gvalue_to_property_value() {
        // Integer
        let v = 42i32.to_value();
        assert!(matches!(
            gvalue_to_property_value(&v),
            Some(PropertyValue::Int(42))
        ));

        // Boolean
        let v = true.to_value();
        assert!(matches!(
            gvalue_to_property_value(&v),
            Some(PropertyValue::Bool(true))
        ));

        // String
        let v = "test".to_string().to_value();
        assert!(
            matches!(gvalue_to_property_value(&v), Some(PropertyValue::String(s)) if s == "test")
        );

        // Float
        let v = 2.5f64.to_value();
        if let Some(PropertyValue::Float(f)) = gvalue_to_property_value(&v) {
            assert!((f - 2.5).abs() < 0.001);
        } else {
            panic!("Expected Float");
        }
    }

    // ========================================================================
    // Enum Conversion Tests
    // ========================================================================

    #[test]
    fn test_enum_conversion_direct() {
        init_gst();

        // Create element with enum property set using from_str (enums need this)
        let elem = gst::ElementFactory::make("videotestsrc")
            .property_from_str("pattern", "ball")
            .build()
            .unwrap();

        // Get the property value
        let pattern_value = elem.property_value("pattern");

        println!("Pattern type: {:?}", pattern_value.type_());
        println!(
            "Is enum: {}",
            pattern_value.type_().is_a(gst::glib::Type::ENUM)
        );

        // Try to get as i32
        match pattern_value.get::<i32>() {
            Ok(val) => println!("Value as i32: {}", val),
            Err(e) => println!("Failed to get as i32: {:?}", e),
        }

        // Try enum class lookup
        if let Some(enum_class) = gst::glib::EnumClass::with_type(pattern_value.type_()) {
            println!("Successfully got enum class");
            if let Ok(int_val) = pattern_value.get::<i32>() {
                println!("Got int value: {}", int_val);
                if let Some(enum_value) = enum_class.value(int_val) {
                    println!("Got enum nick: {}", enum_value.nick());
                } else {
                    println!("enum_class.value({}) returned None", int_val);
                }
            }
        } else {
            println!("EnumClass::with_type returned None");
        }

        // Test gvalue_to_property_value conversion
        let converted = gvalue_to_property_value(&pattern_value);
        println!("Converted value: {:?}", converted);

        // Should be a String "ball", not Int 18
        match converted {
            Some(PropertyValue::String(s)) => {
                assert_eq!(s, "ball", "Expected enum nick 'ball', got '{}'", s);
            }
            other => {
                panic!("Expected PropertyValue::String('ball'), got {:?}", other);
            }
        }
    }

    // ========================================================================
    // Round-trip Tests (Import -> Export preserves enum properties)
    // ========================================================================

    #[test]
    fn test_roundtrip_enum_properties_simple() {
        init_gst();

        let input = "videotestsrc pattern=ball ! fakesink";

        // Parse the pipeline
        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements
        let mut elements = Vec::new();
        for (idx, gst_elem) in bin.iterate_elements().into_iter().flatten().enumerate() {
            let elem = extract_element_info(&gst_elem, idx).unwrap();
            elements.push(elem);
        }

        // Find videotestsrc and verify pattern is "ball" (string, not integer)
        let videotestsrc = elements
            .iter()
            .find(|e| e.element_type == "videotestsrc")
            .expect("videotestsrc not found");

        assert!(
            videotestsrc.properties.contains_key("pattern"),
            "pattern property missing"
        );
        match videotestsrc.properties.get("pattern") {
            Some(PropertyValue::String(s)) => {
                assert_eq!(s, "ball", "Expected pattern='ball', got pattern='{}'", s);
            }
            Some(other) => panic!("Expected pattern as String, got {:?}", other),
            None => panic!("pattern property missing"),
        }
    }

    #[test]
    fn test_roundtrip_enum_export() {
        init_gst();

        let input = "videotestsrc pattern=ball ! videoconvert ! fakesink";

        // Parse
        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements and links
        let mut elements = Vec::new();
        let mut element_id_map: HashMap<String, String> = HashMap::new();

        for (idx, gst_elem) in bin.iterate_elements().into_iter().flatten().enumerate() {
            let gst_name = gst_elem.name().to_string();
            let elem = extract_element_info(&gst_elem, idx).unwrap();
            element_id_map.insert(gst_name, elem.id.clone());
            elements.push(elem);
        }

        // Extract links
        let mut links = Vec::new();
        let mut seen_links: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        for gst_elem in bin.iterate_elements().into_iter().flatten() {
            let gst_name = gst_elem.name().to_string();
            let Some(our_id) = element_id_map.get(&gst_name) else {
                continue;
            };

            for pad in gst_elem.src_pads() {
                if let Some(peer) = pad.peer() {
                    if let Some(peer_elem) = peer.parent_element() {
                        let peer_gst_name = peer_elem.name().to_string();
                        if let Some(peer_our_id) = element_id_map.get(&peer_gst_name) {
                            let link_key = (our_id.clone(), peer_our_id.clone());
                            if !seen_links.contains(&link_key) {
                                seen_links.insert(link_key);
                                links.push(Link {
                                    from: format!("{}:{}", our_id, pad.name()),
                                    to: format!("{}:{}", peer_our_id, peer.name()),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Export back to gst-launch syntax
        let output = elements_to_gst_launch(&elements, &links);

        // Verify the output contains "pattern=ball" (not "pattern=18")
        assert!(
            output.contains("pattern=ball"),
            "Expected 'pattern=ball' in output, got: {}",
            output
        );
        assert!(
            !output.contains("pattern=18"),
            "Should not contain 'pattern=18', got: {}",
            output
        );
    }

    #[test]
    fn test_roundtrip_multiple_enum_properties() {
        init_gst();

        // Test with multiple enum properties (use non-default values)
        // pattern default is "smpte" (0), so "snow" is non-default
        // animation-mode default is "frames" (0), so use "wall-time" (1) instead
        let input = "videotestsrc pattern=snow animation-mode=wall-time ! fakesink";

        // Parse
        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements
        let mut elements = Vec::new();
        for (idx, gst_elem) in bin.iterate_elements().into_iter().flatten().enumerate() {
            let elem = extract_element_info(&gst_elem, idx).unwrap();
            elements.push(elem);
        }

        // Find videotestsrc
        let videotestsrc = elements
            .iter()
            .find(|e| e.element_type == "videotestsrc")
            .expect("videotestsrc not found");

        // Verify both enum properties are strings
        match videotestsrc.properties.get("pattern") {
            Some(PropertyValue::String(s)) => {
                assert_eq!(s, "snow", "Expected pattern='snow'");
            }
            other => panic!("Expected pattern as String, got {:?}", other),
        }

        match videotestsrc.properties.get("animation-mode") {
            Some(PropertyValue::String(s)) => {
                assert_eq!(s, "wall-time", "Expected animation-mode='wall-time'");
            }
            other => panic!("Expected animation-mode as String, got {:?}", other),
        }

        // Export and verify
        let links = Vec::new(); // No links needed for this test
        let output = elements_to_gst_launch(&elements, &links);

        assert!(
            output.contains("pattern=snow"),
            "Expected 'pattern=snow' in output"
        );
        assert!(
            output.contains("animation-mode=wall-time"),
            "Expected 'animation-mode=wall-time' in output"
        );
    }

    #[test]
    fn test_roundtrip_no_extra_properties() {
        init_gst();

        // THE REAL ROUND-TRIP TEST: Only explicitly set properties should be exported
        // This is what the user expected!
        let input = "videotestsrc pattern=ball ! videoconvert ! fakesink";

        // Parse
        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements and links
        let mut elements = Vec::new();
        let mut element_id_map: HashMap<String, String> = HashMap::new();

        for (idx, gst_elem) in bin.iterate_elements().into_iter().flatten().enumerate() {
            let gst_name = gst_elem.name().to_string();
            let elem = extract_element_info(&gst_elem, idx).unwrap();
            element_id_map.insert(gst_name, elem.id.clone());
            elements.push(elem);
        }

        // Extract links
        let mut links = Vec::new();
        let mut seen_links: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        for gst_elem in bin.iterate_elements().into_iter().flatten() {
            let gst_name = gst_elem.name().to_string();
            let Some(our_id) = element_id_map.get(&gst_name) else {
                continue;
            };

            for pad in gst_elem.src_pads() {
                if let Some(peer) = pad.peer() {
                    if let Some(peer_elem) = peer.parent_element() {
                        let peer_gst_name = peer_elem.name().to_string();
                        if let Some(peer_our_id) = element_id_map.get(&peer_gst_name) {
                            let link_key = (our_id.clone(), peer_our_id.clone());
                            if !seen_links.contains(&link_key) {
                                seen_links.insert(link_key);
                                links.push(Link {
                                    from: format!("{}:{}", our_id, pad.name()),
                                    to: format!("{}:{}", peer_our_id, peer.name()),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Export back to gst-launch syntax
        let output = elements_to_gst_launch(&elements, &links);

        // Verify ONLY pattern=ball is set on videotestsrc, no other properties
        let videotestsrc = elements
            .iter()
            .find(|e| e.element_type == "videotestsrc")
            .expect("videotestsrc not found");

        assert_eq!(
            videotestsrc.properties.len(),
            1,
            "Expected only 1 property (pattern), got: {:?}",
            videotestsrc.properties
        );
        assert!(matches!(
            videotestsrc.properties.get("pattern"),
            Some(PropertyValue::String(s)) if s == "ball"
        ));

        // Verify videoconvert has NO non-default properties
        let videoconvert = elements
            .iter()
            .find(|e| e.element_type == "videoconvert")
            .expect("videoconvert not found");

        assert!(
            videoconvert.properties.is_empty(),
            "Expected no non-default properties on videoconvert, got: {:?}",
            videoconvert.properties
        );

        // Verify fakesink has NO non-default properties
        let fakesink = elements
            .iter()
            .find(|e| e.element_type == "fakesink")
            .expect("fakesink not found");

        assert!(
            fakesink.properties.is_empty(),
            "Expected no non-default properties on fakesink, got: {:?}",
            fakesink.properties
        );

        // The output should be clean - only pattern=ball
        assert!(
            output.contains("pattern=ball"),
            "Expected 'pattern=ball' in output"
        );

        // Should NOT contain any of these default properties
        assert!(
            !output.contains("motion="),
            "Should not contain motion= (default value), got: {}",
            output
        );
        assert!(
            !output.contains("animation-mode="),
            "Should not contain animation-mode= (default value), got: {}",
            output
        );
        assert!(
            !output.contains("chroma-resampler="),
            "Should not contain chroma-resampler= (default value), got: {}",
            output
        );
        assert!(
            !output.contains("method="),
            "Should not contain method= (default value), got: {}",
            output
        );
    }

    #[test]
    fn test_roundtrip_preserves_all_property_types() {
        init_gst();

        // Pipeline with mixed property types: enum, bool, int
        let input = "videotestsrc pattern=ball is-live=true num-buffers=100 ! fakesink";

        // Parse
        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements
        let mut elements = Vec::new();
        for (idx, gst_elem) in bin.iterate_elements().into_iter().flatten().enumerate() {
            let elem = extract_element_info(&gst_elem, idx).unwrap();
            elements.push(elem);
        }

        // Find videotestsrc
        let videotestsrc = elements
            .iter()
            .find(|e| e.element_type == "videotestsrc")
            .expect("videotestsrc not found");

        // Verify enum is String
        assert!(matches!(
            videotestsrc.properties.get("pattern"),
            Some(PropertyValue::String(s)) if s == "ball"
        ));

        // Verify bool is Bool
        assert!(matches!(
            videotestsrc.properties.get("is-live"),
            Some(PropertyValue::Bool(true))
        ));

        // Verify int is Int
        assert!(matches!(
            videotestsrc.properties.get("num-buffers"),
            Some(PropertyValue::Int(100))
        ));

        // Export and verify all properties are present
        let links = Vec::new();
        let output = elements_to_gst_launch(&elements, &links);

        assert!(output.contains("pattern=ball"));
        assert!(output.contains("is-live=true"));
        assert!(output.contains("num-buffers=100"));
    }

    // ========================================================================
    // Real-World Pipeline Pattern Tests
    // ========================================================================

    #[test]
    fn test_parse_tee_pattern() {
        init_gst();

        // Skip if x264enc not available (e.g., Windows MSVC GStreamer)
        if gst::ElementFactory::find("x264enc").is_none() {
            println!("x264enc not available, skipping test");
            return;
        }

        // Tee pattern: record and display simultaneously
        let input = r#"videotestsrc ! tee name=t
            t. ! queue ! x264enc ! mp4mux ! filesink location=test.mp4
            t. ! queue ! fakesink"#;

        let cleaned = preprocess_pipeline_string(input);
        let pipeline = gst::parse::launch(&cleaned).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        let elements: Vec<_> = bin.iterate_elements().into_iter().flatten().collect();

        // Should have: videotestsrc, tee, queue (x2), x264enc, mp4mux, filesink, fakesink
        assert!(
            elements.len() >= 6,
            "Expected at least 6 elements in tee pipeline, got {}",
            elements.len()
        );

        // Verify we have a tee element
        let tee_elem = elements
            .iter()
            .find(|e| e.factory().unwrap().name() == "tee")
            .expect("Should have tee element");

        let tee_name: String = tee_elem.property("name");
        assert_eq!(tee_name, "t", "Expected tee to be named 't'");
    }

    #[test]
    fn test_parse_caps_with_properties() {
        init_gst();

        // Caps filter with properties containing hyphens and underscores
        let input = "videotestsrc ! video/x-raw,width=640,height=480,framerate=30/1 ! fakesink";

        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Should parse successfully with capsfilter
        let elements: Vec<_> = bin.iterate_elements().into_iter().flatten().collect();
        assert!(
            elements.len() >= 2,
            "Should have at least videotestsrc and fakesink"
        );
    }

    #[test]
    fn test_parse_properties_with_hyphens() {
        init_gst();

        // Properties with hyphens and underscores
        let input = "videotestsrc is-live=true num-buffers=100 ! fakesink sync=false";

        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements
        let mut elements = Vec::new();
        for (idx, gst_elem) in bin.iterate_elements().into_iter().flatten().enumerate() {
            let elem = extract_element_info(&gst_elem, idx).unwrap();
            elements.push(elem);
        }

        // Find videotestsrc and verify hyphenated properties
        let videotestsrc = elements
            .iter()
            .find(|e| e.element_type == "videotestsrc")
            .expect("videotestsrc not found");

        assert!(matches!(
            videotestsrc.properties.get("is-live"),
            Some(PropertyValue::Bool(true))
        ));
        assert!(matches!(
            videotestsrc.properties.get("num-buffers"),
            Some(PropertyValue::Int(100))
        ));
    }

    #[test]
    fn test_parse_rtp_streaming_pattern() {
        init_gst();

        // Skip if x264enc not available (e.g., Windows MSVC GStreamer)
        if gst::ElementFactory::find("x264enc").is_none() {
            println!("x264enc not available, skipping test");
            return;
        }

        // Simple RTP pattern (without the complex caps string that has typed values)
        let input = "videotestsrc ! x264enc ! rtph264pay ! udpsink port=5000";

        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        let elements: Vec<_> = bin.iterate_elements().into_iter().flatten().collect();

        // Should have: videotestsrc, x264enc, rtph264pay, udpsink
        assert!(
            elements.len() >= 4,
            "Expected at least 4 elements in RTP pipeline, got {}",
            elements.len()
        );

        // Verify we have the RTP payload element
        let types: Vec<_> = elements
            .iter()
            .map(|e| e.factory().unwrap().name().to_string())
            .collect();

        assert!(
            types.contains(&"rtph264pay".to_string()),
            "Missing rtph264pay"
        );
        assert!(types.contains(&"udpsink".to_string()), "Missing udpsink");
    }

    // ========================================================================
    // Complex Multi-Branch Pipeline Tests (Mux)
    // ========================================================================

    #[test]
    fn test_parse_multiline_mux_pipeline() {
        init_gst();

        // Skip if x264enc or lamemp3enc not available (e.g., Windows MSVC GStreamer)
        if gst::ElementFactory::find("x264enc").is_none()
            || gst::ElementFactory::find("lamemp3enc").is_none()
        {
            println!("x264enc or lamemp3enc not available, skipping test");
            return;
        }

        // The user's example pipeline with video and audio branches
        let input = r#"gst-launch-1.0 -v -e videotestsrc \
  ! x264enc \
  ! mp4mux name=mux \
  ! filesink location="bla.mp4" \
  audiotestsrc ! lamemp3enc ! mux."#;

        // Preprocess and parse
        let cleaned = preprocess_pipeline_string(input);
        let pipeline = gst::parse::launch(&cleaned).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements
        let elements: Vec<_> = bin.iterate_elements().into_iter().flatten().collect();

        // Should have 6 elements: videotestsrc, x264enc, mp4mux, filesink, audiotestsrc, lamemp3enc
        assert!(elements.len() >= 5, "Expected at least 5 elements (videotestsrc, x264enc, mp4mux, filesink, audiotestsrc, lamemp3enc), got {}", elements.len());

        // Check that we have the expected element types
        let types: Vec<_> = elements
            .iter()
            .map(|e| e.factory().unwrap().name().to_string())
            .collect();

        assert!(
            types.contains(&"videotestsrc".to_string()),
            "Missing videotestsrc"
        );
        assert!(
            types.contains(&"audiotestsrc".to_string()),
            "Missing audiotestsrc"
        );
        assert!(
            types.contains(&"mp4mux".to_string()) || types.contains(&"qtmux".to_string()),
            "Missing mp4mux/qtmux"
        );
        assert!(types.contains(&"filesink".to_string()), "Missing filesink");

        // Check that mp4mux has a name
        let mux = elements
            .iter()
            .find(|e| {
                let factory_name = e.factory().unwrap().name();
                factory_name == "mp4mux" || factory_name == "qtmux"
            })
            .expect("Should have mp4mux or qtmux");

        let mux_name: String = mux.property("name");
        assert_eq!(
            mux_name, "mux",
            "Expected mux to be named 'mux', got '{}'",
            mux_name
        );
    }

    #[test]
    fn test_roundtrip_mux_pipeline() {
        init_gst();

        // Simpler mux test using funnel (can handle raw data) - create funnel first in main chain, then branch to it
        let input = "videotestsrc ! funnel name=f ! fakesink audiotestsrc ! f.";

        // Parse
        let pipeline = gst::parse::launch(input).unwrap();
        let bin = pipeline.downcast::<gst::Bin>().unwrap();

        // Extract elements and verify we have all the pieces
        let elements: Vec<_> = bin.iterate_elements().into_iter().flatten().collect();

        // Should have: videotestsrc, funnel, fakesink, audiotestsrc (4 elements)
        assert!(
            elements.len() >= 4,
            "Expected at least 4 elements, got {}",
            elements.len()
        );

        // Verify funnel is named
        let funnel = elements
            .iter()
            .find(|e| e.factory().unwrap().name() == "funnel")
            .expect("Should have funnel");

        let funnel_name: String = funnel.property("name");
        assert_eq!(funnel_name, "f", "Expected funnel to be named 'f'");
    }
}
