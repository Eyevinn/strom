//! Voice isolation on a channel strip, between the HPF and the gate.

use std::collections::HashMap;

use gstreamer as gst;
use strom_types::element::ElementPadRef;
use strom_types::mixer::VOICE_ISOLATION_NO_LIMIT_DB;
use strom_types::PropertyValue;

use super::properties::{get_bool_prop, get_float_prop};
use crate::blocks::BlockBuildError;

/// Add channel `ch`'s voice isolation and return the ids to link in after
/// the HPF and before the gate, or `None` when Strom is built without it.
///
/// The element is always present and passes audio through until enabled, so
/// the switch works while the flow runs. It only runs at 48 kHz; a mixer at
/// another rate converts to 48 kHz and back around it.
pub(super) fn push_voice_isolation(
    instance_id: &str,
    ch: usize,
    properties: &HashMap<String, PropertyValue>,
    sample_rate: u32,
    elements: &mut Vec<(String, gst::Element)>,
    internal_links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<Option<(String, String)>, BlockBuildError> {
    let ch_num = ch + 1;
    let enabled = get_bool_prop(properties, &format!("ch{}_voice_isolation", ch_num), false);
    let limit = get_float_prop(
        properties,
        &format!("ch{}_voice_isolation_limit", ch_num),
        VOICE_ISOLATION_NO_LIMIT_DB as f64,
    );

    #[cfg(not(feature = "voice-isolation"))]
    {
        let _ = (instance_id, limit, sample_rate, elements, internal_links);
        if enabled {
            tracing::warn!(
                "Mixer channel {} asks for voice isolation, but this Strom was built without it",
                ch_num
            );
        }
        Ok(None)
    }

    #[cfg(feature = "voice-isolation")]
    {
        use crate::gst::voice_isolation;

        let vi_id = format!("{}:voiceiso_{}", instance_id, ch);
        let vi = gst::ElementFactory::make(voice_isolation::ELEMENT_NAME)
            .name(&vi_id)
            .property("enabled", enabled)
            .property("attenuation-limit", limit)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("voice isolation ch{}: {}", ch_num, e))
            })?;
        elements.push((vi_id.clone(), vi));

        if sample_rate == voice_isolation::SAMPLE_RATE {
            return Ok(Some((vi_id.clone(), vi_id)));
        }

        // audioresample → capsfilter at each rate, on both sides.
        let mut convert = |suffix: &str, rate: u32| -> Result<(String, String), BlockBuildError> {
            let resample_id = format!("{}:voiceiso_{}_resample_{}", instance_id, ch, suffix);
            let caps_id = format!("{}:voiceiso_{}_caps_{}", instance_id, ch, suffix);
            let resample = gst::ElementFactory::make("audioresample")
                .name(&resample_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", resample_id, e)))?;
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("rate", rate as i32)
                .field("channels", 2i32)
                .field("layout", "interleaved")
                .build();
            let capsfilter = gst::ElementFactory::make("capsfilter")
                .name(&caps_id)
                .property("caps", &caps)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", caps_id, e)))?;
            elements.push((resample_id.clone(), resample));
            elements.push((caps_id.clone(), capsfilter));
            internal_links.push((
                ElementPadRef::pad(&resample_id, "src"),
                ElementPadRef::pad(&caps_id, "sink"),
            ));
            Ok((resample_id, caps_id))
        };
        let (in_first, in_last) = convert("in", voice_isolation::SAMPLE_RATE)?;
        let (out_first, out_last) = convert("out", sample_rate)?;
        internal_links.push((
            ElementPadRef::pad(&in_last, "src"),
            ElementPadRef::pad(&vi_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&vi_id, "src"),
            ElementPadRef::pad(&out_first, "sink"),
        ));
        Ok(Some((in_first, out_last)))
    }
}
