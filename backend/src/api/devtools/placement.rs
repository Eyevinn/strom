//! Which Chromium page belongs to which HTML source.
//!
//! Chromium knows a page by its target id and its current URL, and nothing
//! about blocks. Strom knows which blocks are rendering which URL. A link is
//! minted for a block, so the two have to be matched up, including after the
//! page has navigated away from the block's URL - a login redirect, say.

use super::{pages, valid_target_id, LinkSource};
use crate::state::AppState;
use std::collections::HashSet;
use strom_types::{Flow, FlowId};

/// Two URLs naming the same page. Chromium reports what it navigated to, which
/// is the operator's URL with a trailing slash added on an empty path, so a
/// literal comparison would miss a page the operator would say is theirs.
pub(super) fn same_page(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The `scheme://host:port` a URL belongs to, when it has one.
///
/// `data:` and anything else without an authority has no origin, and gets
/// `None` rather than a guess. This is deliberately textual: it is used only
/// to decide that two URLs came from the same site, never to reach anything.
pub(super) fn origin(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&url[..scheme.len() + 3 + authority_len])
}

/// Same site, whatever path either of them is on now.
pub(super) fn same_origin(a: &str, b: &str) -> bool {
    match (origin(a), origin(b)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// One page Chromium is rendering right now.
#[derive(Clone, Debug)]
pub(super) struct PageTarget {
    pub(super) id: String,
    pub(super) url: String,
}

/// One HTML source in this instance, and the page it was pointed at.
///
/// Every `cefsrc` in the instance shares the one CEF process, so working out
/// which page belongs to which block is an instance-wide question: a block in
/// another flow can be the reason this one's page is ambiguous.
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

    pub(super) fn is(&self, flow_id: &FlowId, block_id: &str) -> bool {
        self.flow_id == *flow_id && self.block_id == block_id
    }
}

/// What asking "which page is this block rendering?" can come back with.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Resolution {
    /// Exactly one page can be this block's.
    Target(String),
    /// More than one could be, so picking would be guessing.
    Ambiguous,
    /// None of the pages can be this block's.
    Unknown,
}

/// Decide which Chromium page belongs to one HTML source.
///
/// Chromium exposes a page's *current* URL and nothing else, so an exact match
/// against the block's configured URL is only right until the page navigates —
/// and the first thing a page behind a login does is redirect to the login,
/// which is exactly when an operator wants this feature. So matching widens in
/// steps, and every step keeps the rule that an answer is only given when it
/// is the only possible one:
///
/// 1. **Exact URL.** The page is still on the URL the block names.
/// 2. **Same origin.** The page redirected or was clicked through within the
///    site the block names. Pages another block claims exactly are its, not
///    ours, and another source pointed at the same site makes this a tie.
/// 3. **Sole survivor.** Exactly one source has no page and exactly one page
///    has no source. They can only be each other — this is what carries a
///    cross-origin login redirect.
///
/// Anything short of that is [`Resolution::Ambiguous`] or
/// [`Resolution::Unknown`], never a guess.
pub(super) fn resolve_target(
    targets: &[PageTarget],
    sources: &[HtmlSource],
    want: &HtmlSource,
) -> Resolution {
    // 1. Still on the URL it was given.
    let exact: Vec<&PageTarget> = targets
        .iter()
        .filter(|t| same_page(&t.url, &want.url))
        .collect();
    match exact.len() {
        1 => return Resolution::Target(exact[0].id.clone()),
        0 => {}
        _ => return Resolution::Ambiguous,
    }

    // A source with an exact match has its page; that page is not up for grabs.
    let settled = |s: &HtmlSource| targets.iter().any(|t| same_page(&t.url, &s.url));
    let claimed_exactly = |t: &PageTarget| sources.iter().any(|s| same_page(&t.url, &s.url));

    // 2. Same site, different path - a redirect or a click.
    let candidates: Vec<&PageTarget> = targets
        .iter()
        .filter(|t| !claimed_exactly(t) && same_origin(&t.url, &want.url))
        .collect();
    let rivals = sources
        .iter()
        .filter(|s| {
            !s.is(&want.flow_id, &want.block_id) && !settled(s) && same_origin(&s.url, &want.url)
        })
        .count();
    if !candidates.is_empty() && (candidates.len() > 1 || rivals > 0) {
        return Resolution::Ambiguous;
    }
    if candidates.len() == 1 {
        return Resolution::Target(candidates[0].id.clone());
    }

    // 3. One page left over, one source left over. A cross-origin login
    //    redirect lands here: the URL shares nothing with what was configured,
    //    but there is nothing else it could be.
    let spoken_for = |t: &PageTarget| {
        claimed_exactly(t)
            || sources
                .iter()
                .any(|s| !settled(s) && same_origin(&t.url, &s.url))
    };
    let unclaimed: Vec<&PageTarget> = targets.iter().filter(|t| !spoken_for(t)).collect();
    let homeless: Vec<&HtmlSource> = sources
        .iter()
        .filter(|s| !settled(s) && !targets.iter().any(|t| same_origin(&t.url, &s.url)))
        .collect();
    if unclaimed.len() == 1 && homeless.len() == 1 && homeless[0].is(&want.flow_id, &want.block_id)
    {
        return Resolution::Target(unclaimed[0].id.clone());
    }

    Resolution::Unknown
}

/// Ask Chromium which pages exist right now.
pub(super) async fn page_targets(port: u16) -> Option<Vec<PageTarget>> {
    // A popup is a page too, but it is not what any HTML source renders: it
    // belongs to the page that opened it. Counted as a page, it would look
    // like a source's page that no block can be matched to.
    let targets = pages::all_targets(port).await?;
    Some(
        targets
            .into_iter()
            .filter(|t| t.kind == "page" && t.opener.is_none() && valid_target_id(&t.id))
            .map(|t| PageTarget {
                id: t.id,
                url: t.url,
            })
            .collect(),
    )
}

/// Every HTML source in the instance, because one CEF process serves them all.
pub(super) async fn html_sources(app: &AppState) -> Vec<HtmlSource> {
    let running: HashSet<FlowId> = app.pipelines_read().await.keys().copied().collect();
    html_sources_in(app.get_flows().await, &running)
}

/// The HTML sources in the flows that are running.
///
/// A stopped flow renders nothing, so its blocks can never own a page. Left
/// in, each one looks like a source whose page has not been found yet, and
/// the "one page left over, one source left over" rule in
/// [`resolve_target`] then refuses every cross-origin redirect in the
/// instance.
pub(super) fn html_sources_in(flows: Vec<Flow>, running: &HashSet<FlowId>) -> Vec<HtmlSource> {
    flows
        .into_iter()
        .filter(|flow| running.contains(&flow.id))
        .flat_map(|flow| {
            let flow_id = flow.id;
            let flow_name = flow.name;
            flow.blocks
                .into_iter()
                .filter(|b| b.block_definition_id == crate::blocks::builtin::html_input::BLOCK_ID)
                .map(move |b| HtmlSource {
                    flow_id,
                    // A block with no `url` property still renders the block's
                    // default, and a bare address is rendered as https://, so
                    // ask the block rather than the stored map.
                    url: crate::blocks::builtin::html_input::checked_url(&b.properties)
                        .unwrap_or_else(|_| crate::blocks::builtin::html_input::url(&b.properties)),
                    remote_control: crate::blocks::builtin::html_input::remote_control_enabled(
                        &b.properties,
                    ),
                    strict: crate::blocks::builtin::html_input::strict_network(&b.properties),
                    flow_name: flow_name.clone(),
                    block_name: b.name.clone().unwrap_or_else(|| b.id.clone()),
                    block_id: b.id,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, url: &str) -> PageTarget {
        PageTarget {
            id: id.to_string(),
            url: url.to_string(),
        }
    }

    fn source(block_id: &str, url: &str) -> HtmlSource {
        HtmlSource {
            flow_id: FlowId::new_v4(),
            block_id: block_id.to_string(),
            url: url.to_string(),
            remote_control: true,
            strict: true,
            flow_name: "Flow".to_string(),
            block_name: block_id.to_string(),
        }
    }

    // --- Working out which page belongs to which block ---

    #[test]
    fn a_trailing_slash_does_not_make_it_a_different_page() {
        assert!(same_page("https://example.com", "https://example.com/"));
        assert!(same_page("file:///demo/a.html", "file:///demo/a.html"));
        assert!(!same_page("https://example.com/a", "https://example.com/b"));
    }

    #[test]
    fn an_origin_is_the_scheme_host_and_port() {
        assert_eq!(
            origin("https://example.com/a/b?c"),
            Some("https://example.com")
        );
        assert_eq!(
            origin("https://example.com:8443/a"),
            Some("https://example.com:8443")
        );
        assert_eq!(origin("https://example.com"), Some("https://example.com"));
        assert_eq!(origin("data:text/html,hi"), None);
        assert!(same_origin("https://a.example/x", "https://a.example/y"));
        assert!(!same_origin("https://a.example/x", "https://b.example/x"));
        // Without an origin there is nothing to compare, so nothing matches.
        assert!(!same_origin("data:text/html,a", "data:text/html,a"));
    }

    #[test]
    fn a_page_still_on_its_url_is_matched_exactly() {
        let targets = vec![
            target("AAAA", "https://a.example/"),
            target("BBBB", "https://b.example/"),
        ];
        let sources = vec![
            source("one", "https://a.example"),
            source("two", "https://b.example"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Target("AAAA".to_string())
        );
        assert_eq!(
            resolve_target(&targets, &sources, &sources[1]),
            Resolution::Target("BBBB".to_string())
        );
    }

    #[test]
    fn a_same_site_redirect_still_resolves() {
        // The page was pointed at /dashboard and the server sent it to /login.
        // This is the ordinary shape of the case the feature exists for.
        let targets = vec![
            target("AAAA", "https://app.example/login?next=/dashboard"),
            target("BBBB", "https://other.example/"),
        ];
        let sources = vec![
            source("one", "https://app.example/dashboard"),
            source("two", "https://other.example/"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Target("AAAA".to_string())
        );
    }

    #[test]
    fn a_cross_origin_login_redirect_resolves_when_nothing_else_could_be_it() {
        // The single HTML source in the instance was sent to an identity
        // provider on a different host. Its URL now shares nothing with what
        // was configured, and there is still only one page it can be.
        let targets = vec![target("AAAA", "https://login.idp.example/?redirect=x")];
        let sources = vec![source("one", "https://app.example/dashboard")];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Target("AAAA".to_string())
        );
    }

    #[test]
    fn a_cross_origin_redirect_with_a_second_stranded_source_is_refused() {
        // Two sources have both wandered off their configured URLs. Either
        // page could be either source's, so neither gets an answer.
        let targets = vec![
            target("AAAA", "https://login.idp.example/"),
            target("BBBB", "https://sso.other.example/"),
        ];
        let sources = vec![
            source("one", "https://app.example/dashboard"),
            source("two", "https://intranet.example/home"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Unknown
        );
        assert_eq!(
            resolve_target(&targets, &sources, &sources[1]),
            Resolution::Unknown
        );
    }

    #[test]
    fn two_blocks_on_one_url_are_refused_rather_than_guessed_between() {
        let targets = vec![
            target("AAAA", "https://a.example/"),
            target("BBBB", "https://a.example/"),
        ];
        let sources = vec![
            source("one", "https://a.example"),
            source("two", "https://a.example"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn two_blocks_on_one_site_are_refused_once_both_have_navigated() {
        // Both were pointed into the same site and both redirected, so the one
        // page left cannot be attributed. Widening the match must not turn
        // this into a guess.
        let targets = vec![target("AAAA", "https://app.example/login")];
        let sources = vec![
            source("one", "https://app.example/a"),
            source("two", "https://app.example/b"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn a_page_another_block_is_sitting_on_exactly_is_not_up_for_grabs() {
        // "two" is exactly where it said it would be, so its page is its own;
        // "one" must not be handed it just because they share a site.
        let targets = vec![target("BBBB", "https://app.example/b")];
        let sources = vec![
            source("one", "https://app.example/a"),
            source("two", "https://app.example/b"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Unknown
        );
        assert_eq!(
            resolve_target(&targets, &sources, &sources[1]),
            Resolution::Target("BBBB".to_string())
        );
    }

    #[test]
    fn a_block_whose_flow_is_not_running_gets_no_page() {
        let targets: Vec<PageTarget> = Vec::new();
        let sources = vec![source("one", "https://a.example")];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Unknown
        );
    }

    fn html_flow(name: &str, url: &str) -> Flow {
        let mut flow = Flow::new(name);
        flow.blocks.push(
            serde_json::from_value(serde_json::json!({
                "id": format!("{}-html", name),
                "block_definition_id": crate::blocks::builtin::html_input::BLOCK_ID,
                "properties": { "url": url, "remote_control": true },
                "position": { "x": 0.0, "y": 0.0 }
            }))
            .expect("a valid block"),
        );
        flow
    }

    #[test]
    fn a_stopped_flow_does_not_stop_a_redirect_from_resolving() {
        // The page asked for google.com and landed on www.google.com - a
        // different origin, so only the sole-survivor rule can place it. Two
        // stopped flows with HTML blocks used to count as sources still
        // looking for their page, and the link was refused.
        let live = html_flow("live", "google.com");
        let stopped_a = html_flow("stopped-a", "file:///tmp/tall.html");
        let stopped_b = html_flow("stopped-b", "file:///tmp/busy.html");
        let running: HashSet<FlowId> = [live.id].into_iter().collect();

        let sources = html_sources_in(vec![stopped_a, live.clone(), stopped_b], &running);
        assert_eq!(sources.len(), 1, "only the running flow renders anything");

        let targets = vec![target("AAAA", "https://www.google.com/")];
        let want = sources
            .iter()
            .find(|s| s.flow_id == live.id)
            .expect("the live block is a source");
        assert_eq!(
            resolve_target(&targets, &sources, want),
            Resolution::Target("AAAA".to_string())
        );
    }

    #[test]
    fn the_page_is_told_which_source_it_controls() {
        let mut src = source("block-id", "https://a.example/login");
        src.flow_name = "Studio A".to_string();
        src.block_name = "Scoreboard".to_string();
        let sent: serde_json::Value =
            serde_json::from_str(&src.link_source().context_message()).expect("valid JSON");
        assert_eq!(sent["method"], "Strom.context");
        assert_eq!(sent["params"]["flow"], "Studio A");
        assert_eq!(sent["params"]["block"], "Scoreboard");
        assert_eq!(sent["params"]["home"], "https://a.example/login");
    }
}
