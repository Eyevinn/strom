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

use crate::gst::video_adapt::{self, Adapter, Consumer, Refusal};
use crate::gst::video_input_bridge::resplice;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// What the consumer needs in front of it to take `caps`.
fn decide(consumer_sink: &gst::Pad, caps: &gst::CapsRef) -> Result<Vec<Adapter>, Refusal> {
    let takes = consumer_sink.query_caps(None);
    video_adapt::decide(
        caps,
        Consumer::Exactly(&takes),
        video_adapt::factory_available,
    )
}

fn on_query(front: &Front, pad: &gst::Pad, query: &mut gst::QueryRef) -> gst::PadProbeReturn {
    let Some(consumer_sink) = front.consumer_sink.upgrade() else {
        // The pipeline is being torn down.
        return gst::PadProbeReturn::Ok;
    };
    match query.view_mut() {
        gst::QueryViewMut::AcceptCaps(q) => {
            let caps = q.caps_owned();
            let accepted = match decide(&consumer_sink, &caps) {
                Ok(adapters) if adapters.is_empty() => consumer_sink.query_accept_caps(&caps),
                // The download goes in with the CAPS event.
                Ok(_) => true,
                Err(refusal) => {
                    if caps.is_fixed() && !front.refused.swap(true, Ordering::AcqRel) {
                        post_refusal(front, pad, &caps, &refusal);
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
    let takes = consumer_sink.query_caps(None);
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
    let factory = gst::ElementFactory::find(download.factory())?;
    let sink_template = factory
        .static_pad_templates()
        .into_iter()
        .find(|t| t.direction() == gst::PadDirection::Sink)?
        .caps();
    let mut in_memory = gst::Caps::new_empty();
    {
        let in_memory = in_memory.get_mut().expect("new caps are writable");
        for (structure, features) in takes.iter_with_features() {
            let system_memory = !features.is_any()
                && (features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
                    || !features.iter().any(|f| f.starts_with("memory:")));
            if structure.name() == "video/x-raw" && system_memory {
                in_memory.append_structure_full(
                    structure.to_owned(),
                    Some(gst::CapsFeatures::new([feature])),
                );
            }
        }
    }
    let mut in_memory =
        in_memory.intersect_with_mode(&sink_template, gst::CapsIntersectMode::First);
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
    let wanted = match decide(&consumer_sink, caps) {
        Ok(wanted) => wanted,
        Err(refusal) => {
            if !front.refused.swap(true, Ordering::AcqRel) {
                post_refusal(front, pad, caps, &refusal);
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
    let new = match build_adapters(&wanted, &prefix) {
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

/// The elements for `adapters`, in link order, ending in a system-memory pin:
/// `gldownload` and `cudadownload` also output GL memory, which the consumer's
/// converter would pass through to an encoder that lists GL memory.
fn build_adapters(adapters: &[Adapter], prefix: &str) -> Result<Vec<gst::Element>, String> {
    let mut elements = video_adapt::build_elements(adapters, prefix)?;
    if !elements.is_empty() {
        elements.push(
            gst::ElementFactory::make("capsfilter")
                .name(format!("{}_system_memory", prefix))
                .property("caps", gst::Caps::new_empty_simple("video/x-raw"))
                .build()
                .map_err(|e| format!("capsfilter could not be created: {}", e))?,
        );
    }
    Ok(elements)
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

/// Fail the flow because nothing here can adapt `caps` for the consumer.
fn post_refusal(front: &Front, pad: &gst::Pad, caps: &gst::CapsRef, refusal: &Refusal) {
    let (label, consumer) = (&front.label, &front.consumer);
    let Some(element) = pad.parent_element() else {
        warn!("{}: input {} refused: {}", label, caps, refusal);
        return;
    };
    match refusal {
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
                "Arrived: {}. {} takes raw video in system memory, or in a GPU memory its \
                 encoder lists, and nothing here downloads {}. Feed it from a source that \
                 outputs system, GL, CUDA, D3D12 or D3D11 memory.",
                caps,
                consumer,
                feature
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

    #[test]
    fn a_download_is_pinned_to_system_memory() {
        let _ = gst::init();
        let elements = build_adapters(&[Adapter::GlDownload], "venc:input").expect("adapters");
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
        assert!(build_adapters(&[], "venc:input")
            .expect("adapters")
            .is_empty());
    }
}
