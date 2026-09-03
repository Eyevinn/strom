use super::{PipelineError, PipelineManager};
use crate::gst::{rtp_hdrext, thread_priority};
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::PipelineState;
use tracing::{error, info};

impl PipelineManager {
    /// Start the pipeline (set to PLAYING state).
    pub fn start(&mut self) -> Result<PipelineState, PipelineError> {
        info!("Starting pipeline: {}", self.flow_name);
        info!("Pipeline has {} elements", self.elements.len());

        // Disable RTP header extension aggregation before any state changes, so
        // the handler is in place before decodebin can autoplug a depayloader.
        // Works around gstreamer#5057, which aborts the whole process.
        rtp_hdrext::install(&self.pipeline);

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
        let (result, current_state, pending_state) =
            self.pipeline.state(gst::ClockTime::from_mseconds(500));
        info!(
            "Pipeline '{}' state after start: result={:?}, current={:?}, pending={:?}",
            self.flow_name, result, current_state, pending_state
        );

        // `gst_element_get_state()` reports a still-running transition as
        // `Ok(Async)`. `Err` is only ever GST_STATE_CHANGE_FAILURE, whatever
        // the pending state says: a failure with `pending == Playing` is a
        // pipeline that aborted the PAUSED -> PLAYING transition (an element
        // posted a fatal error on the bus) and will never leave PAUSED on its
        // own. Reporting that as "async in progress" starts a dead flow —
        // every element that only serves traffic in PLAYING stays down, and
        // whepserversink's signaller never opens its HTTP port, so all WHEP
        // offers for the flow's whole lifetime fail with 502.
        if let Err(e) = result {
            error!(
                "Pipeline '{}' failed to reach PLAYING state: {:?} (current: {:?}, pending: {:?})",
                self.flow_name, e, current_state, pending_state
            );
            return Err(PipelineError::StateChange(format!(
                "State change failed: {:?} - current: {:?}, pending: {:?}",
                e, current_state, pending_state
            )));
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
