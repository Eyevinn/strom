//! A file sink that starts a new file at a point where the stream can begin.
//!
//! Each container the Live Recorder writes can be cut at known places, and a
//! file that starts with the container's header and continues from such a
//! place plays on its own:
//!
//! - Fragmented MP4 (`isofmp4mux`): the init segment (`ftyp` + `moov`) arrives
//!   once, flagged `DISCONT | HEADER`. Each fragment header (`moof`) is flagged
//!   `HEADER` without `DELTA_UNIT`.
//! - Matroska (`matroskamux streamable=true`): the header is the caps'
//!   `streamheader`. A cluster that starts on a keyframe is neither `HEADER` nor
//!   `DELTA_UNIT`.
//! - MPEG-TS (`mpegtsmux`): PAT and PMT are the caps' `streamheader`. The packet
//!   that starts a keyframe is `HEADER` without `DELTA_UNIT`.
//!
//! A split closes the file, opens the next, writes the header, and carries on
//! from the next cut point. MP4 and Matroska carry absolute times, so the sink
//! moves each file's times back to start at zero; MPEG-TS players expect a
//! running PTS.
//!
//! It is a plain element with a sink pad, not a `BaseSink`: it does not sync to
//! the clock and has nothing to preroll. A `BaseSink` that has not had a buffer
//! by PLAYING still counts as not prerolled, and a first buffer arriving while
//! the pipeline heads for PAUSED, as at a stop, waits in preroll for a PLAYING
//! that never comes. Without that machinery, a recording whose first data
//! arrives late changes no state, and the stop drain always gets its data
//! through. It flushes at every cut point, so a crash loses at most what came
//! after the last one.

use super::{mkv_cluster, mp4_boxes};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gstreamer as gst;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a stop waits for the muxer to hand over what it holds.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// Longest the sink keeps written data in its buffer. Cut points flush too, but
/// while the video is stalled there are none, and the audio still has to reach
/// the disk.
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);

/// Called once for every file the sink opens, with its index, its path, and the
/// running time of the file's t=0 when the sink can tell it (MP4 and Matroska).
///
/// For those the call waits until the file's first fragment or cluster, which is
/// where its t=0 is decided, so it can come up to a fragment after the file was
/// opened. A file that ends before then is reported without a start.
pub type FileOpenedFn =
    Arc<dyn Fn(&FragmentFileSink, u32, &std::path::Path, Option<gst::ClockTime>) + Send + Sync>;

/// A file to report: index, path, and the running time of its t=0.
type Report = (u32, PathBuf, Option<gst::ClockTime>);

/// When the sink starts a new file. Both limits apply; whichever is reached
/// first splits, at the next cut point.
#[derive(Clone, Debug, Default)]
pub struct SplitPolicy {
    pub max_duration: Option<gst::ClockTime>,
    pub max_bytes: Option<u64>,
}

/// The container the sink is writing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    FragmentedMp4,
    Matroska,
    MpegTs,
}

impl Format {
    fn is_cut_point(self, flags: gst::BufferFlags) -> bool {
        let header = flags.contains(gst::BufferFlags::HEADER);
        let delta = flags.contains(gst::BufferFlags::DELTA_UNIT);
        let discont = flags.contains(gst::BufferFlags::DISCONT);
        match self {
            Format::FragmentedMp4 => header && !delta && !discont,
            Format::Matroska => !header && !delta,
            Format::MpegTs => header && !delta,
        }
    }

    /// Its header comes in the caps' `streamheader`, not as a buffer.
    fn header_in_caps(self) -> bool {
        self != Format::FragmentedMp4
    }
}

glib::wrapper! {
    pub struct FragmentFileSink(ObjectSubclass<imp::FragmentFileSink>)
        @extends gst::Element, gst::Object;
}

impl FragmentFileSink {
    /// `location` holds one `%05d`, replaced by the file index.
    pub fn new(name: &str, location: &str, format: Format, policy: SplitPolicy) -> Self {
        let sink: Self = glib::Object::builder().property("name", name).build();
        let mut settings = sink.imp().settings.lock().unwrap();
        settings.location = location.to_string();
        settings.format = format;
        settings.policy = policy;
        drop(settings);
        sink
    }

    /// Start a new file at the next cut point.
    pub fn split_now(&self) {
        self.imp().split_requested.store(true, Ordering::SeqCst);
    }

    /// The pads to send EOS into when the flow stops, so the muxer hands over
    /// what it still holds before the sink closes the file: each track's queue.
    pub fn set_drain_pads(&self, pads: Vec<gst::glib::WeakRef<gst::Pad>>) {
        *self.imp().drain_pads.lock().unwrap() = pads;
    }

    /// Report each file the sink opens. Replaces an earlier callback.
    pub fn set_file_opened_callback(&self, callback: FileOpenedFn) {
        *self.imp().on_file_opened.lock().unwrap() = Some(callback);
    }
}

mod imp {
    use super::*;
    use std::sync::LazyLock;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "stromfragmentsink",
            gst::DebugColorFlags::empty(),
            Some("Strom splitting file sink"),
        )
    });

    #[derive(Default)]
    pub struct Settings {
        pub location: String,
        pub format: Format,
        pub policy: SplitPolicy,
    }

    #[derive(Default)]
    struct State {
        file: Option<BufWriter<File>>,
        /// Index the next file opened gets.
        next_index: u32,
        /// The container header, written at the start of every file.
        header: Option<Vec<u8>>,
        /// PTS of the first cut point in the current file.
        file_start: Option<gst::ClockTime>,
        bytes_in_file: u64,
        /// The cut point a split is due at, and what followed it, held back
        /// until the next cut point. If the stream ends first, it is the
        /// muxer's tail and goes at the end of the current file rather than
        /// alone into a new one.
        held: Option<(Option<gst::ClockTime>, Vec<Chunk>)>,
        /// MP4: track id → timescale, from the init segment.
        timescales: HashMap<u32, u32>,
        /// MP4: decode time (ns) the current file starts at.
        /// Matroska: cluster timestamp the current file starts at.
        file_base: Option<u64>,
        /// The current file, opened but not reported yet: its start is not
        /// known until its first fragment or cluster.
        unreported: Option<(u32, PathBuf)>,
        last_flush: Option<Instant>,
    }

    /// One buffer's bytes, and whether the muxer flagged it as a header.
    struct Chunk {
        header: bool,
        bytes: Vec<u8>,
    }

    pub struct FragmentFileSink {
        sinkpad: gst::Pad,
        pub(super) settings: Mutex<Settings>,
        pub(super) split_requested: AtomicBool,
        pub(super) on_file_opened: Mutex<Option<FileOpenedFn>>,
        pub(super) drain_pads: Mutex<Vec<gst::glib::WeakRef<gst::Pad>>>,
        state: Mutex<State>,
        /// Set when EOS reaches the sink, for a stop that waits for it.
        eos: Mutex<bool>,
        eos_cond: Condvar,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FragmentFileSink {
        const NAME: &'static str = "StromFragmentFileSink";
        type Type = super::FragmentFileSink;
        type ParentType = gst::Element;

        fn with_class(klass: &Self::Class) -> Self {
            let templ = klass.pad_template("sink").unwrap();
            let sinkpad = gst::Pad::builder_from_template(&templ)
                .chain_function(|_pad, parent, buffer| {
                    FragmentFileSink::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |imp| imp.chain(buffer),
                    )
                })
                .event_function(|_pad, parent, event| {
                    FragmentFileSink::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.sink_event(event),
                    )
                })
                .build();
            Self {
                sinkpad,
                settings: Mutex::default(),
                split_requested: AtomicBool::default(),
                on_file_opened: Mutex::default(),
                drain_pads: Mutex::default(),
                state: Mutex::default(),
                eos: Mutex::default(),
                eos_cond: Condvar::default(),
            }
        }
    }

    impl ObjectImpl for FragmentFileSink {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.add_pad(&self.sinkpad).unwrap();
            // A sink to the bin: counted for EOS and asked for latency.
            obj.set_element_flags(gst::ElementFlags::SINK);
        }
    }

    impl GstObjectImpl for FragmentFileSink {}

    impl ElementImpl for FragmentFileSink {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("video/quicktime")
                            .field("variant", "iso-fragmented")
                            .build(),
                    )
                    .structure(gst::Structure::new_empty("video/x-matroska"))
                    // What matroskamux calls a recording without video.
                    .structure(gst::Structure::new_empty("audio/x-matroska"))
                    .structure(gst::Structure::new_empty("video/mpegts"))
                    .build();
                vec![gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap()]
            });
            TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            // Sinks change state first, so here everything upstream is still
            // PLAYING, and this sink still renders: in PAUSED it would hold the
            // muxer's last data waiting for PLAYING. Only on the way down to
            // READY or NULL; a pause must not end the file.
            match transition {
                gst::StateChange::ReadyToPaused => {
                    *self.state.lock().unwrap() = State::default();
                    *self.eos.lock().unwrap() = false;
                    self.split_requested.store(false, Ordering::SeqCst);
                }
                gst::StateChange::PlayingToPaused if self.stopping() => self.drain(),
                _ => {}
            }
            let result = self.parent_change_state(transition)?;
            if transition == gst::StateChange::PausedToReady {
                let mut reports = Vec::new();
                {
                    let mut state = self.state.lock().unwrap();
                    self.flush_held(&mut state, &mut reports);
                    Self::report_unstarted(&mut state, &mut reports);
                    state.file = None;
                }
                self.report(reports);
            }
            Ok(result)
        }
    }

    impl FragmentFileSink {
        fn format(&self) -> Format {
            self.settings.lock().unwrap().format
        }

        fn path_for(&self, index: u32) -> PathBuf {
            let settings = self.settings.lock().unwrap();
            PathBuf::from(settings.location.replace("%05d", &format!("{:05}", index)))
        }

        /// Close the current file, if any, and open the next one with the
        /// container header at its start.
        fn open_next_file(
            &self,
            state: &mut State,
            reports: &mut Vec<Report>,
        ) -> Result<(), gst::ErrorMessage> {
            if let Some((index, path)) = state.unreported.take() {
                reports.push((index, path, None));
            }
            if let Some(mut file) = state.file.take() {
                let _ = file.flush();
            }
            let index = state.next_index;
            let path = self.path_for(index);
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let file = File::create(&path).map_err(|e| {
                gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Could not create {}: {}", path.display(), e]
                )
            })?;
            let mut file = BufWriter::with_capacity(256 * 1024, file);
            let mut written = 0u64;
            if let Some(header) = &state.header {
                file.write_all(header).map_err(|e| {
                    gst::error_msg!(
                        gst::ResourceError::Write,
                        ["Could not write to {}: {}", path.display(), e]
                    )
                })?;
                written = header.len() as u64;
            }
            state.file = Some(file);
            state.next_index += 1;
            state.file_start = None;
            state.file_base = None;
            state.bytes_in_file = written;
            state.unreported = Some((index, path));
            Ok(())
        }

        /// Running time of the current file's t=0: `None` until it is known,
        /// `Some(None)` when the file will not have one.
        fn file_start_running_time(&self, state: &State) -> Option<Option<gst::ClockTime>> {
            let ns = match self.format() {
                // isofmp4mux and matroskamux write running time into the file,
                // whatever segment they put on their output: the decode time
                // and the cluster timestamp (in the default 1 ms TimestampScale)
                // the file was moved back by.
                Format::FragmentedMp4 => state.file_base?,
                Format::Matroska => state.file_base?.checked_mul(1_000_000)?,
                // A player starts the file at its earliest PTS, which the sink
                // would have to read from the PES headers. The cut point's time
                // is not it: that is the PAT/PMT ahead of the keyframe, stamped
                // with whichever frame mpegtsmux was writing.
                Format::MpegTs => return Some(None),
            };
            Some(Some(gst::ClockTime::from_nseconds(ns)))
        }

        /// Report the current file if its start is now known.
        fn report_if_started(&self, state: &mut State, reports: &mut Vec<Report>) {
            if state.unreported.is_none() {
                return;
            }
            if let Some(start) = self.file_start_running_time(state) {
                let (index, path) = state.unreported.take().unwrap();
                reports.push((index, path, start));
            }
        }

        /// Report the current file without a start: it ended before it had one.
        fn report_unstarted(state: &mut State, reports: &mut Vec<Report>) {
            if let Some((index, path)) = state.unreported.take() {
                reports.push((index, path, None));
            }
        }

        fn split_due(&self, state: &State, pts: Option<gst::ClockTime>) -> bool {
            let policy = self.settings.lock().unwrap().policy.clone();
            let by_time = match (policy.max_duration, state.file_start, pts) {
                (Some(max), Some(start), Some(pts)) => pts.saturating_sub(start) >= max,
                _ => false,
            };
            let by_size = policy
                .max_bytes
                .is_some_and(|max| state.bytes_in_file >= max);
            by_time || by_size
        }

        fn write(&self, state: &mut State, bytes: &[u8]) -> Result<(), gst::ErrorMessage> {
            let Some(file) = state.file.as_mut() else {
                return Ok(());
            };
            file.write_all(bytes).map_err(|e| {
                gst::error_msg!(
                    gst::ResourceError::Write,
                    ["Could not write recording: {}", e]
                )
            })?;
            state.bytes_in_file += bytes.len() as u64;
            Ok(())
        }

        /// Write data that may hold the container's absolute times, moved so
        /// the file starts at zero.
        fn write_timed(
            &self,
            state: &mut State,
            header: bool,
            bytes: &[u8],
        ) -> Result<(), gst::ErrorMessage> {
            match self.format() {
                // Only a fragment header holds decode times; media is never parsed.
                Format::FragmentedMp4 if header => {
                    let mut bytes = bytes.to_vec();
                    if state.file_base.is_none() {
                        state.file_base = mp4_boxes::fragment_start_ns(&bytes, &state.timescales);
                    }
                    if let Some(base) = state.file_base {
                        mp4_boxes::shift_decode_times(&mut bytes, &state.timescales, base);
                    }
                    self.write(state, &bytes)
                }
                Format::Matroska => {
                    let Some(ts) = mkv_cluster::cluster_timestamp(bytes) else {
                        return self.write(state, bytes);
                    };
                    let base = *state.file_base.get_or_insert(ts);
                    let mut bytes = bytes.to_vec();
                    mkv_cluster::shift_cluster_timestamp(&mut bytes, base);
                    self.write(state, &bytes)
                }
                Format::FragmentedMp4 | Format::MpegTs => self.write(state, bytes),
            }
        }

        fn flush_file(state: &mut State) {
            if let Some(file) = state.file.as_mut() {
                let _ = file.flush();
            }
            state.last_flush = Some(Instant::now());
        }

        /// Put held data at the end of the current file: the stream ended
        /// before another cut point came to start the next file with.
        fn flush_held(&self, state: &mut State, reports: &mut Vec<Report>) {
            if let Some((_, chunks)) = state.held.take() {
                if let Err(msg) = self.write_chunks(state, &chunks) {
                    gst::warning!(CAT, imp = self, "{:?}", msg);
                }
            }
            Self::flush_file(state);
            self.report_if_started(state, reports);
        }

        fn write_chunks(
            &self,
            state: &mut State,
            chunks: &[Chunk],
        ) -> Result<(), gst::ErrorMessage> {
            for chunk in chunks {
                self.write_timed(state, chunk.header, &chunk.bytes)?;
            }
            Ok(())
        }

        /// Whether the pipeline this sink is in is on its way below PAUSED.
        fn stopping(&self) -> bool {
            let mut top: gst::Element = self.obj().clone().upcast();
            while let Some(parent) = top.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
                top = parent;
            }
            matches!(top.pending_state(), gst::State::Ready | gst::State::Null)
        }

        /// Finish the file on a stop: without EOS the muxer keeps what it has
        /// not handed over, and the recording ends that much short.
        fn drain(&self) {
            if *self.eos.lock().unwrap() {
                return;
            }
            // No file yet does not mean no data: a connected track that never
            // carried anything keeps the muxer from writing its header, and the
            // others wait in their queues. EOS on that track ends the wait.
            let pads: Vec<gst::Pad> = self
                .drain_pads
                .lock()
                .unwrap()
                .iter()
                .filter_map(|p| p.upgrade())
                .collect();
            if pads.is_empty() {
                return;
            }
            for pad in &pads {
                pad.send_event(gst::event::Eos::new());
            }
            let eos = self.eos.lock().unwrap();
            let (eos, timeout) = self
                .eos_cond
                .wait_timeout_while(eos, DRAIN_TIMEOUT, |seen| !*seen)
                .unwrap();
            if timeout.timed_out() && !*eos {
                gst::warning!(
                    CAT,
                    imp = self,
                    "The muxer did not finish within {:?}; the file may end short",
                    DRAIN_TIMEOUT
                );
            }
        }

        /// Called without the state lock: the callback may look at the element.
        fn report(&self, reports: Vec<Report>) {
            if reports.is_empty() {
                return;
            }
            let callback = self.on_file_opened.lock().unwrap().clone();
            for (index, path, start) in reports {
                gst::info!(
                    CAT,
                    imp = self,
                    "Writing {} from running time {:?}",
                    path.display(),
                    start
                );
                if let Some(callback) = &callback {
                    callback(&self.obj(), index, &path, start);
                }
            }
        }

        fn handle(
            &self,
            buffer: &gst::Buffer,
            bytes: &[u8],
            reports: &mut Vec<Report>,
        ) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().unwrap();
            self.handle_locked(&mut state, buffer, bytes, reports)?;
            self.report_if_started(&mut state, reports);
            Ok(())
        }

        fn handle_locked(
            &self,
            state: &mut State,
            buffer: &gst::Buffer,
            bytes: &[u8],
            reports: &mut Vec<Report>,
        ) -> Result<(), gst::ErrorMessage> {
            let format = self.format();
            let flags = buffer.flags();
            let header = flags.contains(gst::BufferFlags::HEADER);

            if format == Format::FragmentedMp4
                && flags.contains(gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT)
            {
                // A new init segment means a new stream configuration, or the
                // first one. It starts a file.
                self.flush_held(state, reports);
                state.header = Some(bytes.to_vec());
                state.timescales = mp4_boxes::track_timescales(bytes);
                self.open_next_file(state, reports)?;
                return Ok(());
            }

            if format.is_cut_point(flags) {
                if let Some((pts, held)) = state.held.take() {
                    // The cut point the split was due at was not the last one:
                    // it starts the next file.
                    self.open_next_file(state, reports)?;
                    state.file_start = pts;
                    self.write_chunks(state, &held)?;
                }
                if state.file.is_none() {
                    self.open_next_file(state, reports)?;
                } else if self.split_requested.swap(false, Ordering::SeqCst) {
                    // Asked for by the operator: start the next file here, so the
                    // new file name shows at once rather than a fragment later.
                    self.open_next_file(state, reports)?;
                } else if self.split_due(state, buffer.pts()) {
                    Self::flush_file(state);
                    state.held = Some((
                        buffer.pts(),
                        vec![Chunk {
                            header,
                            bytes: bytes.to_vec(),
                        }],
                    ));
                    return Ok(());
                }
                if state.file_start.is_none() {
                    state.file_start = buffer.pts();
                }
                // Everything before this point is on disk if the process dies.
                Self::flush_file(state);
                self.write_timed(state, header, bytes)?;
                return Ok(());
            }

            if let Some((_, held)) = state.held.as_mut() {
                held.push(Chunk {
                    header,
                    bytes: bytes.to_vec(),
                });
                return Ok(());
            }
            if state.file.is_none() {
                // Matroska and MPEG-TS: what comes before the first cut point
                // cannot start a file, and its header is in the caps.
                return Ok(());
            }
            self.write_timed(state, header, bytes)?;
            if state
                .last_flush
                .is_none_or(|t| t.elapsed() >= FLUSH_INTERVAL)
            {
                Self::flush_file(state);
            }
            Ok(())
        }
    }

    impl FragmentFileSink {
        fn chain(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let map = buffer.map_readable().map_err(|_| {
                gst::element_imp_error!(self, gst::ResourceError::Read, ["Could not map buffer"]);
                gst::FlowError::Error
            })?;
            let mut reports = Vec::new();
            let result = self.handle(&buffer, map.as_slice(), &mut reports);
            self.report(reports);
            match result {
                Ok(()) => Ok(gst::FlowSuccess::Ok),
                Err(msg) => {
                    self.post_error_message(msg);
                    Err(gst::FlowError::Error)
                }
            }
        }

        fn sink_event(&self, event: gst::Event) -> bool {
            match event.view() {
                gst::EventView::Caps(caps) if self.format().header_in_caps() => {
                    let header = caps
                        .caps()
                        .structure(0)
                        .and_then(|s| s.get::<gst::ArrayRef>("streamheader").ok())
                        .map(|array| {
                            array
                                .iter()
                                .filter_map(|v| v.get::<gst::Buffer>().ok())
                                .filter_map(|b| {
                                    b.map_readable().ok().map(|m| m.as_slice().to_vec())
                                })
                                .flatten()
                                .collect::<Vec<u8>>()
                        });
                    if let Some(header) = header.filter(|h| !h.is_empty()) {
                        self.state.lock().unwrap().header = Some(header);
                    }
                }
                gst::EventView::Eos(_) => {
                    let mut reports = Vec::new();
                    self.flush_held(&mut self.state.lock().unwrap(), &mut reports);
                    self.report(reports);
                    *self.eos.lock().unwrap() = true;
                    self.eos_cond.notify_all();
                    // The bin counts a sink's EOS message to post its own.
                    let obj = self.obj();
                    let _ = obj.post_message(gst::message::Eos::builder().src(&*obj).build());
                }
                _ => {}
            }
            // A sink consumes every event that reaches it.
            true
        }
    }
}
