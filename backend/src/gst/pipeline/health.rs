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
//! A branch that never started is the other thing reported here, and the scan
//! above cannot see it: with no link there is no pad task to pause, and a tee
//! with `allow-not-linked=true` - what every block output bus ends in - absorbs
//! the `not-linked` that would otherwise surface upstream. The flow reaches
//! `PLAYING` carrying nothing on that branch.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use strom_types::flow::{BlockHealth, BlockHealthStatus};
use strom_types::Link;

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
/// Three kinds of legitimately paused task are excluded.
///
/// An element held below `PLAYING` - a webrtcbin added for a newly connected
/// WHEP consumer, say - pauses its tasks as part of that transition.
///
/// A pad that has seen EOS is finished by definition: `gst_base_src_loop` pauses
/// its task on the way out for *every* reason including end of stream, and the
/// element stays `PLAYING` afterwards, so a finite source that has played out
/// would otherwise be reported as failed forever. The stall this looks for
/// pushes no EOS.
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
fn stalled_pad_on(element: &gst::Element) -> Option<StalledPad> {
    if element.current_state() != gst::State::Playing {
        return None;
    }
    element
        .pads()
        .into_iter()
        .find(|pad| {
            pad.task_state() == gst::TaskState::Paused
                && !pad.pad_flags().contains(gst::PadFlags::EOS)
                && pad.peer().is_some()
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

/// Links the linker has given up on, recorded as the flow declares them.
///
/// A refused link is retried only by the `pad-added` handler on its source
/// element, so two kinds of refusal are final. Construction records one whose
/// source pad already exists or can be requested: that pad will not be added
/// again. The handler records every attempt it makes on a pad that appears
/// later. A late pad it cannot link gets an auto-tee as its peer, so the pad's
/// own state cannot show the refusal; the record does.
///
/// A link whose source pad has not appeared yet is never recorded. That is
/// what deferral is for: `decodebin` before it has typefound, a
/// `whipserversrc` before a publisher arrives.
///
/// Recorded links are the ones the flow declares, not the ones construction
/// rewrote to go through an auto-tee, so a failure names a link and a block the
/// operator can find.
#[derive(Clone, Default)]
pub(crate) struct UnformedLinks(Arc<Mutex<Vec<LinkAttempts>>>);

struct LinkAttempts {
    link: Link,
    /// The latest outcome for each source pad tried: `true` if it linked.
    pads: Vec<(String, bool)>,
}

impl UnformedLinks {
    /// Record the outcome of linking `link` from the source pad `pad`.
    pub(crate) fn record(&self, link: &Link, pad: &str, formed: bool) {
        let mut attempts = self.0.lock().unwrap();
        let index = match attempts
            .iter()
            .position(|a| a.link.from == link.from && a.link.to == link.to)
        {
            Some(index) => index,
            None => {
                attempts.push(LinkAttempts {
                    link: link.clone(),
                    pads: Vec::new(),
                });
                attempts.len() - 1
            }
        };
        let pads = &mut attempts[index].pads;
        match pads.iter_mut().find(|(name, _)| name == pad) {
            Some(entry) => entry.1 = formed,
            None => pads.push((pad.to_string(), formed)),
        }
    }

    /// Links with no source pad linked, in the order first recorded.
    ///
    /// One pad linking is enough: a link asking for `src` is tried against
    /// every `src_N` a demuxer adds, and only the matching stream links.
    pub(crate) fn unformed(&self) -> Vec<Link> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.pads.iter().all(|(_, formed)| !formed))
            .map(|a| a.link.clone())
            .collect()
    }
}

/// Report health for every block with at least one element in `elements`.
///
/// `unformed_links` are the links the linker gave up on, from
/// [`UnformedLinks::unformed`].
///
/// Call only while the pipeline is playing; a paused task is expected in any
/// other pipeline state.
pub(crate) fn scan_block_health(
    elements: &HashMap<String, gst::Element>,
    unformed_links: &[Link],
) -> Vec<BlockHealth> {
    let mut by_block: BTreeMap<&str, BlockHealth> = BTreeMap::new();

    for (element_id, element) in elements {
        let block_id = owning_block(element_id);
        let entry = by_block.entry(block_id).or_insert_with(|| BlockHealth {
            block_id: block_id.to_string(),
            status: BlockHealthStatus::Ok,
            detail: None,
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

    // Reported against the block owning the source element, which is where the
    // branch dies, and allowed to overwrite a stall found above: a task parked
    // behind a link that never formed is the symptom, and the link is the cause.
    // Where a block has several, the first names it, as in the scan above.
    let mut named_by_link: std::collections::HashSet<String> = std::collections::HashSet::new();
    for link in unformed_links {
        let (from_ref, _) = link.to_pad_refs();
        let block_id = owning_block(&from_ref.element_id);
        if !named_by_link.insert(block_id.to_string()) {
            continue;
        }
        let Some(entry) = by_block.get_mut(block_id) else {
            continue;
        };
        entry.status = BlockHealthStatus::Failed;
        entry.detail = Some(format!(
            "link {} -> {} never formed - this branch is carrying no data",
            link.from, link.to
        ));
    }

    by_block.into_values().collect()
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

        let unformed_links = self.unformed_links.clone();
        let cached_state = self.cached_state.clone();
        let block_health = self.block_health.clone();
        let events = self.events.clone();
        let flow_id = self.flow_id;
        let flow_name = self.flow_name.clone();

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEALTH_POLL_INTERVAL);
            let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();
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

                let mut snapshot = scan_block_health(&elements, &unformed_links.unformed());

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
                        }
                    } else {
                        strikes.remove(&health.block_id);
                    }
                }

                // Blocks whose elements have gone away drop out of the scan
                // entirely; forget them rather than leaving a stale entry that
                // no later iteration can clear.
                failed.retain(|block_id| snapshot.iter().any(|h| &h.block_id == block_id));
                strikes.retain(|block_id, _| snapshot.iter().any(|h| &h.block_id == block_id));

                for health in &snapshot {
                    let was_failed = failed.contains(&health.block_id);
                    match (health.status.is_failed(), was_failed) {
                        (true, false) => {
                            failed.insert(health.block_id.clone());
                            tracing::error!(
                                "Block '{}' in flow '{}' is not passing data: {}. The pipeline still reports Playing",
                                health.block_id,
                                flow_name,
                                health.detail.as_deref().unwrap_or("no detail")
                            );
                            events.broadcast(strom_types::StromEvent::BlockHealthChanged {
                                flow_id,
                                block_id: health.block_id.clone(),
                                status: health.status,
                                detail: health.detail.clone(),
                            });
                        }
                        (false, true) => {
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
                            });
                        }
                        _ => {}
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

    /// `videoconvert:src` and `audioconvert:sink` have no format in common, so
    /// a link between them is refused, now and on every retry.
    fn unlinkable_pair() -> (HashMap<String, gst::Element>, Link) {
        gst::init().unwrap();
        let elements: HashMap<String, gst::Element> = [
            ("vconv", "videoconvert"),
            ("aconv", "audioconvert"),
            ("holder", "fakesink"),
        ]
        .into_iter()
        .map(|(id, factory)| {
            (
                id.to_string(),
                gst::ElementFactory::make(factory)
                    .name(id)
                    .build()
                    .expect("element should build"),
            )
        })
        .collect();
        let link = Link {
            from: "vconv:src".to_string(),
            to: "aconv:sink".to_string(),
        };
        (elements, link)
    }

    fn failure_detail(health: &[BlockHealth], block_id: &str) -> Option<String> {
        health
            .iter()
            .find(|h| h.block_id == block_id && h.status == BlockHealthStatus::Failed)
            .and_then(|h| h.detail.clone())
    }

    fn link(from: &str, to: &str) -> Link {
        Link {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    #[test]
    fn an_unformed_link_fails_the_block_owning_its_source() {
        let (elements, link) = unlinkable_pair();

        assert!(
            scan_block_health(&elements, &[])
                .iter()
                .all(|h| h.status == BlockHealthStatus::Ok),
            "nothing is stalled, so the pad-task scan must find nothing"
        );

        let detail = failure_detail(&scan_block_health(&elements, &[link]), "vconv")
            .expect("the unformed link must fail the block owning its source");
        assert!(
            detail.contains("vconv:src -> aconv:sink"),
            "the detail must name the link: {}",
            detail
        );
    }

    #[test]
    fn the_first_unformed_link_names_the_block() {
        gst::init().unwrap();
        let elements: HashMap<String, gst::Element> = ["mix:dist", "mix:mv"]
            .into_iter()
            .map(|id| {
                (
                    id.to_string(),
                    gst::ElementFactory::make("identity")
                        .build()
                        .expect("identity should build"),
                )
            })
            .collect();
        let unformed = [
            link("mix:dist:src", "out:sink"),
            link("mix:mv:src", "mv_out:sink"),
        ];

        let detail = failure_detail(&scan_block_health(&elements, &unformed), "mix")
            .expect("the block must be reported");
        assert!(
            detail.contains("mix:dist:src -> out:sink"),
            "the first unformed link must name the block: {}",
            detail
        );
    }

    /// Reporting a pad that has not appeared yet would mark every `decodebin`
    /// failed for as long as it takes to typefind.
    #[test]
    fn only_a_source_pad_that_exists_or_can_be_requested_is_final() {
        gst::init().unwrap();
        let make = |factory: &str| gst::ElementFactory::make(factory).build().unwrap();

        assert!(PipelineManager::source_pad_is_available(
            &make("videoconvert"),
            "src"
        ));
        assert!(
            PipelineManager::source_pad_is_available(&make("tee"), "src_1"),
            "a request pad that was refused is released again, and nothing retries it"
        );
        assert!(
            !PipelineManager::source_pad_is_available(&make("decodebin"), "src_0"),
            "decodebin's src pads appear once it has typefound"
        );
        assert!(
            !PipelineManager::source_pad_is_available(&make("videoconvert"), "sink"),
            "a sink pad is not a source pad"
        );
    }

    /// A link asking for `src` is tried against every `src_N` a demuxer adds.
    #[test]
    fn one_linked_pad_forms_the_link() {
        let links = UnformedLinks::default();
        let declared = link("demux:src", "vconv:sink");

        links.record(&declared, "src_0", false);
        assert_eq!(links.unformed().len(), 1);

        links.record(&declared, "src_1", true);
        assert!(
            links.unformed().is_empty(),
            "the video stream linked, so the audio stream's refusal is not a failure"
        );
    }

    /// A publisher that reconnects re-adds the same pad; its latest attempt counts.
    #[test]
    fn a_pad_refused_after_it_once_linked_is_unformed() {
        let links = UnformedLinks::default();
        let declared = link("whip:src_0", "vconv:sink");

        links.record(&declared, "src_0", true);
        links.record(&declared, "src_0", false);
        assert_eq!(links.unformed().len(), 1);
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

    /// A live chain that keeps the pipeline running, plus a declared link between
    /// two elements with no format in common.
    ///
    /// Nothing feeds the unlinkable pair, so no pad task parks and the pad-task
    /// scan has nothing to find - the same blind spot a real flow presents when
    /// a block's `allow-not-linked` output tee absorbs the `not-linked` from
    /// below an unformed link.
    fn unformed_link_flow() -> Flow {
        let mut flow = Flow::new("unformed link health test");
        flow.elements = vec![
            element(
                "src",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element("sink", "fakesink", &[("sync", PropertyValue::Bool(false))]),
            element("vconv", "videoconvert", &[]),
            element("aconv", "audioconvert", &[]),
        ];
        flow.links = vec![
            Link {
                from: "src".to_string(),
                to: "sink".to_string(),
            },
            Link {
                from: "vconv:src".to_string(),
                to: "aconv:sink".to_string(),
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

    /// A link that construction could not make and nothing retried. The branch
    /// below it carries no data, and with no pad task to pause there is nothing
    /// for the stalled-pad scan to find.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_that_never_formed_is_reported_on_a_playing_pipeline() {
        gst::init().unwrap();

        let mut manager = PipelineManager::new(
            &unformed_link_flow(),
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
                || failure_detail(&manager.get_block_health(), "vconv").is_some(),
                HEALTH_POLL_INTERVAL * (CONFIRMATIONS_BEFORE_FAILED + 4),
            ),
            "the unformed link was not reported: {:?}",
            manager.get_block_health()
        );

        let health = manager.get_block_health();
        let detail = failure_detail(&health, "vconv").unwrap();
        assert!(
            detail.contains("vconv:src -> aconv:sink"),
            "the detail must name the link: {}",
            detail
        );
        // Nothing is stalled: the pad-task scan contributes no failure here, so
        // removing the unformed-link check leaves this flow reading healthy.
        assert!(
            health
                .iter()
                .filter(|h| h.status == BlockHealthStatus::Failed)
                .all(|h| h.block_id == "vconv"),
            "only the block owning the unformed link should fail: {:?}",
            health
        );
        assert_eq!(manager.get_state(), strom_types::PipelineState::Playing);

        manager.stop().expect("pipeline should stop");
    }

    /// Play `flow` until some block is reported failed or the report window
    /// closes, and return what the health scan last said. `inspect` sees the
    /// running pipeline before it stops.
    fn health_after_playing(
        flow: &Flow,
        inspect: impl FnOnce(&PipelineManager),
    ) -> Vec<BlockHealth> {
        gst::init().unwrap();
        let mut manager = PipelineManager::new(
            flow,
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
        wait_for(
            || {
                manager
                    .get_block_health()
                    .iter()
                    .any(|h| h.status.is_failed())
            },
            HEALTH_POLL_INTERVAL * (CONFIRMATIONS_BEFORE_FAILED + 4),
        );
        let health = manager.get_block_health();
        inspect(&manager);
        manager.stop().expect("pipeline should stop");
        health
    }

    fn failed_blocks(health: &[BlockHealth]) -> Vec<&str> {
        health
            .iter()
            .filter(|h| h.status.is_failed())
            .map(|h| h.block_id.as_str())
            .collect()
    }

    /// `vconv:src` feeds a fakesink and an audioconvert, so construction puts an
    /// auto-tee on it and the refused link starts from one of the tee's pads.
    fn fan_out_flow(refused_to: &str) -> Flow {
        let mut flow = Flow::new("fan-out unformed link health test");
        flow.elements = vec![
            element(
                "src",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element("vconv", "videoconvert", &[]),
            element("vsink", "fakesink", &[("sync", PropertyValue::Bool(false))]),
            element("aconv", "audioconvert", &[]),
            element(
                "asink",
                "fakesink",
                &[
                    ("sync", PropertyValue::Bool(false)),
                    ("async", PropertyValue::Bool(false)),
                ],
            ),
        ];
        flow.links = vec![
            link("src", "vconv"),
            link("vconv:src", "vsink:sink"),
            link("vconv:src", refused_to),
            link("aconv", "asink"),
        ];
        flow
    }

    /// The failure must land on a block in the flow and name the link the flow
    /// declares, not the auto-tee construction inserted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_branch_of_a_fan_out_is_reported_against_its_source() {
        let health = health_after_playing(&fan_out_flow("aconv:sink"), |_| {});
        assert_eq!(failed_blocks(&health), ["vconv"], "{:?}", health);
        let detail = failure_detail(&health, "vconv").unwrap();
        assert!(
            detail.contains("vconv:src -> aconv:sink"),
            "the detail must name the declared link: {}",
            detail
        );
    }

    /// Linking to an element with no pad named requests a pad on the tee and
    /// releases it again when the link is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_fan_out_branch_naming_no_sink_pad_is_reported() {
        let health = health_after_playing(&fan_out_flow("aconv"), |_| {});
        assert_eq!(failed_blocks(&health), ["vconv"], "{:?}", health);
    }

    /// `decodebin` passes raw video through on a pad it adds once it has
    /// typefound, after construction has finished.
    fn late_pad_flow(to: &str) -> Flow {
        let mut flow = Flow::new("late pad health test");
        flow.elements = vec![
            element(
                "src",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element("dec", "decodebin", &[]),
            element(
                "vsink",
                "fakesink",
                &[
                    ("sync", PropertyValue::Bool(false)),
                    ("async", PropertyValue::Bool(false)),
                ],
            ),
            element("aconv", "audioconvert", &[]),
            element(
                "asink",
                "fakesink",
                &[
                    ("sync", PropertyValue::Bool(false)),
                    ("async", PropertyValue::Bool(false)),
                ],
            ),
        ];
        flow.links = vec![
            link("src", "dec"),
            link("dec:src", to),
            link("aconv", "asink"),
        ];
        flow
    }

    /// The `pad-added` handler gives a pad it cannot link an auto-tee, so the
    /// pad has a peer and only the handler's record shows the refusal.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_pad_that_cannot_link_is_reported() {
        let health = health_after_playing(&late_pad_flow("aconv:sink"), |_| {});
        assert_eq!(failed_blocks(&health), ["dec"], "{:?}", health);
        let detail = failure_detail(&health, "dec").unwrap();
        assert!(
            detail.contains("dec:src -> aconv:sink"),
            "the detail must name the declared link: {}",
            detail
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_pad_that_links_is_not_reported() {
        let health = health_after_playing(&late_pad_flow("vsink:sink"), |manager| {
            let vsink = manager.elements.get("vsink").unwrap();
            assert!(
                vsink.static_pad("sink").unwrap().is_linked(),
                "decodebin's late pad never linked, so this test proves nothing"
            );
        });
        assert!(
            failed_blocks(&health).is_empty(),
            "a late pad that linked must read as healthy: {:?}",
            health
        );
    }

    /// `decodebin` unpacks a stream with audio and video into two late pads.
    /// The flow's one link from `dec:src` is tried against both: the audio pad
    /// is refused by `videoconvert`, the video pad links.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_pad_refused_beside_one_that_linked_is_not_reported() {
        let mut flow = Flow::new("demuxed late pads health test");
        flow.elements = vec![
            element(
                "vsrc",
                "videotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element(
                "asrc",
                "audiotestsrc",
                &[("is-live", PropertyValue::Bool(true))],
            ),
            element("mux", "matroskamux", &[]),
            element("dec", "decodebin", &[]),
            element("vconv", "videoconvert", &[]),
            element("vsink", "fakesink", &[("sync", PropertyValue::Bool(false))]),
        ];
        flow.links = vec![
            link("vsrc:src", "mux:video_0"),
            link("asrc:src", "mux:audio_0"),
            link("mux", "dec"),
            link("dec:src", "vconv:sink"),
            link("vconv", "vsink"),
        ];

        let health = health_after_playing(&flow, |manager| {
            let vsink = manager.elements.get("vsink").unwrap();
            let vconv_linked = manager
                .elements
                .get("vconv")
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .is_linked();
            assert!(
                vconv_linked && vsink.static_pad("sink").unwrap().is_linked(),
                "the video pad never linked, so this test proves nothing"
            );
            let dec_pads = manager.elements.get("dec").unwrap().src_pads().len();
            assert_eq!(
                dec_pads, 2,
                "decodebin must add an audio and a video pad for the refusal to happen"
            );
        });
        assert!(
            failed_blocks(&health).is_empty(),
            "the audio pad's refusal must not fail a link the video pad formed: {:?}",
            health
        );
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
