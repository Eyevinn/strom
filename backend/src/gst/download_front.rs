//! Caps-driven downloads in front of a consumer that takes exactly what its
//! caps list, such as an encoder behind its `videoconvert`.
//!
//! A block emits the memory type it naturally produces and the *consuming*
//! block adapts its own input. What reaches an encoder's input is often known
//! only at runtime: `decodebin` picks a decoder, and `nvh264dec` decodes to
//! CUDA memory, `vtdec` to GL memory, `d3d12h264dec` to D3D12 memory. The
//! encoder may take that memory directly (`nvh264enc` takes CUDA, GL and
//! system memory), or not at all (`x264enc` takes system memory only).
//!
//! [`install`] watches the sink pad of the input's front element (an
//! `identity`) and puts what [`video_adapt::decide`] calls for with
//! [`Consumer::Exactly`] between the front element and the consumer:
//!
//! * memory the consumer lists passes straight through, with nothing in
//!   between (CUDA into `nvh264enc` stays on the GPU);
//! * GL, CUDA, D3D12 or D3D11 memory it leaves out goes through its download,
//!   pinned to system memory, which the consumer's converter then turns into a
//!   format the encoder takes;
//! * memory nothing here downloads, a download that is not installed, or
//!   encoded video, fail the flow with an element error that names what
//!   arrived, the consumer, and what is missing.
//!
//! The consumer's caps are what its sink pad answers to a caps query, asked
//! again each time: a `videoconvert` with an encoder behind it answers with the
//! GPU memory types and formats the encoder takes, and system memory in any
//! format.
//!
//! The probes are on the front element's *sink* pad and fire per query and per
//! event, never per buffer:
//!
//! * A CAPS event is handled before the front element sees it, on the
//!   streaming thread that carries the buffers, so no buffer crosses the link
//!   while the adapters are replaced (the same in-probe splice as
//!   [`crate::gst::video_input_bridge`]). The front element then negotiates
//!   against the new chain: an `identity` asks its peer, so it would otherwise
//!   refuse caps only the download takes.
//! * An ACCEPT_CAPS query, which upstream sends before new caps, is answered
//!   for the input as it will be: yes for what a download adapts, no (with the
//!   element error) for what nothing adapts, and the consumer's own answer
//!   for the rest.
//! * A CAPS query is answered by the consumer itself, whatever is spliced in
//!   front of it. When the query offers nothing the consumer takes directly,
//!   or offers nothing at all (a link check), the answer also lists the
//!   consumer's system-memory formats in each memory a download is installed
//!   for, last. A producer that can choose (a decoder) still picks what the
//!   consumer takes directly, and one that can only offer GPU memory still
//!   links.
//!
//! The adapters follow the caps both ways: a producer that switches from CUDA
//! to system memory gets its download taken out again.

use crate::blocks::BlockBuildError;
use crate::gst::video_adapt::{self, Adapter, Consumer, Refusal, VIDEO_CONVERT_FACTORY};
use crate::gst::video_input_bridge::resplice;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::element::ElementPadRef;
use tracing::{info, warn};

/// The adapters spliced in between the front element and the consumer.
#[derive(Default)]
struct Spliced {
    adapters: Vec<Adapter>,
    elements: Vec<glib::WeakRef<gst::Element>>,
    /// How many chains have been spliced in, so each gets its own names: the
    /// old chain is still in the bin while its replacement is linked.
    generation: u32,
}

/// What one input's front knows. Weak handles only: the probes that share it
/// are owned by a pad of the same pipeline.
struct Front {
    /// Names the input in logs, errors and the adapters' element names.
    label: String,
    /// Names the consumer in errors, e.g. `Video Encoder (x264enc)`.
    consumer: String,
    front_src: glib::WeakRef<gst::Pad>,
    consumer_sink: glib::WeakRef<gst::Pad>,
    /// Taken per CAPS event only, on the streaming thread.
    spliced: Mutex<Spliced>,
    /// A refusal has been posted for the current caps; cleared once caps are
    /// adapted, so a later refusal is reported again.
    refused: AtomicBool,
}

/// Watch `front`'s sink pad and splice the downloads its producer needs between
/// `front` and `consumer_sink`, which `front`'s source pad feeds.
///
/// `front` is a passthrough element in front of the consumer (an `identity`
/// with `silent=true`). `label` names the input in logs, errors and the
/// inserted elements; `consumer` names the consumer in errors.
pub fn install(front: &gst::Element, consumer_sink: &gst::Pad, label: &str, consumer: &str) {
    let (Some(front_sink), Some(front_src)) = (front.static_pad("sink"), front.static_pad("src"))
    else {
        warn!(
            "{}: the input's front element lacks a sink or src pad, so its input is not adapted",
            label
        );
        return;
    };
    let state = Arc::new(Front {
        label: label.to_string(),
        consumer: consumer.to_string(),
        front_src: front_src.downgrade(),
        consumer_sink: consumer_sink.downgrade(),
        spliced: Mutex::new(Spliced::default()),
        refused: AtomicBool::new(false),
    });

    // QUERY_DOWNSTREAM on a sink pad: queries arriving from upstream, before
    // the front element answers them. Per query, never per buffer.
    let queries = Arc::clone(&state);
    front_sink.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |pad, info| {
        if info.mask.contains(gst::PadProbeType::PULL) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(gst::PadProbeData::Query(ref mut query)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        on_query(&queries, pad, query)
    });

    // EVENT_DOWNSTREAM, not BUFFER: per event, and only CAPS is looked at.
    front_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        on_caps(&state, pad, caps_event.caps());
        gst::PadProbeReturn::Ok
    });
}

/// An input's front and the `videoconvert` behind it, built by
/// [`convert_front`]. The caller adds both to its block, with [`Self::link`]
/// between them: the front splices its downloads in on that link.
pub struct ConvertFront {
    pub front_id: String,
    pub front: gst::Element,
    pub convert_id: String,
    pub convert: gst::Element,
}

impl ConvertFront {
    /// The link from the front's `src` to the converter's `sink`.
    pub fn link(&self) -> (ElementPadRef, ElementPadRef) {
        (
            ElementPadRef::pad(&self.front_id, "src"),
            ElementPadRef::pad(&self.convert_id, "sink"),
        )
    }
}

/// Build the front an input with a converter behind it needs:
/// `front_id` (`identity`, `silent=true`) and `convert_id` (plain
/// `videoconvert`, set up by [`crate::gpu::configure_video_convert`]), with
/// [`install`] watching the front for `consumer`.
///
/// For an input whose consumer is the converter itself. The `videoconvert`
/// passes GPU memory its consumer lists straight through and converts system
/// memory; the front downloads the GPU memory the consumer leaves out.
/// `consumer` names the consumer in errors.
pub fn convert_front(
    front_id: &str,
    convert_id: &str,
    consumer: &str,
) -> Result<ConvertFront, BlockBuildError> {
    let front = gst::ElementFactory::make("identity")
        .name(front_id)
        .property("silent", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;
    let convert = gst::ElementFactory::make(VIDEO_CONVERT_FACTORY)
        .name(convert_id)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("{}: {}", VIDEO_CONVERT_FACTORY, e))
        })?;
    crate::gpu::configure_video_convert(&convert);
    let convert_sink = convert.static_pad("sink").ok_or_else(|| {
        BlockBuildError::ElementCreation(format!("{} has no sink pad", VIDEO_CONVERT_FACTORY))
    })?;
    install(&front, &convert_sink, front_id, consumer);
    Ok(ConvertFront {
        front_id: front_id.to_string(),
        front,
        convert_id: convert_id.to_string(),
        convert,
    })
}

/// A decision, with the consumer's caps it was made against (for errors).
struct Decision {
    takes: gst::Caps,
    adapters: Result<Vec<Adapter>, Refusal>,
}

/// What the consumer needs in front of it to take `caps`.
///
/// Asked again for every ACCEPT_CAPS query and CAPS event, never cached: the
/// consumer's answer is read as it is now, with everything linked behind it.
fn decide(consumer_sink: &gst::Pad, caps: &gst::CapsRef) -> Decision {
    let takes = consumer_sink.query_caps(None);
    let adapters = video_adapt::decide(
        caps,
        Consumer::Exactly(&takes),
        video_adapt::factory_available,
    );
    Decision { takes, adapters }
}

fn on_query(front: &Front, pad: &gst::Pad, query: &mut gst::QueryRef) -> gst::PadProbeReturn {
    let Some(consumer_sink) = front.consumer_sink.upgrade() else {
        // The pipeline is being torn down.
        return gst::PadProbeReturn::Ok;
    };
    match query.view_mut() {
        gst::QueryViewMut::AcceptCaps(q) => {
            let caps = q.caps_owned();
            let decision = decide(&consumer_sink, &caps);
            let accepted = match &decision.adapters {
                Ok(adapters) if adapters.is_empty() => consumer_sink.query_accept_caps(&caps),
                // The download goes in with the CAPS event.
                Ok(_) => true,
                Err(problem) => {
                    // A refused ACCEPT_CAPS means the CAPS event never comes:
                    // upstream fails not-negotiated with "Internal data stream
                    // error" and nothing else. So the reason is posted here,
                    // for fixed caps only (what a producer is about to send,
                    // not a range it is exploring), as WHEP Output's bridge
                    // does (#985).
                    if caps.is_fixed() && !front.refused.swap(true, Ordering::AcqRel) {
                        post_problem(front, pad, &caps, &decision.takes, problem);
                    }
                    false
                }
            };
            q.set_result(accepted);
            gst::PadProbeReturn::Handled
        }
        gst::QueryViewMut::Caps(q) => {
            let filter = q.filter_owned();
            let answer = answer_caps(&consumer_sink, filter.as_ref());
            q.set_result(&answer);
            gst::PadProbeReturn::Handled
        }
        _ => gst::PadProbeReturn::Ok,
    }
}

/// The input's answer to a CAPS query with `filter` (see the module docs).
fn answer_caps(consumer_sink: &gst::Pad, filter: Option<&gst::Caps>) -> gst::Caps {
    let mut answer = consumer_sink.query_caps(filter);
    if filter.is_some() && !answer.is_empty() {
        return answer;
    }
    let takes = match filter {
        None => answer.clone(),
        Some(_) => consumer_sink.query_caps(None),
    };
    for download in Adapter::DOWNLOADS {
        if let Some(alternative) = downloadable(&takes, download, filter) {
            if !alternative.is_subset(&answer) {
                answer.merge(alternative);
            }
        }
    }
    answer
}

/// The system-memory raw video in `takes`, in the memory `download` takes,
/// limited to what its element takes and to `filter`. `None` when the download
/// is not installed or there is nothing to add.
fn downloadable(
    takes: &gst::CapsRef,
    download: Adapter,
    filter: Option<&gst::Caps>,
) -> Option<gst::Caps> {
    let feature = download.memory_feature()?;
    let sink_template = download.sink_template()?;
    let mut in_memory = gst::Caps::new_empty();
    {
        let in_memory = in_memory.get_mut().expect("new caps are writable");
        for (structure, features) in takes.iter_with_features() {
            if structure.name() == "video/x-raw"
                && !features.is_any()
                && video_adapt::is_system_memory(features)
            {
                in_memory.append_structure_full(
                    structure.to_owned(),
                    Some(gst::CapsFeatures::new([feature])),
                );
            }
        }
    }
    let mut in_memory = in_memory.intersect_with_mode(sink_template, gst::CapsIntersectMode::First);
    if let Some(filter) = filter {
        in_memory = filter.intersect_with_mode(&in_memory, gst::CapsIntersectMode::First);
    }
    (!in_memory.is_empty()).then_some(in_memory)
}

/// A CAPS event arriving at the front element, before it handles it: bring
/// the adapters between it and the consumer in line with `caps`.
fn on_caps(front: &Front, pad: &gst::Pad, caps: &gst::CapsRef) {
    let (Some(front_src), Some(consumer_sink)) =
        (front.front_src.upgrade(), front.consumer_sink.upgrade())
    else {
        return;
    };
    let decision = decide(&consumer_sink, caps);
    let wanted = match decision.adapters {
        Ok(wanted) => wanted,
        Err(problem) => {
            if !front.refused.swap(true, Ordering::AcqRel) {
                post_problem(front, pad, caps, &decision.takes, &problem);
            }
            return;
        }
    };
    front.refused.store(false, Ordering::Release);

    let mut spliced = front.spliced.lock().unwrap_or_else(|p| p.into_inner());
    if wanted == spliced.adapters {
        return;
    }
    let old: Vec<gst::Element> = spliced
        .elements
        .iter()
        .filter_map(|weak| weak.upgrade())
        .collect();
    if old.len() != spliced.elements.len() {
        // The pipeline is being torn down under us.
        return;
    }
    let prefix = match spliced.generation {
        0 => front.label.clone(),
        n => format!("{}_{}", front.label, n),
    };
    let new = match video_adapt::build_elements(&wanted, &prefix) {
        Ok(new) => new,
        Err(e) => {
            post_splice_failure(front, pad, caps, &e);
            return;
        }
    };
    match resplice(&front_src, &consumer_sink, &old, &new) {
        Ok(()) => {
            if new.is_empty() {
                info!(
                    "{}: input {} needs no download, removed the adapters",
                    front.label, caps
                );
            } else {
                info!(
                    "{}: {} does not take input {}, inserted {}",
                    front.label,
                    front.consumer,
                    caps,
                    new.iter()
                        .map(|e| e.name().to_string())
                        .collect::<Vec<_>>()
                        .join(" ! ")
                );
                spliced.generation += 1;
            }
            spliced.elements = new.iter().map(|e| e.downgrade()).collect();
            spliced.adapters = wanted;
        }
        Err(e) => post_splice_failure(front, pad, caps, &e),
    }
}

/// The readable name of the memory a caps feature names.
fn memory_name(feature: &str) -> &str {
    match feature {
        video_adapt::GL_MEMORY_FEATURE => "GL memory",
        video_adapt::CUDA_MEMORY_FEATURE => "CUDA memory",
        video_adapt::D3D12_MEMORY_FEATURE => "D3D12 memory",
        video_adapt::D3D11_MEMORY_FEATURE => "D3D11 memory",
        other => other,
    }
}

/// The memory types `takes` lists for raw video, readable: "system memory,
/// CUDA memory and GL memory".
fn memory_types(takes: &gst::CapsRef) -> String {
    if takes.is_any() {
        return "any memory".to_string();
    }
    let mut names: Vec<&str> = Vec::new();
    for (structure, features) in takes.iter_with_features() {
        if structure.name() != "video/x-raw" {
            continue;
        }
        let name = if features.is_any() {
            "any memory"
        } else if video_adapt::is_system_memory(features) {
            "system memory"
        } else {
            features
                .iter()
                .find(|f| f.starts_with("memory:"))
                .map(|f| memory_name(f.as_str()))
                .unwrap_or("system memory")
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    match names.as_slice() {
        [] => "no raw video".to_string(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
    }
}

/// Fail the flow because nothing here can adapt `caps` for the consumer, whose
/// caps are `takes`.
fn post_problem(
    front: &Front,
    pad: &gst::Pad,
    caps: &gst::CapsRef,
    takes: &gst::CapsRef,
    refusal: &Refusal,
) {
    let (label, consumer) = (&front.label, &front.consumer);
    let Some(element) = pad.parent_element() else {
        warn!("{}: input {} refused: {:?}", label, caps, refusal);
        return;
    };
    match refusal {
        Refusal::NotDownloadable { download } => {
            let factory = download.factory();
            let memory = download
                .memory_feature()
                .map(memory_name)
                .unwrap_or("this memory");
            gst::element_error!(
                element,
                gst::CoreError::Negotiation,
                (
                    "{}: {} receives raw video in {} that {} cannot download",
                    label,
                    consumer,
                    memory,
                    factory
                ),
                [
                    "Arrived: {}. {} takes {} and does not take this directly, and {} \
                     does not take this pixel format. Feed it in a format {} takes, or in \
                     system memory.",
                    caps,
                    consumer,
                    memory_types(takes),
                    factory,
                    factory
                ]
            );
        }
        Refusal::MissingFactory { factory } => {
            let download = Adapter::download_from(factory);
            let memory = download
                .and_then(Adapter::memory_feature)
                .map(memory_name)
                .unwrap_or("this memory");
            let plugin = download.map(Adapter::plugin).unwrap_or("that provides it");
            gst::element_error!(
                element,
                gst::CoreError::MissingPlugin,
                (
                    "{}: {} receives raw video in {}, which it can only take through {}, and {} is not installed",
                    label,
                    consumer,
                    memory,
                    factory,
                    factory
                ),
                [
                    "Arrived: {}. {} does not take {} directly, and downloading it needs {} \
                     from the GStreamer {} plugin. Install it, or feed this input from a source \
                     that outputs system memory.",
                    caps,
                    consumer,
                    memory,
                    factory,
                    plugin
                ]
            );
        }
        Refusal::UnsupportedMemory { feature } => gst::element_error!(
            element,
            gst::CoreError::Negotiation,
            (
                "{}: {} receives raw video in {}, which it does not take",
                label,
                consumer,
                memory_name(feature)
            ),
            [
                "Arrived: {}. {} takes raw video in {}, and nothing here turns {} into \
                 that. Feed it from a source that outputs {}.",
                caps,
                consumer,
                memory_types(takes),
                feature,
                memory_types(takes)
            ]
        ),
        Refusal::NotRawVideo { media } => gst::element_error!(
            element,
            gst::CoreError::Negotiation,
            (
                "{}: {} receives {}, which is not raw video",
                label,
                consumer,
                media
            ),
            [
                "Arrived: {}. {} encodes raw video. Connect a decoded video output to it, \
                 or link the encoded stream past it.",
                caps,
                consumer
            ]
        ),
    }
}

/// Fail the flow because the downloads `caps` need could not be put in place.
fn post_splice_failure(front: &Front, pad: &gst::Pad, caps: &gst::CapsRef, reason: &str) {
    let Some(element) = pad.parent_element() else {
        warn!(
            "{}: input {} could not be adapted: {}",
            front.label, caps, reason
        );
        return;
    };
    gst::element_error!(
        element,
        gst::CoreError::Negotiation,
        (
            "{}: {} could not adapt its video input",
            front.label,
            front.consumer
        ),
        [
            "Arrived: {}. Inserting the download it needs failed: {}",
            caps,
            reason
        ]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    /// A pad answering caps queries with `takes`: a capsfilter's sink pad.
    fn consumer(takes: &str) -> (gst::Element, gst::Pad) {
        let _ = gst::init();
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", caps(takes))
            .build()
            .expect("capsfilter");
        let sink = filter.static_pad("sink").expect("sink pad");
        (filter, sink)
    }

    /// A query that offers what the consumer takes directly gets the
    /// consumer's own answer: a decoder that can choose is not steered to a
    /// download.
    #[test]
    fn a_query_the_consumer_answers_is_not_widened() {
        let (_filter, sink) = consumer("video/x-raw, format=NV12");
        let filter = caps("video/x-raw(memory:GLMemory); video/x-raw");
        let answer = answer_caps(&sink, Some(&filter));
        assert!(
            answer
                .iter_with_features()
                .all(|(_, f)| !f.contains(video_adapt::GL_MEMORY_FEATURE)),
            "{answer}"
        );
    }

    /// A link check (no filter) and a producer that offers only GPU memory
    /// are told the consumer takes it through a download, after what it takes
    /// directly.
    #[test]
    fn a_link_check_lists_downloadable_memory_last() {
        let _ = gst::init();
        if !video_adapt::factory_available(video_adapt::GL_DOWNLOAD_FACTORY) {
            return;
        }
        let (_filter, sink) = consumer("video/x-raw, format=NV12");
        let answer = answer_caps(&sink, None);
        assert!(
            answer
                .features(0)
                .is_some_and(|f| !f.contains(video_adapt::GL_MEMORY_FEATURE)),
            "what the consumer takes directly should come first: {answer}"
        );
        assert!(answer.can_intersect(&caps("video/x-raw(memory:GLMemory), format=NV12")));

        let gl_only = caps("video/x-raw(memory:GLMemory), format=NV12");
        let answer = answer_caps(&sink, Some(&gl_only));
        assert!(!answer.is_empty(), "GL memory should be offered a download");
    }

    /// The memory list in a refusal says what the encoder takes, not what
    /// arrived.
    #[test]
    fn memory_types_lists_what_the_consumer_takes() {
        assert_eq!(
            memory_types(&caps(
                "video/x-raw(memory:VAMemory); video/x-raw(memory:DMABuf)"
            )),
            "memory:VAMemory and memory:DMABuf"
        );
        assert_eq!(
            memory_types(&caps(
                "video/x-raw(memory:CUDAMemory); video/x-raw(memory:GLMemory); video/x-raw"
            )),
            "CUDA memory, GL memory and system memory"
        );
    }

    /// The front is a silent `identity` watched by [`install`], and the
    /// converter a plain `videoconvert` with the configured thread count.
    #[test]
    fn convert_front_builds_a_watched_identity_and_a_threaded_videoconvert() {
        let _ = gst::init();
        let built = convert_front("enc:input", "enc:videoconvert", "Test").expect("front");
        assert_eq!(built.front.name(), "enc:input");
        assert_eq!(built.convert.name(), "enc:videoconvert");
        assert_eq!(
            built
                .front
                .factory()
                .map(|f| f.name().to_string())
                .as_deref(),
            Some("identity")
        );
        assert!(built.front.property::<bool>("silent"));
        assert_eq!(
            built
                .convert
                .factory()
                .map(|f| f.name().to_string())
                .as_deref(),
            Some(VIDEO_CONVERT_FACTORY)
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            built.convert.property::<u32>("n-threads"),
            crate::gpu::video_convert_threads(),
            "configure_video_convert did not reach the converter"
        );
        // Encoded video is refused at once by the installed front; a bare
        // identity would ask its unlinked peer instead.
        let mut query = gst::query::AcceptCaps::new(&caps("video/x-h264"));
        let front_sink = built.front.static_pad("sink").expect("sink pad");
        assert!(!front_sink.query(&mut query) || !query.result());
        assert_eq!(
            built.link(),
            (
                ElementPadRef::pad("enc:input", "src"),
                ElementPadRef::pad("enc:videoconvert", "sink"),
            )
        );
    }

    #[test]
    fn a_download_is_pinned_to_system_memory() {
        // What the front builds for GL memory into a software encoder.
        let encoder = caps("video/x-raw, format=I420");
        let gl = caps("video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240");
        let wanted = video_adapt::decide(
            &gl,
            Consumer::Exactly(&encoder),
            video_adapt::factory_available,
        )
        .expect("GL memory is downloaded");
        let elements = video_adapt::build_elements(&wanted, "venc:input").expect("adapters");
        let factories: Vec<String> = elements
            .iter()
            .map(|e| {
                e.factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(factories, ["gldownload", "capsfilter"]);
        assert_eq!(
            elements[1].property::<gst::Caps>("caps"),
            gst::Caps::new_empty_simple("video/x-raw")
        );
        assert!(video_adapt::build_elements(&[], "venc:input")
            .expect("adapters")
            .is_empty());
    }
}
