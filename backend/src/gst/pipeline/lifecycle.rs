use super::{PipelineError, PipelineManager};
use crate::gst::thread_priority;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::time::{Duration, Instant};
use strom_types::PipelineState;
use tracing::{error, info, warn};

/// How long to keep re-driving a failed transition to PLAYING before giving up.
/// Measured on a live feed, the input relinked its pads 1.4-2.1 s after the
/// transient error, so the window has to outlast that. A genuinely broken
/// pipeline fails every attempt inside the window and then reports the failure.
const STATE_CHANGE_RETRY_WINDOW: Duration = Duration::from_secs(3);

/// How long to wait between re-drives. The only way to learn that the input has
/// settled is to try, so poll rather than sleep out a guess: at 200 ms the
/// recovery is picked up within 200 ms of it happening instead of at the next
/// multiple of a long settle time.
const STATE_CHANGE_RETRY_INTERVAL: Duration = Duration::from_millis(200);

/// How long to wait for the re-driven transition to report a verdict.
const STATE_CHANGE_RETRY_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(2);

impl PipelineManager {
    /// Start the pipeline (set to PLAYING state).
    pub fn start(&mut self) -> Result<PipelineState, PipelineError> {
        info!("Starting pipeline: {}", self.flow_name);
        info!("Pipeline has {} elements", self.elements.len());

        // Set up thread priority handler FIRST (before any state changes)
        // This must be done before the pipeline starts so we catch all thread enter events
        info!(
            "Setting up thread priority handler (requested: {:?}, registry: {})...",
            self.properties.thread_priority,
            self.thread_registry.is_some()
        );
        let priority_state = thread_priority::setup_thread_priority_handler(
            &self.pipeline,
            self.properties.thread_priority,
            self.assigned_cpus.clone(),
            self.flow_id,
            self.thread_registry.clone(),
        );
        self.thread_priority_state = Some(priority_state);
        info!("Thread priority handler installed");

        // Populate session thread config so consumer-added callbacks can install
        // sync handlers on dynamically created session pipelines (WHEP/WebRTC)
        self.session_thread_config.populate(
            self.properties.thread_priority,
            self.assigned_cpus.clone(),
            self.flow_id,
            self.thread_registry.clone(),
        );

        // Set up bus watch before starting
        info!("Setting up bus watch...");
        self.setup_bus_watch();
        info!("Bus watch set up");

        // Start QoS aggregation and periodic broadcast task
        info!("Starting QoS stats aggregation task...");
        self.start_qos_broadcast_task();
        info!("QoS stats task started");

        // Configure clock before starting
        info!(
            "Configuring clock (type: {:?})...",
            self.properties.clock_type
        );
        self.configure_clock()?;
        info!("Clock configured");

        // Set to READY state first to ensure aggregator request pads are fully initialized
        info!("Setting pipeline '{}' to READY state...", self.flow_name);
        self.pipeline
            .set_state(gst::State::Ready)
            .map_err(|e| PipelineError::StateChange(format!("Failed to reach READY: {}", e)))?;
        info!("Pipeline in READY state");

        // Now apply pad properties (aggregator request pads are now accessible)
        info!("Applying pad properties after READY state...");
        self.apply_pad_properties();
        info!("Pad properties applied");

        info!(
            "Setting pipeline '{}' to PLAYING state (this may block)...",
            self.flow_name
        );
        let state_change_result = self.pipeline.set_state(gst::State::Playing);
        info!("set_state(Playing) call returned");

        match &state_change_result {
            Ok(gst::StateChangeSuccess::Success) => {
                info!("Pipeline '{}' set to PLAYING: Success", self.flow_name);
            }
            Ok(gst::StateChangeSuccess::Async) => {
                info!(
                    "Pipeline '{}' set to PLAYING: Async (state change in progress)",
                    self.flow_name
                );
            }
            Ok(gst::StateChangeSuccess::NoPreroll) => {
                info!(
                    "Pipeline '{}' set to PLAYING: NoPreroll (live source)",
                    self.flow_name
                );
            }
            Err(e) => {
                error!("Pipeline '{}' failed to start: {}", self.flow_name, e);
            }
        }

        let state_change_success = state_change_result
            .map_err(|e| PipelineError::StateChange(format!("Failed to start: {}", e)))?;

        if state_change_success == gst::StateChangeSuccess::NoPreroll {
            info!("Pipeline '{}' is live (NoPreroll)", self.flow_name);
        }

        // Query the actual state GStreamer has reached so far.
        // For Async pipelines this returns the current state (e.g. Paused)
        // without blocking. For NoPreroll/Success it returns the final state.
        //
        // 500 ms is a sample, not a verdict, and deliberately so: a transition
        // still in flight comes back as Ok(Async) and is reported as started.
        // Waiting for it to settle instead would break every input that legally
        // sits in PAUSED until its first packet arrives — an srtsrc or rtspsrc
        // with no sender yet must start successfully and pick the feed up later.
        // The cost is that a fatal error arriving after the sample is not seen
        // here; the bus handler still reports it, the flow just gets there by
        // way of a PipelineError event rather than a failed start().
        let (mut result, mut current_state, mut pending_state) =
            self.pipeline.state(gst::ClockTime::from_mseconds(500));
        info!(
            "Pipeline '{}' state after start: result={:?}, current={:?}, pending={:?}",
            self.flow_name, result, current_state, pending_state
        );

        // `gst_element_get_state()` reports a still-running transition as
        // `Ok(Async)`. `Err` is only ever GST_STATE_CHANGE_FAILURE, whatever
        // the pending state says: a failure with `pending == Playing` is a
        // pipeline that aborted its transition and will never leave its
        // current state on its own.
        //
        // A live input can poison that transition transiently. An MPEG-TS feed
        // that re-signals its PMT makes tsdemux drain the previous program: it
        // pushes EOS out of the pad it is removing, and the parser decodebin
        // autoplugged for that short-lived pad has no complete access unit yet,
        // so GstBaseParse errors fatally. The element aborts its own state,
        // which fails the whole pipeline's transition. Measured on a live feed
        // the two programs were 38 ms apart — under one frame at 25 fps.
        //
        // The input recovers on its own a second or two later: decodebin drops
        // the doomed chain and exposes the new program's pads. Only the
        // pipeline's state stays poisoned. Re-driving the transition once the
        // input has settled picks it up — verified: a failed state change
        // re-driven after the failing condition clears returns Success, and
        // does not need a detour via READY or a drained bus.
        //
        // This deliberately touches nothing in the data path. An earlier
        // attempt dropped the EOS at the parser instead; that removed the error
        // but stopped decodebin from ever exposing the replacement pads, and
        // turned a loud failure into a silent hang.
        let retry_deadline = Instant::now() + STATE_CHANGE_RETRY_WINDOW;
        let mut attempts = 0u32;
        while result.is_err() {
            let now = Instant::now();
            if now >= retry_deadline {
                break;
            }
            attempts += 1;
            warn!(
                "Pipeline '{}' failed its transition (current: {:?}, pending: {:?}) — \
                 re-driving, attempt {} ({:?} of the {:?} window left). A transient \
                 input error during startup (e.g. a demuxer program change) can do this.",
                self.flow_name,
                current_state,
                pending_state,
                attempts,
                retry_deadline - now,
                STATE_CHANGE_RETRY_WINDOW
            );

            std::thread::sleep(STATE_CHANGE_RETRY_INTERVAL);

            let _ = self.pipeline.set_state(gst::State::Playing);
            (result, current_state, pending_state) =
                self.pipeline.state(STATE_CHANGE_RETRY_TIMEOUT);

            if result.is_ok() {
                info!(
                    "Pipeline '{}' reached {:?} on attempt {} (pending: {:?})",
                    self.flow_name, current_state, attempts, pending_state
                );
            }
        }

        // Still failing after the retries: the flow is genuinely dead. Say so
        // rather than reporting it started — callers go on to register WHEP
        // endpoints, and whepserversink's signaller never opens its HTTP port
        // on a pipeline whose transition aborted, so every WHEP offer for the
        // flow's whole lifetime would answer 502 against a "running" flow.
        if let Err(e) = result {
            error!(
                "Pipeline '{}' failed to reach PLAYING state after {} re-drive attempt(s) \
                 over {:?}: {:?} (current: {:?}, pending: {:?})",
                self.flow_name,
                attempts,
                STATE_CHANGE_RETRY_WINDOW,
                e,
                current_state,
                pending_state
            );
            return Err(PipelineError::StateChange(format!(
                "State change failed: {:?} - current: {:?}, pending: {:?}",
                e, current_state, pending_state
            )));
        }

        // Reaching PLAYING is not the same as having a data path. The state
        // says nothing about which of the two things happened upstream:
        //
        //   - the input recovered (decodebin dropped the doomed chain and
        //     exposed the replacement pads) — what the re-drive is for; or
        //   - the input died. A basesrc that takes a flow error pauses its task
        //     *and pushes EOS*. That EOS completes the sink's preroll, so the
        //     re-driven transition reports Success on a pipeline that will never
        //     carry another buffer.
        //
        // Measured on `audiotestsrc ! identity error-after=1 ! fakesink`: the
        // re-drive returns Success with the pipeline in PLAYING and zero buffers
        // ever reaching the sink. Reporting that as started is the silent
        // failure this whole path exists to avoid, so check the outputs before
        // believing the state. Only on the retry path — a pipeline that started
        // first time is not suspect, and this must not slow the normal start.
        if attempts > 0 {
            let (eos_sinks, total_sinks) = self.eos_sink_count();
            if total_sinks > 0 && eos_sinks == total_sinks {
                error!(
                    "Pipeline '{}' reached {:?} after {} re-drive attempt(s), but all {} \
                     output(s) are EOS — the input died rather than recovered, so the flow \
                     would produce nothing for its whole lifetime",
                    self.flow_name, current_state, attempts, total_sinks
                );
                return Err(PipelineError::StateChange(format!(
                    "Pipeline reached {:?} with no live data path: all {} output(s) are EOS",
                    current_state, total_sinks
                )));
            }
            info!(
                "Pipeline '{}' has a live data path after recovery ({}/{} output(s) EOS)",
                self.flow_name, eos_sinks, total_sinks
            );
        }

        let actual_state = match current_state {
            gst::State::Null => PipelineState::Null,
            gst::State::Ready => PipelineState::Ready,
            gst::State::Paused => PipelineState::Paused,
            gst::State::Playing => PipelineState::Playing,
            _ => PipelineState::Null,
        };
        *self.cached_state.write().unwrap() = actual_state;

        // Attach automatic buffer age monitoring probes and start the
        // periodic broadcast task that reads probe slots off the hot path.
        self.attach_automatic_probes();
        self.probe_manager.start_broadcast_task();

        // Start periodic thumbnail deactivation task
        self.start_thumbnail_deactivation_task();

        Ok(actual_state)
    }

    /// Count the pipeline's sink elements, and how many of them have seen EOS
    /// on every one of their sink pads.
    ///
    /// A sink whose pads are all EOS will never receive another buffer. When
    /// *every* sink is in that state the pipeline has no live output at all —
    /// which is how a dead input looks after the transition was re-driven. One
    /// EOS sink among several is not that: it is the doomed chain being drained
    /// while the rest of the graph runs, exactly what the re-drive recovers
    /// from. `iterate_sinks()` covers bins too, since a bin holding a sink
    /// carries the sink flag itself.
    fn eos_sink_count(&self) -> (usize, usize) {
        let mut eos = 0usize;
        let mut total = 0usize;
        let mut sinks = self.pipeline.iterate_sinks();
        while let Ok(Some(element)) = sinks.next() {
            let pads: Vec<gst::Pad> = element
                .pads()
                .into_iter()
                .filter(|pad| pad.direction() == gst::PadDirection::Sink)
                .collect();
            if pads.is_empty() {
                continue;
            }
            total += 1;
            if pads
                .iter()
                .all(|pad| pad.pad_flags().contains(gst::PadFlags::EOS))
            {
                warn!(
                    "Pipeline '{}': output '{}' is EOS",
                    self.flow_name,
                    element.name()
                );
                eos += 1;
            }
        }
        (eos, total)
    }

    /// Start periodic task that deactivates idle thumbnail branches.
    fn start_thumbnail_deactivation_task(&mut self) {
        if self.thumbnail_deactivation_task.is_some() {
            return;
        }

        let taps = self.thumbnail_taps.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let store = taps.lock().unwrap();
                for block_taps in store.values() {
                    for tap in block_taps {
                        tap.maybe_deactivate();
                    }
                }
            }
        });

        self.thumbnail_deactivation_task = Some(task);
    }

    /// Attach automatic buffer age monitoring probes to key measurement points.
    fn attach_automatic_probes(&self) {
        let pipeline = self.pipeline.clone();
        self.probe_manager.attach_automatic(
            &pipeline,
            &self.elements,
            &self.blocks,
            &self.block_definitions,
        );
    }

    /// Stop the pipeline (set to NULL state).
    pub fn stop(&mut self) -> Result<PipelineState, PipelineError> {
        info!("Stopping pipeline: {}", self.flow_name);

        // Stop buffer age broadcast task and deactivate probes BEFORE
        // set_state(Null) — same order as Drop. This removes probe closures
        // (and their weak pipeline refs) before GStreamer tries to deactivate
        // pads, avoiding contention during state transition.
        self.probe_manager.stop_broadcast_task();
        self.probe_manager.deactivate_all();

        // Remove bus watch when stopped to free resources
        self.remove_bus_watch();

        // Stop QoS broadcast task
        self.stop_qos_broadcast_task();

        // Stop thumbnail deactivation task
        if let Some(task) = self.thumbnail_deactivation_task.take() {
            task.abort();
        }

        // Drop cached volume control sources. The bindings themselves are
        // owned by the elements and released when the pipeline goes to NULL.
        self.volume_ramps.clear();

        // Run set_state on a dedicated OS thread to avoid "Cannot start a runtime
        // from within a runtime" panics. Some GStreamer elements (e.g. whipserversrc)
        // internally call block_on() during state transitions, which is incompatible
        // with being called from within a tokio runtime context.
        let pipeline = self.pipeline.clone();
        let result = std::thread::spawn(move || pipeline.set_state(gst::State::Null))
            .join()
            .map_err(|_| PipelineError::StateChange("set_state thread panicked".to_string()))?
            .map_err(|e| PipelineError::StateChange(format!("Failed to stop: {}", e)))?;
        let _ = result;

        // Remove thread priority handler
        thread_priority::remove_thread_priority_handler(&self.pipeline);
        self.thread_priority_state = None;

        // Unregister all threads belonging to this flow from the registry
        if let Some(ref registry) = self.thread_registry {
            registry.unregister_flow(&self.flow_id);
        }

        // Update cached state
        *self.cached_state.write().unwrap() = PipelineState::Null;

        Ok(PipelineState::Null)
    }

    /// Pause the pipeline.
    pub fn pause(&self) -> Result<PipelineState, PipelineError> {
        info!("Pausing pipeline: {}", self.flow_name);

        self.pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| PipelineError::StateChange(format!("Failed to pause: {}", e)))?;

        // Update cached state
        *self.cached_state.write().unwrap() = PipelineState::Paused;

        Ok(PipelineState::Paused)
    }
}
