//! Remote control links for HTML sources.
//!
//! Minting a link is a round trip to the server, and what happens with the
//! result depends on which button was pressed: a QR code to point a phone at,
//! or a tab opened straight away. The request is spawned and its answer picked
//! up on a later frame, the same way block thumbnails are.

use super::{get_local_storage, remove_local_storage, set_local_storage, spawn_task};
use egui::Context;
use strom_types::FlowId;

/// What the operator asked for when they asked for a link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LinkPurpose {
    /// Show the link as a QR code next to the block.
    Qr,
    /// Open the link in a new tab straight away.
    Open,
}

fn storage_key(flow_id: FlowId, block_id: &str) -> String {
    format!("devtools_link_{}_{}", flow_id, block_id)
}

impl super::StromApp {
    /// Ask the server for a link to one HTML source.
    pub(super) fn request_devtools_link(
        &mut self,
        ctx: &Context,
        flow_id: FlowId,
        block_id: String,
        purpose: LinkPurpose,
    ) {
        let key = storage_key(flow_id, &block_id);
        if self.devtools_link_pending.contains(&key) {
            return;
        }
        self.devtools_link_pending.insert(key.clone());

        let api = self.api.clone();
        let ctx = ctx.clone();
        let flow = flow_id.to_string();
        let block = block_id.clone();
        let purpose_tag = match purpose {
            LinkPurpose::Qr => "qr",
            LinkPurpose::Open => "open",
        };

        spawn_task(async move {
            match api.create_block_devtools_link(&flow, &block).await {
                Ok(link) => set_local_storage(&key, &format!("{}|{}", purpose_tag, link.path)),
                // The server's message says what to do about it — whether the
                // flow is stopped, the property is off, or two blocks share a
                // URL — so carry it through rather than inventing one.
                Err(e) => set_local_storage(&format!("{}_err", key), &e.to_string()),
            }
            ctx.request_repaint();
        });
    }

    /// Pick up links the server has handed back.
    pub(super) fn check_devtools_links(&mut self, ctx: &Context) {
        let pending: Vec<String> = self.devtools_link_pending.iter().cloned().collect();

        for key in pending {
            let err_key = format!("{}_err", key);
            if let Some(message) = get_local_storage(&err_key) {
                remove_local_storage(&err_key);
                self.devtools_link_pending.remove(&key);
                self.status = format!("Remote control link: {}", message);
                continue;
            }

            let Some(value) = get_local_storage(&key) else {
                continue;
            };
            remove_local_storage(&key);
            self.devtools_link_pending.remove(&key);

            let Some((purpose, path)) = value.split_once('|') else {
                continue;
            };
            // The block id is the tail of the storage key, after the flow id.
            let block_id = key
                .rsplit_once('_')
                .map(|(_, b)| b.to_string())
                .unwrap_or_default();

            let server_hostname = self.system_info.as_ref().map(|s| s.hostname.as_str());
            // base_url ends in /api; the link is served from the server root.
            let server_base = self.api.base_url().trim_end_matches("/api").to_string();
            let url =
                super::make_external_url(&format!("{}{}", server_base, path), server_hostname);

            match purpose {
                "open" => ctx.open_url(egui::OpenUrl::new_tab(&url)),
                _ => self.qr_inline = Some((block_id, url)),
            }
        }
    }
}
