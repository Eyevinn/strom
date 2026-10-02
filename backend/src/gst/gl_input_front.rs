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
//! Only a producer that offers CUDA memory *and nothing else* is adapted. One
//! that also offers system or GL memory (a decoder) settles on one of those
//! from `glupload`'s caps and is left alone.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The caps feature that marks a buffer as living in CUDA memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// The element that brings CUDA memory into GL. Ships with the GStreamer
/// nvcodec plugin, so it exists on NVIDIA builds only.
pub const CUDA_ADAPTER_FACTORY: &str = "cudadownload";

/// What a GL consumer's input needs in front of its `glupload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontDecision {
    /// `glupload` takes these caps as they are: system memory (it uploads), GL
    /// memory (it passes through), or caps that offer one of those among others.
    Direct,
    /// CUDA memory only: put `cudadownload` in front of `glupload`.
    CudaAdapter,
    /// CUDA memory only, and `cudadownload` is not installed.
    CudaAdapterMissing,
}

/// Decide the input front for `caps` arriving at a GL consumer's input.
///
/// `caps` are what the producer offers: the caps of an ACCEPT_CAPS query or the
/// filter of a CAPS query. `cuda_adapter_available` says whether
/// [`CUDA_ADAPTER_FACTORY`] can be created.
pub fn decide(caps: &gst::CapsRef, cuda_adapter_available: bool) -> FrontDecision {
    if !offers_only_cuda_memory(caps) {
        return FrontDecision::Direct;
    }
    if cuda_adapter_available {
        FrontDecision::CudaAdapter
    } else {
        FrontDecision::CudaAdapterMissing
    }
}

/// True when every structure in `caps` is raw video in CUDA memory.
fn offers_only_cuda_memory(caps: &gst::CapsRef) -> bool {
    if caps.is_any() || caps.is_empty() {
        return false;
    }
    caps.iter_with_features().all(|(structure, features)| {
        structure.name() == "video/x-raw" && features.contains(CUDA_MEMORY_FEATURE)
    })
}

/// The caps a query arriving at the input says the producer offers, if any.
fn offered_caps(query: &gst::QueryRef) -> Option<gst::Caps> {
    match query.view() {
        gst::QueryView::AcceptCaps(q) => Some(q.caps_owned()),
        gst::QueryView::Caps(q) => q.filter_owned(),
        _ => None,
    }
}

/// Watch `front_sink` — the first sink pad of a GL consumer's video input — and
/// splice [`CUDA_ADAPTER_FACTORY`] in front of `upload` when the producer turns
/// out to offer CUDA memory only. `upload` is the input's `glupload`; `label`
/// names the input in logs and in the error posted when the adapter is missing.
pub fn install(front_sink: &gst::Pad, upload: &gst::Element, label: &str) {
    let label = label.to_string();
    // A weak handle: the probe is owned by a pad in the same pipeline, so a
    // strong one would keep the pipeline alive forever.
    let upload_weak = upload.downgrade();
    // Queries can arrive from several threads at once. The lock serialises the
    // splice; `settled` lets the ones that lost the race through untouched.
    let splice_lock = Arc::new(Mutex::new(()));
    let settled = Arc::new(AtomicBool::new(false));

    // QUERY_DOWNSTREAM fires per query, never per buffer. The probe stays until
    // the input has been adapted, so a producer that switches to CUDA memory
    // mid-stream (a Media Player moving to a file the GPU decodes) is caught too.
    front_sink.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |pad, info| {
        // Act before the query is answered, not on its way back.
        if !info.mask.contains(gst::PadProbeType::PUSH) || settled.load(Ordering::Acquire) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(gst::PadProbeData::Query(ref query)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(caps) = offered_caps(query) else {
            return gst::PadProbeReturn::Ok;
        };
        if !offers_only_cuda_memory(&caps) {
            return gst::PadProbeReturn::Ok;
        }

        let _guard = splice_lock.lock().unwrap_or_else(|p| p.into_inner());
        if settled.load(Ordering::Acquire) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(upload) = upload_weak.upgrade() else {
            // The pipeline is being torn down.
            return gst::PadProbeReturn::Remove;
        };

        let available = gst::ElementFactory::find(CUDA_ADAPTER_FACTORY).is_some();
        match decide(&caps, available) {
            FrontDecision::Direct => gst::PadProbeReturn::Ok,
            FrontDecision::CudaAdapter => {
                match splice_adapter(&upload, CUDA_ADAPTER_FACTORY, &label) {
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
                settled.store(true, Ordering::Release);
                gst::PadProbeReturn::Remove
            }
            FrontDecision::CudaAdapterMissing => {
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
                settled.store(true, Ordering::Release);
                gst::PadProbeReturn::Remove
            }
        }
    });
}

/// Insert an `adapter_factory` element between `upload`'s sink pad and its peer.
fn splice_adapter(upload: &gst::Element, adapter_factory: &str, label: &str) -> Result<(), String> {
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
        .link(&upload_sink)
        .map_err(|e| format!("could not link {} to {}: {}", name, upload.name(), e))
        .and_then(|_| {
            feeder
                .link(&sink)
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

    #[test]
    fn cuda_memory_gets_the_adapter() {
        assert_eq!(decide(&caps(CUDA), true), FrontDecision::CudaAdapter);
    }

    #[test]
    fn cuda_memory_without_the_adapter_is_reported() {
        assert_eq!(
            decide(&caps(CUDA), false),
            FrontDecision::CudaAdapterMissing
        );
    }

    /// `glupload` passes GL memory through and uploads system memory itself.
    #[test]
    fn gl_and_system_memory_go_direct() {
        for c in [
            "video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080",
            "video/x-raw, format=NV12, width=1920, height=1080",
            "video/x-raw(memory:SystemMemory), format=NV12",
            "video/x-raw(memory:DMABuf), format=DMA_DRM",
        ] {
            assert_eq!(decide(&caps(c), true), FrontDecision::Direct, "{}", c);
            assert_eq!(decide(&caps(c), false), FrontDecision::Direct, "{}", c);
        }
    }

    /// A producer that offers CUDA alongside something `glupload` takes — a
    /// decoder's template — settles on the latter during negotiation.
    #[test]
    fn cuda_among_other_memory_types_goes_direct() {
        let offer = caps(&format!(
            "{}; video/x-raw(memory:GLMemory), format=NV12; video/x-raw, format=NV12",
            CUDA
        ));
        assert_eq!(decide(&offer, true), FrontDecision::Direct);
        assert_eq!(decide(&offer, false), FrontDecision::Direct);
    }

    /// Extra features next to CUDA memory (an overlay meta) do not change what
    /// the frames live in.
    #[test]
    fn cuda_memory_with_a_meta_feature_gets_the_adapter() {
        let c =
            caps("video/x-raw(memory:CUDAMemory, meta:GstVideoOverlayComposition), format=NV12");
        assert_eq!(decide(&c, true), FrontDecision::CudaAdapter);
    }

    #[test]
    fn unconstrained_audio_and_encoded_caps_go_direct() {
        for c in [
            "ANY",
            "EMPTY",
            "audio/x-raw, rate=48000",
            "video/x-h264, stream-format=byte-stream",
        ] {
            assert_eq!(decide(&caps(c), true), FrontDecision::Direct, "{}", c);
        }
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
