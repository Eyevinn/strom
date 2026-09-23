//! `builtin.whep_output` must not add BaseSink's processing deadline to the
//! flow's latency.
//!
//! whepserversink's input appsinks only hand buffers to the per-viewer session
//! pipelines, yet their default 20 ms deadline is added to the pipeline
//! latency and waited out twice before a packet is sent: by the appsink, and
//! by webrtcbin's clocksync in the session.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::{builtin, BlockBuildContext};
use strom_types::PropertyValue;

const DEFAULT_PROCESSING_DEADLINE: gst::ClockTime = gst::ClockTime::from_mseconds(20);

struct Built {
    pipeline: gst::Pipeline,
    elements: HashMap<String, gst::Element>,
    sink: gst::Element,
}

/// Build the real `builtin.whep_output` block, wire its internal links, and
/// feed every audio and video input with a live test source.
fn build_block(id: &str, num_audio: u32, num_video: u32) -> Built {
    gst::init().expect("gstreamer init");
    // Both tests call this; a second registration returns an error.
    let _ = gstrswebrtc::plugin_register_static();

    let properties: HashMap<String, PropertyValue> = [
        (
            "endpoint_id".to_string(),
            PropertyValue::String(id.to_string()),
        ),
        (
            "num_audio_tracks".to_string(),
            PropertyValue::UInt(num_audio.into()),
        ),
        (
            "num_video_tracks".to_string(),
            PropertyValue::UInt(num_video.into()),
        ),
    ]
    .into();
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let result = builtin::get_builder("builtin.whep_output")
        .expect("whep_output builder")
        .build("whep", &properties, &ctx)
        .expect("whep_output build");

    let pipeline = gst::Pipeline::new();
    let mut elements: HashMap<String, gst::Element> = HashMap::new();
    for (eid, element) in &result.elements {
        pipeline.add(element).expect("add block element");
        elements.insert(eid.clone(), element.clone());
    }
    for (from, to) in &result.internal_links {
        let src = &elements[&from.element_id];
        let dst = &elements[&to.element_id];
        let src_pad = src
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("source pad");
        let dst_name = to.pad_name.as_deref().unwrap_or("sink");
        let dst_pad = dst
            .static_pad(dst_name)
            .or_else(|| dst.request_pad_simple(dst_name))
            .unwrap_or_else(|| panic!("no sink pad {dst_name} on {}", dst.name()));
        src_pad.link(&dst_pad).expect("internal link");
    }

    for slot in 0..num_audio {
        let qid = if slot == 0 {
            "whep:audio_queue".to_string()
        } else {
            format!("whep:audio_queue_{slot}")
        };
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .build()
            .expect("audiotestsrc");
        pipeline.add(&src).expect("add audio source");
        src.link(&elements[&qid]).expect("link audio source");
    }
    for slot in 0..num_video {
        let qid = if slot == 0 {
            "whep:video_queue".to_string()
        } else {
            format!("whep:video_queue_{slot}")
        };
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
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
        pipeline.add_many([&src, &caps]).expect("add video source");
        src.link(&caps).expect("link videotestsrc -> capsfilter");
        caps.link(&elements[&qid]).expect("link video source");
    }

    let sink = elements["whep:whepserversink"].clone();
    Built {
        pipeline,
        elements,
        sink,
    }
}

/// Every appsink that is a direct child of the whepserversink bin, with its
/// current processing-deadline.
fn direct_child_appsinks(sink: &gst::Element) -> Vec<(String, u64)> {
    let bin = sink.clone().downcast::<gst::Bin>().expect("sink is a bin");
    bin.iterate_elements()
        .into_iter()
        .flatten()
        .filter(|e| e.factory().is_some_and(|f| f.name() == "appsink"))
        .map(|e| {
            (
                e.name().to_string(),
                e.property::<u64>("processing-deadline"),
            )
        })
        .collect()
}

/// One flag per direct-child appsink of the whepserversink bin, set by the
/// first buffer to reach it. The appsinks exist once the inputs are linked,
/// so call this before PLAYING.
fn flag_first_buffer_at_appsinks(sink: &gst::Element) -> Vec<Arc<AtomicBool>> {
    let bin = sink.clone().downcast::<gst::Bin>().expect("sink is a bin");
    bin.iterate_elements()
        .into_iter()
        .flatten()
        .filter(|e| e.factory().is_some_and(|f| f.name() == "appsink"))
        .map(|e| {
            let reached = Arc::new(AtomicBool::new(false));
            let flag = reached.clone();
            e.static_pad("sink").expect("appsink sink pad").add_probe(
                gst::PadProbeType::BUFFER,
                move |_, _| {
                    flag.store(true, Ordering::Relaxed);
                    gst::PadProbeReturn::Remove
                },
            );
            reached
        })
        .collect()
}

fn wait_playing(pipeline: &gst::Pipeline) {
    pipeline.set_state(gst::State::Playing).expect("PLAYING");
    let (res, state, _) = pipeline.state(gst::ClockTime::from_seconds(15));
    assert!(
        res.is_ok() && state == gst::State::Playing,
        "pipeline did not reach PLAYING: {res:?} {state:?}"
    );
}

/// The pipeline can answer the latency query as non-live for a moment after
/// reaching PLAYING, so poll until it reports a live result.
fn live_pipeline_latency(pipeline: &gst::Pipeline) -> gst::ClockTime {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut query = gst::query::Latency::new();
        if pipeline.query(&mut query) {
            let (live, min, _) = query.result();
            if live {
                return min;
            }
        }
        assert!(
            Instant::now() < deadline,
            "pipeline never answered the latency query as live"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn whep_output_latency_excludes_appsink_processing_deadline() {
    let built = build_block("latency-test", 1, 0);
    wait_playing(&built.pipeline);
    let min = live_pipeline_latency(&built.pipeline);

    // audiotestsrc reports one buffer of latency; the block's share is the
    // pipeline latency minus that.
    let src = built.elements["whep:audio_queue"]
        .static_pad("sink")
        .and_then(|p| p.peer())
        .and_then(|p| p.parent_element())
        .expect("audio source");
    let mut src_query = gst::query::Latency::new();
    assert!(src.query(&mut src_query), "source latency query failed");
    let (_, src_min, _) = src_query.result();
    let _ = built.pipeline.set_state(gst::State::Null);

    let added = min.saturating_sub(src_min);
    assert!(
        added < DEFAULT_PROCESSING_DEADLINE,
        "whep_output adds {added} to its source's {src_min} of latency; \
         the appsink's processing deadline is still counted"
    );
}

/// The latency check above covers a single audio input. Each input track gets
/// its own appsink, so check every one across several audio and video tracks.
#[test]
fn every_input_appsink_is_zeroed_multi_track() {
    let built = build_block("latency-multi", 2, 2);
    // Check after data has reached every appsink, so the test also catches
    // anything that resets the deadline when streaming starts.
    let reached = flag_first_buffer_at_appsinks(&built.sink);
    wait_playing(&built.pipeline);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reached.iter().all(|r| r.load(Ordering::Relaxed)) {
        assert!(
            Instant::now() < deadline,
            "no buffer reached some appsinks within 10 s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let appsinks = direct_child_appsinks(&built.sink);
    let _ = built.pipeline.set_state(gst::State::Null);

    assert_eq!(
        appsinks.len(),
        4,
        "expected one appsink per input track, got {appsinks:?}"
    );
    for (name, deadline) in &appsinks {
        assert_eq!(
            *deadline, 0,
            "appsink {name} kept processing-deadline {deadline}; all of {appsinks:?}"
        );
    }
}
