//! Remote control links for HTML sources.
//!
//! Minting a link is a round trip to the server, and what happens with the
//! result depends on which button was pressed: a QR code to point a phone at,
//! or a tab opened straight away. The request is spawned and its answer picked
//! up on a later frame.
//!
//! The answer is a credential, so it travels in memory only. Block thumbnails
//! come back through local storage, which is on disk in a browser; a link left
//! there by a tab closed at the wrong moment would be a working key.

use super::spawn_task;
use egui::Context;
use std::sync::{Arc, Mutex};
use strom_types::devtools::DevToolsLink;
use strom_types::FlowId;

/// What the operator asked for when they asked for a link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LinkPurpose {
    /// Show the link as a QR code next to the block.
    Qr,
    /// Open the link in a new tab straight away.
    Open,
}

/// The server's answer to one request, waiting for the next frame.
pub struct LinkAnswer {
    flow_id: FlowId,
    block_id: String,
    purpose: LinkPurpose,
    /// The link, or the server's message saying why there is none.
    result: Result<DevToolsLink, String>,
}

/// Where spawned requests leave their answers.
pub type LinkInbox = Arc<Mutex<Vec<LinkAnswer>>>;

impl super::StromApp {
    /// Ask the server for a link to one HTML source.
    pub(super) fn request_devtools_link(
        &mut self,
        ctx: &Context,
        flow_id: FlowId,
        block_id: String,
        purpose: LinkPurpose,
    ) {
        let pending = (flow_id, block_id.clone());
        if self.devtools_link_pending.contains(&pending) {
            return;
        }
        self.devtools_link_pending.insert(pending);

        let api = self.api.clone();
        let ctx = ctx.clone();
        let inbox = self.devtools_link_inbox.clone();
        let flow = flow_id.to_string();

        spawn_task(async move {
            // The server's message says what to do about a refusal - the flow
            // is stopped, the property is off - so carry it through rather
            // than inventing one.
            let result = api
                .create_block_devtools_link(&flow, &block_id)
                .await
                .map_err(|e| e.to_string());
            if let Ok(mut inbox) = inbox.lock() {
                inbox.push(LinkAnswer {
                    flow_id,
                    block_id,
                    purpose,
                    result,
                });
            }
            ctx.request_repaint();
        });
    }

    /// Pick up links the server has handed back.
    pub(super) fn check_devtools_links(&mut self, ctx: &Context) {
        let answers: Vec<LinkAnswer> = match self.devtools_link_inbox.lock() {
            Ok(mut inbox) => std::mem::take(&mut *inbox),
            Err(_) => return,
        };

        for answer in answers {
            self.devtools_link_pending
                .remove(&(answer.flow_id, answer.block_id.clone()));
            let link = match answer.result {
                Ok(link) => link,
                Err(message) => {
                    self.status = format!("Remote control link: {}", message);
                    continue;
                }
            };

            // What the link hands over, in the server's words, shown next to
            // the block for as long as the link is.
            self.devtools_link_warning = Some((answer.block_id.clone(), link.warning));

            // base_url ends in /api; the link is served from the server root.
            let server_base = self.api.base_url().trim_end_matches("/api").to_string();
            let url = format!("{}{}", server_base, link.path);

            match answer.purpose {
                // Opened in this browser, so the address it already reaches
                // the server on is the right one. Rewriting it to the server's
                // hostname breaks wherever that name does not resolve here,
                // such as a container id.
                LinkPurpose::Open => ctx.open_url(egui::OpenUrl::new_tab(&url)),
                // A QR code is for another device, where localhost is wrong.
                LinkPurpose::Qr => {
                    let server_hostname = self.system_info.as_ref().map(|s| s.hostname.as_str());
                    let url = super::make_external_url(&url, server_hostname);
                    self.qr_inline = Some((answer.block_id, url));
                }
            }
        }
    }
}
