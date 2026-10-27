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

use crate::gst::video_adapt::{self, Adapter};
use crate::gst::video_input_bridge::resplice;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The CPU scaler and converter, with the thread count the block sets.
const SYSTEM_FACTORIES: [&str; 2] = ["videoscale", video_adapt::VIDEO_CONVERT_FACTORY];

/// Fields a scaler and converter can change.
const ADAPTABLE_FIELDS: [&str; 6] = [
    "width",
    "height",
    "format",
    "pixel-aspect-ratio",
    "colorimetry",
    "chroma-site",
];

/// A GPU scaler and converter, working in one memory type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaler {
    Gl,
    Cuda,
    D3d12,
    D3d11,
}

impl Scaler {
    pub const ALL: [Scaler; 4] = [Scaler::Gl, Scaler::Cuda, Scaler::D3d12, Scaler::D3d11];

    /// The element factories, in link order. `glcolorscale` takes RGBA only,
    /// so GL converts to it and back.
    pub fn factories(self) -> &'static [&'static str] {
        match self {
            Scaler::Gl => &["glcolorconvert", "glcolorscale", "glcolorconvert"],
            Scaler::Cuda => &["cudaconvertscale"],
            Scaler::D3d12 => &["d3d12convert"],
            Scaler::D3d11 => &["d3d11convert"],
        }
    }

    /// The caps feature of the memory it works in.
    pub fn memory_feature(self) -> &'static str {
        self.download()
            .memory_feature()
            .expect("every scaler's memory has a download")
    }

    /// The download out of its memory.
    pub fn download(self) -> Adapter {
        match self {
            Scaler::Gl => Adapter::GlDownload,
            Scaler::Cuda => Adapter::CudaDownload,
            Scaler::D3d12 => Adapter::D3d12Download,
            Scaler::D3d11 => Adapter::D3d11Download,
        }
    }

    /// The scaler for the memory `features` name, if there is one.
    fn for_features(features: &gst::CapsFeaturesRef) -> Option<Scaler> {
        Scaler::ALL
            .into_iter()
            .find(|s| features.contains(s.memory_feature()))
    }

    /// The pad template of its first (sink) or last (src) element, in its
    /// memory only. `None` when an element is not installed.
    fn template(self, direction: gst::PadDirection) -> Option<gst::Caps> {
        let factories = self.factories();
        if !factories.iter().all(|f| video_adapt::factory_available(f)) {
            return None;
        }
        let factory = match direction {
            gst::PadDirection::Sink => factories[0],
            _ => factories[factories.len() - 1],
        };
        let template = factory_template(factory, direction)?;
        Some(in_memory(&template, Some(self.memory_feature())))
    }
}

/// The chain between `input` and `output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// `videoscale ! videoconvert`.
    System,
    /// The download, pinned to system memory, then `videoscale !
    /// videoconvert`: the GPU scaler is not installed.
    DownloadFirst(Adapter),
    /// The GPU scaler, then, when downstream does not take its memory, the
    /// download, pinned to system memory, and `videoconvert`.
    Gpu {
        scaler: Scaler,
        download: Option<Adapter>,
    },
}

/// Why the input cannot be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    /// Not raw video.
    NotRawVideo { media: String },
    /// Raw video in a memory nothing here scales or downloads.
    UnsupportedMemory { feature: String },
    /// The download this memory needs is not installed.
    MissingDownload(Adapter),
    /// Neither the GPU scaler nor the download takes this pixel format.
    NotDownloadable(Adapter),
}

/// Choose the chain for `caps`, given what the block's output takes now
/// (`output_takes`: its capsfilter's caps, limited to what downstream takes).
/// `available` says whether an element factory exists.
fn plan(
    caps: &gst::CapsRef,
    output_takes: &gst::CapsRef,
    available: impl Fn(&str) -> bool,
) -> Result<Path, Problem> {
    let (Some(structure), Some(features)) = (caps.structure(0), caps.features(0)) else {
        return Err(Problem::NotRawVideo {
            media: "empty caps".to_string(),
        });
    };
    if structure.name() != "video/x-raw" {
        return Err(Problem::NotRawVideo {
            media: structure.name().to_string(),
        });
    }
    if video_adapt::is_system_memory(features) {
        return Ok(Path::System);
    }
    let Some(scaler) = Scaler::for_features(features) else {
        let feature = features
            .iter()
            .find(|f| f.starts_with("memory:"))
            .map(|f| f.to_string())
            .unwrap_or_else(|| "an unknown memory".to_string());
        return Err(Problem::UnsupportedMemory { feature });
    };
    let download = scaler.download();
    // The GPU scaler must be installed and take this pixel format; otherwise
    // the frame is downloaded first and scaled on the CPU.
    let scaler_takes = scaler.factories().iter().all(|f| available(f))
        && scaler
            .template(gst::PadDirection::Sink)
            .is_some_and(|sink| caps.can_intersect(&sink));
    if !scaler_takes {
        return if !available(download.factory()) {
            Err(Problem::MissingDownload(download))
        } else if download.sink_template().is_some() && !download.sink_takes(caps) {
            Err(Problem::NotDownloadable(download))
        } else {
            Ok(Path::DownloadFirst(download))
        };
    }
    // Downstream takes the scaler's output as it is: stay on the GPU.
    let in_scaler_memory = in_memory(output_takes, Some(scaler.memory_feature()));
    let producible = scaler
        .template(gst::PadDirection::Src)
        .is_some_and(|src| in_scaler_memory.can_intersect(&src));
    if producible {
        return Ok(Path::Gpu {
            scaler,
            download: None,
        });
    }
    if available(download.factory()) {
        Ok(Path::Gpu {
            scaler,
            download: Some(download),
        })
    } else {
        Err(Problem::MissingDownload(download))
    }
}

impl Path {
    /// The element factories, in link order. A capsfilter pinning system
    /// memory is named `capsfilter`.
    fn factories(self) -> Vec<&'static str> {
        match self {
            Path::System => SYSTEM_FACTORIES.to_vec(),
            Path::DownloadFirst(download) => {
                let mut factories = vec![download.factory(), "capsfilter"];
                factories.extend(SYSTEM_FACTORIES);
                factories
            }
            Path::Gpu { scaler, download } => {
                let mut factories = scaler.factories().to_vec();
                if let Some(download) = download {
                    factories.extend([
                        download.factory(),
                        "capsfilter",
                        video_adapt::VIDEO_CONVERT_FACTORY,
                    ]);
                }
                factories
            }
        }
    }

    /// True when the first element takes `caps`.
    fn takes(self, caps: &gst::CapsRef) -> bool {
        match self {
            Path::System => factory_template("videoscale", gst::PadDirection::Sink)
                .is_some_and(|t| caps.can_intersect(&t)),
            Path::DownloadFirst(download) => download.sink_takes(caps),
            Path::Gpu { scaler, .. } => scaler
                .template(gst::PadDirection::Sink)
                .is_some_and(|t| caps.can_intersect(&t)),
        }
    }
}

/// Build the elements for `path`, named `<prefix>_<factory><index>`.
fn build(path: Path, prefix: &str) -> Result<Vec<gst::Element>, String> {
    path.factories()
        .into_iter()
        .enumerate()
        .map(|(i, factory)| {
            let name = format!("{}_{}{}", prefix, factory, i);
            if factory == "capsfilter" {
                return video_adapt::system_memory_pin(&name);
            }
            let element = gst::ElementFactory::make(factory)
                .name(name)
                .build()
                .map_err(|e| format!("{} could not be created: {}", factory, e))?;
            crate::gpu::configure_video_convert(&element);
            Ok(element)
        })
        .collect()
}

/// The chain spliced in between `input` and `output`.
struct Spliced {
    path: Path,
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
            path: Path::System,
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
                Ok(path) => path.takes(&caps),
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
/// that is downloaded after scaling or before it.
fn takes(output_takes: &gst::CapsRef, available: impl Fn(&str) -> bool) -> gst::Caps {
    let mut direct = gst::Caps::new_empty();
    let mut downloaded = gst::Caps::new_empty();
    for scaler in Scaler::ALL {
        let feature = scaler.memory_feature();
        let download_available = available(scaler.download().factory());
        if let Some(sink) = scaler.template(gst::PadDirection::Sink) {
            if !in_memory(output_takes, Some(feature)).is_empty() {
                direct.merge(sink);
            } else if download_available {
                downloaded.merge(sink);
            }
        } else if download_available {
            if let Some(sink) = scaler.download().sink_template() {
                downloaded.merge(in_memory(sink, Some(feature)));
            }
        }
    }
    if let Some(system) = factory_template("videoscale", gst::PadDirection::Sink) {
        direct.merge(in_memory(&system, None));
    }
    direct.merge(downloaded);
    direct
}

/// What the block can deliver when upstream offers `offered`: each raw video
/// structure at any size and pixel format, in its own memory when that is
/// system memory or a scaler for it is installed, and in system memory when
/// it can be downloaded. Limited to `filter`.
fn deliverable(
    offered: &gst::Caps,
    filter: Option<&gst::Caps>,
    available: impl Fn(&str) -> bool,
) -> gst::Caps {
    if offered.is_any() {
        return filter.cloned().unwrap_or_else(gst::Caps::new_any);
    }
    let mut answer = gst::Caps::new_empty();
    {
        let answer = answer.get_mut().expect("new caps are writable");
        for (structure, features) in offered.iter_with_features() {
            if structure.name() != "video/x-raw" || features.is_any() {
                continue;
            }
            let mut adaptable = structure.to_owned();
            adaptable.remove_fields(ADAPTABLE_FIELDS);
            if video_adapt::is_system_memory(features) {
                answer.append_structure(adaptable);
                continue;
            }
            let Some(scaler) = Scaler::for_features(features) else {
                continue;
            };
            if scaler.factories().iter().all(|f| available(f)) {
                answer.append_structure_full(
                    adaptable.clone(),
                    Some(gst::CapsFeatures::new([scaler.memory_feature()])),
                );
            }
            if available(scaler.download().factory()) {
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

/// The raw video structures of `caps` in the memory `feature` names, or in
/// system memory for `None`.
fn in_memory(caps: &gst::CapsRef, feature: Option<&str>) -> gst::Caps {
    let mut picked = gst::Caps::new_empty();
    {
        let picked = picked.get_mut().expect("new caps are writable");
        for (structure, features) in caps.iter_with_features() {
            let matches = if features.is_any() {
                false
            } else {
                match feature {
                    Some(feature) => features.contains(feature),
                    None => video_adapt::is_system_memory(features),
                }
            };
            if structure.name() == "video/x-raw" && matches {
                picked.append_structure_full(structure.to_owned(), Some(features.to_owned()));
            }
        }
    }
    picked
}

/// The pad template of `factory` in `direction`. `None` when it is not
/// installed.
fn factory_template(factory: &str, direction: gst::PadDirection) -> Option<gst::Caps> {
    gst::ElementFactory::find(factory)?
        .static_pad_templates()
        .into_iter()
        .find(|t| t.direction() == direction)
        .map(|t| t.caps())
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
        Ok(path) => path,
        Err(problem) => {
            if !front.refused.swap(true, Ordering::AcqRel) {
                post_problem(front, pad, caps, &problem);
            }
            return;
        }
    };
    front.refused.store(false, Ordering::Release);

    let mut spliced = front.spliced.lock().unwrap_or_else(|p| p.into_inner());
    if wanted == spliced.path {
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
    let new = match build(wanted, &prefix) {
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
            spliced.path = wanted;
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
fn post_problem(front: &Front, pad: &gst::Pad, caps: &gst::CapsRef, problem: &Problem) {
    let (label, block) = (&front.label, &front.block);
    let Some(element) = pad.parent_element() else {
        warn!("{}: input {} refused: {:?}", label, caps, problem);
        return;
    };
    match problem {
        Problem::NotRawVideo { media } => gst::element_error!(
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
        Problem::UnsupportedMemory { feature } => gst::element_error!(
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
        Problem::MissingDownload(download) => {
            let memory = download
                .memory_feature()
                .map(memory_name)
                .unwrap_or("this memory");
            gst::element_error!(
                element,
                gst::CoreError::MissingPlugin,
                (
                    "{}: {} receives raw video in {}, and {} is not installed",
                    label,
                    block,
                    memory,
                    download.factory()
                ),
                [
                    "Arrived: {}. What follows {} does not take {}, or {} cannot scale it \
                     here, so it needs {} from the GStreamer {} plugin. Install it, or feed \
                     this input from a source that outputs system memory.",
                    caps,
                    block,
                    memory,
                    block,
                    download.factory(),
                    download.plugin()
                ]
            );
        }
        Problem::NotDownloadable(download) => {
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
                    download.factory()
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
        assert_eq!(plan(&input, &output(&[""]), all), Ok(Path::System));
    }

    /// GL memory stays on the GPU when downstream takes GL memory.
    #[test]
    fn gl_memory_is_scaled_on_the_gpu_for_a_gl_consumer() {
        if !gl_scaler() {
            return;
        }
        let input = caps("video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240");
        let out = output(&["", video_adapt::GL_MEMORY_FEATURE]);
        assert_eq!(
            plan(&input, &out, all),
            Ok(Path::Gpu {
                scaler: Scaler::Gl,
                download: None
            })
        );
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
            Ok(Path::Gpu {
                scaler: Scaler::Gl,
                download: Some(Adapter::GlDownload)
            })
        );
    }

    /// Without the GPU scaler, the frame is downloaded and scaled on the CPU.
    #[test]
    fn a_missing_scaler_downloads_first() {
        let input = caps("video/x-raw(memory:CUDAMemory), format=NV12, width=320, height=240");
        let no_scaler = |f: &str| f != "cudaconvertscale";
        assert_eq!(
            plan(&input, &output(&[""]), no_scaler),
            Ok(Path::DownloadFirst(Adapter::CudaDownload))
        );
        assert_eq!(
            plan(&input, &output(&[""]), |_| false),
            Err(Problem::MissingDownload(Adapter::CudaDownload))
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
            Ok(Path::DownloadFirst(Adapter::CudaDownload))
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
            Err(Problem::UnsupportedMemory {
                feature: "memory:NVMM".to_string()
            })
        );
    }

    #[test]
    fn encoded_video_is_refused() {
        let input = caps("video/x-h264, stream-format=byte-stream");
        assert_eq!(
            plan(&input, &output(&[""]), all),
            Err(Problem::NotRawVideo {
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
}
