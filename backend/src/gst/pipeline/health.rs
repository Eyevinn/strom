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
//! A paused task on an element that is itself `PLAYING` is always wrong, so
//! that is the signal used here. `gst_pad_get_task_state()` returns `Stopped`
//! both for a pad whose task was stopped and for a pad that never had one, so
//! `Stopped` cannot be told apart from the common case and is not reported.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{BTreeMap, HashMap};
use strom_types::flow::{BlockHealth, BlockHealthStatus};

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
/// Only elements that are themselves playing are considered. A sub-bin held in
/// `PAUSED` inside a playing pipeline - a WebRTC session bin mid-setup, say -
/// has paused pad tasks legitimately.
fn stalled_pad_on(element: &gst::Element) -> Option<StalledPad> {
    if element.current_state() != gst::State::Playing {
        return None;
    }
    element
        .pads()
        .into_iter()
        .find(|pad| pad.task_state() == gst::TaskState::Paused)
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
pub(crate) fn scan_block_health(elements: &HashMap<String, gst::Element>) -> Vec<BlockHealth> {
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
                "pad task stopped on {}:{} - this branch is not passing data",
                stalled.element, stalled.pad
            ));
        }
    }

    by_block.into_values().collect()
}

/// How often the running pipeline is scanned for stalled pad tasks.
const HEALTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

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

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEALTH_POLL_INTERVAL);
            let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();

            loop {
                interval.tick().await;

                // Only meaningful while playing - a paused task is expected in
                // every other state, including during startup and shutdown.
                if *cached_state.read().unwrap() != strom_types::PipelineState::Playing {
                    if !failed.is_empty() {
                        failed.clear();
                    }
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

                let snapshot = scan_block_health(&elements);

                for health in &snapshot {
                    let was_failed = failed.contains(&health.block_id);
                    match (health.status.is_failed(), was_failed) {
                        (true, false) => {
                            failed.insert(health.block_id.clone());
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
