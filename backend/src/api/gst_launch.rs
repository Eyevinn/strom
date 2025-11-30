//! gst-launch-1.0 parsing and export API handlers.

use axum::{extract::State, http::StatusCode, Json};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    api::{
        ErrorResponse, ExportGstLaunchRequest, ExportGstLaunchResponse, ParseGstLaunchRequest,
        ParseGstLaunchResponse,
    },
    element::{Element, Link, PropertyValue},
};
use tracing::{debug, info, warn};

use crate::state::AppState;

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

    // Parse the pipeline using GStreamer's native parser
    let pipeline = match gst::parse::launch(&req.pipeline) {
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
    let bin = match pipeline.downcast::<gst::Bin>() {
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
    let mut seen_links: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    // Re-iterate to find links (need to get elements from bin again)
    for gst_elem in bin.iterate_elements().into_iter().flatten() {
        let gst_name = gst_elem.name().to_string();
        let Some(our_id) = element_id_map.get(&gst_name) else {
            continue;
        };

        // Check source pads for outgoing links
        for pad in gst_elem.src_pads() {
            if let Some(peer) = pad.peer() {
                if let Some(peer_elem) = peer.parent_element() {
                    let peer_gst_name = peer_elem.name().to_string();
                    if let Some(peer_our_id) = element_id_map.get(&peer_gst_name) {
                        let link_key = (our_id.clone(), peer_our_id.clone());
                        if !seen_links.contains(&link_key) {
                            seen_links.insert(link_key);

                            // Determine if we need to specify pad names
                            let from = if needs_pad_specification(&gst_elem, &pad) {
                                format!("{}:{}", our_id, pad.name())
                            } else {
                                our_id.clone()
                            };

                            let to = if needs_pad_specification(&peer_elem, &peer) {
                                format!("{}:{}", peer_our_id, peer.name())
                            } else {
                                peer_our_id.clone()
                            };

                            links.push(Link { from, to });
                        }
                    }
                }
            }
        }
    }

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

/// Extract properties that differ from their default values.
fn extract_non_default_properties(gst_elem: &gst::Element) -> HashMap<String, PropertyValue> {
    let mut properties = HashMap::new();

    // Get the element's class properties
    let obj_class = gst_elem.class();

    for pspec in obj_class.list_properties() {
        let prop_name = pspec.name();

        // Skip read-only and construct-only properties
        if !pspec.flags().contains(gstreamer::glib::ParamFlags::WRITABLE) {
            continue;
        }

        // Skip the "name" property - we handle it separately
        if prop_name == "name" || prop_name == "parent" {
            continue;
        }

        // Try to get the current value
        let current_value = match gst_elem.property_value(prop_name) {
            value => value,
        };

        // Try to get the default value
        let default_value = pspec.default_value();

        // Compare and only include if different from default
        if !values_equal(&current_value, &default_value) {
            if let Some(prop_value) = gvalue_to_property_value(&current_value) {
                debug!(
                    "Property {} differs from default: {:?}",
                    prop_name, prop_value
                );
                properties.insert(prop_name.to_string(), prop_value);
            }
        }
    }

    properties
}

/// Check if two GValues are equal.
fn values_equal(a: &gstreamer::glib::Value, b: &gstreamer::glib::Value) -> bool {
    // Try to compare based on type
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

    // For enums and flags, compare the underlying integer
    if a.type_().is_a(gstreamer::glib::Type::ENUM) {
        // Try to get as i32 (common for enums)
        if let (Ok(av), Ok(bv)) = (a.get::<i32>(), b.get::<i32>()) {
            return av == bv;
        }
    }

    // For unknown types, assume they're different to be safe
    false
}

/// Convert a GValue to our PropertyValue type.
fn gvalue_to_property_value(value: &gstreamer::glib::Value) -> Option<PropertyValue> {
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

    // For enums, try to get the nick (string representation)
    if value.type_().is_a(gstreamer::glib::Type::ENUM) {
        if let Ok(v) = value.get::<i32>() {
            // Return as integer - the frontend can resolve the enum nick if needed
            return Some(PropertyValue::Int(v as i64));
        }
    }

    None
}

/// Check if a pad needs explicit specification (has multiple pads of same direction).
fn needs_pad_specification(elem: &gst::Element, pad: &gst::Pad) -> bool {
    let direction = pad.direction();
    let pads: Vec<_> = elem
        .pads()
        .into_iter()
        .filter(|p| p.direction() == direction)
        .collect();

    // Need specification if there's more than one pad of the same direction
    pads.len() > 1
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
    let element_map: HashMap<&str, &Element> = elements.iter().map(|e| (e.id.as_str(), e)).collect();

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
            result.push_str(&format_element(current, needs_name.contains(current.id.as_str())));

            // Get outgoing connections
            let targets = outgoing.get(current.id.as_str());

            match targets {
                None | Some(targets) if targets.is_empty() => {
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
                    None | Some(targets) if targets.is_empty() => break,
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
