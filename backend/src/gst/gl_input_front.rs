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

use crate::gst::video_adapt::{self, Adapter, Consumer};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The element that brings CUDA memory into GL. Ships with the GStreamer
/// nvcodec plugin, so it exists on NVIDIA builds only.
pub const CUDA_ADAPTER_FACTORY: &str = video_adapt::CUDA_DOWNLOAD_FACTORY;

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
        // The registry is only consulted for caps that call for the adapter.
        let decision =
            video_adapt::decide(&caps, Consumer::GlUpload, video_adapt::factory_available);
        if decision.as_ref().is_ok_and(|adapters| adapters.is_empty()) {
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

        match decision {
            // The GL-upload rule calls for nothing but the CUDA adapter.
            Ok(adapters) if !adapters.contains(&Adapter::CudaDownload) => gst::PadProbeReturn::Ok,
            Ok(_) => {
                match splice_when_idle(&upload, CUDA_ADAPTER_FACTORY, &label, SPLICE_WAIT) {
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
                settled.store(true, Ordering::Release);
                gst::PadProbeReturn::Remove
            }
        }
    });
}

/// How long a splice waits for the input's data flow to pause. A push into a
/// GL input finishes in a frame time or so; the wait is for that, not more.
const SPLICE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Splice `adapter_factory` in front of `upload` while no buffer is moving
/// between `upload` and its feeder, and wait up to `wait` for it.
///
/// At flow start nothing flows yet, but a producer can also turn to CUDA
/// memory mid-stream - a Media Player moving on to a file the GPU decodes -
/// while the feeder's streaming thread is still pushing the previous file's
/// frames. Relinking under it would hand a buffer to a pad that is unlinked
/// (`not-linked`, a flow error) or to an adapter not yet started (`flushing`,
/// which stops the feeder for good). An IDLE probe holds the data flow off
/// for the splice: it runs at once, on this thread, when the pad is idle, and
/// otherwise on the streaming thread as soon as the current push returns.
fn splice_when_idle(
    upload: &gst::Element,
    adapter_factory: &str,
    label: &str,
    wait: std::time::Duration,
) -> Result<(), String> {
    let feeder = upload
        .static_pad("sink")
        .and_then(|sink| sink.peer())
        .ok_or_else(|| format!("{} is not linked", upload.name()))?;
    let (done, result) = std::sync::mpsc::sync_channel(1);
    // The probe lives on a pad of the same pipeline: weak handle only.
    let upload_weak = upload.downgrade();
    let factory = adapter_factory.to_string();
    let label_owned = label.to_string();
    feeder.add_probe(gst::PadProbeType::IDLE, move |_, _| {
        let outcome = upload_weak
            .upgrade()
            .ok_or_else(|| "the input is gone".to_string())
            .and_then(|upload| splice_adapter(&upload, &factory, &label_owned));
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
    result.recv_timeout(wait).unwrap_or_else(|_| {
        Err(format!(
            "data kept flowing for {:?}; inserting once it pauses",
            wait
        ))
    })
}

/// Insert an `adapter_factory` element between `upload`'s sink pad and its peer.
/// The caller makes sure no buffer moves across that link meanwhile.
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

        let deferred = splice_when_idle(
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

        splice_when_idle(&up, "identity", "test input", SPLICE_WAIT).unwrap();
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
