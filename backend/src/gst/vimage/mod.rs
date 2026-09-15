//! A vImage-backed video converter for macOS.
//!
//! Colour conversion is the largest fixed cost in Strom's macOS pipelines:
//! every encoder, compositor and WebRTC sink wants Y'CbCr while the sources
//! and the HTML renderer produce packed RGB. `videoconvert` does that on the
//! CPU; Accelerate's vImage does the same work through hand-tuned NEON kernels
//! across its own thread pool.
//!
//! The element registered here, `stromvimageconvert`, is a drop-in for
//! `videoconvert` and `videoconvertscale`. It exposes the same `n-threads`
//! property, so [`crate::gpu::configure_video_convert`] reaches it unchanged,
//! and it never fails a conversion `videoconvert` would have managed: format
//! pairs without a vImage path — and every resize — run on
//! `GstVideoConverter`, which is the code `videoconvert` itself is built on.
//!
//! Registration is static and idempotent; see [`register`].

mod accelerate;
mod convert;
mod imp;
mod plan;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_video as gst_video;
use tracing::warn;

/// The GStreamer factory name. Prefixed because this element lives in Strom's
/// tree rather than in `gst-plugins-rs`; the prefix keeps it from ever
/// colliding with an upstream element of the obvious name.
pub const ELEMENT_NAME: &str = "stromvimageconvert";

glib::wrapper! {
    pub struct VImageConvert(ObjectSubclass<imp::VImageConvert>)
        @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Rank NONE: this element is selected by name through
    // `VideoConvertMode`, never autoplugged.
    gst::Element::register(
        Some(plugin),
        ELEMENT_NAME,
        gst::Rank::NONE,
        VImageConvert::static_type(),
    )
}

gst::plugin_define!(
    stromvimage,
    "vImage-backed video conversion for macOS",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT/X11",
    "strom",
    "strom",
    "https://github.com/Eyevinn/strom"
);

/// Register the plugin with the GStreamer registry, once per process.
///
/// Returns whether `stromvimageconvert` is available afterwards. A `false`
/// here is not fatal: [`crate::gpu::detect_convert_mode`] treats it as "stay
/// on `videoconvert`", which is exactly the behaviour before this element
/// existed.
///
/// Callers must have initialised GStreamer first.
pub fn register() -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| match plugin_register_static() {
        Ok(()) => true,
        Err(e) => {
            warn!(
                "could not register the {} plugin, staying on videoconvert: {}",
                ELEMENT_NAME, e
            );
            false
        }
    })
}
