//! Caps-driven front for the video inputs of a GL consumer.
//!
//! A GL consumer puts `glupload` on each video input. `glupload` takes system
//! memory, GL memory and DMABuf, but not `memory:CUDAMemory`, so a producer that
//! can only offer CUDA memory — an `appsrc` re-pushing frames that `nvh264dec`
//! decoded in another pipeline, a `cudaconvert` — cannot negotiate with it.
//! `cudadownload` is the CUDA-to-GL interop element (not a CPU download): with
//! it in front, the frames reach `glupload` as GL memory and pass straight
//! through.
//!
//! Whether that is needed depends on what the producer offers, which is only
//! known at runtime. [`install`] watches the input's first sink pad and splices
//! the adapter in front of `glupload` when the caps there call for it.
//!
//! The probe watches *queries*, not the CAPS event. Before a CAPS event is sent
//! to this pad, upstream checks that it will be taken: a transform such as
//! `identity` settling its caps asks its peer with an ACCEPT_CAPS query (and a
//! CAPS query filtered by the new caps), and an element that proxies caps
//! (`queue`) forwards the ACCEPT_CAPS check guarding its own sink pad. Those
//! queries reach this pad and are answered by `glupload`; when it refuses,
//! upstream fails not-negotiated and no CAPS event ever arrives here.
//!
//! Pixel formats are adapted too: `glupload` uploads a fixed list from system
//! memory, and a source in a format that is not on it (`AYUV64`, which a
//! ProRes 4444 clip decodes to on macOS, or `GBR_10LE`) needs a `videoconvert`
//! in front. It converts on the CPU, so it is there only while the input needs
//! it. It goes in, and comes out again, on the input's streaming thread, in a
//! probe on the front element's source pad, as a CAPS event leaves it: the
//! frames before the event go through the old chain and the event and the
//! frames after it through the new one. It comes out when the caps are ones
//! `glupload` takes directly, or CUDA memory (`cudadownload` then goes in
//! directly in front of `glupload`, never in front of the converter).
//!
//! The queries that come before the CAPS event are answered for the input as
//! it will be: an ACCEPT_CAPS for a format only the converter takes is
//! accepted while the converter is out, and one for caps the input takes
//! without it is answered by what follows the converter while it is in. The
//! answer to a CAPS query lists what `glupload` (or `cudadownload`) takes
//! directly ahead of what only the converter takes, so a producer that can
//! choose, such as a decoder, still picks GL or CUDA memory or an uploadable
//! format over a CPU conversion. Raw video in system memory in any format is
//! added to the answer, last, only when the query offers nothing the input
//! takes directly, or offers nothing at all (a link check): a query that
//! offers something the input takes directly gets the same answer as before.
//!
//! Only a producer that offers CUDA memory *and nothing else* is adapted. One
//! that also offers system or GL memory (a decoder) picks from what the input
//! answers its CAPS query with. `glupload`'s own answer has no CUDA memory in
//! it, so `nvh264dec` would pick system memory and download every frame, only
//! for `glupload` to upload it again. When the adapter is installed, the answer
//! therefore also lists the input's system-memory caps in CUDA memory, last.
//! A decoder that can output CUDA memory picks it, its ACCEPT_CAPS is then CUDA
//! only, and the adapter goes in: the frames stay on the GPU. A producer that
//! cannot output CUDA memory never chooses the extra entries.

use crate::gst::video_adapt::{self, Adapter, Consumer, CUDA_MEMORY_FEATURE};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The element that brings CUDA memory into GL. Ships with the GStreamer
/// nvcodec plugin, so it exists on NVIDIA builds only.
pub const CUDA_ADAPTER_FACTORY: &str = video_adapt::CUDA_DOWNLOAD_FACTORY;

/// The system-memory raw video caps in `answer`, also listed in CUDA memory:
/// the formats the adapter can bring into GL, within `filter`, the query's
/// filter. `None` when there is nothing to add, or when CUDA memory is
/// already in the answer.
fn cuda_alternative(
    answer: &gst::CapsRef,
    adapter_sink: &gst::CapsRef,
    filter: Option<&gst::CapsRef>,
) -> Option<gst::Caps> {
    if answer.is_any()
        || answer
            .iter_with_features()
            .any(|(_, features)| features.contains(CUDA_MEMORY_FEATURE))
    {
        return None;
    }
    let mut cuda = gst::Caps::new_empty();
    {
        let cuda = cuda.get_mut().expect("new caps are writable");
        for (structure, features) in answer.iter_with_features() {
            let system_memory = features.size() == 0
                || (features.size() == 1 && features.contains("memory:SystemMemory"));
            if structure.name() == "video/x-raw" && system_memory {
                cuda.append_structure_full(
                    structure.to_owned(),
                    Some(gst::CapsFeatures::new([CUDA_MEMORY_FEATURE])),
                );
            }
        }
    }
    let mut cuda = cuda.intersect_with_mode(adapter_sink, gst::CapsIntersectMode::First);
    if let Some(filter) = filter {
        cuda = cuda.intersect_with_mode(filter, gst::CapsIntersectMode::First);
    }
    (!cuda.is_empty()).then_some(cuda)
}

/// `answer`, the input's reply to a CAPS query, with its system-memory raw
/// video caps also listed in CUDA memory, after everything else.
///
/// `adapter_sink` is what the adapter's sink pad takes in CUDA memory, so only
/// formats it can bring into GL are offered. `filter` is the query's filter.
/// `None` when there is nothing to add, or when CUDA memory is already in the
/// answer.
fn with_cuda_alternative(
    answer: &gst::CapsRef,
    adapter_sink: &gst::CapsRef,
    filter: Option<&gst::CapsRef>,
) -> Option<gst::Caps> {
    let cuda = cuda_alternative(answer, adapter_sink, filter)?;
    let mut widened = answer.to_owned();
    widened.make_mut().append(cuda);
    Some(widened)
}

/// The answer to a CAPS query while the input's `videoconvert` is in.
///
/// `direct` is what the element behind the converter takes, unfiltered
/// (`glupload`, or `cudadownload` when that is in too); `via_convert` is the
/// converter's own answer. What `direct` takes comes first, then its CUDA
/// alternative (when `adapter_sink` is given, so `cudadownload` is installed
/// but not yet in), and only then what the converter adds: a producer that can
/// give the input something it takes without a CPU conversion picks that.
fn direct_paths_first(
    direct: &gst::CapsRef,
    via_convert: &gst::CapsRef,
    adapter_sink: Option<&gst::CapsRef>,
    filter: Option<&gst::CapsRef>,
) -> gst::Caps {
    let mut answer = match filter {
        Some(filter) => direct.intersect_with_mode(filter, gst::CapsIntersectMode::First),
        None => direct.to_owned(),
    };
    if let Some(cuda) = adapter_sink.and_then(|sink| cuda_alternative(direct, sink, filter)) {
        answer.merge(cuda);
    }
    answer.merge(via_convert.to_owned());
    answer
}

/// What the adapter's sink pad takes in CUDA memory, or `None` when the adapter
/// is not installed. Looked up once: the registry does not change under a
/// running process.
fn adapter_cuda_sink_caps() -> Option<&'static gst::Caps> {
    static CAPS: std::sync::OnceLock<Option<gst::Caps>> = std::sync::OnceLock::new();
    CAPS.get_or_init(|| {
        let factory = gst::ElementFactory::find(CUDA_ADAPTER_FACTORY)?;
        let template = factory
            .static_pad_templates()
            .into_iter()
            .find(|t| t.direction() == gst::PadDirection::Sink)?
            .caps();
        let mut cuda = gst::Caps::new_empty();
        {
            let cuda = cuda.get_mut().expect("new caps are writable");
            for (structure, features) in template.iter_with_features() {
                if features.contains(CUDA_MEMORY_FEATURE) {
                    cuda.append_structure_full(structure.to_owned(), Some(features.to_owned()));
                }
            }
        }
        (!cuda.is_empty()).then_some(cuda)
    })
    .as_ref()
}

/// The caps a query arriving at the input says the producer offers, if any.
fn offered_caps(query: &gst::QueryRef) -> Option<gst::Caps> {
    match query.view() {
        gst::QueryView::AcceptCaps(q) => Some(q.caps_owned()),
        gst::QueryView::Caps(q) => q.filter_owned(),
        _ => None,
    }
}

/// The name of the factory that made `element`, or `""`.
fn factory_of(element: &gst::Element) -> glib::GString {
    element
        .factory()
        .map(|f| f.name())
        .unwrap_or_else(|| glib::GString::from(""))
}

/// The adapters between the input's front element (whose source pad is
/// `front_src`) and `upload`, in link order.
fn adapters_between(front_src: &gst::Pad, upload: &gst::Element) -> Vec<gst::Element> {
    let mut chain = Vec::new();
    let mut next = front_src.peer().and_then(|p| p.parent_element());
    // Two adapters at most; the bound only guards against a loop.
    while let Some(element) = next {
        if &element == upload || chain.len() > 4 {
            break;
        }
        next = element
            .static_pad("src")
            .and_then(|p| p.peer())
            .and_then(|p| p.parent_element());
        chain.push(element);
    }
    chain
}

/// The adapter made by `factory` between `front_src` and `upload`, if any.
fn adapter_in(front_src: &gst::Pad, upload: &gst::Element, factory: &str) -> Option<gst::Element> {
    adapters_between(front_src, upload)
        .into_iter()
        .find(|e| factory_of(e) == factory)
}

/// What one GL input's front knows. Weak handles only: the probes that share
/// it are owned by pads of the same pipeline.
struct Front {
    label: String,
    upload: glib::WeakRef<gst::Element>,
    /// The front element's source pad: the adapters go in behind it.
    front_src: glib::WeakRef<gst::Pad>,
    /// What `glupload` uploads from system memory. `ANY` when it cannot be
    /// read, which never calls for a converter.
    uploads: gst::Caps,
    /// Queries can arrive from several threads at once. The lock serialises
    /// the query-time `cudadownload` splice; `cuda_in` lets the ones that
    /// lost the race through untouched.
    splice_lock: Mutex<()>,
    /// `cudadownload` is in, on its way in, or reported missing.
    cuda_in: AtomicBool,
    /// `videoconvert` is in. Set and cleared on the streaming thread, where
    /// it goes in and comes out.
    convert_in: AtomicBool,
}

impl Front {
    fn decide(&self, caps: &gst::CapsRef) -> Result<Vec<Adapter>, video_adapt::Refusal> {
        // The registry is only consulted for caps that call for the CUDA
        // adapter.
        video_adapt::decide(
            caps,
            Consumer::GlUpload(&self.uploads),
            video_adapt::factory_available,
        )
    }

    /// The sink pad of the element right behind the input's `videoconvert`,
    /// when that is in: what takes the frames once it is out.
    fn behind_convert(&self) -> Option<gst::Pad> {
        let front_src = self.front_src.upgrade()?;
        let upload = self.upload.upgrade()?;
        adapter_in(&front_src, &upload, video_adapt::VIDEO_CONVERT_FACTORY)?
            .static_pad("src")?
            .peer()
    }
}

/// Watch `front_sink` — the first sink pad of a GL consumer's video input — and
/// splice what the producer turns out to need in front of `upload`: when it
/// offers CUDA memory only, [`CUDA_ADAPTER_FACTORY`]; when it offers system
/// memory only, in a pixel format `glupload` does not upload,
/// [`video_adapt::VIDEO_CONVERT_FACTORY`]. `upload` is the input's `glupload`;
/// `label` names the input in logs and in the error posted when the input
/// cannot be adapted.
///
/// `cudadownload` stays once it is in, as it always has. `videoconvert` comes
/// out again as soon as a CAPS event carries
/// caps the input takes without it (see the module docs).
pub fn install(front_sink: &gst::Pad, upload: &gst::Element, label: &str) {
    let Some(front_src) = front_sink
        .parent_element()
        .and_then(|front| front.static_pad("src"))
    else {
        warn!(
            "{}: the input's front element has no src pad, so its input is not adapted",
            label
        );
        return;
    };
    let front = Arc::new(Front {
        label: label.to_string(),
        upload: upload.downgrade(),
        front_src: front_src.downgrade(),
        uploads: video_adapt::gl_upload_system_caps()
            .cloned()
            .unwrap_or_else(gst::Caps::new_any),
        splice_lock: Mutex::new(()),
        cuda_in: AtomicBool::new(false),
        convert_in: AtomicBool::new(false),
    });

    // QUERY_DOWNSTREAM fires per query, never per buffer. The probe stays for
    // the life of the input, so a producer that switches format mid-stream is
    // caught too.
    let queries = Arc::clone(&front);
    front_sink.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |pad, info| {
        on_query(&queries, pad, info)
    });

    // EVENT_DOWNSTREAM fires per event, never per buffer, and only CAPS
    // events are looked at.
    front_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        if let Some(gst::PadProbeData::Event(ref event)) = info.data {
            if let gst::EventView::Caps(c) = event.view() {
                return on_caps_leaving_front(&front, pad, c.caps());
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// A query arriving at the input's front sink pad, on its way in (`PUSH`) or
/// with its answer (`PULL`).
fn on_query(front: &Front, pad: &gst::Pad, info: &mut gst::PadProbeInfo) -> gst::PadProbeReturn {
    if info.mask.contains(gst::PadProbeType::PULL) {
        if let Some(gst::PadProbeData::Query(query)) = &mut info.data {
            if let gst::QueryViewMut::Caps(q) = query.view_mut() {
                answer_caps_query(front, q);
            }
        }
        return gst::PadProbeReturn::Ok;
    }

    let Some(gst::PadProbeData::Query(query)) = &mut info.data else {
        return gst::PadProbeReturn::Ok;
    };
    let Some(caps) = offered_caps(query) else {
        return gst::PadProbeReturn::Ok;
    };
    let decision = front.decide(&caps);
    let convert_in = front.convert_in.load(Ordering::Acquire);
    let cuda_in = front.cuda_in.load(Ordering::Acquire);

    // The converter goes in and out on the streaming thread, as the CAPS
    // event leaves the front element (see the module docs). Until then, an
    // ACCEPT_CAPS query is answered for the input as it will be then.
    if let (Ok(adapters), gst::QueryViewMut::AcceptCaps(q)) = (&decision, query.view_mut()) {
        let needs_convert = adapters.contains(&Adapter::VideoConvert);
        if needs_convert && !convert_in {
            // Raw video in system memory only, which the converter takes.
            q.set_result(convert_sink_caps().is_some());
            return gst::PadProbeReturn::Handled;
        }
        if !needs_convert && convert_in {
            let accepted = if adapters.contains(&Adapter::CudaDownload) && !cuda_in {
                adapter_cuda_sink_caps().is_some_and(|sink| caps.can_intersect(sink))
            } else {
                front
                    .behind_convert()
                    .is_some_and(|direct| direct.query_accept_caps(&caps))
            };
            q.set_result(accepted);
            return gst::PadProbeReturn::Handled;
        }
    }
    if convert_in {
        // CUDA memory while the converter is in also waits for the CAPS
        // event, so `cudadownload` never lands in front of the converter.
        return gst::PadProbeReturn::Ok;
    }

    // CUDA memory only: `cudadownload` goes in at once.
    let installed = |adapter: &Adapter| match adapter {
        Adapter::CudaDownload => front.cuda_in.load(Ordering::Acquire),
        Adapter::VideoConvert | Adapter::GlDownload => true,
    };
    if decision
        .as_ref()
        .is_ok_and(|adapters| adapters.iter().all(installed))
    {
        return gst::PadProbeReturn::Ok;
    }

    let _guard = front.splice_lock.lock().unwrap_or_else(|p| p.into_inner());
    if front.cuda_in.load(Ordering::Acquire) {
        return gst::PadProbeReturn::Ok;
    }
    let (Some(upload), Some(front_src)) = (front.upload.upgrade(), front.front_src.upgrade())
    else {
        // The pipeline is being torn down.
        return gst::PadProbeReturn::Remove;
    };
    let label = &front.label;
    match decision {
        Ok(_) => {
            match splice_when_idle(
                &front_src,
                &upload,
                CUDA_ADAPTER_FACTORY,
                label,
                SPLICE_WAIT,
            ) {
                Ok(()) => info!(
                    "{}: input offers CUDA memory only ({}), inserted {} in front of {}",
                    label,
                    caps,
                    CUDA_ADAPTER_FACTORY,
                    upload.name()
                ),
                Err(e) => {
                    // Leave the query to fail as it would have: the flow
                    // reports not-negotiated, and this says why.
                    warn!(
                        "{}: input offers CUDA memory only, but {} could not be inserted: {}",
                        label, CUDA_ADAPTER_FACTORY, e
                    );
                }
            }
        }
        Err(_missing) => {
            if let Some(element) = pad.parent_element() {
                gst::element_error!(
                    element,
                    gst::CoreError::Negotiation,
                    (
                        "{} receives CUDA memory, which this GL input cannot take without {}",
                        label,
                        CUDA_ADAPTER_FACTORY
                    ),
                    [
                        "The producer offers {} only. Install the GStreamer nvcodec \
                         plugin, which provides {}, or feed this input from a source \
                         that outputs system or GL memory.",
                        caps,
                        CUDA_ADAPTER_FACTORY
                    ]
                );
            }
        }
    }
    front.cuda_in.store(true, Ordering::Release);
    gst::PadProbeReturn::Ok
}

/// Adjust the input's answer to a CAPS query (see the module docs).
fn answer_caps_query(front: &Front, q: &mut gst::query::Caps) {
    let filter = q.filter_owned();
    let cuda_in = front.cuda_in.load(Ordering::Acquire);
    if front.convert_in.load(Ordering::Acquire) {
        // What only the converter takes goes last.
        if let (Some(direct), Some(mut answer)) = (front.behind_convert(), q.result_owned()) {
            let adapter_sink = if cuda_in {
                None
            } else {
                adapter_cuda_sink_caps()
            };
            // The converter takes any raw video in system memory. Its own
            // answer says so only when what follows it lists system memory,
            // which `glupload` before GStreamer 1.24.13 does not for an unfiltered
            // query (see `takes_directly`).
            if filter.is_none() {
                if let Some(sink) = convert_sink_caps() {
                    answer.merge(sink.clone());
                }
            }
            let ordered = direct_paths_first(
                &takes_directly(&direct),
                &answer,
                adapter_sink.map(|c| c.as_ref()),
                filter.as_deref(),
            );
            q.set_result(&ordered);
        }
        return;
    }
    let Some(mut answer) = q.result_owned() else {
        return;
    };
    let mut changed = false;
    // CUDA memory is added, so a decoder upstream can choose it.
    if !cuda_in {
        if let Some(widened) = adapter_cuda_sink_caps()
            .and_then(|sink| with_cuda_alternative(&answer, sink, filter.as_deref()))
        {
            answer = widened;
            changed = true;
        }
    }
    // A producer that offers nothing the input takes directly, or that asks
    // without saying what it offers (a link check), is told the converter
    // takes any raw video in system memory, last.
    let offers_only_convertible = filter.as_ref().is_some_and(|f| {
        front
            .decide(f)
            .is_ok_and(|adapters| adapters.contains(&Adapter::VideoConvert))
    });
    if filter.is_none() || offers_only_convertible {
        if let Some(sink) = convert_sink_caps() {
            let convertible = match &filter {
                Some(f) => sink.intersect_with_mode(f, gst::CapsIntersectMode::First),
                None => sink.clone(),
            };
            if !convertible.is_empty() && !front.uploads.is_any() {
                answer.merge(convertible);
                changed = true;
            }
        }
    }
    if changed {
        q.set_result(&answer);
    }
}

/// What `pad`, the element behind the input's converter, takes without it.
///
/// Before GStreamer 1.24.13 (1.24.2 on Ubuntu 24.04, among others),
/// `glupload` answers a CAPS query with only the caps of
/// the upload method it is using, whenever those meet the query's filter. With
/// the converter in, the converter writes into `glupload`'s own buffer pool,
/// the method is the GL memory one, and an unfiltered query gets GL memory
/// only: no system-memory formats, so no CUDA alternative either. A query
/// filtered by system memory misses that method, and `glupload` then lists
/// what every method takes. 1.24.13 and later list every method in either case.
fn takes_directly(pad: &gst::Pad) -> gst::Caps {
    let mut caps = pad.query_caps(None);
    if let Some(system) = convert_sink_caps() {
        caps.merge(pad.query_caps(Some(system)));
    }
    caps
}

/// What `videoconvert` takes in system memory: its sink template's raw video
/// entries without a memory feature. `None` when it is not installed. Read
/// once: the registry does not change under a running process.
fn convert_sink_caps() -> Option<&'static gst::Caps> {
    static CAPS: std::sync::OnceLock<Option<gst::Caps>> = std::sync::OnceLock::new();
    CAPS.get_or_init(|| {
        let template = gst::ElementFactory::find(video_adapt::VIDEO_CONVERT_FACTORY)?
            .static_pad_templates()
            .into_iter()
            .find(|t| t.direction() == gst::PadDirection::Sink)?
            .caps();
        let mut system = gst::Caps::new_empty();
        {
            let system = system.get_mut().expect("new caps are writable");
            for (structure, features) in template.iter_with_features() {
                if structure.name() == "video/x-raw"
                    && !features.is_any()
                    && (features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
                        || !features.iter().any(|f| f.starts_with("memory:")))
                {
                    system.append_structure(structure.to_owned());
                }
            }
        }
        (!system.is_empty()).then_some(system)
    })
    .as_ref()
}

/// A CAPS event leaving the input's front element through `front_src`, on the
/// input's streaming thread: the frames before it have gone, the ones after it
/// follow. Make the adapters fit `caps` before the event reaches its peer —
/// the pad's peer is read after its probes have run, so the event and every
/// frame after it go to the new one.
fn on_caps_leaving_front(
    front: &Front,
    front_src: &gst::Pad,
    caps: &gst::CapsRef,
) -> gst::PadProbeReturn {
    let Some(upload) = front.upload.upgrade() else {
        return gst::PadProbeReturn::Remove;
    };
    let label = &front.label;
    let adapters = match front.decide(caps) {
        Ok(adapters) => adapters,
        // Reported by the query probe; nothing here can help.
        Err(_) => return gst::PadProbeReturn::Ok,
    };
    let wants_convert = adapters.contains(&Adapter::VideoConvert);
    let convert = adapter_in(front_src, &upload, video_adapt::VIDEO_CONVERT_FACTORY);

    match (convert, wants_convert) {
        (Some(convert), false) => match splice_out(&convert) {
            Ok(()) => {
                front.convert_in.store(false, Ordering::Release);
                info!(
                    "{}: input now delivers {}, which needs no conversion; removed {}",
                    label,
                    caps,
                    convert.name()
                );
            }
            Err(e) => warn!("{}: {} could not be removed: {}", label, convert.name(), e),
        },
        (None, true) => {
            insert_at_front(
                front,
                front_src,
                video_adapt::VIDEO_CONVERT_FACTORY,
                &front.convert_in,
                caps,
            );
        }
        _ => {}
    }
    if adapters.contains(&Adapter::CudaDownload)
        && adapter_in(front_src, &upload, CUDA_ADAPTER_FACTORY).is_none()
    {
        // CUDA memory after a converter period: the queries were answered for
        // the input without the converter, so `cudadownload` goes in now.
        insert_at_front(front, front_src, CUDA_ADAPTER_FACTORY, &front.cuda_in, caps);
    }
    gst::PadProbeReturn::Ok
}

/// Splice `factory` in behind `front_src` at once. Only for the input's
/// streaming thread, inside a push on `front_src`: nothing else moves data
/// across that link meanwhile.
fn insert_at_front(
    front: &Front,
    front_src: &gst::Pad,
    factory: &str,
    flag: &AtomicBool,
    caps: &gst::CapsRef,
) {
    let label = &front.label;
    let Some(next) = front_src.peer().and_then(|p| p.parent_element()) else {
        return;
    };
    match splice_adapter(&next, factory, label, gst::PadLinkCheck::HIERARCHY) {
        Ok(()) => {
            flag.store(true, Ordering::Release);
            info!(
                "{}: input now delivers {}, inserted {} in front of {}",
                label,
                caps,
                factory,
                next.name()
            );
        }
        Err(e) => {
            warn!("{}: {} could not be inserted: {}", label, factory, e);
            if let Some(element) = front_src.parent_element() {
                gst::element_error!(
                    element,
                    gst::CoreError::Negotiation,
                    (
                        "{} receives {}, and {} could not be inserted to adapt it",
                        label,
                        caps,
                        factory
                    ),
                    ["{}", e]
                );
            }
        }
    }
}

/// Take `adapter` out and link its feeder to what it fed. The caller makes
/// sure no buffer moves across either link meanwhile.
fn splice_out(adapter: &gst::Element) -> Result<(), String> {
    let (Some(sink), Some(src)) = (adapter.static_pad("sink"), adapter.static_pad("src")) else {
        return Err(format!("{} lacks a sink or src pad", adapter.name()));
    };
    let feeder = sink
        .peer()
        .ok_or_else(|| format!("{} is not fed", adapter.name()))?;
    let next = src
        .peer()
        .ok_or_else(|| format!("{} feeds nothing", adapter.name()))?;
    let bin = adapter
        .parent()
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .ok_or_else(|| format!("{} has no parent bin", adapter.name()))?;

    feeder
        .unlink(&sink)
        .map_err(|e| format!("could not unlink {}: {}", adapter.name(), e))?;
    if let Err(e) = src.unlink(&next) {
        let _ = feeder.link(&sink);
        return Err(format!("could not unlink {}: {}", adapter.name(), e));
    }
    // No caps check: the CAPS event on its way says what flows next.
    if let Err(e) = feeder.link_full(&next, gst::PadLinkCheck::HIERARCHY) {
        let _ = src.link(&next);
        let _ = feeder.link(&sink);
        return Err(format!("could not link around {}: {:?}", adapter.name(), e));
    }
    let _ = adapter.set_state(gst::State::Null);
    bin.remove(adapter).map_err(|e| {
        format!(
            "{} could not be removed from {}: {}",
            adapter.name(),
            bin.name(),
            e
        )
    })
}

/// Why a splice did not happen while the caller waited.
#[derive(Debug)]
enum SpliceError {
    /// Data kept flowing; the splice happens once it pauses.
    Deferred(String),
    /// The adapter could not be put in.
    Failed(String),
}

impl std::fmt::Display for SpliceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpliceError::Deferred(e) | SpliceError::Failed(e) => f.write_str(e),
        }
    }
}

/// How long a splice waits for the input's data flow to pause. A push into a
/// GL input finishes in a frame time or so; the wait is for that, not more.
const SPLICE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Splice `adapter_factory` in behind `front_src`, the source pad of the
/// input's front element, while no buffer is moving across it, and wait up to
/// `wait` for it. `upload` is the input's `glupload`, where the adapters end.
///
/// At flow start nothing flows yet, but a producer can also turn to CUDA
/// memory mid-stream - a Media Player moving on to a file the GPU decodes -
/// while the feeder's streaming thread is still pushing the previous file's
/// frames. Relinking under it would hand a buffer to a pad that is unlinked
/// (`not-linked`, a flow error) or to an adapter not yet started (`flushing`,
/// which stops the feeder for good). An IDLE probe holds the data flow off
/// for the splice: it runs at once, on this thread, when the pad is idle, and
/// otherwise on the streaming thread as soon as the current push returns.
/// By then a CAPS event may have put the same adapter in; it is not doubled.
fn splice_when_idle(
    front_src: &gst::Pad,
    upload: &gst::Element,
    adapter_factory: &str,
    label: &str,
    wait: std::time::Duration,
) -> Result<(), SpliceError> {
    let (done, result) = std::sync::mpsc::sync_channel(1);
    // The probe lives on a pad of the same pipeline: weak handle only.
    let upload_weak = upload.downgrade();
    let factory = adapter_factory.to_string();
    let label_owned = label.to_string();
    front_src.add_probe(gst::PadProbeType::IDLE, move |front_src, _| {
        let outcome = upload_weak
            .upgrade()
            .ok_or_else(|| "the input is gone".to_string())
            .and_then(|upload| {
                if adapter_in(front_src, &upload, &factory).is_some() {
                    return Ok(());
                }
                let next = front_src
                    .peer()
                    .and_then(|p| p.parent_element())
                    .ok_or_else(|| "the input's front element is not linked".to_string())?;
                splice_adapter(&next, &factory, &label_owned, gst::PadLinkCheck::DEFAULT)
            });
        if let Err(std::sync::mpsc::TrySendError::Disconnected(outcome)) = done.try_send(outcome) {
            // The caller stopped waiting; this is the only report left.
            match outcome {
                Ok(()) => info!(
                    "{}: inserted {} after waiting for data to pause",
                    label_owned, factory
                ),
                Err(e) => warn!("{}: {} could not be inserted: {}", label_owned, factory, e),
            }
        }
        gst::PadProbeReturn::Remove
    });
    match result.recv_timeout(wait) {
        Ok(outcome) => outcome.map_err(SpliceError::Failed),
        Err(_) => Err(SpliceError::Deferred(format!(
            "data kept flowing for {:?}; inserting once it pauses",
            wait
        ))),
    }
}

/// Insert an `adapter_factory` element between `upload`'s sink pad and its peer,
/// linking with `check`. The caller makes sure no buffer moves across that
/// link meanwhile.
fn splice_adapter(
    upload: &gst::Element,
    adapter_factory: &str,
    label: &str,
    check: gst::PadLinkCheck,
) -> Result<(), String> {
    // Strong references stay local to this function — never captured in a
    // closure, so no reference cycle can outlive the pipeline.
    let upload_sink = upload
        .static_pad("sink")
        .ok_or_else(|| format!("{} has no sink pad", upload.name()))?;
    let feeder = upload_sink
        .peer()
        .ok_or_else(|| format!("{} is not linked", upload.name()))?;
    let bin = upload
        .parent()
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .ok_or_else(|| format!("{} has no parent bin", upload.name()))?;

    let name = format!("{}_{}", upload.name(), adapter_factory);
    let adapter = gst::ElementFactory::make(adapter_factory)
        .name(&name)
        .build()
        .map_err(|e| format!("{} could not be created: {}", adapter_factory, e))?;
    let (Some(sink), Some(src)) = (adapter.static_pad("sink"), adapter.static_pad("src")) else {
        return Err(format!("{} lacks a sink or src pad", name));
    };
    bin.add(&adapter)
        .map_err(|e| format!("{} could not be added to {}: {}", name, bin.name(), e))?;

    // Unlink before linking: `glupload` accepts one upstream link only.
    if let Err(e) = feeder.unlink(&upload_sink) {
        let _ = bin.remove(&adapter);
        return Err(format!("could not unlink {}: {}", upload.name(), e));
    }
    let spliced = src
        .link_full(&upload_sink, check)
        .map_err(|e| format!("could not link {} to {}: {}", name, upload.name(), e))
        .and_then(|_| {
            feeder
                .link_full(&sink, check)
                .map_err(|e| format!("could not link {} to {}: {}", label, name, e))
        })
        // State last, so the element negotiates against a complete topology.
        .and_then(|_| {
            adapter
                .sync_state_with_parent()
                .map(|_| ())
                .map_err(|e| format!("{} could not reach the pipeline state: {}", name, e))
        });

    if spliced.is_err() {
        // Put the input back as it was, so it fails the way it did before
        // rather than with a half-built adapter in the way.
        let _ = feeder.unlink(&sink);
        let _ = src.unlink(&upload_sink);
        let _ = adapter.set_state(gst::State::Null);
        let _ = bin.remove(&adapter);
        let _ = feeder.link(&upload_sink);
    }
    spliced
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        // Caps parsing needs the type system registered.
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    const CUDA: &str = "video/x-raw(memory:CUDAMemory), format=NV12, width=1920, height=1080";

    /// Stands in for `cudadownload`'s sink template, which CI does not have.
    const ADAPTER_SINK: &str = "video/x-raw(memory:CUDAMemory), format={ NV12, I420 }";

    fn has_cuda(c: &gst::CapsRef) -> bool {
        c.iter_with_features()
            .any(|(_, f)| f.contains(CUDA_MEMORY_FEATURE))
    }

    /// What `glupload` answers, widened: system memory stays first, so a
    /// producer that cannot output CUDA memory settles as before, and the CUDA
    /// entries carry the answer's constraints.
    #[test]
    fn a_system_memory_answer_also_offers_cuda_memory_last() {
        let answer = caps(
            "video/x-raw(memory:GLMemory), format=RGBA; \
             video/x-raw, format={ NV12, RGBA }, width=1920, height=1080",
        );
        let widened =
            with_cuda_alternative(&answer, &caps(ADAPTER_SINK), None).expect("CUDA added");
        assert_eq!(widened.size(), 3);
        for i in 0..2 {
            assert_eq!(widened.structure(i), answer.structure(i));
            assert_eq!(
                widened.features(i).map(|f| f.to_string()),
                answer.features(i).map(|f| f.to_string())
            );
        }
        let (cuda, features) = widened.iter_with_features().nth(2).unwrap();
        assert!(features.contains(CUDA_MEMORY_FEATURE));
        // RGBA is not something the adapter takes; the size is kept.
        assert_eq!(cuda.get::<&str>("format").unwrap(), "NV12");
        assert_eq!(cuda.get::<i32>("width").unwrap(), 1920);
    }

    /// While the converter is in, what the input takes without it comes
    /// first, then its CUDA alternative, then what only the converter adds.
    #[test]
    fn with_the_converter_in_direct_paths_come_first() {
        let direct = caps(
            "video/x-raw(memory:GLMemory), format=RGBA; \
             video/x-raw, format={ NV12, RGBA }",
        );
        let via_convert = caps("video/x-raw(memory:GLMemory), format=RGBA; video/x-raw");
        let answer = direct_paths_first(&direct, &via_convert, Some(&caps(ADAPTER_SINK)), None);
        assert_eq!(answer.size(), 4, "{answer}");
        assert!(answer.features(0).unwrap().contains("memory:GLMemory"));
        assert!(answer.structure(1).unwrap().has_field("format"), "{answer}");
        assert!(answer.features(2).unwrap().contains(CUDA_MEMORY_FEATURE));
        assert!(
            !answer.structure(3).unwrap().has_field("format"),
            "{answer}"
        );

        // With `cudadownload` already in there is no CUDA alternative to add,
        // and a filter applies to every part.
        let filter = caps("video/x-raw, format=NV12");
        let answer = direct_paths_first(&direct, &via_convert, None, Some(&filter));
        assert!(!has_cuda(&answer));
        assert_eq!(
            answer.structure(0).unwrap().get::<&str>("format").unwrap(),
            "NV12"
        );
    }

    #[test]
    fn the_query_filter_is_respected() {
        let answer = caps("video/x-raw, format={ NV12, I420 }");
        let widened = with_cuda_alternative(
            &answer,
            &caps(ADAPTER_SINK),
            Some(&caps("video/x-raw(memory:CUDAMemory), format=I420")),
        )
        .expect("CUDA added");
        let (cuda, _) = widened.iter_with_features().last().unwrap();
        assert_eq!(cuda.get::<&str>("format").unwrap(), "I420");

        // A filter that rules CUDA memory out leaves the answer alone.
        let system_only = caps("video/x-raw, format=NV12");
        assert!(with_cuda_alternative(&answer, &caps(ADAPTER_SINK), Some(&system_only)).is_none());
    }

    #[test]
    fn nothing_is_added_when_there_is_nothing_to_widen() {
        let adapter = caps(ADAPTER_SINK);
        for c in [
            // Already offers CUDA memory.
            "video/x-raw(memory:CUDAMemory), format=NV12; video/x-raw, format=NV12",
            // GL memory only: no system-memory entry to mirror.
            "video/x-raw(memory:GLMemory), format=RGBA",
            // A format the adapter cannot take.
            "video/x-raw, format=RGBA",
            "ANY",
        ] {
            assert!(
                with_cuda_alternative(&caps(c), &adapter, None).is_none(),
                "{}",
                c
            );
        }
        assert!(!has_cuda(&caps("video/x-raw, format=NV12")));
    }

    /// The widened answer is what the decoder then accepts with: CUDA memory
    /// only, which is exactly what puts the adapter in.
    #[test]
    fn choosing_the_added_cuda_memory_gets_the_adapter() {
        let widened = with_cuda_alternative(
            &caps("video/x-raw, format=NV12, width=1280, height=720"),
            &caps(ADAPTER_SINK),
            None,
        )
        .unwrap();
        let mut chosen = gst::Caps::new_empty();
        for (s, f) in widened.iter_with_features() {
            if f.contains(CUDA_MEMORY_FEATURE) {
                chosen
                    .make_mut()
                    .append_structure_full(s.to_owned(), Some(f.to_owned()));
            }
        }
        assert!(has_cuda(&chosen));
        assert_eq!(
            video_adapt::decide(&chosen, Consumer::GlUpload(&gst::Caps::new_any()), |_| true),
            Ok(vec![Adapter::CudaDownload])
        );
    }

    /// `appsrc ! queue ! identity(up) ! appsink`, playing, with buffers pushed
    /// from a thread until `stop` is set. `up` stands in for `glupload`, the
    /// queue for the input's front queue; the adapter is an `identity` too.
    fn flowing_input() -> (
        gst::Pipeline,
        gst::Element,
        gstreamer_app::AppSink,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let _ = gst::init();
        let pipeline = gst::Pipeline::new();
        let src = gstreamer_app::AppSrc::builder()
            .format(gst::Format::Time)
            .caps(&caps(
                "video/x-raw, format=GRAY8, width=4, height=4, framerate=100/1",
            ))
            .build();
        let queue = gst::ElementFactory::make("queue")
            .name("front")
            .build()
            .unwrap();
        let up = gst::ElementFactory::make("identity")
            .name("up")
            .build()
            .unwrap();
        let sink = gstreamer_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([src.upcast_ref(), &queue, &up, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([src.upcast_ref(), &queue, &up, sink.upcast_ref()]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_feed = Arc::clone(&stop);
        let feed = std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop_feed.load(Ordering::Acquire) {
                let mut buf = gst::Buffer::with_size(16).unwrap();
                buf.get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_mseconds(n * 10));
                if src.push_buffer(buf).is_err() {
                    break;
                }
                n += 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        (pipeline, up, sink, stop, feed)
    }

    fn feeder_of(up: &gst::Element) -> Option<String> {
        up.static_pad("sink")
            .and_then(|p| p.peer())
            .and_then(|p| p.parent_element())
            .map(|e| e.name().to_string())
    }

    fn no_error(pipeline: &gst::Pipeline) {
        let bus = pipeline.bus().unwrap();
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            panic!("the input failed: {:?}", msg);
        }
    }

    /// A producer can turn to CUDA memory mid-stream, a Media Player moving on
    /// to a file the GPU decodes, while the input's queue is still pushing the
    /// previous file's frames. Relinking under that push hands the next buffer
    /// to an unlinked pad or to an adapter not yet started, and the input
    /// stops. The splice has to wait for the push in progress to return, and
    /// the frames have to keep coming afterwards.
    #[test]
    fn a_splice_while_a_frame_is_being_pushed_waits_for_it() {
        let (pipeline, up, sink, stop, feed) = flowing_input();
        assert!(sink.pull_sample().is_ok(), "frames flow before the splice");

        // Hold the queue's streaming thread inside a push into `up`.
        let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
        let up_sink = up.static_pad("sink").unwrap();
        let hold = up_sink
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                move |_, _| {
                    let _ = held_tx.try_send(());
                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();
        held_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a push reaches up and is held");

        let front_src = pipeline
            .by_name("front")
            .and_then(|q| q.static_pad("src"))
            .unwrap();
        let deferred = splice_when_idle(
            &front_src,
            &up,
            "identity",
            "test input",
            std::time::Duration::from_millis(200),
        );
        assert!(deferred.is_err(), "the splice did not wait for the push");
        assert_eq!(
            feeder_of(&up).as_deref(),
            Some("front"),
            "the link was changed under a push in progress"
        );

        // The push returns; the deferred splice runs before the next one.
        up_sink.remove_probe(hold);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while feeder_of(&up).as_deref() != Some("up_identity") {
            assert!(std::time::Instant::now() < deadline, "the splice never ran");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        for _ in 0..20 {
            assert!(
                sink.try_pull_sample(gst::ClockTime::from_seconds(2))
                    .is_some(),
                "frames stopped after the splice"
            );
        }
        no_error(&pipeline);

        stop.store(true, Ordering::Release);
        let _ = pipeline.set_state(gst::State::Null);
        feed.join().unwrap();
    }

    /// At flow start nothing has been pushed yet: the splice happens at once,
    /// before the query that asked for it is answered.
    #[test]
    fn a_splice_on_an_idle_input_happens_at_once() {
        let _ = gst::init();
        let pipeline = gst::Pipeline::new();
        let queue = gst::ElementFactory::make("queue")
            .name("front")
            .build()
            .unwrap();
        let up = gst::ElementFactory::make("identity")
            .name("up")
            .build()
            .unwrap();
        pipeline.add_many([&queue, &up]).unwrap();
        queue.link(&up).unwrap();

        let front_src = queue.static_pad("src").unwrap();
        splice_when_idle(&front_src, &up, "identity", "test input", SPLICE_WAIT).unwrap();
        assert_eq!(feeder_of(&up).as_deref(), Some("up_identity"));
        let _ = pipeline.set_state(gst::State::Null);
    }

    /// The probe reads what the producer offers from both query types that
    /// reach the input before a CAPS event, and ignores unfiltered caps queries
    /// (the linker's compatibility check sends those).
    #[test]
    fn offered_caps_come_from_accept_caps_and_filtered_caps_queries() {
        let cuda = caps(CUDA);
        let accept = gst::query::AcceptCaps::new(&cuda);
        assert_eq!(offered_caps(&accept), Some(cuda.clone()));
        let filtered = gst::query::Caps::new(Some(&cuda));
        assert_eq!(offered_caps(&filtered), Some(cuda));
        let unfiltered = gst::query::Caps::new(None);
        assert_eq!(offered_caps(&unfiltered), None);
    }
}
