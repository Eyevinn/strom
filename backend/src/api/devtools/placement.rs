//! Which Chromium page belongs to which HTML source.
//!
//! Every page is named when it is born (see [`crate::cef_pages`]), so this is
//! a lookup, never a guess: two blocks on the same URL, or a page that has
//! logged in and redirected anywhere, are told apart by the target id their
//! page was born with.

use super::{pages, valid_target_id, LinkSource};
use crate::cef_pages::{self, PageOwner};
use crate::state::AppState;
use std::collections::HashSet;
use strom_types::{Flow, FlowId};

/// One page Chromium is rendering right now.
#[derive(Clone, Debug)]
pub(super) struct PageTarget {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) url: String,
}

/// One HTML Input block in a running flow, and the page it was pointed at.
#[derive(Clone, Debug)]
pub(super) struct HtmlSource {
    pub(super) flow_id: FlowId,
    pub(super) block_id: String,
    /// The page the operator pointed the block at, not necessarily the one it
    /// is showing now.
    pub(super) url: String,
    pub(super) remote_control: bool,
    /// Strict Network Access, carried into the link's filter.
    pub(super) strict: bool,
    pub(super) flow_name: String,
    /// The block's own name, or its id when it has none.
    pub(super) block_name: String,
}

impl HtmlSource {
    pub(super) fn link_source(&self) -> LinkSource {
        LinkSource {
            flow_id: self.flow_id,
            block_id: self.block_id.clone(),
            home_url: self.url.clone(),
            flow_name: self.flow_name.clone(),
            block_name: self.block_name.clone(),
            strict: self.strict,
        }
    }

    fn owner(&self) -> PageOwner {
        PageOwner::Block {
            flow_id: self.flow_id,
            block_id: self.block_id.clone(),
        }
    }
}

/// The page `source` is rendering, if it has been born and is still open.
///
/// A recorded target that Chromium no longer lists is a page that has since
/// closed; the source's next page replaces the record when it is born.
pub(super) fn page_of(source: &HtmlSource, targets: &[PageTarget]) -> Option<PageTarget> {
    let id = cef_pages::target_of(&source.owner())?;
    targets.iter().find(|t| t.id == id).cloned()
}

/// Ask Chromium which pages exist right now.
pub(super) async fn page_targets(port: u16) -> Option<Vec<PageTarget>> {
    // A popup is a page too, but it is not what any HTML source renders: it
    // belongs to the page that opened it, and a session follows it there.
    let targets = pages::all_targets(port).await?;
    Some(
        targets
            .into_iter()
            .filter(|t| t.kind == "page" && t.opener.is_none() && valid_target_id(&t.id))
            .map(|t| PageTarget {
                id: t.id,
                title: t.title,
                url: t.url,
            })
            .collect(),
    )
}

/// The HTML Input block `block_id` of flow `flow_id`, if that flow is running.
pub(super) async fn running_source(
    app: &AppState,
    flow_id: &FlowId,
    block_id: &str,
) -> Option<HtmlSource> {
    let running: HashSet<FlowId> = app.pipelines_read().await.keys().copied().collect();
    let flow = app.get_flow(flow_id).await?;
    source_in(flow, block_id, &running)
}

/// The source that page `target_id` was born for, if it is an HTML Input
/// block in a running flow. A raw `cefsrc` element's page has an owner too,
/// but no Remote Control switch, so it is never one a link can be minted for.
pub(super) async fn source_of_page(app: &AppState, target_id: &str) -> Option<HtmlSource> {
    match cef_pages::owner_of(target_id)? {
        PageOwner::Block { flow_id, block_id } => running_source(app, &flow_id, &block_id).await,
        PageOwner::Element { .. } => None,
    }
}

fn source_in(flow: Flow, block_id: &str, running: &HashSet<FlowId>) -> Option<HtmlSource> {
    // A stopped flow renders nothing, so a page recorded for it is gone.
    if !running.contains(&flow.id) {
        return None;
    }
    let block = flow.blocks.into_iter().find(|b| {
        b.id == block_id && b.block_definition_id == crate::blocks::builtin::html_input::BLOCK_ID
    })?;
    Some(HtmlSource {
        flow_id: flow.id,
        // A block with no `url` property still renders the block's default,
        // and a bare address is rendered as https://, so ask the block rather
        // than the stored map.
        url: crate::blocks::builtin::html_input::checked_url(&block.properties)
            .unwrap_or_else(|_| crate::blocks::builtin::html_input::url(&block.properties)),
        remote_control: crate::blocks::builtin::html_input::remote_control_enabled(
            &block.properties,
        ),
        strict: crate::blocks::builtin::html_input::strict_network(&block.properties),
        flow_name: flow.name,
        block_name: block.name.clone().unwrap_or_else(|| block.id.clone()),
        block_id: block.id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html_flow(name: &str, block_ids: &[&str]) -> Flow {
        let mut flow = Flow::new(name);
        for id in block_ids {
            flow.blocks.push(
                serde_json::from_value(serde_json::json!({
                    "id": id,
                    "block_definition_id": crate::blocks::builtin::html_input::BLOCK_ID,
                    "properties": { "remote_control": true },
                    "position": { "x": 0.0, "y": 0.0 }
                }))
                .expect("a valid block"),
            );
        }
        flow
    }

    fn target(id: &str, url: &str) -> PageTarget {
        PageTarget {
            id: id.to_string(),
            title: String::new(),
            url: url.to_string(),
        }
    }

    fn born(source: &HtmlSource, target_id: &str) {
        crate::cef_pages::record_for_test(source.owner(), target_id.to_string());
    }

    #[test]
    fn two_blocks_on_the_same_url_each_get_their_own_page() {
        // Both on the block's default URL, and both pages still on it.
        let flow = html_flow("Studio", &["a", "b"]);
        let running: HashSet<FlowId> = [flow.id].into_iter().collect();
        let a = source_in(flow.clone(), "a", &running).unwrap();
        let b = source_in(flow, "b", &running).unwrap();
        assert_eq!(a.url, b.url);
        born(&a, "A1A1");
        born(&b, "B1B1");
        let targets = [target("A1A1", &a.url), target("B1B1", &b.url)];
        assert_eq!(page_of(&a, &targets).unwrap().id, "A1A1");
        assert_eq!(page_of(&b, &targets).unwrap().id, "B1B1");
    }

    #[test]
    fn a_page_is_found_wherever_it_has_navigated() {
        let flow = html_flow("Studio", &["a"]);
        let running: HashSet<FlowId> = [flow.id].into_iter().collect();
        let a = source_in(flow, "a", &running).unwrap();
        born(&a, "A2A2");
        let targets = [target("A2A2", "https://login.example.com/oauth?x=1")];
        assert_eq!(page_of(&a, &targets).unwrap().id, "A2A2");
    }

    #[test]
    fn a_page_that_has_closed_is_not_handed_out() {
        let flow = html_flow("Studio", &["a"]);
        let running: HashSet<FlowId> = [flow.id].into_iter().collect();
        let a = source_in(flow, "a", &running).unwrap();
        born(&a, "A3A3");
        assert!(page_of(&a, &[target("OTHER", "https://example.com/")]).is_none());
    }

    #[test]
    fn a_block_whose_flow_is_not_running_is_no_source() {
        let flow = html_flow("Studio", &["a"]);
        assert!(source_in(flow, "a", &HashSet::new()).is_none());
    }

    #[test]
    fn the_page_is_told_which_source_it_controls() {
        let mut flow = html_flow("Studio A", &["block-id"]);
        flow.blocks[0].name = Some("Scoreboard".to_string());
        flow.blocks[0].properties.insert(
            "url".to_string(),
            strom_types::PropertyValue::String("https://a.example/login".to_string()),
        );
        let running: HashSet<FlowId> = [flow.id].into_iter().collect();
        let src = source_in(flow, "block-id", &running).unwrap();
        let sent: serde_json::Value =
            serde_json::from_str(&src.link_source().context_message()).expect("valid JSON");
        assert_eq!(sent["method"], "Strom.context");
        assert_eq!(sent["params"]["source"], "Scoreboard");
        assert!(
            sent["params"].get("flow").is_none(),
            "the flow's name stays with this instance"
        );
        assert_eq!(sent["params"]["home"], "https://a.example/login");
    }
}
