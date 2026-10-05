//! Consumer-side adaptation of a WHEP Output video input.
//!
//! The convention is that a block emits the memory type it naturally produces
//! and the *consuming* block adapts its own input, so a GPU-to-GPU flow is
//! never forced through a download and re-upload. Most consumers can pick
//! their input front at build time. This module covers the case where they
//! cannot: what arrives on a WHEP Output video pad — which memory it lives in
//! and which pixel format it carries — depends on which decoder `decodebin`
//! autoplugged upstream, or on how a mixer was configured, and is only known
//! once caps are negotiated. It can also change mid-stream: a Media Player
//! moving on to a file that another decoder handles, or a mixer reconfigured.
//!
//! Two things are adapted, decided again on every CAPS event, by splicing
//! elements between the watched pad and its peer:
//!
//! * **Memory.** GL memory is always downloaded to system memory:
//!   `whepserversink` advertises `video/x-raw(memory:GLMemory)` on its video
//!   request pads, so GL frames negotiate all the way to the sink and its
//!   encoder discovery then finds no encoder that can take them — video goes
//!   missing while audio keeps working. Other GPU memory (CUDA, NVMM, D3D11,
//!   VA, DMABuf) is left in place when the sink accepts it: the encoders that
//!   advertise those really do take them, and downloading would cost a full
//!   round trip per frame. When the sink refuses it, CUDA memory is
//!   downloaded, and any other memory fails the flow with an element error
//!   that says what arrived and what the sink takes. Both rules come from
//!   [`video_adapt::decide`].
//!
//! * **The pixel format** is converted to 8-bit 4:2:0 once the frames are in
//!   system memory. `webrtcsink` builds a full encoding chain per consumer,
//!   each starting with its own `videoconvert`, so an RGBA input is converted
//!   once per viewer — and into whatever the encoder's caps happen to prefer,
//!   which for VideoToolbox is 16-bit `RGBA64_LE` and for `vp9enc` is 4:4:4
//!   `Y444`. Converting once here leaves a hardware encoder's converter in
//!   passthrough. The VP9 and AV1 software encoders do not take NV12, so their
//!   consumers still convert, but from NV12 to I420 rather than from RGBA.
//!
//! The probes stay for the life of the pad. They fire per event and per
//! query, never per buffer. When the adapters already in place suit the new
//! caps (a resolution change, say), the event passes untouched; when they do
//! not, the chain is replaced.
//!
//! Upstream asks before it sends new caps (an ACCEPT_CAPS query, which a
//! `queue` forwards through the watched pad), so the bridge answers that
//! query itself: yes for what it will adapt, no for what it refuses (with the
//! element error that says why), and the sink's own answer for the rest.
//! Left to the adapters already in place, a converter spliced in for RGBA
//! would refuse the GL memory a producer switches to, and the CAPS event that
//! would have re-adapted the input would never come.
//!
//! Relinking from inside a push-event probe is a supported pattern:
//! `gst_pad_push_event_unchecked` resolves the peer and rechecks pending sticky
//! events *after* running its probes, so the event that triggered the splice
//! lands on the newly inserted elements. A CAPS event is serialized with the
//! buffers on the same streaming thread, so no buffer is in flight across the
//! link while the probe runs: the probe itself holds the data flow off, which
//! is what a BLOCK or IDLE probe would otherwise be there for. An IDLE probe
//! would not do here, since it would run only after the event had already
//! reached the old peer.

use crate::gst::video_adapt::{self, Adapter, Consumer, Refusal};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// What `whepserversink` can really take on a video pad: raw video in system
/// memory.
const SINK_TAKES: &str = "video/x-raw";

/// The format the input is converted to when it needs converting at all.
///
/// `vtenc_h264`/`vtenc_h265` and the NVENC and VA encoders take it natively,
/// and `x264enc` lists it alongside I420. The VP9 and AV1 software encoders do
/// not accept NV12 at all (`vp9enc`, `av1enc`, `rav1enc` and `svtav1enc` list
/// I420), so their consumers still convert NV12 to I420 each. That costs a
/// couple of points against an encode that costs fifteen.
const NATIVE_FORMAT: &str = "NV12";

/// Formats left alone: 8-bit 4:2:0. Converting one into the other would cost
/// a pass over every frame and at best move the per-consumer conversion from
/// one set of encoders to another.
const NATIVE_FORMATS: &[&str] = &["NV12", "I420"];

/// The converter spliced in once the frames are in system memory. Plain
/// `videoconvert`, not an auto-plugger: by then there is no GPU path for one
/// to win, and `autovideoconvert` behind `gldownload` crashed or hung in 9 of
/// 20 runs on GStreamer 1.24.2, where `videoconvert` ran 20 of 20.
const CONVERT_FACTORY: &str = "videoconvert";

/// What a WHEP Output video input needs in front of its sink.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Plan {
    /// Downloads, in link order.
    memory: Vec<Adapter>,
    /// Whether a converter to [`NATIVE_FORMAT`] follows them.
    convert: bool,
}

/// Decide what `caps` need in front of the sink.
///
/// `sink_accepts` says whether the sink takes `caps` as they are; it is asked
/// only for raw video in a GPU memory other than GL. `available` says whether
/// an element factory exists (see [`video_adapt::decide`]).
fn plan(
    caps: &gst::CapsRef,
    sink_accepts: impl FnOnce() -> bool,
    available: impl Fn(&str) -> bool,
) -> Result<Plan, Refusal> {
    // whepserversink advertises GL memory on its video pads but cannot encode
    // it, so its caps are corrected here to what it can really take.
    let takes = gst::Caps::new_empty_simple(SINK_TAKES);
    let gl = video_adapt::decide(caps, Consumer::Accepts(&takes), &available)?;
    let on_cpu = video_adapt::decide(caps, Consumer::SystemMemory, &available);
    let in_system_memory = matches!(&on_cpu, Ok(adapters) if adapters.is_empty());

    let memory = if !gl.is_empty() {
        gl
    } else if in_system_memory || sink_accepts() {
        // System memory, or a GPU memory the sink's encoders take directly.
        Vec::new()
    } else {
        // The sink refuses this memory: download it if anything here can.
        on_cpu?
    };

    // The converter takes system memory only, which is where the frames are
    // after any download.
    let convert = (in_system_memory || !memory.is_empty()) && needs_format_conversion(caps);
    Ok(Plan { memory, convert })
}

/// True when raw video in `caps`' pixel format should be converted to
/// [`NATIVE_FORMAT`] before the sink fans it out to its consumers. Only the
/// format is judged: whether the frames are in a memory the converter can
/// read is [`plan`]'s call.
///
/// A format carrying more than 8 bits per component is left alone. Converting
/// it would silently drop the input's precision, and the encoders that can use
/// the extra bits would have kept them.
fn needs_format_conversion(caps: &gst::CapsRef) -> bool {
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    if structure.name() != "video/x-raw" {
        return false;
    }
    let Ok(format) = structure.get::<String>("format") else {
        // An unfixed format means negotiation has not settled on one; there is
        // nothing to compare against and nothing to convert.
        return false;
    };
    if NATIVE_FORMATS.contains(&format.as_str()) {
        return false;
    }
    let Ok(parsed) = format.parse::<gst_video::VideoFormat>() else {
        return false;
    };
    let info = gst_video::VideoFormatInfo::from_format(parsed);
    info.depth().iter().copied().max().unwrap_or(8) <= 8
}

/// The adapters currently spliced in, and the pad they feed.
///
/// Weak handles only: this lives in a probe owned by a pad of the same
/// pipeline, so a strong one would keep the pipeline alive forever.
#[derive(Default)]
struct Spliced {
    plan: Plan,
    elements: Vec<glib::WeakRef<gst::Element>>,
    /// The sink's pad, the original peer. `None` while nothing is spliced in:
    /// the sink is then the watched pad's peer.
    target: Option<glib::WeakRef<gst::Pad>>,
    /// How many chains have been spliced in. The old chain is still in the bin
    /// while its replacement is linked, so each chain after the first gets
    /// its own element names.
    generation: u32,
}

/// Watch `src_pad` and adapt what arrives on it to what `whepserversink` can
/// encode, by splicing elements in front of its peer.
///
/// The probes stay for the life of the pad and decide again on every CAPS
/// event, so a producer that switches memory type or pixel format mid-stream
/// is adapted too. `name_prefix` names the inserted elements and the input in
/// logs and error messages.
pub fn install_video_input_bridge(src_pad: &gst::Pad, name_prefix: &str) {
    let name_prefix = Arc::new(name_prefix.to_string());
    // Taken per CAPS event and per caps query only; never on the buffer path.
    let state = Arc::new(Mutex::new(Spliced::default()));

    // Before a CAPS event is sent, upstream checks that it will be taken: an
    // ACCEPT_CAPS query, which `queue` forwards from its sink pad through this
    // one. Left to the peer, a sink that refuses CUDA memory the bridge would
    // download, or a converter already spliced in that cannot take the GL
    // memory a producer switched to, refuses the caps, upstream fails
    // not-negotiated, and the CAPS event that would have re-adapted the input
    // never arrives. So the bridge answers for what it can adapt, and asks the
    // sink itself, not the adapters in front of it, about the rest.
    // QUERY_DOWNSTREAM fires per query, never per buffer.
    let query_state = Arc::clone(&state);
    let query_prefix = Arc::clone(&name_prefix);
    src_pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |pad, info| {
        if info.mask.contains(gst::PadProbeType::PULL) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(gst::PadProbeData::Query(ref mut query)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        // Released before any query is sent on: the sink's answer must not
        // wait on a splice in progress on the streaming thread.
        let (target, spliced) = {
            let state = query_state.lock().unwrap_or_else(|p| p.into_inner());
            (target_of(pad, &state), state.target.is_some())
        };
        let Some(target) = target else {
            return gst::PadProbeReturn::Ok;
        };
        match query.view_mut() {
            gst::QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps_owned();
                let accepted = match plan(
                    &caps,
                    || target.query_accept_caps(&caps),
                    video_adapt::factory_available,
                ) {
                    // Nothing to adapt: the sink's own answer.
                    Ok(wanted) if wanted == Plan::default() => {
                        if !spliced {
                            return gst::PadProbeReturn::Ok;
                        }
                        target.query_accept_caps(&caps)
                    }
                    // The adapters for these caps go in with the CAPS event.
                    Ok(_) => true,
                    Err(refusal) => {
                        if caps.is_fixed() {
                            post_refusal(pad, &caps, &query_prefix, &refusal);
                        }
                        false
                    }
                };
                q.set_result(accepted);
                gst::PadProbeReturn::Handled
            }
            // What the sink takes, not what the adapters in front of it take:
            // a producer that offers GL memory must still be able to pick it.
            gst::QueryViewMut::Caps(q) if spliced => {
                let answer = target.query_caps(q.filter_owned().as_ref());
                q.set_result(&answer);
                gst::PadProbeReturn::Handled
            }
            _ => gst::PadProbeReturn::Ok,
        }
    });

    // EVENT_DOWNSTREAM, not BLOCK/BUFFER: this fires per event, not per buffer.
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let caps = caps_event.caps();
        let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
        adapt(pad, caps, &name_prefix, &mut state);
        gst::PadProbeReturn::Ok
    });
}

/// The sink's pad: the original peer of `pad`, whatever is spliced in front of
/// it. `None` when `pad` is not linked yet, or the pipeline is being torn down.
fn target_of(pad: &gst::Pad, state: &Spliced) -> Option<gst::Pad> {
    match &state.target {
        Some(weak) => weak.upgrade(),
        None => pad.peer(),
    }
}

/// Bring the chain between `pad` and the sink in line with `caps`.
fn adapt(pad: &gst::Pad, caps: &gst::CapsRef, name_prefix: &str, state: &mut Spliced) {
    let Some(target) = target_of(pad, state) else {
        return;
    };

    let wanted = match plan(
        caps,
        || target.query_accept_caps(&caps.to_owned()),
        video_adapt::factory_available,
    ) {
        Ok(wanted) => wanted,
        Err(refusal) => {
            post_refusal(pad, caps, name_prefix, &refusal);
            return;
        }
    };
    if wanted == state.plan {
        return;
    }

    let old: Vec<gst::Element> = state
        .elements
        .iter()
        .filter_map(|weak| weak.upgrade())
        .collect();
    if old.len() != state.elements.len() {
        // The pipeline is being torn down under us.
        return;
    }

    let element_prefix = match state.generation {
        0 => name_prefix.to_string(),
        n => format!("{}_{}", name_prefix, n),
    };
    let new = match build_adapters(&wanted, &element_prefix) {
        Ok(new) => new,
        Err(e) => {
            post_splice_failure(pad, caps, name_prefix, &e);
            return;
        }
    };

    match resplice(pad, &target, &old, &new) {
        Ok(()) => {
            if new.is_empty() {
                info!(
                    "{}: input {} needs no adaptation, removed the adapters",
                    name_prefix, caps
                );
            } else {
                info!(
                    "{}: adapted input {} with {}",
                    name_prefix,
                    caps,
                    new.iter()
                        .map(|e| e.name().to_string())
                        .collect::<Vec<_>>()
                        .join(" ! ")
                );
            }
            state.elements = new.iter().map(|e| e.downgrade()).collect();
            state.target = (!new.is_empty()).then(|| target.downgrade());
            state.plan = wanted;
            if !new.is_empty() {
                state.generation += 1;
            }
        }
        Err(e) => post_splice_failure(pad, caps, name_prefix, &e),
    }
}

/// Fail the flow because nothing here can adapt `caps` for the sink.
fn post_refusal(pad: &gst::Pad, caps: &gst::CapsRef, name_prefix: &str, refusal: &Refusal) {
    let Some(element) = pad.parent_element() else {
        warn!("{}: input {} refused: {}", name_prefix, caps, refusal);
        return;
    };
    match refusal {
        Refusal::UnsupportedMemory { feature } => gst::element_error!(
            element,
            gst::CoreError::Negotiation,
            (
                "{}: WHEP Output receives raw video in {}, which its encoders do not take",
                name_prefix,
                feature
            ),
            [
                "Arrived: {}. WHEP Output takes raw video in system, GL or CUDA memory, \
                 a GPU memory its encoders accept, or encoded video. Feed it from a \
                 source that outputs one of those, or encode with builtin.videoenc first.",
                caps
            ]
        ),
        Refusal::MissingFactory { factory } => gst::element_error!(
            element,
            gst::CoreError::MissingPlugin,
            (
                "{}: WHEP Output receives video it can only take through {}, which is not installed",
                name_prefix,
                factory
            ),
            [
                "Arrived: {}. Its encoders do not take this memory directly, and \
                 downloading it needs {}. Install the GStreamer plugin that provides \
                 it, or feed this input from a source that outputs system memory.",
                caps,
                factory
            ]
        ),
    }
}

/// Fail the flow because the adapters `caps` need could not be put in place.
fn post_splice_failure(pad: &gst::Pad, caps: &gst::CapsRef, name_prefix: &str, reason: &str) {
    let Some(element) = pad.parent_element() else {
        warn!(
            "{}: input {} could not be adapted: {}",
            name_prefix, caps, reason
        );
        return;
    };
    gst::element_error!(
        element,
        gst::CoreError::Negotiation,
        (
            "{}: WHEP Output could not adapt its video input",
            name_prefix
        ),
        [
            "Arrived: {}. WHEP Output needs raw video in system memory in a format \
             its encoders take, and inserting the adapters failed: {}",
            caps,
            reason
        ]
    );
}

/// Build the elements for `plan`, in the order they must be linked.
///
/// The downloads come first: the converter takes system memory only, and the
/// format it has to produce is the same either way.
fn build_adapters(plan: &Plan, name_prefix: &str) -> Result<Vec<gst::Element>, String> {
    let mut adapters = video_adapt::build_elements(&plan.memory, name_prefix)?;

    // Not a caps adaptation: converting once here saves every viewer's
    // encoding chain its own conversion.
    if plan.convert {
        let convert = gst::ElementFactory::make(CONVERT_FACTORY)
            .name(format!("{}_videoconvert", name_prefix))
            .build()
            .map_err(|e| format!("{} could not be created: {}", CONVERT_FACTORY, e))?;
        // Threads the conversion on macOS; a no-op elsewhere.
        crate::gpu::configure_video_convert(&convert);
        adapters.push(convert);
        adapters.push(
            gst::ElementFactory::make("capsfilter")
                .name(format!("{}_format", name_prefix))
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", NATIVE_FORMAT)
                        .build(),
                )
                .build()
                .map_err(|e| format!("capsfilter could not be created: {}", e))?,
        );
    }

    Ok(adapters)
}

/// Replace the adapters `old` between `src_pad` and `target` with `new`.
///
/// Either list may be empty: an empty `old` means `src_pad` is linked straight
/// to `target`, an empty `new` that it will be. On failure the link is put
/// back as it was, with `old` in place and `new` gone from the bin, so the
/// input fails the way it would have rather than with a half-built chain in
/// the way. On success `old` is shut down and removed.
///
/// The caller makes sure no buffer moves across the link meanwhile.
fn resplice(
    src_pad: &gst::Pad,
    target: &gst::Pad,
    old: &[gst::Element],
    new: &[gst::Element],
) -> Result<(), String> {
    // Strong references stay local to this function — never captured in a
    // closure, so no reference cycle can outlive the pipeline.
    let bin = src_pad
        .parent_element()
        .and_then(|element| element.parent())
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .ok_or_else(|| "source pad's element has no parent bin".to_string())?;

    let ends = |chain: &[gst::Element]| -> Result<(gst::Pad, gst::Pad), String> {
        match (chain.first(), chain.last()) {
            (Some(first), Some(last)) => Ok((
                first
                    .static_pad("sink")
                    .ok_or_else(|| format!("{} has no sink pad", first.name()))?,
                last.static_pad("src")
                    .ok_or_else(|| format!("{} has no src pad", last.name()))?,
            )),
            _ => Ok((target.clone(), src_pad.clone())),
        }
    };
    // For an empty chain both ends are the direct link itself.
    let (old_in, old_out) = ends(old)?;
    let (new_in, new_out) = ends(new)?;

    let mut added = Vec::new();
    let built = (|| -> Result<(), String> {
        for adapter in new {
            bin.add(adapter).map_err(|e| {
                format!(
                    "{} could not be added to {}: {}",
                    adapter.name(),
                    bin.name(),
                    e
                )
            })?;
            added.push(adapter.clone());
        }
        for pair in new.windows(2) {
            pair[0].link(&pair[1]).map_err(|e| {
                format!(
                    "could not link {} to {}: {}",
                    pair[0].name(),
                    pair[1].name(),
                    e
                )
            })?;
        }
        Ok(())
    })();
    if let Err(e) = built {
        for adapter in &added {
            let _ = bin.remove(adapter);
        }
        return Err(e);
    }

    // Unlink before linking: the target accepts one upstream link only.
    let unlinked = src_pad
        .unlink(&old_in)
        .and_then(|_| {
            if old.is_empty() {
                Ok(())
            } else {
                old_out.unlink(target)
            }
        })
        .map_err(|e| format!("could not unlink {}: {}", src_pad.name(), e));
    let spliced = unlinked.and_then(|_| {
        if new.is_empty() {
            return src_pad.link(target).map(|_| ()).map_err(|e| {
                format!(
                    "could not link {} to {}: {}",
                    src_pad.name(),
                    target.name(),
                    e
                )
            });
        }
        new_out.link(target).map_err(|e| {
            format!(
                "could not link {} to {}: {}",
                new_out.name(),
                target.name(),
                e
            )
        })?;
        src_pad.link(&new_in).map_err(|e| {
            format!(
                "could not link {} to {}: {}",
                src_pad.name(),
                new_in.name(),
                e
            )
        })?;
        // State last, so the elements negotiate against a complete topology.
        for adapter in new {
            adapter.sync_state_with_parent().map_err(|e| {
                format!(
                    "{} could not reach the pipeline state: {}",
                    adapter.name(),
                    e
                )
            })?;
        }
        Ok(())
    });

    if let Err(e) = spliced {
        // Put the input back as it was.
        if !new.is_empty() {
            let _ = src_pad.unlink(&new_in);
            let _ = new_out.unlink(target);
        } else {
            let _ = src_pad.unlink(target);
        }
        for adapter in new {
            let _ = adapter.set_state(gst::State::Null);
            let _ = bin.remove(adapter);
        }
        if !old.is_empty() {
            let _ = old_out.link(target);
        }
        let _ = src_pad.link(&old_in);
        return Err(e);
    }

    for adapter in old {
        if let Err(e) = adapter.set_state(gst::State::Null) {
            debug!("{} did not shut down cleanly: {}", adapter.name(), e);
        }
        let _ = bin.remove(adapter);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn caps(s: &str) -> gst::Caps {
        // Caps parsing needs the type system registered.
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    fn all(_: &str) -> bool {
        true
    }

    fn none(_: &str) -> bool {
        false
    }

    /// The sink is asked only about GPU memory other than GL.
    fn not_asked() -> bool {
        panic!("the sink should not have been asked")
    }

    fn plan_for(c: &str) -> Plan {
        plan(&caps(c), not_asked, all).expect("plan")
    }

    /// 8-bit 4:2:0: NV12 is what the hardware encoders take, I420 what the VP9
    /// and AV1 software encoders take.
    #[test]
    fn eight_bit_420_is_left_alone() {
        for format in ["NV12", "I420"] {
            assert_eq!(
                plan_for(&format!(
                    "video/x-raw, format={}, width=1920, height=1080",
                    format
                )),
                Plan::default(),
                "{} should be passed through",
                format
            );
        }
    }

    /// Everything webrtcsink would otherwise convert once per consumer: packed
    /// RGB from a mixer or a screen capture, packed and planar YUV that is not
    /// 4:2:0, and gray.
    #[test]
    fn other_eight_bit_formats_are_converted() {
        for format in [
            "RGBA", "BGRA", "RGB", "BGRx", "UYVY", "YUY2", "Y42B", "Y444", "GRAY8",
        ] {
            assert_eq!(
                plan_for(&format!(
                    "video/x-raw, format={}, width=1920, height=1080",
                    format
                )),
                Plan {
                    memory: vec![],
                    convert: true
                },
                "{} should be converted",
                format
            );
        }
    }

    /// GL memory in a format the encoders do not take needs both adaptations;
    /// one already in an encoder-native format only needs the download.
    #[test]
    fn gl_memory_is_downloaded_and_judged_on_its_format() {
        assert_eq!(
            plan_for("video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080"),
            Plan {
                memory: vec![Adapter::GlDownload],
                convert: true
            }
        );
        assert_eq!(
            plan_for("video/x-raw(memory:GLMemory), format=NV12, width=1920, height=1080"),
            Plan {
                memory: vec![Adapter::GlDownload],
                convert: false
            }
        );
    }

    /// The encoders that advertise these memory types take them directly, so
    /// nothing is downloaded or converted when the sink accepts them.
    #[test]
    fn gpu_memory_the_sink_accepts_is_left_in_place() {
        for feature in [
            "memory:CUDAMemory",
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
            "memory:DMABuf",
        ] {
            assert_eq!(
                plan(
                    &caps(&format!("video/x-raw({}), format=RGBA", feature)),
                    || true,
                    all
                ),
                Ok(Plan::default()),
                "{} should be left in place",
                feature
            );
        }
    }

    /// CUDA memory the sink refuses is downloaded, then judged on its format.
    #[test]
    fn cuda_memory_the_sink_refuses_is_downloaded() {
        assert_eq!(
            plan(
                &caps("video/x-raw(memory:CUDAMemory), format=RGBA"),
                || false,
                all
            ),
            Ok(Plan {
                memory: vec![Adapter::CudaDownload],
                convert: true
            })
        );
        assert_eq!(
            plan(
                &caps("video/x-raw(memory:CUDAMemory), format=RGBA"),
                || false,
                none
            ),
            Err(Refusal::MissingFactory {
                factory: video_adapt::CUDA_DOWNLOAD_FACTORY
            })
        );
    }

    /// Memory nothing here can download, refused by the sink, is a refusal.
    #[test]
    fn other_memory_the_sink_refuses_is_a_refusal() {
        for feature in [
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
            "memory:DMABuf",
        ] {
            assert_eq!(
                plan(
                    &caps(&format!("video/x-raw({}), format=NV12", feature)),
                    || false,
                    all
                ),
                Err(Refusal::UnsupportedMemory {
                    feature: feature.to_string()
                }),
                "{}",
                feature
            );
        }
    }

    /// Converting these would drop precision the encoder behind the pad may
    /// well be able to keep.
    #[test]
    fn deeper_than_eight_bit_is_left_alone() {
        for format in ["P010_10LE", "I420_10LE", "AYUV64", "RGBA64_LE", "v210"] {
            assert_eq!(
                plan_for(&format!("video/x-raw, format={}", format)),
                Plan::default(),
                "{} should be passed through",
                format
            );
        }
    }

    /// WHEP Output also takes pre-encoded video, which no converter can touch,
    /// and before negotiation settles there is nothing to compare against.
    #[test]
    fn encoded_and_unfixed_caps_are_left_alone() {
        for c in [
            "video/x-h264, stream-format=avc, alignment=au",
            "video/x-h265",
            "video/x-vp9",
            "video/x-av1",
            "audio/x-raw, rate=48000",
            "video/x-raw, width=1920, height=1080",
            "ANY",
            "EMPTY",
        ] {
            assert_eq!(
                plan_for(c),
                Plan::default(),
                "{} should be passed through",
                c
            );
        }
    }

    fn factories(adapters: &[gst::Element]) -> Vec<String> {
        adapters
            .iter()
            .map(|e| {
                e.factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The download has to come first: the converter takes system memory.
    #[test]
    fn downloads_are_built_before_the_converter() {
        let _ = gst::init();
        let adapters = build_adapters(
            &Plan {
                memory: vec![Adapter::GlDownload],
                convert: true,
            },
            "video_queue",
        )
        .expect("adapters");
        assert_eq!(
            factories(&adapters),
            ["gldownload", "videoconvert", "capsfilter"]
        );
        assert!(build_adapters(&Plan::default(), "video_queue")
            .expect("adapters")
            .is_empty());
    }

    /// `queue ! capsfilter(video/x-raw)`, with `old` between them, and an
    /// adapter whose output the capsfilter can never take.
    fn failing_splice(old: Option<&gst::Element>) -> (gst::Pipeline, gst::Pad, gst::Pad) {
        let _ = gst::init();
        let pipeline = gst::Pipeline::new();
        let queue = gst::ElementFactory::make("queue").build().expect("queue");
        let video_only = gst::ElementFactory::make("capsfilter")
            .property("caps", caps("video/x-raw"))
            .build()
            .expect("capsfilter");
        pipeline.add_many([&queue, &video_only]).expect("add");
        match old {
            Some(old) => {
                pipeline.add(old).expect("add");
                gst::Element::link_many([&queue, old, &video_only]).expect("link");
            }
            None => queue.link(&video_only).expect("link"),
        }
        (
            pipeline,
            queue.static_pad("src").expect("queue src"),
            video_only.static_pad("sink").expect("capsfilter sink"),
        )
    }

    fn audio_only() -> gst::Element {
        gst::ElementFactory::make("capsfilter")
            .name("audio_only")
            .property("caps", caps("audio/x-raw"))
            .build()
            .expect("capsfilter")
    }

    /// A link that cannot be made leaves the input linked to its sink as it
    /// was, with nothing left behind in the bin. Before, the source pad stayed
    /// unlinked and the half-built chain stayed in the bin.
    #[test]
    fn a_failed_splice_restores_the_original_link() {
        let (pipeline, src_pad, target) = failing_splice(None);
        let result = resplice(&src_pad, &target, &[], &[audio_only()]);
        assert!(result.is_err(), "the splice should have failed");
        assert_eq!(
            src_pad.peer().as_ref(),
            Some(&target),
            "the input should be linked to its sink again"
        );
        assert!(
            pipeline.by_name("audio_only").is_none(),
            "the adapter should have been removed from the bin"
        );
    }

    /// The same with adapters already in place: they stay in place.
    #[test]
    fn a_failed_resplice_keeps_the_adapters_in_place() {
        let _ = gst::init();
        let old = gst::ElementFactory::make("identity")
            .name("old")
            .build()
            .expect("identity");
        let (pipeline, src_pad, target) = failing_splice(Some(&old));
        let result = resplice(
            &src_pad,
            &target,
            std::slice::from_ref(&old),
            &[audio_only()],
        );
        assert!(result.is_err(), "the splice should have failed");
        assert_eq!(
            src_pad.peer().and_then(|p| p.parent_element()).as_ref(),
            Some(&old),
            "the input should feed the old adapter again"
        );
        assert_eq!(
            old.static_pad("src").and_then(|p| p.peer()).as_ref(),
            Some(&target),
            "the old adapter should feed the sink again"
        );
        assert!(pipeline.by_name("audio_only").is_none());
    }

    /// A pipeline through the bridge, reporting what the sink negotiated, how
    /// many buffers reached it, and how many elements the pipeline ended with.
    fn run_pipeline(format: &str) -> (String, usize, usize) {
        let _ = gst::init();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 15i32)
            .build()
            .expect("videotestsrc");
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", format)
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .build()
            .expect("capsfilter");
        let queue = gst::ElementFactory::make("queue")
            .name("video_queue")
            .build()
            .expect("queue");
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink");

        pipeline
            .add_many([&src, &filter, &queue, &sink])
            .expect("add");
        gst::Element::link_many([&src, &filter, &queue, &sink]).expect("link");

        let queue_src = queue.static_pad("src").expect("queue src pad");
        install_video_input_bridge(&queue_src, "video_queue");

        let sink_pad = sink.static_pad("sink").expect("fakesink sink pad");
        let buffers = Arc::new(AtomicUsize::new(0));
        let counter = buffers.clone();
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

        pipeline.set_state(gst::State::Playing).expect("play");

        let bus = pipeline.bus().expect("bus");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                match msg.view() {
                    gst::MessageView::Eos(_) => break,
                    gst::MessageView::Error(e) => {
                        panic!("pipeline error: {} ({:?})", e.error(), e.debug())
                    }
                    _ => {}
                }
            }
        }

        let negotiated = sink_pad
            .current_caps()
            .and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok()))
            .unwrap_or_default();
        let elements = pipeline.iterate_elements().into_iter().flatten().count();

        pipeline.set_state(gst::State::Null).expect("null");
        (negotiated, buffers.load(Ordering::Relaxed), elements)
    }

    /// Without the conversion, RGBA reaches the sink and every consumer
    /// converts it for itself.
    #[test]
    fn rgba_is_converted_before_the_peer() {
        let (format, buffers, elements) = run_pipeline("RGBA");
        assert_eq!(format, "NV12", "the peer should see converted video");
        assert!(buffers > 0, "no buffers reached the peer");
        assert_eq!(
            elements, 6,
            "expected a converter and a capsfilter to be spliced in"
        );
    }

    /// A path that already carries an encoder-native format must not pay for a
    /// conversion it does not need.
    #[test]
    fn nv12_reaches_the_peer_untouched() {
        let (format, buffers, elements) = run_pipeline("NV12");
        assert_eq!(format, "NV12");
        assert!(buffers > 0, "no buffers reached the peer");
        assert_eq!(elements, 4, "nothing should have been spliced in");
    }

    /// I420 is native to the software encoders and accepted by the hardware
    /// ones, so it is passed through as it is.
    #[test]
    fn i420_reaches_the_peer_untouched() {
        let (format, buffers, elements) = run_pipeline("I420");
        assert_eq!(format, "I420");
        assert!(buffers > 0, "no buffers reached the peer");
        assert_eq!(elements, 4, "nothing should have been spliced in");
    }
}
