//! Recorder helpers shared by `recorder_test` and `recorder_stall_test`: a
//! recorder built through the real `RecorderBuilder`, the sources that feed it,
//! and the files it wrote.
//!
//! Include it as `#[path = "common/recorder.rs"] pub mod recorder;` next to
//! `pub mod common;`. The `pub` is what keeps the helpers a given test binary
//! does not call from being reported as dead code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
/// The per-container muxers and demuxers of `unfed_track` are checked there.
pub const REQUIRED: &[&str] = &[
    "splitmuxsink",
    "multifilesink",
    "mp4mux",
    "mpegtsmux",
    "tsdemux",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "videotestsrc",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "appsrc",
    "identity",
    "queue",
    "filesrc",
    "fakesink",
];

pub fn plugins_available() -> bool {
    crate::common::plugins_available(REQUIRED)
}

/// A recorder block built through the real builder and added to a pipeline.
pub struct Recorder {
    instance_id: String,
    /// Holds the element-setup hooks; see [`Recorder::run_setups`].
    ctx: BlockBuildContext,
    elements: HashMap<String, gst::Element>,
}

impl Recorder {
    /// The block element with the internal id `name`, e.g. `video_input_0`.
    pub fn input(&self, name: &str) -> gst::Element {
        self.elements
            .get(&format!("{}:{}", self.instance_id, name))
            .cloned()
            .unwrap_or_else(|| panic!("recorder {} exposes no {}", self.instance_id, name))
    }

    /// Run the element-setup hooks, as the pipeline manager does after linking
    /// and before PLAYING. They decide which tracks are connected and get a
    /// `splitmuxsink` pad, so they must run after the inputs are linked.
    pub fn run_setups(&self) {
        for setup in self.ctx.take_element_setups() {
            setup(uuid::Uuid::new_v4(), EventBroadcaster::with_capacity(16));
        }
    }
}

/// Build a recorder named `instance_id` that writes to `<media_root>/recordings`
/// with its instance id as filename prefix, add it to `pipeline`, and make the
/// links it declares, as the pipeline manager would. `props` is the rest of
/// its configuration.
pub fn add_recorder(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
    props: &[(&str, PropertyValue)],
) -> Recorder {
    let mut properties: HashMap<String, PropertyValue> = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    properties.insert(
        "output_dir".to_string(),
        PropertyValue::String("recordings".into()),
    );
    properties.insert(
        "filename_prefix".to_string(),
        PropertyValue::String(instance_id.to_string()),
    );
    properties.insert(
        "_media_path".to_string(),
        PropertyValue::String(media_root.to_string_lossy().to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = RecorderBuilder
        .build(instance_id, &properties, &ctx)
        .expect("recorder block builds");

    let mut elements = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        elements.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = pipeline
            .by_name(&from.element_id)
            .expect("internal link source element is in the pipeline");
        let dst = pipeline
            .by_name(&to.element_id)
            .expect("internal link sink element is in the pipeline");
        src.link_pads(from.pad_name.as_deref(), &dst, to.pad_name.as_deref())
            .expect("internal recorder link");
    }

    Recorder {
        instance_id: instance_id.to_string(),
        ctx,
        elements,
    }
}

/// An mp4 recorder with `num_video` video and `num_audio` audio tracks.
pub fn add_mp4_recorder(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
    num_video: u64,
    num_audio: u64,
) -> Recorder {
    add_recorder(
        pipeline,
        instance_id,
        media_root,
        &[
            ("container", PropertyValue::String("mp4".into())),
            ("num_video_tracks", PropertyValue::UInt(num_video)),
            ("num_audio_tracks", PropertyValue::UInt(num_audio)),
        ],
    )
}

/// 320x240@30 H.264 with a keyframe every 10 frames, added to `pipeline`.
/// Returns the encoder, for the caller to link on. `num_buffers` of -1 runs
/// until the pipeline stops.
pub fn video_source(pipeline: &gst::Pipeline, num_buffers: i32, live: bool) -> gst::Element {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", live)
        .build()
        .expect("videotestsrc");
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .build()
        .expect("capsfilter");
    let enc = gst::ElementFactory::make("x264enc")
        .property("key-int-max", 10u32)
        .property_from_str("tune", "zerolatency")
        .build()
        .expect("x264enc");

    pipeline.add_many([&src, &caps, &enc]).unwrap();
    gst::Element::link_many([&src, &caps, &enc]).unwrap();
    enc
}

/// AAC from `audiotestsrc`, added to `pipeline`. Returns the encoder, for the
/// caller to link on. `num_buffers` of -1 runs until the pipeline stops.
pub fn audio_source(pipeline: &gst::Pipeline, num_buffers: i32, live: bool) -> gst::Element {
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", live)
        .build()
        .expect("audiotestsrc");
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make("avenc_aac")
        .build()
        .expect("avenc_aac");

    pipeline.add_many([&src, &conv, &resample, &enc]).unwrap();
    gst::Element::link_many([&src, &conv, &resample, &enc]).unwrap();
    enc
}

/// The files under `<media_root>/recordings` whose name starts with `prefix`,
/// sorted.
pub fn recordings(media_root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(media_root.join("recordings"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

pub fn total_bytes(files: &[PathBuf]) -> u64 {
    files
        .iter()
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum()
}
