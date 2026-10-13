//! Keeps the profile and level a WHEP viewer offers from blocking pre-encoded
//! video.
//!
//! webrtcsink copies the viewer's offered profile onto the capsfilters around
//! that viewer's payloader: the one directly upstream (the encoder filter)
//! gets the profiles compatible with the offer, the ones downstream get its
//! `profile-level-id`. Browsers offer H.264 Baseline first and the block
//! answers with that payload type, so a High profile stream fails at those
//! filters with `not-negotiated` on its first keyframe. Browsers decode it
//! regardless, so the profile fields are removed.
//!
//! The encoder filter also gets the offered level and every level below it.
//! Chrome offers level 3.1 on every H.264 payload type, so 1080p (level 4) and
//! 720p at 50/60 fps (level 3.2) fail the same way. Chrome decodes those too,
//! so `level` is removed as well. That sends above the level the viewer
//! declared, which RFC 6184 forbids even with `level-asymmetry-allowed=1`; the
//! alternative is no pre-encoded H.264 above 720p30 for any Chrome viewer.
//!
//! webrtcsink sets a viewer's filter caps after `payloader-setup`, from
//! `connect_input_stream`, so each filter is watched with `notify::caps`
//! rather than only stripped once. The filters are found from the payloader
//! itself: by the time `payloader-setup` fires, its whole chain is already
//! built and linked.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::info;

/// H.264 / AV1 fields, then H.265 fields.
const PROFILE_FIELDS: &[&str] = &[
    "profile-level-id",
    "profile",
    "level",
    "profile-id",
    "tier-flag",
    "level-id",
    "tx-mode",
];

/// Strip profile fields from every capsfilter webrtcsink puts around a
/// payloader, for discovery and for each viewer.
pub(super) fn install(whepserversink: &gst::Element) {
    whepserversink.connect("payloader-setup", false, |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let payloader = values[3].get::<gst::Element>().unwrap();

        info!(
            "WHEP Output: payloader-setup fired: consumer_id={}, payloader={} (factory={})",
            consumer_id,
            payloader.name(),
            payloader
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );

        // The encoder filter sits directly upstream of the payloader.
        let upstream = payloader
            .static_pad("sink")
            .and_then(|pad| pad.peer())
            .and_then(|pad| pad.parent_element())
            .filter(is_capsfilter);
        if let Some(filter) = upstream {
            watch(&filter, &consumer_id);
        }

        // Output and payloader filters follow it, up to webrtcbin (or the
        // discovery pipeline's appsink).
        let mut next = payloader.static_pad("src").and_then(|pad| pad.peer());
        while let Some(filter) = next
            .and_then(|pad| pad.parent_element())
            .filter(is_capsfilter)
        {
            watch(&filter, &consumer_id);
            next = filter.static_pad("src").and_then(|pad| pad.peer());
        }

        Some(false.to_value())
    });
}

fn is_capsfilter(element: &gst::Element) -> bool {
    element
        .factory()
        .is_some_and(|f| f.name().as_str() == "capsfilter")
}

/// Strip `filter` now and whenever its caps are set again.
fn watch(filter: &gst::Element, consumer_id: &str) {
    strip(filter, consumer_id);
    let consumer_id = consumer_id.to_string();
    filter.connect_notify(Some("caps"), move |filter, _| strip(filter, &consumer_id));
}

/// Remove the profile fields from `filter`'s caps. Setting the stripped caps
/// notifies again, and that call finds nothing to remove.
fn strip(filter: &gst::Element, consumer_id: &str) {
    let caps: gst::Caps = filter.property("caps");
    if !caps
        .iter()
        .any(|s| PROFILE_FIELDS.iter().any(|f| s.has_field(*f)))
    {
        return;
    }

    let mut stripped = gst::Caps::new_empty();
    for s in caps.iter() {
        let mut s = s.to_owned();
        s.remove_fields(PROFILE_FIELDS.iter().copied());
        stripped.merge_structure(s);
    }
    info!(
        "WHEP Output: Stripped profile from capsfilter {} for {}: {:?}",
        filter.name(),
        consumer_id,
        stripped
    );
    filter.set_property("caps", &stripped);
}
