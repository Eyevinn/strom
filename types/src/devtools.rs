//! Types for the Chromium DevTools proxy that fronts `cefsrc` browsers.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// One browser page rendered by a `cefsrc` element.
///
/// A CEF process hosts every `cefsrc` in the instance, so the targets of all
/// HTML sources arrive in one list. The `id` is Chromium's, and it changes
/// whenever the page is recreated — it identifies a live page, not a block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DevToolsTarget {
    /// Chromium's target id for this page.
    pub id: String,
    /// The page's current title, as the document reports it.
    pub title: String,
    /// The URL the page is currently showing.
    pub url: String,
}

/// A minted remote control link.
///
/// The path is the whole credential: it names no target, carries no API token
/// and spells out no address or port, so handing it over hands over exactly
/// what it says and nothing more. It dies on its own if it goes unused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DevToolsLink {
    /// Path to open in a browser, relative to this Strom instance.
    pub path: String,
    /// How long the link survives without being used. Each use starts it over.
    pub expires_in_seconds: u64,
    /// What the holder of this link can actually reach. See
    /// [`REMOTE_CONTROL_WARNING`].
    pub warning: String,
}

/// Every page currently rendered by a `cefsrc` element.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DevToolsTargets {
    /// Whether the instance has a Chromium remote debugging port configured.
    /// When false the list is empty and no target can be opened.
    pub enabled: bool,
    /// What a client must tell the operator before handing them a link, in
    /// Strom's words rather than each client's own. Present whenever remote
    /// control is enabled. See [`REMOTE_CONTROL_WARNING`].
    pub warning: Option<String>,
    /// The pages available for remote control.
    pub targets: Vec<DevToolsTarget>,
}

/// What remote control actually grants, for any client that offers it.
///
/// One CEF process serves every `cefsrc` in a Strom instance, so a session
/// opened against one HTML source is not confined to it. Isolating customers
/// from each other is a matter of running a Strom process per customer, not
/// something this API can do.
pub const REMOTE_CONTROL_WARNING: &str = "Remote control is a debugging tool. \
    One browser process serves every HTML source in this Strom instance, so \
    whoever opens this link reaches all of them, every page they are logged in \
    to, and the files on the host. Give it only to someone you would trust with \
    the instance itself. To keep customers apart, run a Strom process per \
    customer.";
