//! A file sink for fragmented MP4 that starts a new file at a fragment boundary.
//!
//! `isofmp4mux` emits its init segment (`ftyp` + `moov`) once, flagged
//! `DISCONT | HEADER`, and then one `moof` + `mdat` per fragment. Each fragment
//! header is flagged `HEADER` without `DELTA_UNIT`; everything else is
//! `DELTA_UNIT`. A fragment carries the index of its own samples, so a file that
//! starts with the init segment and continues at any fragment header is a
//! complete, playable fragmented MP4. That is all a split needs: close the file,
//! open the next, write the init segment again, carry on.
//!
//! The sink is not async and does not sync to the clock. It never prerolls, so a
//! recording whose first data arrives late does not take the pipeline out of
//! PLAYING, and it writes each buffer as it arrives, so a crash loses at most
//! the fragment the muxer has not handed over yet.

use super::mp4_boxes;
use gst::glib;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How long a stop waits for the muxer to hand over what it holds.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// Called with the index and path of every file the sink opens.
pub type FileOpenedFn = Arc<dyn Fn(u32, &std::path::Path) + Send + Sync>;

/// When the sink starts a new file. Both limits apply; whichever is reached
/// first splits, at the next fragment.
#[derive(Clone, Debug, Default)]
pub struct SplitPolicy {
    pub max_duration: Option<gst::ClockTime>,
    pub max_bytes: Option<u64>,
}

glib::wrapper! {
    pub struct FragmentFileSink(ObjectSubclass<imp::FragmentFileSink>)
        @extends gst_base::BaseSink, gst::Element, gst::Object;
}

impl FragmentFileSink {
    /// `location` holds one `%05d`, replaced by the file index.
    pub fn new(name: &str, location: &str, policy: SplitPolicy) -> Self {
        let sink: Self = glib::Object::builder().property("name", name).build();
        let mut settings = sink.imp().settings.lock().unwrap();
        settings.location = location.to_string();
        settings.policy = policy;
        drop(settings);
        sink
    }

    /// Start a new file at the next fragment.
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
            Some("Strom fragmented MP4 file sink"),
        )
    });

    #[derive(Default)]
    pub struct Settings {
        pub location: String,
        pub policy: SplitPolicy,
    }

    #[derive(Default)]
    struct State {
        file: Option<File>,
        /// Index the next file opened gets.
        next_index: u32,
        /// The init segment, written at the start of every file.
        init_segment: Option<Vec<u8>>,
        /// PTS of the first fragment in the current file.
        file_start: Option<gst::ClockTime>,
        bytes_in_file: u64,
        /// The fragment a split is due at, held back until the next fragment
        /// starts. If the stream ends first, it is the muxer's tail and goes at
        /// the end of the current file rather than alone into a new one.
        held: Option<(Option<gst::ClockTime>, Vec<u8>)>,
        /// Track id → timescale, from the init segment.
        timescales: HashMap<u32, u32>,
        /// Decode time the current file starts at, taken from its first
        /// fragment and subtracted from every fragment in it.
        file_base_ns: Option<u64>,
    }

    #[derive(Default)]
    pub struct FragmentFileSink {
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
        type ParentType = gst_base::BaseSink;
    }

    impl ObjectImpl for FragmentFileSink {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_sync(false);
            obj.set_async(false);
        }
    }

    impl GstObjectImpl for FragmentFileSink {}

    impl ElementImpl for FragmentFileSink {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                vec![gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &gst::Caps::builder("video/quicktime")
                        .field("variant", "iso-fragmented")
                        .build(),
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
            // muxer's last fragment waiting for PLAYING. Only on the way down
            // to READY or NULL; a pause must not end the file.
            if transition == gst::StateChange::PlayingToPaused && self.stopping() {
                self.drain();
            }
            self.parent_change_state(transition)
        }
    }

    impl FragmentFileSink {
        fn path_for(&self, index: u32) -> PathBuf {
            let settings = self.settings.lock().unwrap();
            PathBuf::from(settings.location.replace("%05d", &format!("{:05}", index)))
        }

        /// Close the current file, if any, and open the next one with the init
        /// segment at its start.
        fn open_next_file(&self, state: &mut State) -> Result<(u32, PathBuf), gst::ErrorMessage> {
            if let Some(mut file) = state.file.take() {
                let _ = file.flush();
            }
            let index = state.next_index;
            let path = self.path_for(index);
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let mut file = File::create(&path).map_err(|e| {
                gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Could not create {}: {}", path.display(), e]
                )
            })?;
            let mut written = 0u64;
            if let Some(init) = &state.init_segment {
                file.write_all(init).map_err(|e| {
                    gst::error_msg!(
                        gst::ResourceError::Write,
                        ["Could not write to {}: {}", path.display(), e]
                    )
                })?;
                written = init.len() as u64;
            }
            state.file = Some(file);
            state.next_index += 1;
            state.file_start = None;
            state.file_base_ns = None;
            state.bytes_in_file = written;
            Ok((index, path))
        }

        fn split_due(&self, state: &State, fragment_pts: Option<gst::ClockTime>) -> bool {
            let policy = self.settings.lock().unwrap().policy.clone();
            let by_time = match (policy.max_duration, state.file_start, fragment_pts) {
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

        /// Write a fragment, from its header on, with its decode times moved so
        /// the file starts at zero.
        fn write_fragment(&self, state: &mut State, bytes: &[u8]) -> Result<(), gst::ErrorMessage> {
            let mut bytes = bytes.to_vec();
            if state.file_base_ns.is_none() {
                state.file_base_ns = mp4_boxes::fragment_start_ns(&bytes, &state.timescales);
            }
            if let Some(base) = state.file_base_ns {
                mp4_boxes::shift_decode_times(&mut bytes, &state.timescales, base);
            }
            self.write(state, &bytes)
        }

        /// Put a held fragment at the end of the current file: the stream ended
        /// before another fragment came to start the next file with.
        fn flush_held(&self, state: &mut State) {
            if let Some((_, bytes)) = state.held.take() {
                if let Err(msg) = self.write_fragment(state, &bytes) {
                    gst::warning!(CAT, imp = self, "{:?}", msg);
                }
            }
            if let Some(file) = state.file.as_mut() {
                let _ = file.flush();
            }
        }

        /// Whether the pipeline this sink is in is on its way below PAUSED.
        fn stopping(&self) -> bool {
            let mut top: gst::Element = self.obj().clone().upcast();
            while let Some(parent) = top.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
                top = parent;
            }
            matches!(top.pending_state(), gst::State::Ready | gst::State::Null)
        }

        /// Finish the file on a stop: without EOS the muxer keeps its last
        /// fragment, and the recording ends up to a fragment short.
        fn drain(&self) {
            if self.state.lock().unwrap().file.is_none() || *self.eos.lock().unwrap() {
                return;
            }
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
                    "The muxer did not finish within {:?}; the file may end up to a fragment short",
                    DRAIN_TIMEOUT
                );
            }
        }

        fn report_opened(&self, opened: Option<(u32, PathBuf)>) {
            let Some((index, path)) = opened else {
                return;
            };
            gst::info!(CAT, imp = self, "Writing {}", path.display());
            let callback = self.on_file_opened.lock().unwrap().clone();
            if let Some(callback) = callback {
                callback(index, &path);
            }
        }
    }

    impl BaseSinkImpl for FragmentFileSink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            *self.state.lock().unwrap() = State::default();
            *self.eos.lock().unwrap() = false;
            self.split_requested.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().unwrap();
            self.flush_held(&mut state);
            state.file = None;
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let flags = buffer.flags();
            let is_header = flags.contains(gst::BufferFlags::HEADER);
            let init_segment = is_header && flags.contains(gst::BufferFlags::DISCONT);
            let fragment_start =
                is_header && !init_segment && !flags.contains(gst::BufferFlags::DELTA_UNIT);

            let map = buffer.map_readable().map_err(|_| {
                gst::element_imp_error!(self, gst::ResourceError::Read, ["Could not map buffer"]);
                gst::FlowError::Error
            })?;
            let bytes = map.as_slice();

            let mut opened = None;
            let result = (|| -> Result<(), gst::ErrorMessage> {
                let mut state = self.state.lock().unwrap();

                if init_segment {
                    // A new init segment means a new stream configuration, or the
                    // first one. It starts a file.
                    self.flush_held(&mut state);
                    state.init_segment = Some(bytes.to_vec());
                    state.timescales = mp4_boxes::track_timescales(bytes);
                    opened = Some(self.open_next_file(&mut state)?);
                    return Ok(());
                }

                if fragment_start {
                    if let Some((pts, held)) = state.held.take() {
                        // The fragment the split was due at was not the last one:
                        // it starts the next file.
                        opened = Some(self.open_next_file(&mut state)?);
                        state.file_start = pts;
                        self.write_fragment(&mut state, &held)?;
                    }
                    if state.file.is_none() {
                        opened = Some(self.open_next_file(&mut state)?);
                    } else if self.split_requested.swap(false, Ordering::SeqCst)
                        || self.split_due(&state, buffer.pts())
                    {
                        state.held = Some((buffer.pts(), bytes.to_vec()));
                        return Ok(());
                    }
                    if state.file_start.is_none() {
                        state.file_start = buffer.pts();
                    }
                }

                if let Some((_, held)) = state.held.as_mut() {
                    held.extend_from_slice(bytes);
                    return Ok(());
                }
                if state.file.is_none() {
                    opened = Some(self.open_next_file(&mut state)?);
                }
                if fragment_start {
                    self.write_fragment(&mut state, bytes)
                } else {
                    self.write(&mut state, bytes)
                }
            })();

            self.report_opened(opened);
            match result {
                Ok(()) => Ok(gst::FlowSuccess::Ok),
                Err(msg) => {
                    self.post_error_message(msg);
                    Err(gst::FlowError::Error)
                }
            }
        }

        fn event(&self, event: gst::Event) -> bool {
            if let gst::EventView::Eos(_) = event.view() {
                self.flush_held(&mut self.state.lock().unwrap());
                *self.eos.lock().unwrap() = true;
                self.eos_cond.notify_all();
            }
            self.parent_event(event)
        }
    }
}
