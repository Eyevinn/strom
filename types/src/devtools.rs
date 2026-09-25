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
    /// Path on this Strom instance that opens DevTools against this page.
    /// Relative, so it works whatever host or scheme the client reached us on.
    pub open_path: String,
}

/// Every page currently rendered by a `cefsrc` element.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DevToolsTargets {
    /// Whether the instance has a Chromium remote debugging port configured.
    /// When false the list is empty and no target can be opened.
    pub enabled: bool,
    /// The pages available for remote control.
    pub targets: Vec<DevToolsTarget>,
}
