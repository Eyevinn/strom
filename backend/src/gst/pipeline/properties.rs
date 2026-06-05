use super::{PipelineError, PipelineManager};
use crate::gst::volume_ramp::VolumeRampManager;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::mixer::{DEFAULT_VOLUME_RAMP_MS, MUTE_ANTICLICK_RAMP_MS};
use strom_types::{PipelineState, PropertyValue};
use tracing::{debug, info};

impl PipelineManager {
    /// Set a property on an element.
    ///
    /// `ramp_ms` is consulted only for routes that support smooth interpolation
    /// (currently audio `volume`-element `volume` and `mute`). Other properties
    /// are set immediately regardless. `None` selects the per-route default
    /// ramp (short anti-zipper for `volume`, short anti-click for `mute`); a
    /// caller can request a longer broadcast-style fade by passing an explicit
    /// duration (e.g. 500 ms for a route mute on-air/off-air).
    pub(super) fn set_property(
        &self,
        element: &gst::Element,
        element_id: &str,
        prop_name: &str,
        prop_value: &PropertyValue,
        ramp_ms: Option<u32>,
    ) -> Result<(), PipelineError> {
        debug!(
            "Setting property: {}.{} = {:?} (ramp_ms={:?})",
            element_id, prop_name, prop_value, ramp_ms
        );

        // Audio volume element: route through the ramp manager to avoid
        // zipper noise (volume) and click artifacts (mute). The short-circuit
        // installs a control source / schedules the mute toggle and returns;
        // on failure (e.g. pipeline not yet running) we fall through to the
        // direct `set_property` path below. Volume accepts any numeric
        // PropertyValue (Float/Int/UInt) since clients sometimes send an
        // integer 0/1 — the underlying property is `gdouble`.
        if VolumeRampManager::is_volume_element(element) {
            if prop_name == "volume" {
                let target: Option<f64> = match prop_value {
                    PropertyValue::Float(v) => Some(*v),
                    PropertyValue::Int(v) => Some(*v as f64),
                    PropertyValue::UInt(v) => Some(*v as f64),
                    _ => None,
                };
                if let Some(target) = target {
                    if self.volume_ramps.apply_volume_ramp(
                        element,
                        element_id,
                        target,
                        ramp_ms.unwrap_or(DEFAULT_VOLUME_RAMP_MS),
                    ) {
                        return Ok(());
                    }
                }
            } else if prop_name == "mute" {
                if let PropertyValue::Bool(v) = prop_value {
                    if self.volume_ramps.apply_mute(
                        element,
                        element_id,
                        *v,
                        ramp_ms.unwrap_or(MUTE_ANTICLICK_RAMP_MS),
                    ) {
                        return Ok(());
                    }
                }
            }
        }

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
                    } else if type_name == "gdouble" {
                        // Property expects f64 — coerce (e.g. volume sent as int)
                        element.set_property(prop_name, *v as f64);
                    } else if type_name == "gfloat" {
                        // Property expects f32 — coerce
                        element.set_property(prop_name, *v as f32);
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
                    } else if type_name == "gdouble" {
                        // Property expects f64 — coerce
                        element.set_property(prop_name, *v as f64);
                    } else if type_name == "gfloat" {
                        // Property expects f32 — coerce
                        element.set_property(prop_name, *v as f32);
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
                // Check property type to determine if we need f32 or f64
                if let Some(pspec) = element.find_property(prop_name) {
                    let type_name = pspec.value_type().name();
                    if type_name == "gfloat" {
                        // Property expects f32
                        element.set_property(prop_name, *v as f32);
                    } else {
                        // Property expects f64 (gdouble) or unknown, use f64
                        element.set_property(prop_name, *v);
                    }
                } else {
                    // Property not found, try anyway with f64
                    element.set_property(prop_name, *v);
                }
            }
            PropertyValue::Bool(v) => {
                element.set_property(prop_name, *v);
            }
        }

        Ok(())
    }

    /// Update a property on a live element in the pipeline.
    /// Validates that the property can be changed in the current pipeline state.
    ///
    /// `ramp_ms` is consulted only for routes that support smooth interpolation
    /// (currently audio `volume`-element `volume` and `mute`). For other
    /// properties it is silently ignored.
    pub fn update_element_property(
        &self,
        element_id: &str,
        property_name: &str,
        value: &PropertyValue,
        ramp_ms: Option<u32>,
    ) -> Result<(), PipelineError> {
        debug!(
            "Updating property {}.{} to {:?} on running pipeline (ramp_ms={:?})",
            element_id, property_name, value, ramp_ms
        );

        // Get element reference
        let element = self
            .elements
            .get(element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.to_string()))?;

        // Get current pipeline state
        let state = self.get_state();

        // Time Offset block: `offset_ms` is not an element property but a
        // pad-offset. Intercept and apply directly, then trigger a latency
        // recalc so downstream sinks resync. The interceptor logs the new
        // value at debug; we don't double-log here.
        if crate::blocks::builtin::time_offset::try_apply_live_offset(
            element,
            element_id,
            property_name,
            value,
        ) {
            let _ = self.pipeline.recalculate_latency();
            return Ok(());
        }

        // Translate property name/value for elements that need conversion.
        // Mixer lsp-rs elements use different property names than LV2 conventions.
        // AudioGain stores gain in dB but GStreamer volume element expects linear.
        let mut translations = crate::blocks::builtin::mixer::translate_property_for_element(
            element,
            property_name,
            value,
        );
        if translations.is_empty() {
            translations = crate::blocks::builtin::audiogain::translate_property(
                element_id,
                property_name,
                value,
            );
        }

        if translations.is_empty() {
            // No translation needed, use original property
            self.validate_property_mutability(element, element_id, property_name, state)?;
            self.set_property(element, element_id, property_name, value, ramp_ms)?;
        } else {
            for (translated_name, translated_value) in &translations {
                debug!(
                    "Translated property {}.{} -> {}.{} for lsp-rs element",
                    element_id, property_name, element_id, translated_name
                );
                self.validate_property_mutability(element, element_id, translated_name, state)?;
                self.set_property(
                    element,
                    element_id,
                    translated_name,
                    translated_value,
                    ramp_ms,
                )?;
            }
        }

        info!(
            "Successfully updated element property {}.{} to {:?}",
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

        // Time Offset block: `offset_ms` is a pad-offset rather than an
        // element property — mirror the write-path interceptor so the value
        // can be read back via the same API.
        if let Some(v) = crate::blocks::builtin::time_offset::try_read_live_offset(
            element,
            element_id,
            property_name,
        ) {
            return Ok(v);
        }

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

        // Surface synthetic block properties that don't live on the element
        // itself (e.g. Time Offset's `offset_ms`, which is a pad-offset).
        if let Some((name, value)) =
            crate::blocks::builtin::time_offset::live_offset_property_entry(element, element_id)
        {
            properties.insert(name, value);
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
                // Check if it's an enum type
                if pspec.value_type().is_a(glib::Type::ENUM) {
                    // Get the enum value as an integer and convert to nick string
                    let value = pad.property_value(property_name);
                    if let Ok(enum_value) = value.get::<i32>() {
                        // Get the enum class and find the nick for this value
                        if let Some(enum_class) = glib::EnumClass::with_type(pspec.value_type()) {
                            if let Some(enum_val) = enum_class.value(enum_value) {
                                PropertyValue::String(enum_val.nick().to_string())
                            } else {
                                PropertyValue::Int(enum_value as i64)
                            }
                        } else {
                            PropertyValue::Int(enum_value as i64)
                        }
                    } else {
                        return Err(PipelineError::InvalidProperty {
                            element: format!("{}:{}", element_id, pad_name),
                            property: property_name.to_string(),
                            reason: format!("Failed to read enum value for type: {}", type_name),
                        });
                    }
                } else {
                    return Err(PipelineError::InvalidProperty {
                        element: format!("{}:{}", element_id, pad_name),
                        property: property_name.to_string(),
                        reason: format!("Unsupported property type: {}", type_name),
                    });
                }
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
    pub(super) fn set_pad_property(
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

        // Some pad properties are version- or backend-specific. The clearest
        // example is `sizing-policy`, which exists on `GstVideoAggregatorPad`
        // and `GstGLVideoMixerPad` only from GStreamer 1.24 onward. On 1.22
        // calling `set_property_from_str` for a missing property panics
        // ("property '...' of type '...' not found"), which would crash the
        // tokio worker building the pipeline. Skip gracefully instead.
        if !pad.has_property(prop_name) {
            debug!(
                "Pad {}:{} has no property '{}' (likely a GStreamer version/backend difference) - skipping",
                element_id, pad_name, prop_name
            );
            return Ok(());
        }

        // Use set_property_from_str for all types - GStreamer handles type conversion automatically
        let value_str = match prop_value {
            PropertyValue::String(v) => v.clone(),
            PropertyValue::Int(v) => v.to_string(),
            PropertyValue::UInt(v) => v.to_string(),
            PropertyValue::Float(v) => v.to_string(),
            PropertyValue::Bool(v) => v.to_string(),
        };

        pad.set_property_from_str(prop_name, &value_str);

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
                if !mutable_in_paused && !mutable_in_playing && !can_change_at_runtime {
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
                if !mutable_in_paused && !mutable_in_playing && !can_change_at_runtime {
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
}
