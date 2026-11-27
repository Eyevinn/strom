//! Decoding of one segment's coded bytes into raw frames.
//!
//! Two stages per segment:
//! - **demux** in a short-lived pipeline (`appsrc ! demux ! h264parse !
//!   appsink`), CPU only and cheap: the object's access units, in the stream
//!   format the decoder takes;
//! - **decode** in a long-lived [`DecoderSession`] (one per decode worker):
//!   `decoder ! videoconvert ! appsink`, fed through a pad of our own. Per
//!   segment: flush, a new segment event, DISCONT on the first access unit,
//!   then end of stream, which drains the decoder of its last pictures. The
//!   session (and a hardware decoder's context) lives on, so a segment does
//!   not pay a decoder start-up.
//!
//! Frames come out as NV12 in presentation order with their container PTS,
//! which [`map_to_grid`] turns into frames on the group's TAI grid. The PTS
//! are those the demuxer gave, as with a pipeline per segment.

use super::grid::FrameGrid;
use super::source::Segment;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Pick the best available H.264 decoder.
pub fn pick_decoder() -> &'static str {
    // vtdec_hw comes after avdec_h264: in replay it sometimes drops the
    // last frame of a segment (docs/UPSTREAM_NOTES.md), and per-segment
    // decodes were slower with it (seek outside cache p50 235 vs 134 ms).
    for name in ["nvh264dec", "vah264dec", "avdec_h264", "vtdec_hw"] {
        if gst::ElementFactory::find(name).is_some() {
            return name;
        }
    }
    "decodebin"
}

/// Demuxer and appsrc caps for a container MIME type. A source that does
/// not say (`None`) is taken as MPEG-TS, the spike material and TAMS
/// Output's muxed mode; one that names another container is an error.
fn demux_for(container: Option<&str>) -> Result<(&'static str, &'static str), String> {
    match container.map(mime_essence).as_deref() {
        Some("video/mp4") | Some("video/quicktime") => Ok(("qtdemux", "video/quicktime")),
        None | Some("video/mp2t") => Ok((
            "tsdemux",
            "video/mpegts,systemstream=(boolean)true,packetsize=(int)188",
        )),
        Some(other) => Err(format!(
            "container '{}' is not supported (video/mp2t or video/mp4)",
            other
        )),
    }
}

/// A MIME type without parameters, lower case.
fn mime_essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Whether replay can decode a source with this container and codec: H.264
/// in MPEG-TS or MP4. A source that does not name its codec is tried as
/// H.264: the demux names the codec it finds when it is not.
pub fn check_supported(container: Option<&str>, codec: Option<&str>) -> Result<(), String> {
    demux_for(container)?;
    match codec.map(mime_essence).as_deref() {
        None | Some("video/h264") | Some("video/avc") => Ok(()),
        Some(other) => Err(format!(
            "codec '{}' is not supported (replay decodes H.264 only)",
            other
        )),
    }
}

/// Why a decode failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The source is in a container or codec replay does not decode: every
    /// segment would fail the same way.
    Unsupported(String),
    /// This segment did not decode (bad bytes, a timeout, a decoder error).
    Failed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

impl From<String> for DecodeError {
    fn from(m: String) -> Self {
        Self::Failed(m)
    }
}

impl From<&str> for DecodeError {
    fn from(m: &str) -> Self {
        Self::Failed(m.to_string())
    }
}

/// Decoded frames with their container PTS, and the output caps.
pub struct Decoded {
    pub frames: Vec<(u64, gst::Buffer)>,
    pub caps: gst::Caps,
    /// Earliest PTS among the demuxed access units, before decoding: the
    /// object's first sample, even when the decoder drops leading pictures.
    pub first_pts: Option<u64>,
}

/// How long a decode may take before it is given up: five times the
/// segment's duration, at least 5 s and at most 60 s.
pub fn decode_timeout(segment_ns: i64) -> Duration {
    Duration::from_nanos((segment_ns.max(0) as u64).saturating_mul(5))
        .clamp(Duration::from_secs(5), Duration::from_secs(60))
}

/// The least of a decode's timeout the decode stage gets after the demux:
/// `timeout / MIN_DECODE_SHARE`, however long the demux took.
const MIN_DECODE_SHARE: u32 = 2;

/// Hung pipelines (sessions or per-segment pipelines whose teardown has not
/// finished) past which no new decoder is made.
const MAX_WEDGED: usize = 4;

/// Segment bytes shared with the cache, handed to GStreamer without a copy.
struct SharedBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Decodes segments for one worker: a long-lived [`DecoderSession`], made on
/// first use and made again after a failure.
pub struct SegmentDecoder {
    decoder: &'static str,
    session: Option<DecoderSession>,
}

impl SegmentDecoder {
    pub fn new(decoder: &'static str) -> Self {
        Self {
            decoder,
            session: None,
        }
    }

    /// Decode one segment held in memory. Fails on a pipeline error, or when
    /// the stream has not ended within `timeout`.
    pub fn decode(
        &mut self,
        bytes: Arc<Vec<u8>>,
        container: Option<&str>,
        timeout: Duration,
    ) -> Result<Decoded, DecodeError> {
        // decodebin has no sink pad to feed: one pipeline per segment.
        if self.decoder == "decodebin" {
            refuse_while_wedged()?;
            return decode_bytes(bytes, container, self.decoder, timeout);
        }
        let deadline = Instant::now() + timeout;
        if self.session.is_none() {
            // Each hung decoder holds a thread (and maybe a hardware
            // context) that may never come back: past a few, stop making
            // new ones until their teardown finishes.
            refuse_while_wedged()?;
            let started = Instant::now();
            self.session = Some(DecoderSession::new(self.decoder)?);
            debug!(
                "Replay: {} session started in {} ms",
                self.decoder,
                started.elapsed().as_millis()
            );
        }
        let session = self.session.as_mut().expect("made above");
        let started = Instant::now();
        let units = demux_bytes(bytes, container, &session.sink_caps, timeout)?;
        let demux_ms = started.elapsed().as_millis();
        // A slow demux must not leave the decoder no time and get a healthy
        // session given up as wedged: it gets at least a share of its own.
        let left = deadline
            .saturating_duration_since(Instant::now())
            .max(timeout / MIN_DECODE_SHARE);
        let result = session.decode(units, left).map_err(DecodeError::from);
        if result.is_err() {
            // The decoder may be wedged or in error: start afresh next time.
            // Torn down on a thread of its own: a wedged decoder can block
            // its teardown too, and the worker must not wait for it.
            if let Some(s) = self.session.take() {
                retire(s);
            }
        }
        debug!(
            "Replay: demux {} ms, decode {} ms",
            demux_ms,
            started.elapsed().as_millis() - demux_ms
        );
        result
    }
}

/// An object's access units, in the caps the decoder takes.
pub struct Demuxed {
    pub caps: gst::Caps,
    pub units: Vec<gst::Buffer>,
    pub first_pts: Option<u64>,
}

/// Collects what an appsink receives, until end of stream.
#[derive(Default)]
struct SinkOut {
    frames: Vec<(u64, gst::Buffer)>,
    caps: Option<gst::Caps>,
    eos: bool,
}

type SharedOut = Arc<(Mutex<SinkOut>, Condvar)>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether a buffer belongs to a buffer pool: a long-lived decoder recycles
/// those, and the frame store holds frames for seconds, which would starve
/// a bounded pool.
fn from_pool(b: &gst::BufferRef) -> bool {
    // SAFETY: reads the `pool` field of a valid GstBuffer we hold a
    // reference to; it is set once, when the pool hands the buffer out.
    unsafe { !(*b.as_ptr()).pool.is_null() }
}

/// Appsink callbacks that collect samples (copying pooled buffers when
/// `own_pooled`) and note the end of stream.
fn collect_into(sink: &gst_app::AppSink, out: &SharedOut, own_pooled: bool) {
    let on_sample = Arc::clone(out);
    let on_eos = Arc::clone(out);
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                let mut o = lock(&on_sample.0);
                if o.caps.is_none() {
                    o.caps = sample.caps_owned();
                }
                if let Some(b) = sample.buffer_owned() {
                    let b = if own_pooled && from_pool(&b) {
                        b.copy_deep().map_err(|_| gst::FlowError::Error)?
                    } else {
                        b
                    };
                    let pts = b.pts().map(|p| p.nseconds());
                    o.frames.push((pts.unwrap_or(u64::MAX), b));
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .eos(move |_| {
                lock(&on_eos.0).eos = true;
                on_eos.1.notify_all();
            })
            .build(),
    );
}

/// The first error a pipeline posted and nobody has taken yet.
#[derive(Clone, Default)]
struct ErrorSlot(Arc<Mutex<Option<String>>>);

impl ErrorSlot {
    /// Keep the first error `pipeline` posts, and drop every message on its
    /// bus. Nothing else reads these buses: messages left queued (end of
    /// stream, latency and reset-time, a few per segment) would pile up for
    /// as long as the pipeline lives, and a decoder session lives as long as
    /// its worker.
    fn install(pipeline: &gst::Pipeline) -> Result<Self, String> {
        let slot = Self::default();
        let keep = slot.clone();
        pipeline
            .bus()
            .ok_or("no bus")?
            .set_sync_handler(move |_, msg| {
                if let gst::MessageView::Error(e) = msg.view() {
                    lock(&keep.0).get_or_insert_with(|| e.error().to_string());
                }
                gst::BusSyncReply::Drop
            });
        Ok(slot)
    }

    /// Take the error, if any.
    fn take(&self) -> Option<String> {
        lock(&self.0).take()
    }
}

/// Wait for end of stream at the appsink. Fails on a bus error, or when it
/// has not come by `deadline`: a stream that errors before its end (an
/// unlinkable codec, no video stream, garbage) never reaches the appsink as
/// end of stream, so waiting for it alone would block for ever.
fn wait_eos(out: &SharedOut, errors: &ErrorSlot, deadline: Instant) -> Result<SinkOut, String> {
    let mut o = lock(&out.0);
    while !o.eos {
        if let Some(e) = errors.take() {
            return Err(e);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err("no end of stream in time".into());
        }
        let wait = (deadline - now).min(Duration::from_millis(50));
        o = out
            .1
            .wait_timeout(o, wait)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
    if let Some(e) = errors.take() {
        return Err(e);
    }
    Ok(std::mem::take(&mut *o))
}

/// Push a whole object into a pipeline's appsrc as one buffer, then end of
/// stream.
fn feed_appsrc(pipeline: &gst::Pipeline, bytes: Arc<Vec<u8>>) -> Result<(), String> {
    let src = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or("no appsrc")?;
    src.set_max_bytes(bytes.len() as u64 + 1);
    src.push_buffer(gst::Buffer::from_slice(SharedBytes(bytes)))
        .map_err(|e| format!("push: {:?}", e))?;
    let _ = src.end_of_stream();
    Ok(())
}

/// Track the earliest PTS the parser passes on (per buffer: an atomic only).
fn probe_first_pts(pipeline: &gst::Pipeline) -> Arc<AtomicU64> {
    let first_pts = Arc::new(AtomicU64::new(u64::MAX));
    if let Some(pad) = pipeline.by_name("parse").and_then(|p| p.static_pad("src")) {
        let first = Arc::clone(&first_pts);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                first.fetch_min(pts.nseconds(), Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    }
    first_pts
}

/// Record the media type of each stream the demuxer (`demux`) exposes.
fn record_streams(pipeline: &gst::Pipeline) -> Arc<Mutex<Vec<String>>> {
    let streams: Arc<Mutex<Vec<String>>> = Arc::default();
    if let Some(demux) = pipeline.by_name("demux") {
        let seen = Arc::clone(&streams);
        demux.connect_pad_added(move |_, pad| {
            if let Some(name) = pad
                .current_caps()
                .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            {
                lock(&seen).push(name);
            }
        });
    }
    streams
}

/// An error naming the video codec when the demuxed streams have video but
/// no H.264: the only codec replay decodes.
fn unsupported_stream(streams: &[String]) -> Result<(), String> {
    let video: Vec<&str> = streams
        .iter()
        .map(String::as_str)
        .filter(|n| n.starts_with("video/"))
        .collect();
    match video.first() {
        Some(codec) if !video.contains(&"video/x-h264") => Err(format!(
            "the video in the segment is {}; replay decodes H.264 only",
            codec
        )),
        _ => Ok(()),
    }
}

/// Run a short-lived `appsrc ! … ! appsink` pipeline over one object, to its
/// end of stream: what reached the appsink, and the earliest parsed PTS.
fn run_once(
    desc: &str,
    sink_caps: Option<&gst::Caps>,
    bytes: Arc<Vec<u8>>,
    timeout: Duration,
) -> Result<(SinkOut, Option<u64>), DecodeError> {
    let pipeline = gst::parse::launch(desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let result = (|| -> Result<_, DecodeError> {
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or("no appsink")?;
        sink.set_caps(sink_caps);
        let errors = ErrorSlot::install(&pipeline)?;
        let out: SharedOut = Arc::default();
        collect_into(&sink, &out, false);
        let first_pts = probe_first_pts(&pipeline);
        let streams = record_streams(&pipeline);
        // A pipeline outside a flow: give its pool threads normal scheduling.
        crate::gst::thread_priority::reset_streaming_threads(&pipeline);
        feed_appsrc(&pipeline, bytes)?;
        let deadline = Instant::now() + timeout;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;
        let got = wait_eos(&out, &errors, deadline);
        // A stream replay cannot decode (H.265 in TS, say) fails at the
        // demuxer's unlinked pad, or ends with nothing: name the codec.
        if got.as_ref().map_or(true, |g| g.frames.is_empty()) {
            unsupported_stream(&lock(&streams)).map_err(DecodeError::Unsupported)?;
        }
        let got = got?;
        let first = Some(first_pts.load(Ordering::Relaxed)).filter(|p| *p != u64::MAX);
        Ok((got, first))
    })();
    if result.is_ok() {
        let _ = pipeline.set_state(gst::State::Null);
    } else {
        // A pipeline that did not reach its end may be stuck in an element,
        // and so may its teardown: not on the caller's thread.
        retire_on_thread("replay-demux-retire", move || {
            let _ = pipeline.set_state(gst::State::Null);
        });
    }
    result
}

/// Demux one object into access units in `sink_caps` (the decoder's).
pub fn demux_bytes(
    bytes: Arc<Vec<u8>>,
    container: Option<&str>,
    sink_caps: &gst::Caps,
    timeout: Duration,
) -> Result<Demuxed, DecodeError> {
    let (demux, src_caps) = demux_for(container).map_err(DecodeError::Unsupported)?;
    let desc = format!(
        "appsrc name=src format=bytes is-live=false caps=\"{}\" ! {} name=demux ! \
         h264parse name=parse ! appsink name=sink sync=false",
        src_caps, demux
    );
    let (got, first_pts) = run_once(&desc, Some(sink_caps), bytes, timeout)?;
    let caps = got.caps.ok_or("no access units demuxed")?;
    Ok(Demuxed {
        caps,
        units: got.frames.into_iter().map(|(_, b)| b).collect(),
        first_pts,
    })
}

/// Decode one segment held in memory in a pipeline of its own, made and
/// torn down for it.
pub fn decode_bytes(
    bytes: Arc<Vec<u8>>,
    container: Option<&str>,
    decoder: &str,
    timeout: Duration,
) -> Result<Decoded, DecodeError> {
    let (demux, src_caps) = demux_for(container).map_err(DecodeError::Unsupported)?;
    let desc = format!(
        "appsrc name=src format=bytes is-live=false caps=\"{}\" ! {} name=demux ! \
         h264parse name=parse ! {} ! videoconvert ! video/x-raw,format=NV12 ! appsink name=sink sync=false",
        src_caps, demux, decoder
    );
    let (got, first_pts) = run_once(&desc, None, bytes, timeout)?;
    let caps = got.caps.ok_or("no frames decoded")?;
    Ok(Decoded {
        frames: without_untimed(got.frames),
        caps,
        first_pts,
    })
}

/// Frames without a PTS cannot be placed: dropped (marked `u64::MAX`).
fn without_untimed(frames: Vec<(u64, gst::Buffer)>) -> Vec<(u64, gst::Buffer)> {
    frames.into_iter().filter(|(p, _)| *p != u64::MAX).collect()
}

/// A long-lived `decoder ! videoconvert ! appsink` pipeline, fed access
/// units through a pad of our own (pushes run on the calling thread).
pub struct DecoderSession {
    pipeline: gst::Pipeline,
    feed: gst::Pad,
    errors: ErrorSlot,
    out: SharedOut,
    /// What the decoder's sink pad takes: the demux stage's output caps.
    pub sink_caps: gst::Caps,
    /// Caps last sent to the decoder.
    caps: Option<gst::Caps>,
    /// Whether a segment went through (the next one starts with a flush).
    used: bool,
}

impl DecoderSession {
    pub fn new(decoder: &str) -> Result<Self, String> {
        let dec = gst::ElementFactory::make(decoder)
            .build()
            .map_err(|e| e.to_string())?;
        Self::with_decoder(dec)
    }

    /// A session around a decoder element made by the caller.
    fn with_decoder(dec: gst::Element) -> Result<Self, String> {
        let decoder = dec
            .factory()
            .map_or("decoder".into(), |f| f.name().to_string());
        let pipeline = gst::Pipeline::with_name("replay-decoder");
        let make = |f: &str| {
            gst::ElementFactory::make(f)
                .build()
                .map_err(|e| e.to_string())
        };
        let convert = make("videoconvert")?;
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "NV12")
                    .build(),
            )
            .build()
            .map_err(|e| e.to_string())?;
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .async_(false)
            .enable_last_sample(false)
            .build();
        pipeline
            .add_many([&dec, &convert, &filter, sink.upcast_ref()])
            .map_err(|e| e.to_string())?;
        gst::Element::link_many([&dec, &convert, &filter, sink.upcast_ref()])
            .map_err(|e| e.to_string())?;
        let out: SharedOut = Arc::default();
        collect_into(&sink, &out, true);
        let dec_sink = dec.static_pad("sink").ok_or("decoder has no sink pad")?;
        let sink_caps = dec_sink.query_caps(None);
        if sink_caps.is_empty() {
            return Err(format!("{} takes no caps", decoder));
        }
        let feed = gst::Pad::builder(gst::PadDirection::Src)
            .name("feed")
            .build();
        feed.link(&dec_sink).map_err(|e| format!("link: {:?}", e))?;
        let errors = ErrorSlot::install(&pipeline)?;
        crate::gst::thread_priority::reset_streaming_threads(&pipeline);
        let session = Self {
            pipeline,
            feed,
            errors,
            out,
            sink_caps,
            caps: None,
            used: false,
        };
        session
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;
        session
            .feed
            .set_active(true)
            .map_err(|e| format!("activate: {}", e))?;
        if !session.feed.push_event(
            gst::event::StreamStart::builder("replay-decoder")
                .group_id(gst::GroupId::next())
                .build(),
        ) {
            return Err("stream start refused".into());
        }
        Ok(session)
    }

    /// Decode one object's access units: flush (after the first), caps when
    /// they change, a new segment, the units with DISCONT on the first, and
    /// end of stream to drain the decoder.
    ///
    /// The feeding runs on a thread of its own: a push runs the decoder's
    /// chain function on the pushing thread, and a decoder that blocks there
    /// (a hardware decoder out of surfaces, a driver that hangs) would hold
    /// the caller for good. Past `timeout` the decode fails and the session
    /// is wedged: drop it (see [`retire`]) and make a new one.
    pub fn decode(&mut self, d: Demuxed, timeout: Duration) -> Result<Decoded, String> {
        let deadline = Instant::now() + timeout;
        let flush = self.used;
        self.used = true;
        *lock(&self.out.0) = SinkOut::default();
        let caps = (self.caps.as_ref() != Some(&d.caps)).then(|| d.caps.clone());
        if caps.is_some() {
            self.caps = Some(d.caps.clone());
        }
        let first_pts = d.first_pts;
        let feed = self.feed.clone();
        let errors = self.errors.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("replay-feed".into())
            .spawn(move || {
                let _ = tx.send(feed_units(&feed, &errors, flush, caps, d.units));
            })
            .map_err(|e| format!("feed thread: {}", e))?;
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(r) => r?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "decoder did not take the segment within {} ms",
                    timeout.as_millis()
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("feed thread ended without a result".into());
            }
        }
        let got = wait_eos(&self.out, &self.errors, deadline)?;
        let caps = got.caps.ok_or("no frames decoded")?;
        Ok(Decoded {
            frames: without_untimed(got.frames),
            caps,
            first_pts,
        })
    }
}

/// Push one object into a session's decoder: flush (`flush`), caps (when
/// given), a new segment, the units with DISCONT on the first, end of stream.
fn feed_units(
    feed: &gst::Pad,
    errors: &ErrorSlot,
    flush: bool,
    caps: Option<gst::Caps>,
    units: Vec<gst::Buffer>,
) -> Result<(), String> {
    let event = |e: gst::Event| {
        let name = e.type_().name();
        if feed.push_event(e) {
            Ok(())
        } else {
            Err(match errors.take() {
                Some(err) => err,
                None => format!("{} refused", name),
            })
        }
    };
    if flush {
        event(gst::event::FlushStart::new())?;
        event(gst::event::FlushStop::new(true))?;
    }
    if let Some(caps) = caps {
        event(gst::event::Caps::new(&caps))?;
    }
    event(gst::event::Segment::new(&gst::FormattedSegment::<
        gst::ClockTime,
    >::new()))?;
    for (i, mut b) in units.into_iter().enumerate() {
        if i == 0 {
            b.make_mut().set_flags(gst::BufferFlags::DISCONT);
        }
        feed.push(b).map_err(|e| format!("push: {:?}", e))?;
    }
    event(gst::event::Eos::new())
}

/// Pipelines given up as wedged whose teardown has not finished: each holds
/// a thread (its feed, its teardown) that may never return.
static WEDGED: AtomicUsize = AtomicUsize::new(0);

/// Fail while [`MAX_WEDGED`] or more pipelines are still being torn down: a
/// decoder that hangs would hang the next one too, and each holds a thread.
/// Decoding resumes as their teardown finishes.
fn refuse_while_wedged() -> Result<(), String> {
    let n = WEDGED.load(Ordering::Relaxed);
    if n >= MAX_WEDGED {
        return Err(format!(
            "{} decoder pipelines are hung; no new decoder until they are torn down",
            n
        ));
    }
    Ok(())
}

/// Run a teardown on a thread of its own, counted in [`WEDGED`] until it
/// returns.
fn retire_on_thread(name: &str, teardown: impl FnOnce() + Send + 'static) {
    let n = WEDGED.fetch_add(1, Ordering::Relaxed) + 1;
    if n > 1 {
        warn!(
            "Replay: {} decoder pipelines are still being torn down; a decoder may be hung",
            n
        );
    }
    let spawned = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            teardown();
            WEDGED.fetch_sub(1, Ordering::Relaxed);
        });
    if let Err(e) = spawned {
        WEDGED.fetch_sub(1, Ordering::Relaxed);
        warn!("Replay: no thread to tear a decoder pipeline down: {}", e);
    }
}

/// Tear a failed session down on a thread of its own. A flush unblocks a
/// decoder waiting downstream; one stuck in its driver stays stuck, and then
/// only that thread is lost, not a decode worker.
fn retire(session: DecoderSession) {
    retire_on_thread("replay-dec-retire", move || {
        let _ = session.feed.push_event(gst::event::FlushStart::new());
        drop(session);
    });
}

impl Drop for DecoderSession {
    fn drop(&mut self) {
        let _ = self.feed.set_active(false);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Place decoded frames on the TAI timeline: a frame's TAI is the segment's first
/// sample TAI plus its PTS distance from the object's first PTS. That is
/// `first_pts` (the earliest demuxed PTS) when known, so pictures the decoder
/// drops at the start (open GOP, a segment not starting on a keyframe) do not
/// shift the rest; else the earliest decoded PTS. Frames outside the
/// segment's timerange (partial use of an object) are dropped. Container PTS
/// is never taken as TAI: the segment timerange is the link between the two
/// (TS PTS wraps at 33 bits, demuxers rebase it).
///
/// Each frame gets its own TAI, snapped to the segment's grid: the source's
/// rate (from `grid`) through the segment's first sample. Not to the
/// player's grid: a source can change its frame phase at a restart (a new
/// run, after a gap), and frames placed on the first run's phase were shown
/// up to a frame off their capture time. The snap only takes out PTS
/// jitter (90 kHz rounding, writer drift).
pub fn map_to_grid<T>(
    seg: &Segment,
    grid: &FrameGrid,
    frames: Vec<(u64, T)>,
    first_pts: Option<u64>,
) -> Vec<(i64, T)> {
    let Some(decoded_min) = frames.iter().map(|(p, _)| *p).min() else {
        return Vec::new();
    };
    let min_pts = first_pts.map_or(decoded_min, |p| p.min(decoded_min));
    let half = (grid.frame_ns() / 2.0) as i64;
    let base = seg.first_sample_tai();
    let Some(run) = FrameGrid::new(grid.fps_n, grid.fps_d, base) else {
        return Vec::new();
    };
    let mut out: Vec<(i64, T)> = frames
        .into_iter()
        .filter_map(|(pts, f)| {
            let tai = base + (pts - min_pts) as i64;
            (tai >= seg.start_ns - half && tai < seg.end_ns - half)
                .then(|| (run.tai_of_frame(run.nearest_frame(tai)), f))
        })
        .collect();
    out.sort_by_key(|(k, _)| *k);
    out.dedup_by_key(|(k, _)| *k);
    out
}

/// Frame rate from decoded caps, if fixed and non-zero.
pub fn caps_frame_rate(caps: &gst::Caps) -> Option<(u32, u32)> {
    let s = caps.structure(0)?;
    let f = s.get::<gst::Fraction>("framerate").ok()?;
    (f.numer() > 0 && f.denom() > 0).then(|| (f.numer() as u32, f.denom() as u32))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::replay::source::Location;

    const BASE: i64 = 1_800_000_000_000_000_000;

    fn seg(start: i64, end: i64) -> Segment {
        Segment {
            start_ns: start,
            end_ns: end,
            ts_offset_ns: 0,
            object_start_ns: None,
            object_id: "o".into(),
            location: Location::Urls(vec![]),
        }
    }

    #[test]
    fn frames_map_from_segment_start_not_pts() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let k0 = grid.floor_frame(BASE);
        // Container PTS starts at an arbitrary 3600 s (tsdemux rebasing).
        let pts0 = 3_600_000_000_000u64;
        let frames: Vec<(u64, u32)> = (0..50).map(|i| (pts0 + i * 20_000_000, i as u32)).collect();
        let s = seg(BASE + 1_000_000_000, BASE + 2_000_000_000);
        let out = map_to_grid(&s, &grid, frames, None);
        assert_eq!(out.len(), 50);
        assert_eq!(out[0], (grid.tai_of_frame(k0 + 50), 0));
        assert_eq!(out[49], (grid.tai_of_frame(k0 + 99), 49));
    }

    #[test]
    fn jittered_pts_land_on_the_same_frames() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        // 90 kHz rounding: 20 ms = 1800 ticks, but 59.94-style drift of a few
        // µs must not move a frame.
        let frames: Vec<(u64, u32)> = (0..50)
            .map(|i| {
                (
                    i * 20_000_000 + if i % 2 == 0 { 11_111 } else { 0 },
                    i as u32,
                )
            })
            .collect();
        let s = seg(BASE, BASE + 1_000_000_000);
        let out = map_to_grid(&s, &grid, frames, None);
        let ks: Vec<i64> = out
            .iter()
            .map(|(t, _)| grid.floor_frame(*t) - grid.floor_frame(BASE))
            .collect();
        assert!(out.iter().all(|(t, _)| (t - BASE) % 20_000_000 == 0));
        assert_eq!(ks, (0..50).collect::<Vec<i64>>());
    }

    #[test]
    fn partial_object_use_drops_frames_outside_timerange() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let k0 = grid.floor_frame(BASE);
        // Object holds 2 s; the segment uses its second half, retimed by
        // ts_offset so that object time 1 s is flow time BASE.
        let mut s = seg(BASE, BASE + 1_000_000_000);
        s.ts_offset_ns = BASE - 1_000_000_000;
        s.object_start_ns = Some(0);
        let frames: Vec<(u64, u32)> = (0..100).map(|i| (i * 20_000_000, i as u32)).collect();
        let out = map_to_grid(&s, &grid, frames, None);
        assert_eq!(out.len(), 50);
        assert_eq!(out[0], (grid.tai_of_frame(k0), 50));
        assert_eq!(out[49], (grid.tai_of_frame(k0 + 49), 99));
    }

    #[test]
    fn dropped_leading_pictures_do_not_shift_the_segment() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let k0 = grid.floor_frame(BASE);
        // The object starts at PTS 0, but the decoder dropped its first two
        // pictures (leading B-frames of an open GOP).
        let frames: Vec<(u64, u32)> = (2..50).map(|i| (i * 20_000_000, i as u32)).collect();
        let s = seg(BASE, BASE + 1_000_000_000);
        let out = map_to_grid(&s, &grid, frames, Some(0));
        assert_eq!(out.len(), 48);
        assert_eq!(out[0], (grid.tai_of_frame(k0 + 2), 2));
        assert_eq!(out[47], (grid.tai_of_frame(k0 + 49), 49));
    }

    #[test]
    fn frames_follow_the_phase_of_their_own_segment() {
        // The player's grid has the first run's phase. After an ingest
        // restart the source's frames start 8.8 ms later in their 20 ms
        // period: they keep their own times, not the first run's.
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let shift = 8_800_000;
        let start = BASE + 100_000_000_000 + shift;
        let frames: Vec<(u64, u32)> = (0..50)
            .map(|i| {
                (
                    i * 20_000_000 + if i % 3 == 0 { 11_111 } else { 0 },
                    i as u32,
                )
            })
            .collect();
        let out = map_to_grid(&seg(start, start + 1_000_000_000), &grid, frames, None);
        assert_eq!(out.len(), 50);
        for (i, (t, f)) in out.iter().enumerate() {
            assert_eq!(*t, start + i as i64 * 20_000_000, "frame {}", f);
        }
    }

    #[test]
    fn empty_input() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let out = map_to_grid::<u8>(&seg(BASE, BASE + 1), &grid, vec![], None);
        assert!(out.is_empty());
    }

    /// MPEG-TS of `frames` frames of 320x240 at 25 fps, encoded with
    /// `encoder` (with B-frames for H.264).
    pub(in super::super) fn test_ts(encoder: &str, frames: u32) -> Vec<u8> {
        gst::init().unwrap();
        let (enc, parse) = match encoder {
            "x265enc" => ("x265enc key-int-max=25", "h265parse"),
            _ => ("x264enc bframes=2 key-int-max=25", "h264parse"),
        };
        let desc = format!(
            "videotestsrc num-buffers={} ! video/x-raw,width=320,height=240,framerate=25/1 ! \
             {} ! {} ! mpegtsmux ! appsink name=sink sync=false",
            frames, enc, parse
        );
        let p = gst::parse::launch(&desc)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let sink = p
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let mut out = Vec::new();
        while let Ok(s) = sink.pull_sample() {
            out.extend_from_slice(&s.buffer().unwrap().map_readable().unwrap());
        }
        p.set_state(gst::State::Null).unwrap();
        out
    }

    /// Run a decode on another thread; `None` if it has not returned in 30 s.
    fn decode_in_thread(bytes: Vec<u8>, timeout: Duration) -> Option<Result<Decoded, DecodeError>> {
        gst::init().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(decode_bytes(Arc::new(bytes), None, pick_decoder(), timeout));
        });
        rx.recv_timeout(Duration::from_secs(30)).ok()
    }

    #[test]
    fn h264_ts_decodes() {
        let d = decode_in_thread(test_ts("x264enc", 25), Duration::from_secs(10))
            .expect("decode returned")
            .expect("decode ok");
        assert_eq!(d.frames.len(), 25);
        let decoded_min = d.frames.iter().map(|f| f.0).min();
        assert_eq!(d.first_pts, decoded_min);
        assert_eq!(caps_frame_rate(&d.caps), Some((25, 1)));
    }

    #[test]
    fn garbage_fails_instead_of_hanging() {
        let bytes: Vec<u8> = (0..188 * 200u32).map(|i| (i * 7919 % 251) as u8).collect();
        let r = decode_in_thread(bytes, Duration::from_secs(5)).expect("decode returned");
        assert!(r.is_err());
    }

    #[test]
    fn empty_input_fails_instead_of_hanging() {
        let r = decode_in_thread(Vec::new(), Duration::from_secs(5)).expect("decode returned");
        assert!(r.is_err());
    }

    #[test]
    fn h265_ts_fails_instead_of_hanging() {
        // tsdemux's H.265 pad cannot link to h264parse: the stream stops
        // before its end of stream reaches the appsink.
        let r = decode_in_thread(test_ts("x265enc", 25), Duration::from_secs(5))
            .expect("decode returned");
        let e = r.err().expect("an error");
        assert!(matches!(e, DecodeError::Unsupported(_)), "{:?}", e);
        let e = e.to_string();
        assert!(
            e.contains("video/x-h265") && e.contains("H.264 only"),
            "names the codec: {}",
            e
        );
    }

    #[test]
    fn only_video_that_is_not_h264_is_unsupported() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(unsupported_stream(&s(&[])).is_ok());
        assert!(unsupported_stream(&s(&["audio/mpeg"])).is_ok());
        assert!(unsupported_stream(&s(&["video/x-h264", "audio/mpeg"])).is_ok());
        assert!(unsupported_stream(&s(&["audio/mpeg", "video/x-h265"])).is_err());
    }

    /// Decode with a long-lived session, on another thread; `None` if it has
    /// not returned in 30 s.
    fn session_decode_in_thread(
        objects: Vec<Vec<u8>>,
        timeout: Duration,
    ) -> Option<Vec<Result<Decoded, DecodeError>>> {
        gst::init().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut dec = SegmentDecoder::new(pick_decoder());
            let out: Vec<_> = objects
                .into_iter()
                .map(|b| dec.decode(Arc::new(b), None, timeout))
                .collect();
            let _ = tx.send(out);
        });
        rx.recv_timeout(Duration::from_secs(30)).ok()
    }

    #[test]
    fn a_hung_decoder_times_out_instead_of_holding_the_worker() {
        gst::init().unwrap();
        // identity sleeping 5 s per buffer: a decoder that blocks in chain.
        let dec = gst::ElementFactory::make("identity")
            .property("sleep-time", 5_000_000u32)
            .build()
            .unwrap();
        let mut session = DecoderSession::with_decoder(dec).expect("session");
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .field("width", 64i32)
            .field("height", 48i32)
            .field("framerate", gst::Fraction::new(25, 1))
            .build();
        let mut unit = gst::Buffer::from_mut_slice(vec![0u8; 64 * 48 * 3 / 2]);
        unit.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        let d = Demuxed {
            caps,
            units: vec![unit],
            first_pts: Some(0),
        };
        let started = Instant::now();
        let r = session.decode(d, Duration::from_millis(300));
        assert!(r.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "decode returned after {:?}",
            started.elapsed()
        );
        // Teardown does not block the caller either.
        let started = Instant::now();
        retire(session);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn unsupported_container_or_codec_is_an_error() {
        assert!(check_supported(None, None).is_ok());
        assert!(check_supported(Some("video/mp2t"), Some("video/h264")).is_ok());
        assert!(check_supported(Some("video/MP4; foo=bar"), Some("video/H264")).is_ok());
        assert!(check_supported(Some("video/x-matroska"), None).is_err());
        assert!(check_supported(Some("video/mp2t"), Some("video/h265")).is_err());
        assert!(demux_for(Some("application/octet-stream")).is_err());
    }

    fn pts_of(d: &Decoded) -> Vec<u64> {
        d.frames.iter().map(|f| f.0).collect()
    }

    #[test]
    fn a_session_decodes_segment_after_segment_like_a_pipeline_each() {
        // B-frames: the decoder holds pictures back; end of stream must drain
        // them for every segment, not only the last. Every frame keeps its
        // demuxed PTS: 25 fps from the object's first PTS.
        let a = test_ts("x264enc", 25);
        let b = test_ts("x264enc", 13);
        let one = |bytes: &Vec<u8>| {
            decode_in_thread(bytes.clone(), Duration::from_secs(10))
                .unwrap()
                .unwrap()
        };
        let (ra, rb) = (one(&a), one(&b));
        let out = session_decode_in_thread(
            vec![a.clone(), b.clone(), a.clone(), a],
            Duration::from_secs(10),
        )
        .expect("decodes returned");
        let want = [(&ra, 25u64), (&rb, 13), (&ra, 25), (&ra, 25)];
        for (i, (got, (one, n))) in out.iter().zip(want).enumerate() {
            let got = got.as_ref().expect("decode ok");
            assert_eq!(got.first_pts, one.first_pts, "segment {}", i);
            assert_eq!(got.caps, one.caps, "segment {}", i);
            if pick_decoder() == "vtdec_hw" {
                // VideoToolbox mis-stamps some B-frames of this material and
                // may drop a last frame, with a pipeline per segment too
                // (docs/UPSTREAM_NOTES.md): only the count is checked.
                let len = got.frames.len() as u64;
                assert!(len + 1 >= n && len <= n, "segment {}: {} frames", i, len);
                continue;
            }
            let first = got.first_pts.expect("first pts");
            let expect: Vec<u64> = (0..n).map(|k| first + k * 40_000_000).collect();
            assert_eq!(pts_of(got), expect, "segment {}", i);
        }
    }

    #[test]
    fn a_session_leaves_nothing_queued_on_its_bus() {
        // Each segment posts end of stream, latency and reset-time on the
        // session's bus; a long-lived session must not keep them.
        gst::init().unwrap();
        let mut session = DecoderSession::new(pick_decoder()).expect("session");
        let bytes = Arc::new(test_ts("x264enc", 10));
        for _ in 0..5 {
            let d = demux_bytes(
                Arc::clone(&bytes),
                None,
                &session.sink_caps,
                Duration::from_secs(10),
            )
            .expect("demux");
            let got = session.decode(d, Duration::from_secs(10)).expect("decode");
            assert_eq!(got.frames.len(), 10);
        }
        let bus = session.pipeline.bus().unwrap();
        let queued: Vec<_> = std::iter::from_fn(|| bus.pop())
            .map(|m| format!("{:?}", m.type_()))
            .collect();
        assert!(queued.is_empty(), "left on the bus: {:?}", queued);
    }

    #[test]
    fn a_session_recovers_after_a_bad_segment() {
        let good = test_ts("x264enc", 25);
        let bad: Vec<u8> = (0..188 * 200u32).map(|i| (i * 7919 % 251) as u8).collect();
        let out = session_decode_in_thread(
            vec![good.clone(), bad, test_ts("x265enc", 25), Vec::new(), good],
            Duration::from_secs(5),
        )
        .expect("decodes returned");
        assert_eq!(out[0].as_ref().unwrap().frames.len(), 25);
        assert!(matches!(out[1], Err(DecodeError::Failed(_))));
        let h265 = out[2].as_ref().err().unwrap();
        assert!(matches!(h265, DecodeError::Unsupported(_)), "{:?}", h265);
        assert!(h265.to_string().contains("video/x-h265"));
        assert!(out[3].is_err());
        assert_eq!(out[4].as_ref().unwrap().frames.len(), 25);
    }

    /// Real segments (`STROM_REPLAY_TEST_SEGMENTS`: a directory of `.ts`
    /// files): a session gives the same frames, bit for bit, as a pipeline
    /// per segment, in order, backwards and repeated.
    #[test]
    #[ignore]
    fn session_frames_equal_pipeline_frames_on_real_segments() {
        gst::init().unwrap();
        let dir = std::env::var("STROM_REPLAY_TEST_SEGMENTS").expect("segment directory");
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "ts"))
            .collect();
        paths.sort();
        let objects: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let mut order: Vec<usize> = (0..objects.len()).collect();
        order.extend((0..objects.len()).rev());
        order.extend([3, 3, 0, 7, 1]);
        let bytes_of = |b: &gst::Buffer| b.map_readable().unwrap().as_slice().to_vec();
        let one: Vec<Decoded> = objects
            .iter()
            .map(|o| {
                decode_bytes(
                    Arc::new(o.clone()),
                    None,
                    pick_decoder(),
                    Duration::from_secs(10),
                )
                .unwrap()
            })
            .collect();
        let mut dec = SegmentDecoder::new(pick_decoder());
        for i in order {
            let got = dec
                .decode(Arc::new(objects[i].clone()), None, Duration::from_secs(10))
                .unwrap();
            let want = &one[i];
            assert_eq!(pts_of(&got), pts_of(want), "segment {}", i);
            assert_eq!(got.first_pts, want.first_pts, "segment {}", i);
            for (k, (g, w)) in got.frames.iter().zip(&want.frames).enumerate() {
                assert!(
                    bytes_of(&g.1) == bytes_of(&w.1),
                    "segment {} frame {}",
                    i,
                    k
                );
            }
        }
    }

    /// Timing on real segments (`STROM_REPLAY_TEST_SEGMENTS`): `threads`
    /// workers (4 angles x 2 workers) decode every segment, with a session
    /// each and with a pipeline per segment; per-segment wall time, ms.
    #[test]
    #[ignore]
    fn bench_session_against_pipeline_per_segment() {
        gst::init().unwrap();
        let dir = std::env::var("STROM_REPLAY_TEST_SEGMENTS").expect("segment directory");
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "ts"))
            .collect();
        paths.sort();
        let objects: Arc<Vec<Arc<Vec<u8>>>> = Arc::new(
            paths
                .iter()
                .map(|p| Arc::new(std::fs::read(p).unwrap()))
                .collect(),
        );
        let threads = 8;
        for session in [false, true] {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let objects = Arc::clone(&objects);
                    std::thread::spawn(move || {
                        let mut dec = SegmentDecoder::new(pick_decoder());
                        let mut ms = Vec::new();
                        for o in objects.iter() {
                            let t = Instant::now();
                            let d = if session {
                                dec.decode(Arc::clone(o), None, Duration::from_secs(20))
                            } else {
                                decode_bytes(
                                    Arc::clone(o),
                                    None,
                                    pick_decoder(),
                                    Duration::from_secs(20),
                                )
                            };
                            assert!(d.is_ok());
                            ms.push(t.elapsed().as_secs_f64() * 1e3);
                        }
                        ms
                    })
                })
                .collect();
            let mut all: Vec<f64> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect();
            all.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| all[((all.len() as f64 * q) as usize).min(all.len() - 1)];
            eprintln!(
                "BENCH {} session={} threads={} n={} p50={:.0} p95={:.0} max={:.0} ms",
                pick_decoder(),
                session,
                threads,
                all.len(),
                p(0.5),
                p(0.95),
                all[all.len() - 1]
            );
        }
    }

    #[test]
    fn timeout_scales_with_segment_length() {
        assert_eq!(decode_timeout(1_000_000_000), Duration::from_secs(5));
        assert_eq!(decode_timeout(10_000_000_000), Duration::from_secs(50));
        assert_eq!(decode_timeout(i64::MAX), Duration::from_secs(60));
        assert_eq!(decode_timeout(-1), Duration::from_secs(5));
    }
}
