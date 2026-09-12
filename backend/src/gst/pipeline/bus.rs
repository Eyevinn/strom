use super::PipelineManager;
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::{PipelineState, StromEvent};
use tracing::{debug, error, info, trace, warn};

impl PipelineManager {
    /// Set up bus message handlers to monitor pipeline messages.
    pub(super) fn setup_bus_watch(&mut self) {
        // Clean up any existing message handlers first
        if !self.block_message_handlers.is_empty() {
            debug!(
                "Clearing {} existing message handlers for flow: {}",
                self.block_message_handlers.len(),
                self.flow_name
            );
            self.block_message_handlers.clear();
        }

        let Some(bus) = self.pipeline.bus() else {
            error!(
                "Pipeline '{}' does not have a bus - cannot set up message watch",
                self.flow_name
            );
            return;
        };

        // Set up block-specific message handlers using connect_message (allows multiple handlers)
        info!(
            "Connecting {} block message handler(s) for flow: {}",
            self.block_message_connect_fns.len(),
            self.flow_name
        );
        let flow_id = self.flow_id;
        let events_for_blocks = self.events.clone();

        // Take the connect functions (they're FnOnce, so we consume them)
        let connect_fns = std::mem::take(&mut self.block_message_connect_fns);
        for connect_fn in connect_fns {
            // Each block's connect_fn calls bus.connect_message(). It does not
            // take a signal watch of its own — the single one below covers
            // every handler on this bus.
            let handler_id = connect_fn(&bus, flow_id, events_for_blocks.clone());
            debug!("Successfully connected block message handler");
            self.block_message_handlers.push(handler_id);
        }

        // Call element signal setup functions (FnOnce closures that connect GLib signals)
        let element_setups = std::mem::take(&mut self.element_setup_fns);
        for setup_fn in element_setups {
            setup_fn(flow_id, events_for_blocks.clone());
            debug!("Successfully called element signal setup");
        }

        // Enable signal watch on the bus. This is what makes connect_message
        // fire at all, for every handler connected above and below.
        bus.add_signal_watch();
        self.bus_signal_watches += 1;

        // Set up main pipeline message handler using connect_message
        let flow_name = self.flow_name.clone();
        let events = self.events.clone();
        let cached_state = self.cached_state.clone();
        let qos_aggregator = self.qos_aggregator.clone();

        let main_handler_id = bus.connect_message(None, move |_bus, msg| {
            use gst::MessageView;

            // Log ALL bus messages to trace (very verbose)
            trace!("Bus message type: {:?}", msg.type_());

            match msg.view() {
                MessageView::Error(err) => {
                    // Drop errors from WHIP session elements (whipserversrc internals
                    // and appsrc/appsink bridge elements). These post errors when clients
                    // disconnect or during session setup that would otherwise transition
                    // the pipeline to ERROR state, preventing reconnection.
                    let is_whipsrc_internal = err.src().is_some_and(|s| {
                        let name = s.name();
                        // appsrc/appsink elements created for WHIP sessions have
                        // "whipserversrc" in their name
                        if name.as_str().contains("whipserversrc") {
                            return true;
                        }
                        let mut parent = s.parent();
                        while let Some(p) = parent {
                            if p.name().as_str().contains("whipserversrc") {
                                return true;
                            }
                            parent = p.parent();
                        }
                        false
                    });
                    if is_whipsrc_internal {
                        let source = err.src().map(|s| s.name().to_string());
                        warn!(
                            "WHIP client error in flow '{}' (ignored): source={:?}",
                            flow_name, source
                        );
                        return;
                    }

                    let error_msg = err.error().to_string();
                    let debug_info = err.debug();
                    let source = err.src().map(|s| s.name().to_string());

                    error!(
                        "Pipeline error in flow '{}': {} (debug: {:?}, source: {:?})",
                        flow_name, error_msg, debug_info, source
                    );

                    events.broadcast(StromEvent::PipelineError {
                        flow_id,
                        error: error_msg,
                        source,
                    });
                }
                MessageView::Warning(warn) => {
                    let warning_msg = warn.error().to_string();
                    let debug_info = warn.debug();
                    let source = warn.src().map(|s| s.name().to_string());

                    warn!(
                        "Pipeline warning in flow '{}': {} (debug: {:?}, source: {:?})",
                        flow_name, warning_msg, debug_info, source
                    );

                    events.broadcast(StromEvent::PipelineWarning {
                        flow_id,
                        warning: warning_msg,
                        source,
                    });
                }
                MessageView::Info(inf) => {
                    let info_msg = inf.error().to_string();
                    let source = inf.src().map(|s| s.name().to_string());

                    info!(
                        "Pipeline info in flow '{}': {} (source: {:?})",
                        flow_name, info_msg, source
                    );

                    events.broadcast(StromEvent::PipelineInfo {
                        flow_id,
                        message: info_msg,
                        source,
                    });
                }
                MessageView::Eos(_) => {
                    info!("Pipeline '{}' reached end of stream", flow_name);
                    events.broadcast(StromEvent::PipelineEos { flow_id });
                }
                MessageView::StateChanged(state_changed) => {
                    // Log state changes from all elements to debug pausing issues
                    if let Some(source) = msg.src() {
                        let source_name = source.name();
                        let old_state = state_changed.old();
                        let new_state = state_changed.current();
                        let pending_state = state_changed.pending();

                        if source.type_() == gst::Pipeline::static_type() {
                            info!(
                                "Pipeline '{}' state changed: {:?} -> {:?} (pending: {:?})",
                                flow_name,
                                old_state,
                                new_state,
                                pending_state
                            );

                            // Update cached pipeline state
                            let pipeline_state = match new_state {
                                gst::State::Null => PipelineState::Null,
                                gst::State::Ready => PipelineState::Ready,
                                gst::State::Paused => PipelineState::Paused,
                                gst::State::Playing => PipelineState::Playing,
                                _ => PipelineState::Null,
                            };
                            *cached_state.write().unwrap() = pipeline_state;

                            // Broadcast state change so the frontend can update immediately
                            events.broadcast(StromEvent::FlowStateChanged {
                                flow_id,
                                state: format!("{:?}", pipeline_state),
                            });
                        } else {
                            // Log element state changes at debug level to avoid log spam
                            debug!(
                                "Element '{}' in pipeline '{}' state changed: {:?} -> {:?} (pending: {:?})",
                                source_name,
                                flow_name,
                                old_state,
                                new_state,
                                pending_state
                            );
                        }
                    }
                }
                MessageView::Qos(qos) => {
                    // Quality of Service message - collect for aggregation and periodic broadcast
                    if let Some(source_name) = qos.src().map(|s| s.name().to_string()) {
                        let (jitter, proportion, _quality) = qos.values();
                        let (_format, processed) = qos.stats();

                        // Extract processed count as u64 from GenericFormattedValue
                        let processed_count = processed.value() as u64;

                        // Add to aggregator (will be logged and broadcast periodically)
                        // Note: jitter is already i64 from qos.values()
                        qos_aggregator.add_event(
                            source_name,
                            proportion,
                            jitter,
                            processed_count,
                        );
                    }
                }
                _ => {
                    // Ignore other message types
                }
            }
        });

        // Store main handler ID (we'll disconnect it when stopping)
        self.block_message_handlers.push(main_handler_id);
        debug!("Bus message handlers set up for flow: {}", self.flow_name);
    }

    /// Remove the bus message handlers.
    pub(super) fn remove_bus_watch(&mut self) {
        if !self.block_message_handlers.is_empty() {
            let handler_count = self.block_message_handlers.len();
            debug!(
                "Disconnecting {} message handler(s) for flow: {}",
                handler_count, self.flow_name
            );
            // remove_signal_watch is ref-counted — each add_signal_watch call
            // needs exactly one matching remove, and a surplus one is a
            // GStreamer CRITICAL. Take the count from the adds; the handler
            // count is a different number, since a block may register a
            // message handler without taking a watch of its own.
            let watch_count = std::mem::take(&mut self.bus_signal_watches);

            // Disconnect signal handlers from the bus
            if let Some(bus) = self.pipeline.bus() {
                for handler_id in self.block_message_handlers.drain(..) {
                    bus.disconnect(handler_id);
                }
                for _ in 0..watch_count {
                    bus.remove_signal_watch();
                }
            } else {
                // Bus already gone, just clear the handlers
                self.block_message_handlers.clear();
            }
        }
    }

    /// Start the periodic QoS stats broadcast task.
    pub(super) fn start_qos_broadcast_task(&mut self) {
        // Cancel any existing task first
        self.stop_qos_broadcast_task();

        let aggregator = self.qos_aggregator.clone();
        let events = self.events.clone();
        let flow_id = self.flow_id;
        let flow_name = self.flow_name.clone();

        // Spawn task that wakes up every 1 second to broadcast aggregated QoS stats
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
            loop {
                interval.tick().await;

                // Extract and reset aggregated stats
                let stats = aggregator.extract_and_reset();

                if !stats.is_empty() {
                    debug!(
                        "Broadcasting QoS stats for {} element(s) in flow '{}'",
                        stats.len(),
                        flow_name
                    );
                }

                // Broadcast stats for each element
                for (element_name, element_stats) in stats {
                    let avg_proportion = element_stats.avg_proportion();
                    let is_falling_behind = avg_proportion < 1.0;

                    // Parse element name to determine if it's part of a block or standalone
                    // Format: "block_id:element_type" for block elements, "element_id" for standalone
                    let (block_id, element_id, internal_element_type) =
                        if element_name.contains(':') {
                            // Element inside a block
                            let parts: Vec<&str> = element_name.split(':').collect();
                            let block_id = parts[0].to_string();
                            let elem_type = parts.get(1).map(|s| s.to_string());
                            (Some(block_id.clone()), block_id, elem_type)
                        } else {
                            // Standalone element
                            (None, element_name.clone(), None)
                        };

                    // Log aggregated stats
                    if is_falling_behind {
                        let drop_percentage = (1.0 - avg_proportion) * 100.0;
                        warn!(
                            "QoS: '{}' in flow '{}' falling behind {:.1}% ({} events, avg proportion {:.3}, jitter {} ns)",
                            element_name,
                            flow_name,
                            drop_percentage,
                            element_stats.event_count,
                            avg_proportion,
                            element_stats.avg_jitter()
                        );
                    } else {
                        debug!(
                            "QoS: '{}' in flow '{}' OK ({} events, avg proportion {:.3})",
                            element_name, flow_name, element_stats.event_count, avg_proportion
                        );
                    }

                    // Broadcast QoS event to frontend
                    events.broadcast(StromEvent::QoSStats {
                        flow_id,
                        block_id,
                        element_id,
                        element_name: element_name.clone(),
                        internal_element_type,
                        event_count: element_stats.event_count,
                        avg_proportion,
                        min_proportion: element_stats.min_proportion,
                        max_proportion: element_stats.max_proportion,
                        avg_jitter: element_stats.avg_jitter(),
                        total_processed: element_stats.total_processed,
                        is_falling_behind,
                    });
                }
            }
        });

        self.qos_broadcast_task = Some(task);
    }

    /// Stop the periodic QoS stats broadcast task.
    pub(super) fn stop_qos_broadcast_task(&mut self) {
        if let Some(task) = self.qos_broadcast_task.take() {
            task.abort();
        }
    }
}
