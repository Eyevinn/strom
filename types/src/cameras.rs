//! Phone cameras (TallyCam): what the camera API exposes.
//!
//! The phones speak the TallyCam control protocol to Strom's camera hub. These
//! are Strom's own views of them, mapped from the hub in the backend, so the
//! API does not depend on the protocol crate.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

use crate::flow::FlowId;

/// Block property of `builtin.mpegtssrt_input` that binds a phone camera
/// (its device id) to the block. Empty means no camera.
pub const CAMERA_PROPERTY: &str = "camera";

/// Tally light of a camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CameraTally {
    #[default]
    Off,
    Preview,
    Program,
}

/// Which SRT input block a camera is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CameraBinding {
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    pub block_id: String,
    /// Whether the flow is running.
    pub running: bool,
}

/// One known camera.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CameraSummary {
    /// Device id: derived from the phone's device key (stable for the app
    /// install), or the phone's own id for an app without device keys.
    pub id: String,
    pub name: String,
    pub model: String,
    pub platform: String,
    pub app_version: String,
    /// Connected to Strom now.
    pub online: bool,
    pub tally: CameraTally,
    /// SRT input blocks this camera is bound to (the `camera` property).
    pub bindings: Vec<CameraBinding>,
    /// Battery level 0..1, from the latest status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_level: Option<f64>,
    /// Stream state from the latest status: `idle`, `connecting`, `live` or `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_state: Option<String>,
    /// SHA-256 of the phone's device key (hex). The phone shows the start of
    /// it in its settings, to tell cameras apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
}

/// `GET /api/cameras`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CameraListResponse {
    pub cameras: Vec<CameraSummary>,
}

/// `POST /api/cameras/invite`: a one-time provisioning link for a new phone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CameraInviteResponse {
    /// `tallycam://connect?control=...`, what the QR code carries.
    pub deep_link: String,
    /// The device WebSocket URL with the invite token.
    pub control_url: String,
    /// The deep link as a QR code (SVG).
    pub qr_svg: String,
    /// The token admits one new phone and expires after this many seconds.
    pub expires_in_secs: u64,
}
