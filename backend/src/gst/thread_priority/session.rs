//! [`SessionThreadConfig`]: a flow's thread settings, for the pipelines its
//! blocks run of their own.

use super::{ThreadPriorityState, ThreadSetup};
use crate::thread_handle::ThreadHandle;
use crate::thread_registry::ThreadRegistry;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashSet;
use std::sync::Arc;
use strom_types::flow::ThreadPriority;
use strom_types::FlowId;
use tracing::{debug, info, warn};

/// A flow's thread priority, CPU affinity and thread registry, for the
/// pipelines its blocks run of their own.
///
/// The flow's sync handler sees only the flow's own bus. A block that runs
/// part of its work in a pipeline of its own (an isolation pipeline behind an
/// appsink/appsrc pair, a WebRTC session) hands this config to every such
/// pipeline it creates, so the threads there run like the flow's own:
///
/// * [`SessionThreadConfig::install_on_internal_pipeline`] where the block
///   owns the pipeline and its bus (Media Player, WHIP Input).
/// * [`SessionThreadConfig::install_on_webrtcsink`] where webrtcsink owns it
///   (WHEP Output, WHIP Output).
///
/// Created empty at block-build time (`BlockBuildContext::session_thread_config`)
/// and populated in `PipelineManager::start()`, after the blocks are built.
/// Both install paths read it when a thread enters, not when they install, so
/// a pipeline built with the block, or a session created before a later
/// `populate`, uses the flow's current settings.
#[derive(Clone, Default)]
pub struct SessionThreadConfig(Arc<std::sync::Mutex<ConfigSlot>>);

#[derive(Default)]
struct ConfigSlot {
    /// The flow's settings, once the flow has started.
    setup: Option<Arc<ThreadSetup>>,
    /// Threads of an internal pipeline that entered their streaming loop
    /// before the flow populated the settings: a Media Player control call
    /// can preroll its pipeline in that window. Adopted by `populate`.
    early: Vec<EarlyThread>,
}

/// A thread held for [`SessionThreadConfig::populate`], captured on itself.
pub(super) struct EarlyThread {
    handle: ThreadHandle,
    owner: String,
    block_id: Arc<str>,
    pipeline: Arc<str>,
}

impl SessionThreadConfig {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, ConfigSlot> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The flow's settings, if the flow has populated them.
    fn setup(&self) -> Option<Arc<ThreadSetup>> {
        self.slot().setup.clone()
    }

    /// Populate the config with the flow's settings. `state` is the flow's
    /// own, from [`setup_thread_priority_handler`], so the threads configured
    /// here count toward the flow's status. A later call replaces the
    /// settings for every thread that enters after it.
    ///
    /// Threads of an internal pipeline that entered before this call are
    /// registered here. Their priority and CPUs can only be set on the thread
    /// itself, so they keep their own, with a warning.
    pub fn populate(
        &self,
        state: &ThreadPriorityState,
        assigned_cpus: Option<Vec<usize>>,
        flow_id: FlowId,
        thread_registry: Option<ThreadRegistry>,
    ) {
        let setup = Arc::new(ThreadSetup {
            priority: state.requested,
            assigned_cpus,
            flow_id,
            thread_registry,
            state: state.clone(),
        });
        let mut slot = self.slot();
        if slot.setup.is_some() {
            info!(
                "Thread settings of flow {} replaced; threads that enter from now on use the new ones",
                flow_id
            );
        }
        slot.setup = Some(Arc::clone(&setup));
        // Under the lock: a thread that leaves meanwhile is no longer early,
        // and must not be registered after it has gone back to the pool.
        let early = std::mem::take(&mut slot.early);
        if early.is_empty() || !setup.is_active() {
            return;
        }
        if !matches!(setup.priority, ThreadPriority::Normal) || setup.assigned_cpus.is_some() {
            warn!(
                "{} streaming thread(s) of an internal pipeline started before flow {} did and keep \
                 their own priority and CPUs (first: element '{}' in pipeline '{}')",
                early.len(),
                flow_id,
                early[0].owner,
                early[0].pipeline
            );
        }
        for thread in early {
            setup.adopt(thread);
        }
    }

    /// Returns true if thread priority or registration is configured.
    pub fn is_active(&self) -> bool {
        self.setup().is_some_and(|setup| setup.is_active())
    }

    /// Give the streaming threads of `pipeline`, a pipeline block `block_id`
    /// runs of its own and whose bus it owns, the flow's settings.
    ///
    /// Subscribes to the bus's `sync-message::stream-status` signal rather
    /// than taking its sync handler: a bus has only one sync handler, and a
    /// block may need it for something else (`context_forward`). GStreamer
    /// emits `sync-message` beside the sync handler, on the posting thread,
    /// unless that handler drops the message.
    ///
    /// Call it once per pipeline, before the pipeline starts.
    pub fn install_on_internal_pipeline(&self, pipeline: &gst::Pipeline, block_id: &str) {
        let Some(bus) = pipeline.bus() else {
            warn!(
                "Internal pipeline '{}' of block {} has no bus - its threads keep normal priority",
                pipeline.name(),
                block_id
            );
            return;
        };
        bus.enable_sync_message_emission();

        // The closure holds the config and two strings, nothing from
        // GStreamer: the bus belongs to the pipeline.
        let config = self.clone();
        let block_id: Arc<str> = block_id.into();
        let pipeline_name: Arc<str> = pipeline.name().as_str().into();
        bus.connect_sync_message(Some("stream-status"), move |_bus, msg| {
            config.on_internal_stream_status(msg, &block_id, &pipeline_name);
        });
        debug!(
            "Flow thread settings installed on internal pipeline '{}'",
            pipeline.name()
        );
    }

    /// A `STREAM_STATUS` from an internal pipeline, on the thread that posted
    /// it. Before the flow has populated its settings, an entering thread is
    /// held for [`SessionThreadConfig::populate`] and a leaving one let go.
    fn on_internal_stream_status(
        &self,
        msg: &gst::Message,
        block_id: &Arc<str>,
        pipeline: &Arc<str>,
    ) {
        let gst::MessageView::StreamStatus(status) = msg.view() else {
            return;
        };
        let (kind, owner) = status.get();
        let setup = {
            let mut slot = self.slot();
            match slot.setup {
                Some(ref setup) => Arc::clone(setup),
                None => {
                    match kind {
                        gst::StreamStatusType::Enter => slot.early.push(EarlyThread {
                            handle: ThreadHandle::current(),
                            owner: owner.name().to_string(),
                            block_id: Arc::clone(block_id),
                            pipeline: Arc::clone(pipeline),
                        }),
                        gst::StreamStatusType::Leave => {
                            let id = ThreadHandle::current().id();
                            slot.early.retain(|t| t.handle.id() != id);
                        }
                        _ => {}
                    }
                    return;
                }
            }
        };
        if !setup.is_active() {
            return;
        }
        match kind {
            gst::StreamStatusType::Enter => setup.enter(&owner.name(), Some(block_id), pipeline),
            gst::StreamStatusType::Leave => setup.leave(&owner.name(), pipeline),
            _ => {}
        }
    }

    /// Give every session pipeline a webrtcsink-based element of block
    /// `block_id` creates (`whepserversink`, `whipclientsink`) the flow's
    /// settings, through [`SessionThreadConfig::install_on_session_pipeline`].
    ///
    /// `consumer-pipeline-created` fires before webrtcsink sets its own bus
    /// sync handler on the session pipeline. A webrtcsink without the signal
    /// keeps its session threads at normal priority, with a warning.
    pub fn install_on_webrtcsink(&self, sink: &gst::Element, block_id: &str) {
        const SIGNAL: &str = "consumer-pipeline-created";
        if gst::glib::subclass::signal::SignalId::lookup(SIGNAL, sink.type_()).is_none() {
            warn!(
                "{} ({}) has no {} signal - its session threads keep normal priority",
                sink.name(),
                sink.type_().name(),
                SIGNAL
            );
            return;
        }
        let config = self.clone();
        let block_id = block_id.to_string();
        sink.connect(SIGNAL, false, move |values| {
            let session_id = values[1].get::<String>().unwrap_or_default();
            let pipeline = values[2].get::<gst::Pipeline>().ok()?;
            config.install_on_session_pipeline(&pipeline, &session_id, &block_id);
            None
        });
    }

    /// Install pad-probe-based thread priority on a session pipeline of block
    /// `block_id`.
    ///
    /// Call this from the `consumer-pipeline-created` signal handler, which
    /// fires before webrtcsink sets its own bus sync handler. That handler
    /// returns `BusSyncReply::Drop` and routes messages through an internal
    /// channel, so neither a sync handler of ours nor `sync-message` sees the
    /// session's `STREAM_STATUS`, and replacing the handler breaks session
    /// lifecycle. Instead we connect to `deep-element-added` on the pipeline
    /// so that every element that is added gets an `EVENT_DOWNSTREAM` probe on
    /// its sink pad. On `STREAM_START`, which arrives on the streaming thread,
    /// we set priority, CPU affinity, and register the thread, then remove the
    /// probe.
    pub fn install_on_session_pipeline(
        &self,
        pipeline: &gst::Pipeline,
        session_id: &str,
        block_id: &str,
    ) {
        let Some(setup) = self.setup() else {
            debug!(
                "No flow thread settings when session {} was created; its threads keep their own",
                session_id
            );
            return;
        };
        if !setup.is_active() {
            return;
        }

        info!(
            "Installing pad-probe thread priority on session pipeline '{}' for session {} \
             (priority: {:?}, cpus: {:?})",
            pipeline.name(),
            session_id,
            setup.priority,
            setup.assigned_cpus
        );

        let config = self.clone();
        let block_id: Arc<str> = block_id.into();
        let pipeline_name: Arc<str> = pipeline.name().as_str().into();

        // Track which native thread IDs have already been configured so we
        // don't set priority twice when multiple pads share a thread.
        let configured_threads: Arc<std::sync::Mutex<HashSet<u64>>> = Arc::default();

        pipeline.connect("deep-element-added", false, move |args| {
            let added: gst::Element = args[2].get().ok()?;
            let sink_pad = added.static_pad("sink")?;

            let config = config.clone();
            let block_id = Arc::clone(&block_id);
            let pipeline_name = Arc::clone(&pipeline_name);
            let configured = Arc::clone(&configured_threads);
            let element_name = added.name();

            sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                // Only a streaming thread pushes STREAM_START. Any other first
                // event can come from an application thread (an EOS sent on
                // teardown before any data), which must keep its scheduling.
                // Wait for STREAM_START instead.
                if !matches!(info.event(), Some(e) if e.type_() == gst::EventType::StreamStart) {
                    return gst::PadProbeReturn::Ok;
                }

                // Skip if this thread was already configured (multiple pads
                // can share the same streaming thread).
                if !configured
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(ThreadHandle::current().id())
                {
                    return gst::PadProbeReturn::Remove;
                }

                // The flow's settings as they are now, not as they were when
                // the session was created.
                if let Some(setup) = config.setup().filter(|s| s.is_active()) {
                    // These threads never send a Leave, so an elevation here
                    // goes back to the pool with the thread; the next
                    // pipeline that picks it up restores it (see
                    // `reset_streaming_threads`). The registry entry lives
                    // until the flow's entries are dropped by unregister_flow.
                    setup.enter(&element_name, Some(&block_id), &pipeline_name);
                }

                gst::PadProbeReturn::Remove
            });

            None
        });
    }
}

impl ThreadSetup {
    /// Register a thread that entered before the flow had settings, from
    /// another thread. Its priority and CPUs stay as they are: a change made
    /// from outside could not be undone on the thread when it leaves.
    fn adopt(&self, thread: EarlyThread) {
        self.register(thread.handle, &thread.owner, Some(&thread.block_id), None);
    }
}
