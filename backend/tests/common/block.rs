//! A block built through its real builder, assembled into a bare pipeline.

use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom::blocks::{BlockBuildContext, BlockBuildResult};
use strom::events::EventBroadcaster;

/// A build context with no ICE servers and the `all` transport policy.
pub fn context() -> BlockBuildContext {
    BlockBuildContext::new(Vec::new(), "all".to_string())
}

/// Add every element of `built` to `pipeline` and make the internal links it
/// declares, as the pipeline manager does. Returns the elements by ID.
pub fn install(
    pipeline: &gst::Pipeline,
    built: &BlockBuildResult,
) -> HashMap<String, gst::Element> {
    install_except(pipeline, built, &[])
}

/// [`install`], leaving out the elements in `skip` and every link that touches
/// one.
pub fn install_except(
    pipeline: &gst::Pipeline,
    built: &BlockBuildResult,
    skip: &[&str],
) -> HashMap<String, gst::Element> {
    let mut by_id = HashMap::new();
    for (id, element) in &built.elements {
        if skip.contains(&id.as_str()) {
            continue;
        }
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        if skip.contains(&from.element_id.as_str()) || skip.contains(&to.element_id.as_str()) {
            continue;
        }
        let src = by_id
            .get(&from.element_id)
            .unwrap_or_else(|| panic!("internal link source {} missing", from.element_id));
        let sink = by_id
            .get(&to.element_id)
            .unwrap_or_else(|| panic!("internal link target {} missing", to.element_id));
        // `link_pads` finds a static or already-requested pad by name and
        // requests one otherwise, and with no name picks a compatible pad.
        src.link_pads(from.pad_name.as_deref(), sink, to.pad_name.as_deref())
            .unwrap_or_else(|e| {
                panic!(
                    "internal link {} -> {} failed: {:?}",
                    from.to_string_format(),
                    to.to_string_format(),
                    e
                )
            });
    }
    by_id
}

/// Run the element-setup hooks the block registered, as the pipeline manager
/// does after linking and before PLAYING.
pub fn run_setups(ctx: &BlockBuildContext) {
    let flow_id = strom_types::flow::FlowId::new_v4();
    let events = EventBroadcaster::with_capacity(16);
    for setup in ctx.take_element_setups() {
        setup(flow_id, events.clone());
    }
}

/// The real Video Encoder block (`builtin.videoenc`) for `codec`, software
/// encoder, in a bin with ghost pads `sink` (the block's `video_in`) and `src`
/// (its `encoded_out`), so a test can link it like one element.
pub fn videoenc_bin(codec: &str) -> gst::Element {
    let properties = HashMap::from([
        (
            "codec".to_string(),
            strom_types::PropertyValue::String(codec.to_string()),
        ),
        (
            "encoder_preference".to_string(),
            strom_types::PropertyValue::String("software".to_string()),
        ),
    ]);
    let built = strom::blocks::builtin::get_builder("builtin.videoenc")
        .expect("Video Encoder builder")
        .build("videoenc", &properties, &context())
        .expect("Video Encoder builds");
    let bin = gst::Bin::new();
    let mut by_id = HashMap::new();
    for (id, element) in &built.elements {
        bin.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        by_id[&from.element_id]
            .link_pads(
                from.pad_name.as_deref(),
                &by_id[&to.element_id],
                to.pad_name.as_deref(),
            )
            .expect("internal link");
    }
    let ghost = |element: &str, pad: &str, name: &str| {
        let target = by_id[element].static_pad(pad).expect("block pad");
        let ghost = gst::GhostPad::builder_with_target(&target)
            .expect("ghost pad")
            .name(name)
            .build();
        bin.add_pad(&ghost).expect("add ghost pad");
    };
    ghost("videoenc:input", "sink", "sink");
    ghost("videoenc:capsfilter", "src", "src");
    bin.upcast()
}
