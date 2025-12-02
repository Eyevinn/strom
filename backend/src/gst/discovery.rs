//! GStreamer element discovery and introspection.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::element::{ElementInfo, MediaType};
use tracing::{debug, warn};

/// GStreamer element discovery service.
pub struct ElementDiscovery {
    /// Cached element information
    cache: HashMap<String, ElementInfo>,
}

impl ElementDiscovery {
    /// Create a new element discovery service.
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Discover all available GStreamer elements.
    pub fn discover_all(&mut self) -> Vec<ElementInfo> {
        debug!("Discovering all GStreamer elements...");

        let registry = gst::Registry::get();
        let mut elements = Vec::new();

        // Elements to skip during discovery (but still available for use)
        let skip_list = Self::get_discovery_skip_list();

        let features = registry.features(gst::ElementFactory::static_type());

        for feature in features {
            let Some(factory) = feature.downcast_ref::<gst::ElementFactory>() else {
                continue;
            };

            let name = factory.name().to_string();

            // Skip elements that corrupt state during discovery
            if skip_list.contains(&name.as_str()) {
                debug!(
                    "Skipping discovery introspection for element: {} (still usable)",
                    name
                );
                // Create minimal info without introspection so it's still in the element list
                let description = factory
                    .metadata("long-name")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| name.clone());
                let klass = factory
                    .metadata("klass")
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let category = Self::determine_category(&klass);

                let info = ElementInfo {
                    name: name.clone(),
                    description,
                    category,
                    src_pads: Vec::new(),
                    sink_pads: Vec::new(),
                    properties: Vec::new(),
                };
                self.cache.insert(name, info.clone());
                elements.push(info);
                continue;
            }

            // Skip elements we've already cached
            if self.cache.contains_key(&name) {
                elements.push(self.cache[&name].clone());
                continue;
            }

            debug!("Introspecting element: {}", name);

            // Try to introspect the element
            match self.introspect_element_factory(factory) {
                Ok(info) => {
                    self.cache.insert(name, info.clone());
                    elements.push(info);
                }
                Err(e) => {
                    warn!("Failed to introspect element {}: {}", factory.name(), e);
                }
            }
        }

        debug!("Discovered {} elements", elements.len());
        elements
    }

    /// Get list of elements to skip during discovery introspection.
    /// These elements can still be used, but we don't create temporary instances during discovery.
    fn get_discovery_skip_list() -> Vec<&'static str> {
        vec![
            // GES (GStreamer Editing Services) elements that trigger GES initialization
            // GES init can crash with NULL pointer in gst_element_class_get_pad_template()
            "gesdemux", // GES demuxer - triggers GES init which crashes in strcmp
            "gessrc",   // GES source - triggers GES init
            // HLS elements - crash with NULL pointer in gst_element_class_get_pad_template()
            "hlssink2", // Crashes in strcmp during element creation
            "hlssink3", // HLS sink variants - same crash pattern
            "hlssink",
            "hlsdemux", // HLS demuxer - crashes in strcmp
            "hlsdemux2",
            // Video mixer elements - crash with NULL pointer when requesting pads
            "glvideomixer", // Crashes in strcmp when request_pad_simple() is called during linking
            // Aggregator elements - creating temporary instances during discovery corrupts state
            "mpegtsmux", // Creating this during discovery causes lockups when adding to pipeline later
        ]
    }

    /// Get list of elements known to cause crashes during introspection or use.
    /// This list is shared with pipeline creation to prevent creating these elements.
    pub fn get_element_blacklist() -> Vec<&'static str> {
        vec![
            // GES (GStreamer Editing Services) elements that trigger GES initialization
            // GES init can crash with NULL pointer in gst_element_class_get_pad_template()
            "gesdemux", // GES demuxer - triggers GES init which crashes in strcmp
            "gessrc",   // GES source - triggers GES init
            // HLS elements - crash with NULL pointer in gst_element_class_get_pad_template()
            "hlssink2", // Crashes in strcmp during element creation
            "hlssink3", // HLS sink variants - same crash pattern
            "hlssink",
            "hlsdemux", // HLS demuxer - crashes in strcmp
            "hlsdemux2",
            // Video mixer elements - crash with NULL pointer when requesting pads
            "glvideomixer", // Crashes in strcmp when request_pad_simple() is called during linking
        ]
    }

    /// Get information about a specific element by name.
    pub fn get_element_info(&mut self, name: &str) -> Option<ElementInfo> {
        // Check cache first
        if let Some(info) = self.cache.get(name) {
            return Some(info.clone());
        }

        // Try to find and introspect the element
        let registry = gst::Registry::get();
        let factory = registry.find_feature(name, gst::ElementFactory::static_type())?;
        let Some(factory) = factory.downcast_ref::<gst::ElementFactory>() else {
            warn!("Feature '{}' is not an ElementFactory", name);
            return None;
        };

        match self.introspect_element_factory(factory) {
            Ok(info) => {
                self.cache.insert(name.to_string(), info.clone());
                Some(info)
            }
            Err(e) => {
                warn!("Failed to introspect element {}: {}", name, e);
                None
            }
        }
    }

    /// Load properties for a specific element (lazy loading).
    /// Returns the element info with properties populated.
    /// If properties are already cached, returns cached version.
    /// If element has no cached properties, introspects and updates cache.
    pub fn load_element_properties(&mut self, name: &str) -> Option<ElementInfo> {
        // Check if we have this element cached
        if let Some(cached_info) = self.cache.get(name) {
            // If properties are already populated, return cached version
            if !cached_info.properties.is_empty() {
                debug!("Returning cached properties for {}", name);
                return Some(cached_info.clone());
            }
        }

        // Need to load properties
        debug!("Loading properties for element: {}", name);

        let registry = gst::Registry::get();
        let factory = registry.find_feature(name, gst::ElementFactory::static_type())?;
        let Some(factory) = factory.downcast_ref::<gst::ElementFactory>() else {
            warn!("Feature '{}' is not an ElementFactory", name);
            return None;
        };

        // Introspect properties
        match self.introspect_element_properties_lazy(factory) {
            Ok(properties) => {
                // Update cache with properties
                if let Some(cached_info) = self.cache.get_mut(name) {
                    cached_info.properties = properties;
                    Some(cached_info.clone())
                } else {
                    // Element not in cache yet, do full introspection
                    match self.introspect_element_factory(factory) {
                        Ok(mut info) => {
                            // Override with lazy-loaded properties
                            info.properties = properties;
                            self.cache.insert(name.to_string(), info.clone());
                            Some(info)
                        }
                        Err(e) => {
                            warn!("Failed to introspect element {}: {}", name, e);
                            None
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Failed to introspect properties for {}: {}", name, e);
                // Return element info without properties
                self.cache.get(name).cloned()
            }
        }
    }

    /// Load pad properties for a specific element (on-demand introspection).
    /// This introspects Request pad properties safely for a single element.
    /// Unlike bulk discovery which skips Request pads to avoid crashes,
    /// this can safely request pads for a specific element with error handling.
    pub fn load_element_pad_properties(&mut self, name: &str) -> Option<ElementInfo> {
        debug!("Loading pad properties for element: {}", name);

        // Get basic element info from cache or discover it
        let mut element_info = if let Some(info) = self.cache.get(name) {
            info.clone()
        } else {
            // Discover element first
            self.get_element_info(name)?
        };

        // Try to create element and introspect pad properties
        let registry = gst::Registry::get();
        let factory = registry.find_feature(name, gst::ElementFactory::static_type())?;
        let Some(factory) = factory.downcast_ref::<gst::ElementFactory>() else {
            warn!("Feature '{}' is not an ElementFactory", name);
            return None;
        };

        // Create temporary element with error handling
        let element = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory.create().build()
        })) {
            Ok(Ok(elem)) => elem,
            Ok(Err(e)) => {
                warn!("Failed to create element {}: {}", name, e);
                return Some(element_info); // Return info without pad properties
            }
            Err(_) => {
                warn!("Element {} creation caused a panic", name);
                return Some(element_info); // Return info without pad properties
            }
        };

        // Introspect pad properties for each pad template
        for pad_template in factory.static_pad_templates() {
            let template_name = pad_template.name_template().to_string();

            // Get pad to introspect (safely request pads if needed)
            let pad = match pad_template.presence() {
                gst::PadPresence::Always => element.static_pad(&template_name),
                gst::PadPresence::Request => {
                    // For Request pads, safely try to request one
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        element.request_pad_simple(&template_name)
                    })) {
                        Ok(Some(p)) => Some(p),
                        Ok(None) => {
                            debug!("Could not request pad {} for {}", template_name, name);
                            None
                        }
                        Err(_) => {
                            warn!(
                                "Requesting pad {} for {} caused a panic",
                                template_name, name
                            );
                            None
                        }
                    }
                }
                gst::PadPresence::Sometimes => {
                    // Try to find an existing Sometimes pad
                    element.pads().iter().find_map(|p| {
                        if p.name().starts_with(&template_name) {
                            Some(p.clone())
                        } else {
                            None
                        }
                    })
                }
            };

            // Introspect pad properties if we got a pad
            if let Some(pad) = pad {
                let properties = self.introspect_pad_properties(&pad);

                // Update the appropriate pad info with properties
                match pad_template.direction() {
                    gst::PadDirection::Src => {
                        if let Some(pad_info) = element_info
                            .src_pads
                            .iter_mut()
                            .find(|p| p.name == template_name)
                        {
                            pad_info.properties = properties;
                        }
                    }
                    gst::PadDirection::Sink => {
                        if let Some(pad_info) = element_info
                            .sink_pads
                            .iter_mut()
                            .find(|p| p.name == template_name)
                        {
                            pad_info.properties = properties;
                        }
                    }
                    _ => {}
                }
            }
        }

        Some(element_info)
    }

    /// Introspect properties from a specific pad.
    fn introspect_pad_properties(&self, pad: &gst::Pad) -> Vec<strom_types::element::PropertyInfo> {
        use strom_types::element::{PropertyInfo, PropertyType, PropertyValue};

        let mut properties = Vec::new();

        // Get all properties from the pad
        for pspec in pad.list_properties() {
            let name = pspec.name().to_string();
            let description = pspec.blurb().map(|s| s.to_string()).unwrap_or_default();

            // Skip internal/private properties
            if name.starts_with("_") {
                continue;
            }

            // Skip pad name property (cannot be changed)
            if name == "name" {
                continue;
            }

            // Skip write-only properties (not readable)
            if !pspec.flags().contains(glib::ParamFlags::READABLE) {
                continue;
            }

            // Extract property flags
            let flags = pspec.flags();
            let construct_only = flags.contains(glib::ParamFlags::CONSTRUCT_ONLY);
            // In the UI, we set properties during element construction, so both
            // WRITABLE and CONSTRUCT_ONLY properties should be editable
            let writable = flags.contains(glib::ParamFlags::WRITABLE) || construct_only;

            // GStreamer-specific flags
            let flags_bits = flags.bits();
            let mutable_in_ready = (flags_bits & 0x400) != 0;
            let mutable_in_paused = (flags_bits & 0x800) != 0;
            let mutable_in_playing = (flags_bits & 0x1000) != 0;
            let controllable = (flags_bits & 0x200) != 0;
            let mutable_in_null = !construct_only;

            // Determine property type and get default value
            let type_name = pspec.value_type().name();

            // Check for enum first (before string matching) since enum types have specific names like "GstAudioTestSrcWave"
            let (property_type, default_value) = if let Some(param_spec) =
                pspec.downcast_ref::<glib::ParamSpecEnum>()
            {
                // Enum property - handle before string matching
                let enum_class = param_spec.enum_class();
                let values: Vec<String> = enum_class
                    .values()
                    .iter()
                    .map(|v| v.name().to_string())
                    .collect();

                // Skip default value for pad enums - some types like GstPadDirection can't be read as i32
                // The values list is the important part for UI dropdowns anyway
                let default = None;

                (PropertyType::Enum { values }, default)
            } else {
                match type_name {
                    "gchararray" => {
                        let default =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                pad.property::<Option<String>>(&name)
                            }))
                            .ok()
                            .flatten()
                            .map(PropertyValue::String);
                        (PropertyType::String, default)
                    }
                    "gboolean" => {
                        let default =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                pad.property::<bool>(&name)
                            }))
                            .ok()
                            .map(PropertyValue::Bool);
                        (PropertyType::Bool, default)
                    }
                    "gint" | "glong" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecInt>() {
                            let min = param_spec.minimum() as i64;
                            let max = param_spec.maximum() as i64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<i32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::Int(v as i64));
                            (PropertyType::Int { min, max }, default)
                        } else if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecLong>()
                        {
                            // Cast glong to i64 for cross-platform compatibility
                            // glong is i32 on Windows but i64 on Linux
                            #[allow(clippy::unnecessary_cast)]
                            let min = param_spec.minimum() as i64;
                            #[allow(clippy::unnecessary_cast)]
                            let max = param_spec.maximum() as i64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<i64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Int);
                            (PropertyType::Int { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "guint" | "gulong" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecUInt>() {
                            let min = param_spec.minimum() as u64;
                            let max = param_spec.maximum() as u64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<u32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::UInt(v as u64));
                            (PropertyType::UInt { min, max }, default)
                        } else if let Some(param_spec) =
                            pspec.downcast_ref::<glib::ParamSpecULong>()
                        {
                            // Cast gulong to u64 for cross-platform compatibility
                            // gulong is u32 on Windows but u64 on Linux
                            #[allow(clippy::unnecessary_cast)]
                            let min = param_spec.minimum() as u64;
                            #[allow(clippy::unnecessary_cast)]
                            let max = param_spec.maximum() as u64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<u64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::UInt);
                            (PropertyType::UInt { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gint64" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecInt64>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<i64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Int);
                            (PropertyType::Int { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "guint64" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecUInt64>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<u64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::UInt);
                            (PropertyType::UInt { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gfloat" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecFloat>() {
                            let min = param_spec.minimum() as f64;
                            let max = param_spec.maximum() as f64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<f32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::Float(v as f64));
                            (PropertyType::Float { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gdouble" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecDouble>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<f64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Float);
                            (PropertyType::Float { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    _ => {
                        // Skip unsupported property types
                        continue;
                    }
                }
            };

            properties.push(PropertyInfo {
                name,
                description,
                property_type,
                default_value,
                writable,
                construct_only,
                mutable_in_null,
                mutable_in_ready,
                mutable_in_paused,
                mutable_in_playing,
                controllable,
            });
        }

        properties
    }

    /// Introspect a GStreamer element factory.
    /// LIGHTWEIGHT VERSION: Only gets name, description, and category.
    /// Does NOT create element instances or introspect pads/properties during discover_all().
    fn introspect_element_factory(
        &self,
        factory: &gst::ElementFactory,
    ) -> anyhow::Result<ElementInfo> {
        let name = factory.name().to_string();
        let description = factory
            .metadata("long-name")
            .map(|s| s.to_string())
            .unwrap_or_else(|| name.clone());

        // Determine category
        let klass = factory
            .metadata("klass")
            .map(|s| s.to_string())
            .unwrap_or_default();
        let category = Self::determine_category(&klass);

        // IMPORTANT: We do NOT create element instances during discover_all()!
        // Creating thousands of elements is:
        // 1. Very slow (adds seconds to startup)
        // 2. Dangerous - some elements crash when created
        // 3. State-corrupting - elements like mpegtsmux corrupt global state
        //
        // Instead, we only return basic info (name, description, category).
        // Pads and properties can be introspected on-demand when needed.

        Ok(ElementInfo {
            name,
            description,
            category,
            src_pads: Vec::new(),   // No pad introspection during discovery
            sink_pads: Vec::new(),  // No pad introspection during discovery
            properties: Vec::new(), // No property introspection during discovery
        })
    }

    /// Determine element category from its klass.
    fn determine_category(klass: &str) -> String {
        if klass.contains("Source") {
            "Source".to_string()
        } else if klass.contains("Sink") {
            "Sink".to_string()
        } else if klass.contains("Codec") || klass.contains("Encoder") || klass.contains("Decoder")
        {
            "Codec".to_string()
        } else if klass.contains("Filter") || klass.contains("Effect") {
            "Filter".to_string()
        } else if klass.contains("Converter") {
            "Converter".to_string()
        } else if klass.contains("Muxer") {
            "Muxer".to_string()
        } else if klass.contains("Demuxer") || klass.contains("Parser") {
            "Demuxer".to_string()
        } else if klass.contains("Network") {
            "Network".to_string()
        } else {
            "Other".to_string()
        }
    }

    /// Classify media type from caps string.
    /// Currently unused during discover_all() to avoid calling caps.to_string().
    /// Kept for future use when implementing lazy caps loading.
    #[allow(dead_code)]
    fn classify_media_type(caps: &str) -> MediaType {
        let caps_lower = caps.to_lowercase();

        // Check for audio patterns
        let is_audio = caps_lower.contains("audio/") || caps_lower.contains("audio,");

        // Check for video patterns
        let is_video = caps_lower.contains("video/")
            || caps_lower.contains("video,")
            || caps_lower.contains("image/");

        // Classify based on what we found
        match (is_audio, is_video) {
            (true, true) => MediaType::Generic, // Both audio and video = generic/muxed
            (true, false) => MediaType::Audio,  // Audio only
            (false, true) => MediaType::Video,  // Video only
            (false, false) => MediaType::Generic, // Unknown or ANY caps = generic
        }
    }

    /// Clear the cache (useful for testing or forcing refresh).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Introspect pad template properties by getting or creating a pad.
    #[allow(dead_code)]
    fn introspect_pad_template_properties(
        &self,
        element: &gst::Element,
        pad_template: &gst::PadTemplate,
    ) -> Vec<strom_types::element::PropertyInfo> {
        use strom_types::element::{PropertyInfo, PropertyType, PropertyValue};

        // Try to get an existing pad matching this template
        // Wrap in catch_unwind to prevent crashes from problematic elements
        let pad = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match pad_template.presence() {
                gst::PadPresence::Always => {
                    // For Always pads, get the static pad
                    element.static_pad(pad_template.name_template())
                }
                gst::PadPresence::Request => {
                    // For Request pads, we can't safely introspect properties during discovery
                    // because many elements crash when requesting pads at this stage.
                    // Request pad properties (like volume/mute on audiomixer pads) need to be
                    // documented separately or introspected when the element is actually in use.
                    None
                }
                gst::PadPresence::Sometimes => {
                    // For Sometimes pads, they might not exist yet
                    // Try to get one if it exists, otherwise skip
                    element.pads().iter().find_map(|p| {
                        if p.pad_template().as_ref() == Some(pad_template) {
                            Some(p.clone())
                        } else {
                            None
                        }
                    })
                }
            }
        }))
        .ok()
        .flatten();

        let Some(pad) = pad else {
            return Vec::new();
        };

        let mut properties = Vec::new();

        // Get all properties from the pad
        for pspec in pad.list_properties() {
            let name = pspec.name().to_string();
            let description = pspec.blurb().map(|s| s.to_string()).unwrap_or_default();

            // Skip internal/private properties
            if name.starts_with("_") {
                continue;
            }

            // Skip pad name property (cannot be changed)
            if name == "name" {
                continue;
            }

            // Skip write-only properties (not readable)
            if !pspec.flags().contains(glib::ParamFlags::READABLE) {
                continue;
            }

            // Extract property flags
            let flags = pspec.flags();
            let construct_only = flags.contains(glib::ParamFlags::CONSTRUCT_ONLY);
            // In the UI, we set properties during element construction, so both
            // WRITABLE and CONSTRUCT_ONLY properties should be editable
            let writable = flags.contains(glib::ParamFlags::WRITABLE) || construct_only;

            // GStreamer-specific flags
            let flags_bits = flags.bits();
            let mutable_in_ready = (flags_bits & 0x400) != 0;
            let mutable_in_paused = (flags_bits & 0x800) != 0;
            let mutable_in_playing = (flags_bits & 0x1000) != 0;
            let controllable = (flags_bits & 0x200) != 0;
            let mutable_in_null = !construct_only;

            // Determine property type and get default value
            let type_name = pspec.value_type().name();

            // Check for enum first (before string matching) since enum types have specific names like "GstAudioTestSrcWave"
            let (property_type, default_value) = if let Some(param_spec) =
                pspec.downcast_ref::<glib::ParamSpecEnum>()
            {
                // Enum property - handle before string matching
                let enum_class = param_spec.enum_class();
                let values: Vec<String> = enum_class
                    .values()
                    .iter()
                    .map(|v| v.name().to_string())
                    .collect();

                // Skip default value for pad enums - some types like GstPadDirection can't be read as i32
                // The values list is the important part for UI dropdowns anyway
                let default = None;

                (PropertyType::Enum { values }, default)
            } else {
                match type_name {
                    "gchararray" => {
                        let default =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                pad.property::<Option<String>>(&name)
                            }))
                            .ok()
                            .flatten()
                            .map(PropertyValue::String);
                        (PropertyType::String, default)
                    }
                    "gboolean" => {
                        let default =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                pad.property::<bool>(&name)
                            }))
                            .ok()
                            .map(PropertyValue::Bool);
                        (PropertyType::Bool, default)
                    }
                    "gint" | "glong" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecInt>() {
                            let min = param_spec.minimum() as i64;
                            let max = param_spec.maximum() as i64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<i32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::Int(v as i64));
                            (PropertyType::Int { min, max }, default)
                        } else if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecLong>()
                        {
                            // Cast glong to i64 for cross-platform compatibility
                            // glong is i32 on Windows but i64 on Linux
                            #[allow(clippy::unnecessary_cast)]
                            let min = param_spec.minimum() as i64;
                            #[allow(clippy::unnecessary_cast)]
                            let max = param_spec.maximum() as i64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<i64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Int);
                            (PropertyType::Int { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "guint" | "gulong" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecUInt>() {
                            let min = param_spec.minimum() as u64;
                            let max = param_spec.maximum() as u64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<u32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::UInt(v as u64));
                            (PropertyType::UInt { min, max }, default)
                        } else if let Some(param_spec) =
                            pspec.downcast_ref::<glib::ParamSpecULong>()
                        {
                            // Cast gulong to u64 for cross-platform compatibility
                            // gulong is u32 on Windows but u64 on Linux
                            #[allow(clippy::unnecessary_cast)]
                            let min = param_spec.minimum() as u64;
                            #[allow(clippy::unnecessary_cast)]
                            let max = param_spec.maximum() as u64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<u64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::UInt);
                            (PropertyType::UInt { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gint64" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecInt64>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<i64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Int);
                            (PropertyType::Int { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "guint64" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecUInt64>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<u64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::UInt);
                            (PropertyType::UInt { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gfloat" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecFloat>() {
                            let min = param_spec.minimum() as f64;
                            let max = param_spec.maximum() as f64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<f32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::Float(v as f64));
                            (PropertyType::Float { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gdouble" => {
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecDouble>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    pad.property::<f64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Float);
                            (PropertyType::Float { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    _ => {
                        // Skip unsupported property types
                        continue;
                    }
                }
            };

            properties.push(PropertyInfo {
                name,
                description,
                property_type,
                default_value,
                writable,
                construct_only,
                mutable_in_null,
                mutable_in_ready,
                mutable_in_paused,
                mutable_in_playing,
                controllable,
            });
        }

        properties
    }

    /// Introspect element properties from a factory.
    /// During startup discovery, this returns empty to avoid crashes.
    /// Use introspect_element_properties_lazy() for on-demand property loading.
    #[allow(dead_code)]
    fn introspect_properties(
        &self,
        factory: &gst::ElementFactory,
    ) -> anyhow::Result<Vec<strom_types::element::PropertyInfo>> {
        debug!(
            "Skipping property introspection for {} during startup discovery",
            factory.name()
        );
        // Return empty properties - they will be loaded on-demand when needed
        Ok(Vec::new())
    }

    /// Introspect element properties on-demand (lazy loading).
    /// This is called when frontend requests properties for a specific element.
    pub fn introspect_element_properties_lazy(
        &self,
        factory: &gst::ElementFactory,
    ) -> anyhow::Result<Vec<strom_types::element::PropertyInfo>> {
        use strom_types::element::{PropertyInfo, PropertyType, PropertyValue};

        debug!("Lazy-loading properties for {}", factory.name());

        // Create a temporary element instance to introspect properties
        // Wrap in catch_unwind to prevent crashes from problematic elements
        let element = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory.create().build()
        })) {
            Ok(Ok(elem)) => elem,
            Ok(Err(e)) => {
                debug!("Failed to create element {}: {}", factory.name(), e);
                return Ok(Vec::new()); // Return empty properties instead of error
            }
            Err(_) => {
                debug!("Element {} creation caused a panic", factory.name());
                return Ok(Vec::new()); // Return empty properties instead of error
            }
        };

        let mut properties = Vec::new();

        // Get all properties from the element
        for pspec in element.list_properties() {
            let name = pspec.name().to_string();
            let description = pspec.blurb().map(|s| s.to_string()).unwrap_or_default();

            // Debug: log every property we see
            let type_name = pspec.value_type().name();
            debug!("Processing property '{}' (type: {})", name, type_name);

            // Skip internal/private properties
            if name.starts_with("_") {
                debug!("Skipping internal property: {}", name);
                continue;
            }

            // Skip write-only properties (not readable)
            if !pspec.flags().contains(glib::ParamFlags::READABLE) {
                debug!("Skipping write-only property: {}", name);
                continue;
            }

            // Extract property flags for mutability information
            let flags = pspec.flags();
            let construct_only = flags.contains(glib::ParamFlags::CONSTRUCT_ONLY);
            let has_writable_flag = flags.contains(glib::ParamFlags::WRITABLE);
            // In the UI, we set properties during element construction, so both
            // WRITABLE and CONSTRUCT_ONLY properties should be editable
            let writable = has_writable_flag || construct_only;

            // Log details for troubleshooting
            if name == "location" || factory.name() == "souphttpsrc" {
                debug!(
                    "Property '{}' on {}: has_writable={}, construct_only={}, writable={}",
                    name,
                    factory.name(),
                    has_writable_flag,
                    construct_only,
                    writable
                );
            }

            // GStreamer-specific flags (from gstreamer-sys)
            // GST_PARAM_MUTABLE_READY = 1 << (G_PARAM_USER_SHIFT + 2) = 1 << 10 = 0x400
            // GST_PARAM_MUTABLE_PAUSED = 1 << (G_PARAM_USER_SHIFT + 3) = 1 << 11 = 0x800
            // GST_PARAM_MUTABLE_PLAYING = 1 << (G_PARAM_USER_SHIFT + 4) = 1 << 12 = 0x1000
            // GST_PARAM_CONTROLLABLE = 1 << (G_PARAM_USER_SHIFT + 1) = 1 << 9 = 0x200
            let flags_bits = flags.bits();
            let mutable_in_ready = (flags_bits & 0x400) != 0;
            let mutable_in_paused = (flags_bits & 0x800) != 0;
            let mutable_in_playing = (flags_bits & 0x1000) != 0;
            let controllable = (flags_bits & 0x200) != 0;

            // All properties are mutable in NULL state (before construction)
            // unless they're construct-only
            let mutable_in_null = !construct_only;

            // Determine property type and get default value
            let type_name = pspec.value_type().name();

            // Check for enum first (before string matching) since enum types have specific names like "GstAudioTestSrcWave"
            let (property_type, default_value) = if let Some(param_spec) =
                pspec.downcast_ref::<glib::ParamSpecEnum>()
            {
                // Enum property - handle before string matching
                let enum_class = param_spec.enum_class();
                let values: Vec<String> = enum_class
                    .values()
                    .iter()
                    .map(|v| v.name().to_string())
                    .collect();

                // Skip default value for enums - some types like GstAggregatorStartTimeSelection
                // can't be read as i32 in GStreamer 0.24.x without panicking.
                // The values list is the important part for UI dropdowns anyway.
                let default = None;

                (PropertyType::Enum { values }, default)
            } else {
                match type_name {
                    "gchararray" => {
                        // String property - use catch_unwind to handle potential panics
                        let default =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                element.property::<Option<String>>(&name)
                            }))
                            .ok()
                            .flatten()
                            .map(PropertyValue::String);
                        (PropertyType::String, default)
                    }
                    "gboolean" => {
                        // Boolean property
                        let default =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                element.property::<bool>(&name)
                            }))
                            .ok()
                            .map(PropertyValue::Bool);
                        (PropertyType::Bool, default)
                    }
                    "gint" | "glong" => {
                        // Signed integer property
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecInt>() {
                            let min = param_spec.minimum() as i64;
                            let max = param_spec.maximum() as i64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<i32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::Int(v as i64));
                            (PropertyType::Int { min, max }, default)
                        } else if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecLong>()
                        {
                            // Cast glong to i64 for cross-platform compatibility
                            // glong is i32 on Windows but i64 on Linux
                            #[allow(clippy::unnecessary_cast)]
                            let min = param_spec.minimum() as i64;
                            #[allow(clippy::unnecessary_cast)]
                            let max = param_spec.maximum() as i64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<i64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Int);
                            (PropertyType::Int { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "guint" | "gulong" => {
                        // Unsigned integer property
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecUInt>() {
                            let min = param_spec.minimum() as u64;
                            let max = param_spec.maximum() as u64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<u32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::UInt(v as u64));
                            (PropertyType::UInt { min, max }, default)
                        } else if let Some(param_spec) =
                            pspec.downcast_ref::<glib::ParamSpecULong>()
                        {
                            // Cast gulong to u64 for cross-platform compatibility
                            // gulong is u32 on Windows but u64 on Linux
                            #[allow(clippy::unnecessary_cast)]
                            let min = param_spec.minimum() as u64;
                            #[allow(clippy::unnecessary_cast)]
                            let max = param_spec.maximum() as u64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<u64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::UInt);
                            (PropertyType::UInt { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gint64" => {
                        // 64-bit signed integer
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecInt64>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<i64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Int);
                            (PropertyType::Int { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "guint64" => {
                        // 64-bit unsigned integer
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecUInt64>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<u64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::UInt);
                            (PropertyType::UInt { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gfloat" => {
                        // Float property
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecFloat>() {
                            let min = param_spec.minimum() as f64;
                            let max = param_spec.maximum() as f64;
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<f32>(&name)
                                }))
                                .ok()
                                .map(|v| PropertyValue::Float(v as f64));
                            (PropertyType::Float { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "gdouble" => {
                        // Double property
                        if let Some(param_spec) = pspec.downcast_ref::<glib::ParamSpecDouble>() {
                            let min = param_spec.minimum();
                            let max = param_spec.maximum();
                            let default =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    element.property::<f64>(&name)
                                }))
                                .ok()
                                .map(PropertyValue::Float);
                            (PropertyType::Float { min, max }, default)
                        } else {
                            continue;
                        }
                    }
                    "GstCaps" => {
                        // GstCaps property - convert to/from string representation
                        // Used by capsfilter and other elements that manipulate caps
                        let default =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                element.property::<Option<gst::Caps>>(&name)
                            }))
                            .ok()
                            .flatten()
                            .map(|caps| PropertyValue::String(caps.to_string()));
                        (PropertyType::String, default)
                    }
                    _ => {
                        // Skip unsupported property types
                        debug!(
                            "Skipping unsupported property type: {} ({})",
                            name, type_name
                        );
                        continue;
                    }
                }
            };

            properties.push(PropertyInfo {
                name,
                description,
                property_type,
                default_value,
                writable,
                construct_only,
                mutable_in_null,
                mutable_in_ready,
                mutable_in_paused,
                mutable_in_playing,
                controllable,
            });
        }

        Ok(properties)
    }
}

impl Default for ElementDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_element_discovery() {
        gst::init().unwrap();
        let mut discovery = ElementDiscovery::new();
        let elements = discovery.discover_all();
        assert!(!elements.is_empty(), "Should discover some elements");
    }

    #[test]
    fn test_get_specific_element() {
        gst::init().unwrap();
        let mut discovery = ElementDiscovery::new();

        // Try to get a common element
        let info = discovery.get_element_info("fakesrc");
        assert!(info.is_some(), "Should find fakesrc element");

        if let Some(info) = info {
            assert_eq!(info.name, "fakesrc");
        }
    }
}
