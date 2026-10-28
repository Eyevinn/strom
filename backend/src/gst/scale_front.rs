//! Caps-driven scaling and conversion for a block that scales and converts
//! video where it already is (Video Format).
//!
//! A block emits the memory type it naturally produces, and the consuming
//! block adapts its own input. A block that scales and converts has no memory
//! of its own: it works where the video arrives. What arrives is often known
//! only at runtime (`decodebin` picks the decoder), so the chain is chosen
//! from the negotiated caps:
//!
//! | Arrives in | Scaled and converted by | Leaves in |
//! |---|---|---|
//! | system memory | `videoscale ! videoconvert` | system memory |
//! | GL memory | `glcolorconvert ! glcolorscale ! glcolorconvert` | GL memory |
//! | CUDA memory | `cudaconvertscale` | CUDA memory |
//! | D3D12 memory | `d3d12convert` | D3D12 memory |
//! | D3D11 memory | `d3d11convert` | D3D11 memory |
//!
//! When what is downstream does not take that memory at the block's output
//! size and format, the scaled frame is downloaded after the GPU scaler, then
//! converted on the CPU: a 4K frame is scaled down on the GPU before it
//! crosses the bus. When the GPU scaler is not installed, the frame is
//! downloaded first and scaled on the CPU. Memory nothing here scales or
//! downloads, a download that is not installed, or encoded video, fail the
//! flow with an element error that names what arrived and what is missing.
//!
//! The chain is what [`video_adapt::decide`] calls for with
//! [`Consumer::ScaleConvert`]; this module splices it in and answers queries.
//!
//! [`install`] works between two `identity` elements, `input` and `output`,
//! with the chain spliced in between them. The probes fire per query and per
//! event, never per buffer:
//!
//! * On `input`'s sink pad, a CAPS event is handled before `input` sees it, on
//!   the streaming thread that carries the buffers, so no buffer crosses the
//!   link while the chain is replaced (the same in-probe splice as
//!   [`crate::gst::video_input_bridge`]).
//! * On `input`'s sink pad, an ACCEPT_CAPS query is answered for the chain as
//!   it will be after the CAPS event, and a CAPS query with what some chain
//!   takes: first the GPU memory that downstream takes as it is, then system
//!   memory, then GPU memory that has to be downloaded.
//! * On `output`'s source pad, a CAPS query from downstream is answered with
//!   what upstream offers at any size and pixel format, in the memory the
//!   chain can deliver it in. Whatever chain is spliced in at the time, and
//!   whatever order the flow's links are made in, the block links to what it
//!   can produce.

use crate::gst::video_adapt::{
    self, factory_template, in_memory, Adapter, Consumer, Refusal, Scaler,
};
use crate::gst::video_input_bridge::resplice;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Fields a scaler and converter can change.
const ADAPTABLE_FIELDS: [&str; 6] = [
    "width",
    "height",
    "format",
    "pixel-aspect-ratio",
    "colorimetry",
    "chroma-site",
];

/// The chain for `caps`, given what the block's output takes now
/// (`output_takes`). `available` says whether an element factory exists.
fn plan(
    caps: &gst::CapsRef,
    output_takes: &gst::CapsRef,
    available: impl Fn(&str) -> bool,
) -> Result<Vec<Adapter>, Refusal> {
    video_adapt::decide(caps, Consumer::ScaleConvert(output_takes), available)
}

/// True when the chain's first element takes `caps`.
fn chain_takes(chain: &[Adapter], caps: &gst::CapsRef) -> bool {
    chain.first().is_none_or(|first| first.sink_takes(caps))
}

/// True when `chain` downloads.
fn downloads(chain: &[Adapter]) -> bool {
    chain.iter().any(|a| a.memory_feature().is_some())
}

/// Build the elements for `chain`, with the thread count the block sets on
/// its converters.
fn build(chain: &[Adapter], prefix: &str) -> Result<Vec<gst::Element>, String> {
    let elements = video_adapt::build_elements(chain, prefix)?;
    for element in &elements {
        crate::gpu::configure_video_convert(element);
    }
    Ok(elements)
}

/// The chain spliced in between `input` and `output`.
struct Spliced {
    chain: Vec<Adapter>,
    elements: Vec<glib::WeakRef<gst::Element>>,
    /// How many chains have been spliced in, so each gets its own names: the
    /// old chain is still in the bin while its replacement is linked.
    generation: u32,
}

/// What one block's front knows. Weak handles only: the probes that share it
/// are owned by pads of the same pipeline.
struct Front {
    /// Names the input in logs, errors and the chain's element names.
    label: String,
    /// Names the block in errors, e.g. `Video Format`.
    block: String,
    input_src: glib::WeakRef<gst::Pad>,
    input_sink: glib::WeakRef<gst::Pad>,
    output_sink: glib::WeakRef<gst::Pad>,
    /// Taken per CAPS event only, on the streaming thread.
    spliced: Mutex<Spliced>,
    /// A refusal has been posted for the current caps; cleared once a chain
    /// is in place, so a later refusal is reported again.
    refused: AtomicBool,
}

/// Choose and splice the chain between `input` and `output` from the caps
/// that arrive at `input`.
///
/// `input` and `output` are `identity` elements with `silent=true`. `system`
/// is the chain the block built and linked between them, `videoscale !
/// videoconvert`, used until caps say otherwise. `label` names the input in
/// logs, errors and the inserted elements; `block` names the block in errors.
pub fn install(
    input: &gst::Element,
    output: &gst::Element,
    system: &[gst::Element],
    label: &str,
    block: &str,
) {
    let (Some(input_sink), Some(input_src), Some(output_sink), Some(output_src)) = (
        input.static_pad("sink"),
        input.static_pad("src"),
        output.static_pad("sink"),
        output.static_pad("src"),
    ) else {
        warn!(
            "{}: the block's input or output element lacks a pad, so its chain is not chosen \
             from the caps",
            label
        );
        return;
    };
    let state = Arc::new(Front {
        label: label.to_string(),
        block: block.to_string(),
        input_src: input_src.downgrade(),
        input_sink: input_sink.downgrade(),
        output_sink: output_sink.downgrade(),
        spliced: Mutex::new(Spliced {
            chain: video_adapt::CPU_SCALE.to_vec(),
            elements: system.iter().map(|e| e.downgrade()).collect(),
            generation: 0,
        }),
        refused: AtomicBool::new(false),
    });

    // QUERY_DOWNSTREAM on the input's sink pad: queries from upstream.
    let queries = Arc::clone(&state);
    input_sink.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |pad, info| {
        if info.mask.contains(gst::PadProbeType::PULL) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(gst::PadProbeData::Query(ref mut query)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        on_input_query(&queries, pad, query)
    });

    // QUERY_UPSTREAM on the output's source pad: CAPS queries from downstream.
    let downstream = Arc::clone(&state);
    output_src.add_probe(gst::PadProbeType::QUERY_UPSTREAM, move |_, info| {
        if info.mask.contains(gst::PadProbeType::PULL) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(gst::PadProbeData::Query(ref mut query)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::QueryViewMut::Caps(q) = query.view_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(input_sink) = downstream.input_sink.upgrade() else {
            return gst::PadProbeReturn::Ok;
        };
        let filter = q.filter_owned();
        let offered = input_sink.peer_query_caps(None);
        q.set_result(&deliverable(
            &offered,
            filter.as_ref(),
            video_adapt::factory_available,
        ));
        gst::PadProbeReturn::Handled
    });

    // EVENT_DOWNSTREAM, not BUFFER: per event, and only CAPS is looked at.
    input_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
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

/// What the block's output takes now: its capsfilter's caps, limited to what
/// downstream takes. Asked again for every decision, never cached.
fn output_takes(front: &Front) -> gst::Caps {
    front
        .output_sink
        .upgrade()
        .map(|pad| pad.query_caps(None))
        .unwrap_or_else(gst::Caps::new_any)
}

fn on_input_query(front: &Front, pad: &gst::Pad, query: &mut gst::QueryRef) -> gst::PadProbeReturn {
    match query.view_mut() {
        gst::QueryViewMut::AcceptCaps(q) => {
            let caps = q.caps_owned();
            let accepted = match plan(&caps, &output_takes(front), video_adapt::factory_available) {
                Ok(chain) => chain_takes(&chain, &caps),
                Err(problem) => {
                    // A refused ACCEPT_CAPS means the CAPS event never comes,
                    // so the reason is posted here, for fixed caps only (what
                    // a producer is about to send, not a range it explores).
                    if caps.is_fixed() && !front.refused.swap(true, Ordering::AcqRel) {
                        post_problem(front, pad, &caps, &problem);
                    }
                    false
                }
            };
            q.set_result(accepted);
            gst::PadProbeReturn::Handled
        }
        gst::QueryViewMut::Caps(q) => {
            let filter = q.filter_owned();
            let answer = takes(&output_takes(front), video_adapt::factory_available);
            q.set_result(&match filter {
                Some(filter) => filter.intersect_with_mode(&answer, gst::CapsIntersectMode::First),
                None => answer,
            });
            gst::PadProbeReturn::Handled
        }
        _ => gst::PadProbeReturn::Ok,
    }
}

/// What the block takes on its input: raw video in each memory some chain
/// takes. First the GPU memory downstream takes as it is (a decoder that can
/// choose keeps the frames on the GPU), then system memory, then GPU memory
/// that is downloaded after scaling or before it. Each memory is offered as
/// what its chain's first element takes, when [`video_adapt::decide`] finds a
/// chain for it.
///
/// Each memory that needs no download is offered first as what the output
/// takes as it is: the block's size and format, in downstream's order. A
/// producer that can choose (`videotestsrc`, a source with a scaler of its
/// own) then delivers that and nothing is scaled here. Offered at any size
/// only, such a producer fixates to its own defaults: `videotestsrc` sends
/// 320x240 in the first format it lists, `A444_16LE`, which `videoscale`
/// then pillarboxes into a 16:9 output (#1156, #1157).
fn takes(output_takes: &gst::CapsRef, available: impl Fn(&str) -> bool) -> gst::Caps {
    let mut direct = gst::Caps::new_empty();
    let mut downloaded = gst::Caps::new_empty();
    for scaler in Scaler::ALL {
        // What the scaler takes, or else what its download takes.
        let scaler_installed = scaler.adapter().factories().iter().all(|f| available(f));
        let Some(candidate) = scaler
            .adapter()
            .sink_template()
            .filter(|_| scaler_installed)
            .cloned()
            .or_else(|| {
                scaler
                    .download()
                    .sink_template()
                    .map(|t| in_memory(t, Some(scaler.memory_feature())))
            })
        else {
            continue;
        };
        match plan(&candidate, output_takes, &available) {
            Ok(chain) if downloads(&chain) => downloaded.merge(candidate),
            Ok(_) => direct.merge(candidate),
            Err(_) => {}
        }
    }
    // System memory always has a chain: scaling and converting on the CPU.
    if let Some(system) =
        factory_template("videoscale", gst::PadDirection::Sink).map(|t| in_memory(&t, None))
    {
        direct.merge(system);
    }
    let mut answer = gst::Caps::new_empty();
    // An output that takes anything (unlinked, or being torn down) has no
    // size or format to put first.
    let preferred = if output_takes.is_any() {
        gst::Caps::new_empty()
    } else {
        direct.clone()
    };
    for (structure, features) in preferred.iter_with_features() {
        let mut one = gst::Caps::new_empty();
        one.get_mut()
            .expect("new caps are writable")
            .append_structure_full(structure.to_owned(), Some(features.to_owned()));
        // The output's order and values first, so its size and format win.
        answer.merge(output_takes.intersect_with_mode(&one, gst::CapsIntersectMode::First));
    }
    answer.merge(direct);
    answer.merge(downloaded);
    answer
}

/// What the block can deliver when upstream offers `offered`: each raw video
/// structure at any size and pixel format, in its own memory when
/// [`video_adapt::decide`] keeps it there for an output that takes that
/// memory, and in system memory when it finds a chain for an output that
/// takes system memory. Limited to `filter`.
///
/// A GPU structure `decide()` finds no chain for at all is still offered in
/// its own memory when its scaler is installed, and in system memory when its
/// download is: the link then goes through, and the CAPS event fails the flow
/// with the refusal that says why, instead of a bare NOFORMAT at link time.
fn deliverable(
    offered: &gst::Caps,
    filter: Option<&gst::Caps>,
    available: impl Fn(&str) -> bool,
) -> gst::Caps {
    if offered.is_any() {
        return filter.cloned().unwrap_or_else(gst::Caps::new_any);
    }
    let any_system = gst::Caps::new_empty_simple("video/x-raw");
    let mut answer = gst::Caps::new_empty();
    {
        let answer = answer.get_mut().expect("new caps are writable");
        for (structure, features) in offered.iter_with_features() {
            if structure.name() != "video/x-raw" || features.is_any() {
                continue;
            }
            let mut one = gst::Caps::new_empty();
            one.get_mut()
                .expect("new caps are writable")
                .append_structure_full(structure.to_owned(), Some(features.to_owned()));
            let mut adaptable = structure.to_owned();
            adaptable.remove_fields(ADAPTABLE_FIELDS);
            let scaler = Scaler::for_features(features);
            let stays = scaler.is_some_and(|scaler| {
                let mut any_gpu = gst::Caps::new_empty_simple("video/x-raw");
                any_gpu
                    .get_mut()
                    .expect("new caps are writable")
                    .set_features_simple(Some(gst::CapsFeatures::new([scaler.memory_feature()])));
                plan(&one, &any_gpu, &available).is_ok_and(|chain| !downloads(&chain))
            });
            let to_system = plan(&one, &any_system, &available).is_ok();
            let (stays, to_system) = match scaler {
                // No chain at all: offer what is installed, and let the CAPS
                // event post the refusal.
                Some(scaler) if !stays && !to_system => (
                    scaler.adapter().factories().iter().all(|f| available(f)),
                    available(scaler.download().factory()),
                ),
                _ => (stays, to_system),
            };
            if let (true, Some(scaler)) = (stays, scaler) {
                answer.append_structure_full(
                    adaptable.clone(),
                    Some(gst::CapsFeatures::new([scaler.memory_feature()])),
                );
            }
            if to_system {
                answer.append_structure(adaptable);
            }
        }
    }
    answer.simplify();
    match filter {
        Some(filter) => filter.intersect_with_mode(&answer, gst::CapsIntersectMode::First),
        None => answer,
    }
}

/// A CAPS event arriving at the input, before it handles it: bring the chain
/// between input and output in line with `caps`.
fn on_caps(front: &Front, pad: &gst::Pad, caps: &gst::CapsRef) {
    let (Some(input_src), Some(output_sink)) =
        (front.input_src.upgrade(), front.output_sink.upgrade())
    else {
        return;
    };
    let wanted = match plan(caps, &output_takes(front), video_adapt::factory_available) {
        Ok(chain) => chain,
        Err(problem) => {
            if !front.refused.swap(true, Ordering::AcqRel) {
                post_problem(front, pad, caps, &problem);
            }
            return;
        }
    };
    front.refused.store(false, Ordering::Release);

    let mut spliced = front.spliced.lock().unwrap_or_else(|p| p.into_inner());
    if wanted == spliced.chain {
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
    let prefix = format!("{}_{}", front.label, spliced.generation + 1);
    let new = match build(&wanted, &prefix) {
        Ok(new) => new,
        Err(e) => {
            post_splice_failure(front, pad, caps, &e);
            return;
        }
    };
    match resplice(&input_src, &output_sink, &old, &new) {
        Ok(()) => {
            info!(
                "{}: {} takes input {} through {}",
                front.label,
                front.block,
                caps,
                new.iter()
                    .map(|e| e.name().to_string())
                    .collect::<Vec<_>>()
                    .join(" ! ")
            );
            spliced.generation += 1;
            spliced.elements = new.iter().map(|e| e.downgrade()).collect();
            spliced.chain = wanted;
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

/// Fail the flow because nothing here can take `caps`.
fn post_problem(front: &Front, pad: &gst::Pad, caps: &gst::CapsRef, problem: &Refusal) {
    let (label, block) = (&front.label, &front.block);
    let Some(element) = pad.parent_element() else {
        warn!("{}: input {} refused: {:?}", label, caps, problem);
        return;
    };
    match problem {
        Refusal::NotRawVideo { media } => gst::element_error!(
            element,
            gst::CoreError::Negotiation,
            (
                "{}: {} receives {}, which is not raw video",
                label,
                block,
                media
            ),
            [
                "Arrived: {}. {} scales and converts raw video. Decode the stream before it.",
                caps,
                block
            ]
        ),
        Refusal::UnsupportedMemory { feature } => gst::element_error!(
            element,
            gst::CoreError::Negotiation,
            (
                "{}: {} receives raw video in {}, which it does not take",
                label,
                block,
                memory_name(feature)
            ),
            [
                "Arrived: {}. {} scales and converts raw video in system, GL, CUDA, D3D12 \
                 and D3D11 memory. Feed it from a source that outputs one of those.",
                caps,
                block
            ]
        ),
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
                    "{}: {} receives raw video in {}, and {} is not installed",
                    label,
                    block,
                    memory,
                    factory
                ),
                [
                    "Arrived: {}. What follows {} does not take {}, or {} cannot scale it \
                     here, so it needs {} from the GStreamer {} plugin. Install it, or feed \
                     this input from a source that outputs system memory.",
                    caps,
                    block,
                    memory,
                    block,
                    factory,
                    plugin
                ]
            );
        }
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
                    "{}: {} receives raw video in {} in a pixel format it cannot scale or download",
                    label,
                    block,
                    memory
                ),
                [
                    "Arrived: {}. Neither the GPU scaler for {} nor {} takes this pixel \
                     format here. Feed it in another format, or in system memory.",
                    caps,
                    memory,
                    factory
                ]
            );
        }
    }
}

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
            "{}: {} could not set up scaling for its video input",
            front.label,
            front.block
        ),
        [
            "Arrived: {}. Inserting the chain it needs failed: {}",
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

    const OUT_SIZE: &str = "width=160, height=120, format=I420";

    /// What a Video Format output takes when downstream takes `features`.
    fn output(features: &[&str]) -> gst::Caps {
        let mut out = gst::Caps::new_empty();
        for f in features {
            let entry = if f.is_empty() {
                caps(&format!("video/x-raw, {}", OUT_SIZE))
            } else {
                caps(&format!("video/x-raw({}), {}", f, OUT_SIZE))
            };
            out.merge(entry);
        }
        out
    }

    fn all(_: &str) -> bool {
        true
    }

    /// True when the GL scaler is installed here.
    fn gl_scaler() -> bool {
        let _ = gst::init();
        Scaler::Gl.template(gst::PadDirection::Src).is_some()
    }

    #[test]
    fn system_memory_is_scaled_on_the_cpu() {
        let input = caps("video/x-raw, format=NV12, width=320, height=240");
        assert_eq!(
            plan(&input, &output(&[""]), all),
            Ok(video_adapt::CPU_SCALE.to_vec())
        );
    }

    /// GL memory stays on the GPU when downstream takes GL memory.
    #[test]
    fn gl_memory_is_scaled_on_the_gpu_for_a_gl_consumer() {
        if !gl_scaler() {
            return;
        }
        let input = caps("video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240");
        let out = output(&["", video_adapt::GL_MEMORY_FEATURE]);
        assert_eq!(plan(&input, &out, all), Ok(vec![Adapter::GlScale]));
    }

    /// GL memory is scaled on the GPU, then downloaded, for a consumer that
    /// takes system memory only: the smaller frame crosses the bus.
    #[test]
    fn gl_memory_is_scaled_then_downloaded_for_a_system_memory_consumer() {
        if !gl_scaler() {
            return;
        }
        let input = caps("video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240");
        assert_eq!(
            plan(&input, &output(&[""]), all),
            Ok(vec![
                Adapter::GlScale,
                Adapter::GlDownload,
                Adapter::SystemMemoryPin,
                Adapter::VideoConvert
            ])
        );
    }

    /// Without the GPU scaler, the frame is downloaded and scaled on the CPU.
    #[test]
    fn a_missing_scaler_downloads_first() {
        let input = caps("video/x-raw(memory:CUDAMemory), format=NV12, width=320, height=240");
        let no_scaler = |f: &str| f != "cudaconvertscale";
        assert_eq!(
            plan(&input, &output(&[""]), no_scaler),
            Ok(vec![
                Adapter::CudaDownload,
                Adapter::SystemMemoryPin,
                Adapter::VideoScale,
                Adapter::VideoConvert
            ])
        );
        assert_eq!(
            plan(&input, &output(&[""]), |_| false),
            Err(Refusal::MissingFactory {
                factory: video_adapt::CUDA_DOWNLOAD_FACTORY
            })
        );
    }

    /// A GPU scaler that does not take the input (not installed, or not this
    /// pixel format) means download first, never a silent refusal.
    #[test]
    fn a_scaler_that_does_not_take_the_caps_downloads_first() {
        let _ = gst::init();
        if video_adapt::factory_available("cudaconvertscale") {
            return;
        }
        // `available` claims the scaler is there; its template is not.
        let input = caps("video/x-raw(memory:CUDAMemory), format=NV12, width=320, height=240");
        let out = output(&["", video_adapt::CUDA_MEMORY_FEATURE]);
        assert_eq!(
            plan(&input, &out, all),
            Ok(vec![
                Adapter::CudaDownload,
                Adapter::SystemMemoryPin,
                Adapter::VideoScale,
                Adapter::VideoConvert
            ])
        );
    }

    /// GPU memory downstream does not take is not offered when its download
    /// is missing: a decoder that can choose picks system memory instead.
    #[test]
    fn gpu_memory_without_its_download_is_not_offered() {
        if !gl_scaler() {
            return;
        }
        let no_download = |f: &str| f != video_adapt::GL_DOWNLOAD_FACTORY;
        let answer = takes(&output(&[""]), no_download);
        assert!(
            answer
                .iter_with_features()
                .all(|(_, f)| !f.contains(video_adapt::GL_MEMORY_FEATURE)),
            "{answer}"
        );
        let answer = takes(&output(&[""]), all);
        assert!(answer.can_intersect(&caps(
            "video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240"
        )));
    }

    #[test]
    fn memory_nothing_scales_or_downloads_is_refused() {
        let input = caps("video/x-raw(memory:NVMM), format=NV12, width=320, height=240");
        assert_eq!(
            plan(&input, &output(&[""]), all),
            Err(Refusal::UnsupportedMemory {
                feature: "memory:NVMM".to_string()
            })
        );
    }

    #[test]
    fn encoded_video_is_refused() {
        let input = caps("video/x-h264, stream-format=byte-stream");
        assert_eq!(
            plan(&input, &output(&[""]), all),
            Err(Refusal::NotRawVideo {
                media: "video/x-h264".to_string()
            })
        );
    }

    /// Downstream asking what the block delivers is told any size and format,
    /// in the memory upstream offers and, where it downloads, system memory.
    #[test]
    fn downstream_is_offered_any_size_in_each_memory_the_chain_delivers() {
        let offered = caps("video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240");
        let answer = deliverable(&offered, None, all);
        assert!(answer.can_intersect(&output(&[video_adapt::GL_MEMORY_FEATURE])));
        assert!(answer.can_intersect(&output(&[""])));

        let no_gl_scaler = |f: &str| f != "glcolorscale";
        let answer = deliverable(&offered, None, no_gl_scaler);
        assert!(!answer.can_intersect(&output(&[video_adapt::GL_MEMORY_FEATURE])));
        assert!(answer.can_intersect(&output(&[""])));

        let nvmm = caps("video/x-raw(memory:NVMM), format=NV12");
        assert!(deliverable(&nvmm, None, all).is_empty());
    }

    /// GL memory in a pixel format the GPU scaler does not take is not
    /// offered downstream as staying in GL memory: `decide()` downloads it
    /// first. It is still offered in system memory.
    #[test]
    fn gpu_memory_the_scaler_does_not_take_is_offered_in_system_memory_only() {
        if !gl_scaler() {
            return;
        }
        // `glcolorconvert` does not list AYUV64; `gldownload` takes any GL.
        let offered = caps("video/x-raw(memory:GLMemory), format=AYUV64, width=320, height=240");
        let answer = deliverable(&offered, None, all);
        assert!(
            !answer.can_intersect(&output(&[video_adapt::GL_MEMORY_FEATURE])),
            "{answer}"
        );
        assert!(answer.can_intersect(&output(&[""])), "{answer}");
    }

    /// GPU memory `decide()` finds no chain for is still offered as what is
    /// installed, so the link goes through and the CAPS event posts the
    /// refusal, instead of a bare NOFORMAT at link time. Here the CUDA scaler
    /// is claimed but takes nothing (no template here), and `cudadownload` is
    /// missing.
    #[test]
    fn gpu_memory_with_no_chain_still_links() {
        let _ = gst::init();
        if video_adapt::factory_available("cudaconvertscale") {
            return;
        }
        let offered = caps("video/x-raw(memory:CUDAMemory), format=NV12, width=320, height=240");
        let no_download = |f: &str| f != video_adapt::CUDA_DOWNLOAD_FACTORY;
        assert!(
            plan(&offered, &output(&[""]), no_download).is_err(),
            "the CAPS event refuses this"
        );
        let answer = deliverable(&offered, None, no_download);
        assert!(
            answer.can_intersect(&output(&[video_adapt::CUDA_MEMORY_FEATURE])),
            "{answer}"
        );
    }

    /// A decoder that can choose is told GPU memory downstream takes first,
    /// then system memory, then GPU memory that gets downloaded.
    #[test]
    fn the_input_lists_gpu_memory_downstream_takes_first() {
        if !gl_scaler() {
            return;
        }
        let first = |out: &gst::Caps| {
            let answer = takes(out, all);
            answer
                .features(0)
                .is_some_and(|f| f.contains(video_adapt::GL_MEMORY_FEATURE))
        };
        assert!(first(&output(&["", video_adapt::GL_MEMORY_FEATURE])));
        assert!(!first(&output(&[""])));
    }

    /// A producer that can choose is offered the output's size and format
    /// before any size: offered any size first, `videotestsrc` fixates to
    /// 320x240 in `A444_16LE` and is pillarboxed (#1156, #1157).
    #[test]
    fn the_input_offers_the_output_size_and_format_first() {
        let _ = gst::init();
        let answer = takes(&output(&[""]), all);
        let first = answer.structure(0).expect("an answer");
        assert_eq!(first.get::<i32>("width"), Ok(160), "{answer}");
        assert_eq!(first.get::<i32>("height"), Ok(120), "{answer}");
        assert_eq!(first.get::<&str>("format"), Ok("I420"), "{answer}");
        assert!(
            answer.can_intersect(&caps("video/x-raw, format=NV12, width=320, height=240")),
            "a producer that cannot choose still links: {answer}"
        );
    }
}
