//! `stromvoiceisolation`: DPDFNet speech enhancement as a GStreamer element.
//!
//! Off, it passes audio through untouched. On, it keeps speech and
//! suppresses everything else (music, fans, typing, knocks, breaths), writing
//! the mono result into every channel 60 ms late. It reports no latency: the
//! pipeline's latency is fixed when the flow starts, and reporting 60 ms would
//! hold back every channel of the mixer, on or off.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

mod engine;
mod imp;

pub const ELEMENT_NAME: &str = "stromvoiceisolation";
/// The rate the model runs at; the element accepts nothing else.
pub const SAMPLE_RATE: u32 = engine::SAMPLE_RATE as u32;
/// How late the audio is while the element is enabled, in samples at SAMPLE_RATE.
pub const CONTENT_DELAY_SAMPLES: usize = imp::CONTENT_DELAY;

glib::wrapper! {
    pub struct VoiceIsolation(ObjectSubclass<imp::VoiceIsolation>)
        @extends gstreamer_base::BaseTransform, gst::Element, gst::Object;
}

/// Make the element available to `gst::ElementFactory::make`.
pub fn register() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        ELEMENT_NAME,
        gst::Rank::NONE,
        VoiceIsolation::static_type(),
    )
}

#[cfg(test)]
mod tests;
