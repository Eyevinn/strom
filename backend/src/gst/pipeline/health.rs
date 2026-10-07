//! Detection of stalled pad tasks in a running pipeline.
//!
//! A pad task is the thread that drives data out of a pad. Elements pause their
//! own task when a downstream push fails with a non-fatal flow return such as
//! `not-negotiated` - video encoders do this, and so does `GstAggregator`,
//! which additionally marks all its sink pads flushing, stalling everything
//! upstream of it. Neither posts to the bus, and pausing a task is not a state
//! change, so the pipeline and every element in it stay `PLAYING` while that
//! branch carries nothing.
//!
//! `GstBaseSrc` is the exception: it posts a flow error before pausing, so
//! source-side failures already reach the bus handler.
//!
//! Scope: the scan reaches the flow pipeline's own elements and the bins beneath
//! them, and nothing else. Blocks that run an isolated `gst::Pipeline` of their
//! own - WHIP ingest sessions, a Media Player's decode chain - are siblings of
//! that pipeline rather than children of it, so `iterate_recurse` never enters
//! them. An `Ok` here therefore means "no stalled pad task among this flow
//! pipeline's elements", not "this flow is passing data". Sinks that live in the
//! flow pipeline, `whepserversink` included, are covered.
//!
//! A paused task on an element that is itself `PLAYING` is always wrong, so
//! that is the signal used here. `gst_pad_get_task_state()` returns `Stopped`
//! both for a pad whose task was stopped and for a pad that never had one, so
//! `Stopped` cannot be told apart from the common case and is not reported.
//!
//! The scan cannot see a chain that is linked and simply never pushed to: its
//! elements sit idle and `PLAYING` with no task to pause. Only the block that
//! owns it knows, so a block may report on itself through `BlockLiveness`, and
//! the scan folds that verdict in.

use crate::blocks::BlockLiveness;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use strom_types::flow::{BlockHealth, BlockHealthCause, BlockHealthStatus};

/// A pad whose task has been paused while the pipeline is playing.
struct StalledPad {
    element: String,
    pad: String,
}

/// Find the first paused pad task on `element`, and on its children if it is a bin.
fn first_stalled_pad(element: &gst::Element) -> Option<StalledPad> {
    if let Some(stalled) = stalled_pad_on(element) {
        return Some(stalled);
    }
    let bin = element.downcast_ref::<gst::Bin>()?;
    bin.iterate_recurse()
        .into_iter()
        .flatten()
        .find_map(|child| stalled_pad_on(&child))
}

/// Find the first paused pad task on `element` itself.
///
/// Four kinds of legitimately paused task are excluded.
///
/// An element held below `PLAYING` - a webrtcbin added for a newly connected
/// WHEP consumer, say - pauses its tasks as part of that transition.
///
/// A pad that has seen EOS is finished by definition: `gst_base_src_loop` pauses
/// its task on the way out for *every* reason including end of stream, and the
/// element stays `PLAYING` afterwards, so a finite source that has played out
/// would otherwise be reported as failed forever. The stall this looks for
/// pushes no EOS. A parser reading a file in pull mode runs its task on its
/// sink pad, which never carries the EOS flag because the EOS leaves through
/// the source pad; there the EOS returned by its last pull marks it finished.
///
/// An unlinked pad has no branch to report on. A `queue` feeding a block output
/// that the flow leaves unconnected gets `not-linked` from its loop and parks
/// the task there permanently - the vision mixer's `multiview_out` does exactly
/// this whenever a flow uses only the program output.
///
/// That exclusion holds only because every `tee` Strom builds sets
/// `allow-not-linked`, which keeps the `not-linked` at the unconnected branch.
/// A `tee` with its default instead returns `not-linked` upstream once all its
/// branches are unconnected, so the task that parks is the one feeding the tee,
/// on a pad that does have a peer. A raw `tee` element in a user flow does
/// exactly that, and the element upstream of it is reported as failed.
///
/// A task on an element that has never been fed is idle, not stalled, as long
/// as its own pushes have not failed. `vtenc` parks its output task on reaching
/// `PLAYING` and starts it on the first frame, so a Video Encoder on an empty
/// seat would otherwise read as failed for as long as the seat stays empty. A
/// segment must precede the first buffer, so an element with no segment on any
/// sink pad has never received data. A failed push still counts, fed or not.
/// The last flow return cannot stand alone: an aggregator that fails to
/// renegotiate pauses without pushing, so its last flow stays `Ok`.
fn stalled_pad_on(element: &gst::Element) -> Option<StalledPad> {
    if element.current_state() != gst::State::Playing {
        return None;
    }
    let pads = element.pads();
    let sink_pads = || {
        pads.iter()
            .filter(|pad| pad.direction() == gst::PadDirection::Sink)
    };
    let never_fed = sink_pads().next().is_some()
        && sink_pads().all(|pad| pad.sticky_event::<gst::event::Segment>(0).is_none());
    pads.iter()
        .find(|pad| {
            pad.task_state() == gst::TaskState::Paused
                && !pad.pad_flags().contains(gst::PadFlags::EOS)
                && pad.last_flow_result() != Err(gst::FlowError::Eos)
                && pad.peer().is_some()
                && !(never_fed && pad.last_flow_result().is_ok())
        })
        .map(|pad| StalledPad {
            element: element.name().to_string(),
            pad: pad.name().to_string(),
        })
}

/// Block instance ID owning `element_id`.
///
/// Block elements are namespaced `block_id:internal_id` by `BlockBuildResult`.
/// A standalone element has no prefix and is reported under its own ID so that
/// a stall outside a block is not silently dropped.
fn owning_block(element_id: &str) -> &str {
    element_id.split(':').next().unwrap_or(element_id)
}

/// Report health for every block with at least one element in `elements`.
///
/// Call only while the pipeline is playing; a paused task is expected in any
/// other pipeline state.
///
/// `reporters` are blocks that judge their own liveness; see `BlockLiveness`.
/// A reporter for a block with no element left in the pipeline is skipped, so
/// its verdict cannot outlive the elements it describes.
pub(crate) fn scan_block_health(
    elements: &HashMap<String, gst::Element>,
    reporters: &[(String, Arc<dyn BlockLiveness>)],
) -> Vec<BlockHealth> {
    let mut by_block: BTreeMap<&str, BlockHealth> = BTreeMap::new();

    for (element_id, element) in elements {
        let block_id = owning_block(element_id);
        let entry = by_block.entry(block_id).or_insert_with(|| BlockHealth {
            block_id: block_id.to_string(),
            status: BlockHealthStatus::Ok,
            detail: None,
            causes: Vec::new(),
        });

        // A block is failed if any of its elements has a stalled pad; the first
        // one found names the failure.
        if entry.status.is_failed() {
            continue;
        }
        if let Some(stalled) = first_stalled_pad(element) {
            entry.status = BlockHealthStatus::Failed;
            entry.detail = Some(format!(
                "pad task paused on {}:{} - this branch is not passing data",
                stalled.element, stalled.pad
            ));
        }
    }

    // A block that reports on itself. Checked after the pad-task scan, which
    // names the more specific failure when both fire.
    for (block_id, reporter) in reporters {
        let Some(entry) = by_block.get_mut(block_id.as_str()) else {
            continue;
        };
        if entry.status.is_failed() {
            continue;
        }
        if let Some(failure) = reporter.failure() {
            entry.status = BlockHealthStatus::Failed;
            entry.detail = Some(failure.detail);
            entry.causes = failure.causes;
        }
    }

    by_block.into_values().collect()
}

/// A change in one block's health worth announcing.
#[derive(Debug, PartialEq, Eq)]
enum Transition {
    /// The block failed.
    Failed,
    /// The block is still failed, for different reasons. `detail` is not
    /// compared: it may carry a duration that changes on every scan.
    CausesChanged,
    /// The block is healthy again.
    Recovered,
}

/// What to announce for `health`, given the causes it was last announced as
/// failed with (`None` if it was not).
fn transition(
    health: &BlockHealth,
    announced: Option<&Vec<BlockHealthCause>>,
) -> Option<Transition> {
    match (health.status.is_failed(), announced) {
        (true, None) => Some(Transition::Failed),
        (true, Some(causes)) if *causes != health.causes => Some(Transition::CausesChanged),
        (false, Some(_)) => Some(Transition::Recovered),
        _ => None,
    }
}

/// How often the running pipeline is scanned for stalled pad tasks.
const HEALTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Consecutive scans a block must look stalled before it is reported.
///
/// Pad tasks are briefly paused for legitimate reasons - a flushing seek, a
/// dynamic bin being linked in - so a single sample is not enough to call a
/// block failed. Costs one extra poll interval of detection latency.
const CONFIRMATIONS_BEFORE_FAILED: u32 = 2;

impl super::PipelineManager {
    /// Current per-block health. Empty until the health task has run once.
    pub fn get_block_health(&self) -> Vec<BlockHealth> {
        self.block_health.read().unwrap().clone()
    }

    /// Start the periodic scan for stalled pad tasks.
    pub(super) fn start_block_health_task(&mut self) {
        self.stop_block_health_task();

        // WeakRef, not a clone: a task holding strong element references would
        // keep the pipeline alive past drop.
        let weak_elements: Vec<(String, gst::glib::WeakRef<gst::Element>)> = self
            .elements
            .iter()
            .map(|(id, element)| (id.clone(), element.downgrade()))
            .collect();

        let cached_state = self.cached_state.clone();
        let block_health = self.block_health.clone();
        let events = self.events.clone();
        let flow_id = self.flow_id;
        let flow_name = self.flow_name.clone();
        let reporters = self.block_liveness.clone();

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEALTH_POLL_INTERVAL);
            // Blocks announced as failed, with the causes they were announced
            // with.
            let mut failed: HashMap<String, Vec<BlockHealthCause>> = HashMap::new();
            let mut strikes: HashMap<String, u32> = HashMap::new();

            loop {
                interval.tick().await;

                // Only meaningful while playing - a paused task is expected in
                // every other state, including during startup and shutdown.
                if *cached_state.read().unwrap() != strom_types::PipelineState::Playing {
                    failed.clear();
                    strikes.clear();
                    block_health.write().unwrap().clear();
                    continue;
                }

                let elements: HashMap<String, gst::Element> = weak_elements
                    .iter()
                    .filter_map(|(id, weak)| weak.upgrade().map(|el| (id.clone(), el)))
                    .collect();
                if elements.is_empty() {
                    continue;
                }

                let mut snapshot = scan_block_health(&elements, &reporters);

                // A block is only reported once it has looked stalled on
                // CONFIRMATIONS_BEFORE_FAILED scans in a row. Downgrade the
                // unconfirmed ones before anything observes the snapshot.
                for health in &mut snapshot {
                    if health.status.is_failed() {
                        let count = strikes.entry(health.block_id.clone()).or_insert(0);
                        *count += 1;
                        if *count < CONFIRMATIONS_BEFORE_FAILED {
                            health.status = BlockHealthStatus::Ok;
                            health.detail = None;
                            health.causes.clear();
                        }
                    } else {
                        strikes.remove(&health.block_id);
                    }
                }

                // Blocks whose elements have gone away drop out of the scan
                // entirely; forget them rather than leaving a stale entry that
                // no later iteration can clear.
                failed.retain(|block_id, _| snapshot.iter().any(|h| &h.block_id == block_id));
                strikes.retain(|block_id, _| snapshot.iter().any(|h| &h.block_id == block_id));

                for health in &snapshot {
                    match transition(health, failed.get(&health.block_id)) {
                        Some(Transition::Failed) => {
                            failed.insert(health.block_id.clone(), health.causes.clone());
                            tracing::error!(
                                "Block '{}' in flow '{}' has stopped: {}. The pipeline still reports Playing",
                                health.block_id,
                                flow_name,
                                health.detail.as_deref().unwrap_or("no detail")
                            );
                            events.broadcast(strom_types::StromEvent::BlockHealthChanged {
                                flow_id,
                                block_id: health.block_id.clone(),
                                status: health.status,
                                detail: health.detail.clone(),
                                causes: health.causes.clone(),
                            });
                        }
                        Some(Transition::CausesChanged) => {
                            failed.insert(health.block_id.clone(), health.causes.clone());
                            tracing::error!(
                                "Block '{}' in flow '{}' is still stopped: {}",
                                health.block_id,
                                flow_name,
                                health.detail.as_deref().unwrap_or("no detail")
                            );
                            events.broadcast(strom_types::StromEvent::BlockHealthChanged {
                                flow_id,
                                block_id: health.block_id.clone(),
                                status: health.status,
                                detail: health.detail.clone(),
                                causes: health.causes.clone(),
                            });
                        }
                        Some(Transition::Recovered) => {
                            failed.remove(&health.block_id);
                            tracing::info!(
                                "Block '{}' in flow '{}' resumed passing data",
                                health.block_id,
                                flow_name
                            );
                            events.broadcast(strom_types::StromEvent::BlockHealthChanged {
                                flow_id,
                                block_id: health.block_id.clone(),
                                status: health.status,
                                detail: None,
                                causes: Vec::new(),
                            });
                        }
                        None => {}
                    }
                }

                *block_health.write().unwrap() = snapshot;
            }
        });

        self.block_health_task = Some(task);
    }

    /// Stop the periodic health scan and drop the last snapshot.
    pub(super) fn stop_block_health_task(&mut self) {
        if let Some(task) = self.block_health_task.take() {
            task.abort();
        }
        self.block_health.write().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::BlockRegistry;
    use crate::events::EventBroadcaster;
    use crate::gst::pipeline::PipelineManager;
    use std::time::{Duration, Instant};
    use strom_types::{Element, Flow, Link, PropertyValue};

    #[test]
    fn a_failed_block_is_announced_again_when_its_causes_change() {
        use strom_types::flow::{HealthMedium, MediumFault};
        let cause = |slot| BlockHealthCause::WhipMedium {
            slot,
            medium: HealthMedium::Audio,
            fault: MediumFault::PublisherStopped,
        };
        let health = |status, detail: &str, causes| BlockHealth {
            block_id: "whip".to_string(),
            status,
            detail: Some(detail.to_string()),
            causes,
        };
        let one_seat = vec![cause(0)];
        let failed_one = health(
            BlockHealthStatus::Failed,
            "slot 0 for 2.0 s",
            one_seat.clone(),
        );

        assert_eq!(transition(&failed_one, None), Some(Transition::Failed));
        assert_eq!(
            transition(
                &health(
                    BlockHealthStatus::Failed,
                    "slot 0 for 4.0 s",
                    one_seat.clone()
                ),
                Some(&one_seat)
            ),
            None,
            "the same causes with a newer duration in the detail are not news"
        );
        assert_eq!(
            transition(
                &health(
                    BlockHealthStatus::Failed,
                    "slots 0 and 1",
                    vec![cause(0), cause(1)]
                ),
                Some(&one_seat)
            ),
            Some(Transition::CausesChanged),
            "a second seat losing its audio has to reach a client"
        );
        assert_eq!(
            transition(
                &BlockHealth {
                    block_id: "whip".to_string(),
                    status: BlockHealthStatus::Ok,
                    detail: None,
                    causes: Vec::new(),
                },
                Some(&one_seat)
            ),
            Some(Transition::Recovered)
        );
    }

    /// A block that judges its own liveness has to reach the snapshot. Nothing
    /// else can report it: a chain that is never pushed to stalls no pad task,
    /// so the scan itself has nothing to find, and the pipeline stays `Playing`
    /// throughout.
    #[test]
    fn a_blocks_own_failure_report_reaches_its_health() {
        use crate::blocks::LivenessFailure;
        use strom_types::flow::{HealthMedium, MediumFault};
        let _ = gst::init();

        let cause = BlockHealthCause::WhipMedium {
            slot: 0,
            medium: HealthMedium::Audio,
            fault: MediumFault::PublisherStopped,
        };
        struct HalfDead(BlockHealthCause);
        impl BlockLiveness for HalfDead {
            fn failure(&self) -> Option<LivenessFailure> {
                Some(LivenessFailure {
                    detail: "slot 0: the publisher stopped sending audio".to_string(),
                    causes: vec![self.0.clone()],
                })
            }
        }

        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink is part of gstreamer core");
        let elements: HashMap<String, gst::Element> =
            HashMap::from([("whip:sink".to_string(), sink)]);

        let healthy = scan_block_health(&elements, &[]);
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].status, BlockHealthStatus::Ok);

        let reporters: Vec<(String, Arc<dyn BlockLiveness>)> =
            vec![("whip".to_string(), Arc::new(HalfDead(cause.clone())))];
        let reported = scan_block_health(&elements, &reporters);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].block_id, "whip");
        assert_eq!(reported[0].status, BlockHealthStatus::Failed);
        assert_eq!(
            reported[0].detail.as_deref(),
            Some("slot 0: the publisher stopped sending audio")
        );
        assert_eq!(reported[0].causes, vec![cause.clone()]);

        // A verdict about a block with nothing left in the pipeline must not
        // conjure an entry for it.
        let orphan: Vec<(String, Arc<dyn BlockLiveness>)> =
            vec![("gone".to_string(), Arc::new(HalfDead(cause)))];
        let scanned = scan_block_health(&elements, &orphan);
        assert_eq!(
            scanned
                .iter()
                .map(|h| h.block_id.as_str())
                .collect::<Vec<_>>(),
            vec!["whip"]
        );
    }

    #[test]
    fn block_elements_are_attributed_to_their_block() {
        assert_eq!(owning_block("whep:videoenc"), "whep");
        assert_eq!(owning_block("whep:sink:inner"), "whep");
        assert_eq!(owning_block("standalone_element"), "standalone_element");
    }

    fn element(id: &str, element_type: &str, props: &[(&str, PropertyValue)]) -> Element {
        Element {
            id: id.to_string(),
            element_type: element_type.to_string(),
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            pad_properties: HashMap::new(),
            position: (0.0, 0.0),
        }
    }

    /// videotestsrc -> compositor -> capsfilter -> fakesink, as a real flow.
    fn stall_flow() -> Flow {
        let mut flow = Flow::new("block health test");
        flow.elements = vec![
            element(
                "src",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element("comp", "compositor", &[]),
            element("filter", "capsfilter", &[]),
            element("sink", "fakesink", &[("sync", PropertyValue::Bool(false))]),
        ];
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "comp".to_string(),
            },
            Link {
                from: "comp".to_string(),
                to: "filter".to_string(),
            },
            Link {
                from: "filter".to_string(),
                to: "sink".to_string(),
            },
        ];
        flow
    }

    /// videotestsrc -> tee, with one tee branch queued and left unconnected.
    ///
    /// Mirrors a block output the flow does not wire up - the vision mixer's
    /// `multiview_out` is exactly this - where the queue's loop takes
    /// `not-linked` and parks its task for good.
    fn unwired_output_flow() -> Flow {
        let mut flow = Flow::new("unwired output health test");
        flow.elements = vec![
            element(
                "src",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element("split", "tee", &[]),
            element("q_used", "queue", &[]),
            element("sink", "fakesink", &[("sync", PropertyValue::Bool(false))]),
            // Fed by the tee, going nowhere.
            element("q_dangling", "queue", &[]),
        ];
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "split".to_string(),
            },
            Link {
                from: "split".to_string(),
                to: "q_used".to_string(),
            },
            Link {
                from: "q_used".to_string(),
                to: "sink".to_string(),
            },
            Link {
                from: "split".to_string(),
                to: "q_dangling".to_string(),
            },
        ];
        flow
    }

    /// videotestsrc with a fixed buffer count -> fakesink. Runs to EOS.
    fn finite_source_flow() -> Flow {
        let mut flow = Flow::new("eos health test");
        flow.elements = vec![
            element(
                "src",
                "videotestsrc",
                &[("num-buffers", PropertyValue::Int(5))],
            ),
            element("sink", "fakesink", &[("sync", PropertyValue::Bool(false))]),
        ];
        flow.links = vec![Link {
            from: "src".to_string(),
            to: "sink".to_string(),
        }];
        flow
    }

    /// filesrc -> rawaudioparse -> fakesink, reading a short raw audio file.
    ///
    /// rawaudioparse pulls from filesrc, so its pad task runs on its sink pad.
    fn pull_mode_clip_flow(path: &std::path::Path) -> Flow {
        let mut flow = Flow::new("pull mode eos health test");
        flow.elements = vec![
            element(
                "src",
                "filesrc",
                &[(
                    "location",
                    PropertyValue::String(path.to_string_lossy().to_string()),
                )],
            ),
            element("parse", "rawaudioparse", &[]),
            element("sink", "fakesink", &[("sync", PropertyValue::Bool(false))]),
        ];
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "parse".to_string(),
            },
            Link {
                from: "parse".to_string(),
                to: "sink".to_string(),
            },
        ];
        flow
    }

    /// Live source -> identity -> fakesink.
    ///
    /// Unfed, the source is an appsrc that never pushes and stands in for an
    /// empty WHIP seat: nothing reaches the identity, not even a segment. Fed,
    /// it is a live videotestsrc.
    fn identity_flow(fed: bool) -> Flow {
        let mut flow = Flow::new("identity health test");
        let src = if fed {
            element(
                "src",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            )
        } else {
            element(
                "src",
                "appsrc",
                &[
                    ("is-live", PropertyValue::Bool(true)),
                    ("format", PropertyValue::String("time".to_string())),
                ],
            )
        };
        flow.elements = vec![
            src,
            element("mid", "identity", &[]),
            element(
                "sink",
                "fakesink",
                &[
                    ("sync", PropertyValue::Bool(false)),
                    ("async", PropertyValue::Bool(false)),
                ],
            ),
        ];
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "mid".to_string(),
            },
            Link {
                from: "mid".to_string(),
                to: "sink".to_string(),
            },
        ];
        flow
    }

    /// Give `pad` a task that pauses itself, as `vtenc` parks its output task.
    ///
    /// The pause has to come from inside the task: `gst_pad_pause_task` from
    /// another thread waits on the stream lock.
    fn park_task(pad: &gst::Pad) {
        let weak_pad = pad.downgrade();
        pad.start_task(move || {
            if let Some(pad) = weak_pad.upgrade() {
                let _ = pad.pause_task();
            }
        })
        .expect("task should start");
        assert!(
            wait_for(
                || pad.task_state() == gst::TaskState::Paused,
                Duration::from_secs(5)
            ),
            "task never paused"
        );
    }

    fn wait_for(condition: impl Fn() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        condition()
    }

    /// A `queue` whose source pad is left unconnected takes `not-linked` from its
    /// loop and parks the task while the element stays `PLAYING`. Blocks expose
    /// optional outputs built this way, so reporting it would mark most flows
    /// using them permanently failed.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unwired_output_branch_is_not_reported_as_failed() {
        gst::init().unwrap();

        let mut manager = PipelineManager::new(
            &unwired_output_flow(),
            EventBroadcaster::default(),
            &BlockRegistry::new("test_blocks.json"),
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("pipeline should build");

        manager.start().expect("pipeline should start");
        assert!(
            wait_for(
                || manager.get_state() == strom_types::PipelineState::Playing,
                Duration::from_secs(10)
            ),
            "pipeline never reached Playing"
        );

        let went_failed = wait_for(
            || {
                manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.status == BlockHealthStatus::Failed)
            },
            HEALTH_POLL_INTERVAL * (CONFIRMATIONS_BEFORE_FAILED + 3),
        );
        assert!(
            !went_failed,
            "an unwired output branch must not read as failed: {:?}",
            manager.get_block_health()
        );

        manager.stop().expect("pipeline should stop");
    }

    /// `gst_base_src_loop` pauses its pad task on the way out for every reason,
    /// end of stream included, and leaves the element in `PLAYING`. Playing a
    /// finite source to completion must not read as a failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_that_reaches_eos_is_not_reported_as_failed() {
        gst::init().unwrap();

        let mut manager = PipelineManager::new(
            &finite_source_flow(),
            EventBroadcaster::default(),
            &BlockRegistry::new("test_blocks.json"),
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("pipeline should build");

        manager.start().expect("pipeline should start");

        // Let the source play out and the scan run several times over the
        // drained pipeline.
        assert!(
            wait_for(
                || manager
                    .elements
                    .get("src")
                    .map(|e| e.static_pad("src").unwrap().pad_flags())
                    .is_some_and(|f| f.contains(gst::PadFlags::EOS)),
                Duration::from_secs(15)
            ),
            "source never reached EOS"
        );
        // Watch across several scans - long enough that a block would clear the
        // confirmation threshold - and assert it never flips to failed.
        let went_failed = wait_for(
            || {
                manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.status == BlockHealthStatus::Failed)
            },
            HEALTH_POLL_INTERVAL * (CONFIRMATIONS_BEFORE_FAILED + 3),
        );
        assert!(
            !went_failed,
            "a drained finite source must not read as failed: {:?}",
            manager.get_block_health()
        );
        assert!(
            !manager.get_block_health().is_empty(),
            "health scan never produced a snapshot"
        );

        manager.stop().expect("pipeline should stop");
    }

    /// A parser reading a file in pull mode pauses its sink pad task at the end
    /// of the file. The EOS goes out through its source pad, so the paused pad
    /// never carries the EOS flag; what marks it finished is the EOS its last
    /// pull returned. A clip played to the end must not read as a failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pull_mode_parser_at_end_of_file_is_not_reported_as_failed() {
        gst::init().unwrap();

        let path = std::env::temp_dir().join(format!(
            "strom-health-pull-mode-{}.raw",
            uuid::Uuid::new_v4()
        ));
        // A fifth of a second of rawaudioparse's default S16LE 44.1 kHz stereo.
        std::fs::write(&path, vec![0u8; 44_100 * 4 / 5]).unwrap();

        let mut manager = PipelineManager::new(
            &pull_mode_clip_flow(&path),
            EventBroadcaster::default(),
            &BlockRegistry::new("test_blocks.json"),
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("pipeline should build");

        manager.start().expect("pipeline should start");
        assert!(
            wait_for(
                || manager.elements["sink"]
                    .static_pad("sink")
                    .unwrap()
                    .pad_flags()
                    .contains(gst::PadFlags::EOS),
                Duration::from_secs(15)
            ),
            "clip never reached EOS"
        );
        // The premise: the paused task sits on a pad with no EOS flag.
        let parse_sink = manager.elements["parse"].static_pad("sink").unwrap();
        assert!(
            wait_for(
                || parse_sink.task_state() == gst::TaskState::Paused,
                Duration::from_secs(5)
            ),
            "rawaudioparse did not run its sink pad task in pull mode"
        );
        assert!(!parse_sink.pad_flags().contains(gst::PadFlags::EOS));

        let went_failed = wait_for(
            || {
                manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.status == BlockHealthStatus::Failed)
            },
            HEALTH_POLL_INTERVAL * (CONFIRMATIONS_BEFORE_FAILED + 3),
        );
        let health = manager.get_block_health();
        manager.stop().expect("pipeline should stop");
        let _ = std::fs::remove_file(&path);
        assert!(
            !went_failed,
            "a clip played to the end must not read as failed: {:?}",
            health
        );
        assert!(!health.is_empty(), "health scan never produced a snapshot");
    }

    /// `vtenc` parks its output task as soon as it reaches `PLAYING` and only
    /// starts it on the first frame, so a Video Encoder on an empty seat sits
    /// paused with nothing wrong. No element available on Linux CI parks a task
    /// before data, so this parks one on an unfed `identity`, producing the
    /// same pad state: task paused, last push `Ok`, no segment on the input.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_element_that_was_never_fed_is_not_reported_as_failed() {
        gst::init().unwrap();

        let mut manager = PipelineManager::new(
            &identity_flow(false),
            EventBroadcaster::default(),
            &BlockRegistry::new("test_blocks.json"),
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("pipeline should build");

        manager.start().expect("pipeline should start");
        assert!(
            wait_for(
                || manager.get_state() == strom_types::PipelineState::Playing,
                Duration::from_secs(10)
            ),
            "pipeline never reached Playing"
        );

        let mid_src = manager.elements["mid"].static_pad("src").unwrap();
        park_task(&mid_src);

        let went_failed = wait_for(
            || {
                manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.status == BlockHealthStatus::Failed)
            },
            HEALTH_POLL_INTERVAL * (CONFIRMATIONS_BEFORE_FAILED + 3),
        );
        assert!(
            !went_failed,
            "an element that never received data must not read as failed: {:?}",
            manager.get_block_health()
        );
        assert!(
            !manager.get_block_health().is_empty(),
            "health scan never produced a snapshot"
        );

        mid_src.stop_task().expect("task should stop");
        manager.stop().expect("pipeline should stop");
    }

    /// The same parked task on an element that has received data is still
    /// reported, even though its last push succeeded. The scan reads only pad
    /// state, so the identity passing buffers on the upstream thread does not
    /// matter here.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_parked_task_on_an_element_that_was_fed_is_reported() {
        gst::init().unwrap();

        let mut manager = PipelineManager::new(
            &identity_flow(true),
            EventBroadcaster::default(),
            &BlockRegistry::new("test_blocks.json"),
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("pipeline should build");

        manager.start().expect("pipeline should start");
        let mid = manager.elements["mid"].clone();
        assert!(
            wait_for(
                || mid
                    .static_pad("sink")
                    .unwrap()
                    .sticky_event::<gst::event::Segment>(0)
                    .is_some(),
                Duration::from_secs(10)
            ),
            "identity never received data"
        );

        let mid_src = mid.static_pad("src").unwrap();
        park_task(&mid_src);
        assert!(mid_src.last_flow_result().is_ok());

        assert!(
            wait_for(
                || manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.block_id == "mid" && h.status == BlockHealthStatus::Failed),
                Duration::from_secs(15)
            ),
            "a parked task on a fed element was not reported: {:?}",
            manager.get_block_health()
        );

        mid_src.stop_task().expect("task should stop");
        manager.stop().expect("pipeline should stop");
    }

    /// The case from the field: a Video Encoder block on a seat with no guest.
    /// On macOS it selects `vtenc`, whose output task is paused until the first
    /// frame arrives.
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread")]
    async fn an_idle_video_encoder_is_not_reported_as_failed() {
        gst::init().unwrap();

        crate::gpu::detect_gpu_capabilities();

        let mut flow = identity_flow(false);
        flow.elements.retain(|e| e.id != "mid");
        flow.blocks.push(strom_types::BlockInstance {
            id: "venc".to_string(),
            block_definition_id: "builtin.videoenc".to_string(),
            name: None,
            properties: HashMap::from([(
                "codec".to_string(),
                PropertyValue::String("h264".to_string()),
            )]),
            position: strom_types::block::Position { x: 0.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: None,
        });
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "venc:video_in".to_string(),
            },
            Link {
                from: "venc:encoded_out".to_string(),
                to: "sink".to_string(),
            },
        ];

        let mut manager = PipelineManager::new(
            &flow,
            EventBroadcaster::default(),
            &BlockRegistry::new("test_blocks.json"),
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("pipeline should build");

        manager.start().expect("pipeline should start");
        assert!(
            wait_for(
                || manager.get_state() == strom_types::PipelineState::Playing,
                Duration::from_secs(10)
            ),
            "pipeline never reached Playing"
        );

        // The premise: the idle encoder really does sit with a paused task.
        let encoder = manager.elements.get("venc:encoder").unwrap().clone();
        assert!(
            wait_for(
                || encoder.static_pad("src").unwrap().task_state() == gst::TaskState::Paused,
                Duration::from_secs(5)
            ),
            "expected {} to park its output task before data",
            encoder.factory().unwrap().name()
        );

        let went_failed = wait_for(
            || {
                manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.status == BlockHealthStatus::Failed)
            },
            HEALTH_POLL_INTERVAL * (CONFIRMATIONS_BEFORE_FAILED + 3),
        );
        assert!(
            !went_failed,
            "an idle Video Encoder must not read as failed: {:?}",
            manager.get_block_health()
        );

        manager.stop().expect("pipeline should stop");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stalled_branch_is_reported_while_the_pipeline_still_says_playing() {
        gst::init().unwrap();

        let mut manager = PipelineManager::new(
            &stall_flow(),
            EventBroadcaster::default(),
            &BlockRegistry::new("test_blocks.json"),
            vec!["stun:stun.l.google.com:19302".to_string()],
            "all".to_string(),
            None,
            std::path::PathBuf::from("./media"),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("pipeline should build");

        manager.start().expect("pipeline should start");
        assert!(
            wait_for(
                || manager.get_state() == strom_types::PipelineState::Playing,
                Duration::from_secs(10)
            ),
            "pipeline never reached Playing"
        );
        assert!(
            wait_for(
                || !manager.get_block_health().is_empty(),
                Duration::from_secs(10)
            ),
            "health scan never produced a snapshot"
        );
        assert!(
            manager
                .get_block_health()
                .iter()
                .all(|h| h.status == BlockHealthStatus::Ok),
            "a healthy pipeline should report no failures: {:?}",
            manager.get_block_health()
        );

        // Retighten the capsfilter to a format the compositor cannot produce.
        // The next push fails negotiation and the compositor pauses its source
        // pad task without posting anything to the bus.
        manager
            .elements
            .get("filter")
            .expect("capsfilter should exist")
            .set_property(
                "caps",
                gst::Caps::builder("video/x-bayer")
                    .field("format", "bggr")
                    .build(),
            );

        assert!(
            wait_for(
                || manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.block_id == "comp" && h.status == BlockHealthStatus::Failed),
                Duration::from_secs(15)
            ),
            "stalled compositor was not reported: {:?}",
            manager.get_block_health()
        );

        // The failure has to be visible without the pipeline state changing,
        // which is the case the scan exists for.
        assert_eq!(manager.get_state(), strom_types::PipelineState::Playing);

        // An element deliberately held below Playing has paused pad tasks for a
        // legitimate reason and must not be reported. Upstream elements that
        // are still Playing and now blocked behind it are a real stall and stay
        // reported, so only the paused element itself is checked here.
        let compositor = manager.elements.get("comp").unwrap().clone();
        compositor.set_state(gst::State::Paused).unwrap();
        assert!(
            wait_for(
                || manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.block_id == "comp" && h.status == BlockHealthStatus::Ok),
                Duration::from_secs(15)
            ),
            "a paused element should not be reported as failed: {:?}",
            manager.get_block_health()
        );

        manager.stop().expect("pipeline should stop");
    }
}
