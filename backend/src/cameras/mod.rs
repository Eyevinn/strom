//! Phone cameras (TallyCam) in Strom.
//!
//! Strom embeds the `tallycam-hub` library: phones connect to
//! `/api/cameras/device` with the TallyCam control protocol, operator UIs to
//! `/api/cameras/control`. A phone is put in a flow through the `camera`
//! property of an MPEG-TS/SRT input block:
//!
//! - when the flow starts, the block becomes an SRT listener with a fresh
//!   passphrase ([`Cameras::prepare_flow`]), and the phone is told where to
//!   send (`stream_url`) and to go live;
//! - a bound phone that connects while its flow runs is put live too;
//! - when the flow stops, the phone stops streaming and its tally goes off;
//! - while the flow runs, its vision mixers drive the phone's tally
//!   ([`tally`]).
//!
//! The running side (event loop, pushes, tally) is in `state/cameras.rs`,
//! next to the flow state it reads.

pub mod store;
pub mod tally;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use strom_types::cameras::{
    CameraBinding, CameraInviteResponse, CameraSummary, CameraTally, CAMERA_PROPERTY,
};
use strom_types::{Flow, FlowId, PropertyValue};
use tallycam_hub::hub::DEFAULT_INVITE_TTL;
use tallycam_hub::protocol::{DeviceSnapshot, TallyState};
use tallycam_hub::provision::{deep_link_control, qr_svg, with_invite_token};
use tallycam_hub::{Hub, Provisioning};
use tracing::info;

use store::CameraStore;

/// The SRT input block that has a camera mode.
pub const SRT_INPUT_BLOCK: &str = "builtin.mpegtssrt_input";
/// Where phones connect.
pub const DEVICE_PATH: &str = "/api/cameras/device";
/// Where operator UIs (LCP controllers) connect.
pub const CONTROL_PATH: &str = "/api/cameras/control";
/// A binding prepared for a flow that never reported started is dropped
/// after this long (the start failed).
const PREPARED_TIMEOUT: Duration = Duration::from_secs(60);

/// Where phones reach this Strom. See `[cameras]` in the configuration.
#[derive(Debug, Clone, Default)]
pub struct CameraSettings {
    /// Host name or address (no port) phones use for this Strom, in SRT
    /// URLs and invites. Unset: the host the phone or the operator's browser
    /// used, or the first LAN address when that is loopback.
    pub public_host: Option<String>,
    /// The full device WebSocket URL to put in invites, when a proxy sits in
    /// front (`wss://strom.example.com/api/cameras/device`).
    pub device_url: Option<String>,
    /// Strom's HTTP port, for invite URLs built from a host.
    pub http_port: u16,
    /// Strom serves HTTPS, so invites use `wss://`.
    pub tls: bool,
}

/// A camera-bound SRT input of a starting or running flow.
#[derive(Debug, Clone)]
pub struct ActiveBinding {
    pub device: String,
    /// The SRT listener port of the block.
    pub port: u16,
    /// The block's SRT latency, ms.
    pub latency_ms: i64,
    /// Fresh for every flow start.
    pub passphrase: String,
    pub stream_id: String,
    /// The flow reported started (`FlowStarted`).
    pub started: bool,
    prepared_at: Instant,
}

impl ActiveBinding {
    /// The URL the phone sends to, on `host` (as the phone can reach it).
    pub fn stream_url(&self, host: &str) -> String {
        format!(
            "srt://{host}:{}?streamid={}&latency={}&passphrase={}&pbkeylen=16",
            self.port, self.stream_id, self.latency_ms, self.passphrase
        )
    }

    /// The URI the block's `srtsrc` listens with.
    fn listener_uri(&self) -> String {
        format!(
            "srt://:{}?mode=listener&passphrase={}&pbkeylen=16",
            self.port, self.passphrase
        )
    }
}

/// Key of a binding: block ids are unique only within their flow.
pub type BindingKey = (FlowId, String);

/// The camera hub and the bindings of running flows.
pub struct Cameras {
    hub: RwLock<Arc<Hub>>,
    settings: RwLock<CameraSettings>,
    bindings: Mutex<HashMap<BindingKey, ActiveBinding>>,
}

impl Default for Cameras {
    fn default() -> Self {
        Self::new()
    }
}

impl Cameras {
    /// A hub that forgets its cameras on restart, until [`Self::configure`].
    pub fn new() -> Self {
        Self {
            hub: RwLock::new(Self::make_hub(Hub::new())),
            settings: RwLock::new(CameraSettings::default()),
            bindings: Mutex::new(HashMap::new()),
        }
    }

    /// Strom always requires an invite from a phone it does not know.
    fn make_hub(hub: Hub) -> Arc<Hub> {
        hub.set_invite_required(true);
        Arc::new(hub)
    }

    /// Keep known cameras in `data_dir` and apply `settings`. Called once at
    /// startup, before anything connects.
    pub fn configure(&self, data_dir: &std::path::Path, settings: CameraSettings) {
        let store = CameraStore::open(data_dir);
        info!("Keeping known cameras in {}", store.path().display());
        *self.hub.write() = Self::make_hub(Hub::with_store(Arc::new(store)));
        *self.settings.write() = settings;
    }

    pub fn hub(&self) -> Arc<Hub> {
        self.hub.read().clone()
    }

    pub fn settings(&self) -> CameraSettings {
        self.settings.read().clone()
    }

    // -----------------------------------------------------------------------
    // Addresses
    // -----------------------------------------------------------------------

    /// This machine's LAN addresses, best first.
    fn lan_addresses(&self) -> Vec<IpAddr> {
        let port = self.settings.read().http_port;
        Provisioning::new(SocketAddr::from(([0, 0, 0, 0], port))).addresses()
    }

    /// The host a phone should send its SRT stream to.
    pub fn srt_host_for(&self, device: &str) -> Option<String> {
        if let Some(host) = self.settings.read().public_host.clone() {
            return Some(host);
        }
        if let Some(host) = self.hub().device_host(device) {
            return Some(host_without_port(&host).to_string());
        }
        self.lan_addresses().first().map(ip_host)
    }

    /// The device WebSocket URL for an invite. `request_host` is the `Host`
    /// the operator's browser used.
    fn device_url(&self, request_host: Option<&str>) -> Result<String, String> {
        let settings = self.settings.read().clone();
        if let Some(url) = settings.device_url {
            return Ok(url);
        }
        let scheme = if settings.tls { "wss" } else { "ws" };
        let authority = match (&settings.public_host, request_host) {
            (Some(host), _) => format!("{host}:{}", settings.http_port),
            (None, Some(h)) if !is_loopback_host(host_without_port(h)) => h.to_string(),
            _ => {
                let ip = self.lan_addresses().into_iter().next().ok_or(
                    "no address a phone can reach: set cameras.public_host (STROM_CAMERAS_PUBLIC_HOST)",
                )?;
                format!("{}:{}", ip_host(&ip), settings.http_port)
            }
        };
        Ok(format!("{scheme}://{authority}{DEVICE_PATH}"))
    }

    /// A one-time invite for a new phone.
    pub fn invite(&self, request_host: Option<&str>) -> Result<CameraInviteResponse, String> {
        let base = self.device_url(request_host)?;
        let token = self.hub().create_invite(DEFAULT_INVITE_TTL);
        let control_url = with_invite_token(&base, &token);
        let deep_link = deep_link_control(&control_url, None);
        let qr_svg = qr_svg(&deep_link)?;
        Ok(CameraInviteResponse {
            deep_link,
            control_url,
            qr_svg,
            expires_in_secs: DEFAULT_INVITE_TTL.as_secs(),
        })
    }

    // -----------------------------------------------------------------------
    // Bindings
    // -----------------------------------------------------------------------

    /// Make the camera-bound SRT inputs of `flow` (a copy about to be built)
    /// listeners with a fresh passphrase, and remember the bindings. Fails
    /// when a camera is already in another running flow.
    pub fn prepare_flow(&self, flow: &mut Flow) -> Result<(), String> {
        let mut bindings = self.bindings.lock();
        bindings.retain(|(f, _), _| *f != flow.id);
        let mut prepared = Vec::new();
        for block in flow
            .blocks
            .iter_mut()
            .filter(|b| b.block_definition_id == SRT_INPUT_BLOCK)
        {
            let Some(device) = camera_of(&block.properties) else {
                continue;
            };
            if let Some(((other_flow, other_block), _)) =
                bindings.iter().find(|(_, b)| b.device == device)
            {
                return Err(format!(
                    "camera {device} (block {}) is already in use by block {other_block} of flow {other_flow}",
                    block.id
                ));
            }
            if prepared
                .iter()
                .any(|(_, b): &(String, ActiveBinding)| b.device == device)
            {
                return Err(format!(
                    "camera {device} is bound to more than one SRT input in this flow"
                ));
            }
            let binding = ActiveBinding {
                stream_id: stream_id(&device),
                port: srt_port(&block.properties),
                latency_ms: srt_latency(&block.properties),
                passphrase: uuid::Uuid::new_v4().simple().to_string(),
                device,
                started: false,
                prepared_at: Instant::now(),
            };
            block.properties.insert(
                "srt_uri".to_string(),
                PropertyValue::String(binding.listener_uri()),
            );
            info!(
                "Camera {} bound to block {} of flow {}: SRT listener on port {}",
                binding.device, block.id, flow.id, binding.port
            );
            prepared.push((block.id.clone(), binding));
        }
        for (block_id, binding) in prepared {
            bindings.insert((flow.id, block_id), binding);
        }
        Ok(())
    }

    /// The flow is running: its bindings are live. Returns them.
    pub fn flow_started(&self, flow_id: &FlowId) -> Vec<(BindingKey, ActiveBinding)> {
        let mut bindings = self.bindings.lock();
        bindings
            .iter_mut()
            .filter(|((f, _), _)| f == flow_id)
            .map(|(k, b)| {
                b.started = true;
                (k.clone(), b.clone())
            })
            .collect()
    }

    /// The flow stopped: forget its bindings. Returns the devices that are no
    /// longer bound anywhere.
    pub fn flow_stopped(&self, flow_id: &FlowId) -> Vec<String> {
        let mut bindings = self.bindings.lock();
        let removed: Vec<String> = bindings
            .iter()
            .filter(|((f, _), _)| f == flow_id)
            .map(|(_, b)| b.device.clone())
            .collect();
        bindings.retain(|(f, _), _| f != flow_id);
        removed
            .into_iter()
            .filter(|d| !bindings.values().any(|b| &b.device == d))
            .collect()
    }

    /// Drop bindings whose flow is not running: started ones whose flow
    /// stopped without an event we saw, and prepared ones whose start
    /// failed. Returns the devices that are no longer bound anywhere.
    pub fn reconcile(&self, running: &dyn Fn(&FlowId) -> bool) -> Vec<String> {
        let mut bindings = self.bindings.lock();
        let mut removed = Vec::new();
        bindings.retain(|(f, _), b| {
            let keep = running(f) || (!b.started && b.prepared_at.elapsed() < PREPARED_TIMEOUT);
            if !keep {
                removed.push(b.device.clone());
            }
            keep
        });
        removed
            .into_iter()
            .filter(|d| !bindings.values().any(|b| &b.device == d))
            .collect()
    }

    /// The started binding of `device`, if its flow runs.
    pub fn started_binding_of(&self, device: &str) -> Option<(BindingKey, ActiveBinding)> {
        self.bindings
            .lock()
            .iter()
            .find(|(_, b)| b.started && b.device == device)
            .map(|(k, b)| (k.clone(), b.clone()))
    }

    /// Started bindings, by key.
    pub fn started_bindings(&self) -> HashMap<BindingKey, ActiveBinding> {
        self.bindings
            .lock()
            .iter()
            .filter(|(_, b)| b.started)
            .map(|(k, b)| (k.clone(), b.clone()))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Listing
    // -----------------------------------------------------------------------

    /// Every known camera, with the blocks of `flows` it is bound to.
    pub fn list(&self, flows: &[Flow], running: &dyn Fn(&FlowId) -> bool) -> Vec<CameraSummary> {
        let mut bound: HashMap<String, Vec<CameraBinding>> = HashMap::new();
        for flow in flows {
            for block in flow
                .blocks
                .iter()
                .filter(|b| b.block_definition_id == SRT_INPUT_BLOCK)
            {
                if let Some(device) = camera_of(&block.properties) {
                    bound.entry(device).or_default().push(CameraBinding {
                        flow_id: flow.id,
                        block_id: block.id.clone(),
                        running: running(&flow.id),
                    });
                }
            }
        }
        self.hub()
            .devices()
            .into_iter()
            .map(|d| summary(d, &mut bound))
            .collect()
    }
}

fn summary(d: DeviceSnapshot, bound: &mut HashMap<String, Vec<CameraBinding>>) -> CameraSummary {
    CameraSummary {
        bindings: bound.remove(&d.id).unwrap_or_default(),
        name: d.info.name,
        model: d.info.model,
        platform: d.info.platform,
        app_version: d.info.app_version,
        online: d.online,
        tally: from_hub_tally(d.tally),
        battery_level: d
            .status
            .as_ref()
            .and_then(|s| s.battery.as_ref())
            .map(|b| b.level),
        stream_state: d
            .status
            .as_ref()
            .and_then(|s| s.stream.as_ref())
            .map(|s| s.state.clone()),
        id: d.id,
    }
}

pub fn to_hub_tally(t: CameraTally) -> TallyState {
    match t {
        CameraTally::Off => TallyState::Off,
        CameraTally::Preview => TallyState::Preview,
        CameraTally::Program => TallyState::Program,
    }
}

pub fn from_hub_tally(t: TallyState) -> CameraTally {
    match t {
        TallyState::Off => CameraTally::Off,
        TallyState::Preview => CameraTally::Preview,
        TallyState::Program => CameraTally::Program,
    }
}

/// The device id in a block's `camera` property, if set.
pub fn camera_of(properties: &HashMap<String, PropertyValue>) -> Option<String> {
    match properties.get(CAMERA_PROPERTY) {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// The port of the block's `srt_uri` (listener or caller form), or the
/// default URI's port.
fn srt_port(properties: &HashMap<String, PropertyValue>) -> u16 {
    let uri = match properties.get("srt_uri") {
        Some(PropertyValue::String(s)) => s.as_str(),
        _ => strom_types::DEFAULT_SRT_INPUT_URI,
    };
    uri_port(uri)
        .or_else(|| uri_port(strom_types::DEFAULT_SRT_INPUT_URI))
        .unwrap_or(5000)
}

/// The port of `srt://host:port?...`. Parsed by hand: a listener URI has an
/// empty host (`srt://:5000`), which `url::Url` refuses.
fn uri_port(uri: &str) -> Option<u16> {
    let rest = uri.split_once("://")?.1;
    let authority = rest.split(['?', '/']).next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn srt_latency(properties: &HashMap<String, PropertyValue>) -> i64 {
    match properties.get("latency") {
        Some(PropertyValue::Int(i)) => *i,
        Some(PropertyValue::UInt(u)) => *u as i64,
        _ => strom_types::DEFAULT_SRT_LATENCY_MS as i64,
    }
}

/// `cam-` and the first 8 alphanumerics of the device id, lowercased: the
/// stream id the app uses for itself.
fn stream_id(device: &str) -> String {
    let compact: String = device
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_lowercase();
    format!("cam-{compact}")
}

/// `host[:port]` or `[v6][:port]` without the port.
pub fn host_without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return match authority.find(']') {
            Some(end) => &authority[..=end],
            None => authority,
        };
    }
    authority.split(':').next().unwrap_or(authority)
}

fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn ip_host(ip: &IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// `srt_uri` with its passphrase hidden, for logs.
pub fn redact_srt_uri(uri: &str) -> String {
    let Some((base, query)) = uri.split_once('?') else {
        return uri.to_string();
    };
    let query: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if k.eq_ignore_ascii_case("passphrase") => format!("{k}=***"),
            _ => kv.to_string(),
        })
        .collect();
    format!("{base}?{}", query.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::BlockInstance;

    fn srt_block(id: &str, props: serde_json::Value) -> BlockInstance {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "block_definition_id": SRT_INPUT_BLOCK,
            "properties": props,
            "position": {"x": 0.0, "y": 0.0},
        }))
        .unwrap()
    }

    #[test]
    fn prepare_makes_camera_blocks_listeners_with_a_passphrase() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![
            srt_block(
                "cam",
                serde_json::json!({
                    "camera": "CF93DF9D-22AB-4927",
                    "srt_uri": "srt://:7001?mode=listener",
                    "latency": 80,
                }),
            ),
            srt_block(
                "plain",
                serde_json::json!({"srt_uri": "srt://192.0.2.1:7002?mode=caller"}),
            ),
        ];
        cameras.prepare_flow(&mut flow).unwrap();

        let PropertyValue::String(uri) = &flow.blocks[0].properties["srt_uri"] else {
            panic!()
        };
        assert!(uri.starts_with("srt://:7001?mode=listener&passphrase="));
        assert!(uri.ends_with("&pbkeylen=16"));
        let PropertyValue::String(plain) = &flow.blocks[1].properties["srt_uri"] else {
            panic!()
        };
        assert_eq!(plain, "srt://192.0.2.1:7002?mode=caller", "untouched");

        let started = cameras.flow_started(&flow.id);
        assert_eq!(started.len(), 1);
        let b = &started[0].1;
        assert_eq!(b.device, "CF93DF9D-22AB-4927");
        assert_eq!(b.stream_id, "cam-cf93df9d");
        assert!(uri.contains(&b.passphrase), "block and phone share it");
        assert_eq!(
            b.stream_url("192.0.2.10"),
            format!(
                "srt://192.0.2.10:7001?streamid=cam-cf93df9d&latency=80&passphrase={}&pbkeylen=16",
                b.passphrase
            )
        );
        assert!(!redact_srt_uri(uri).contains(&b.passphrase));

        // The same camera in a second flow is refused while the first runs.
        let mut other = Flow::new("other");
        other.blocks = vec![srt_block(
            "cam",
            serde_json::json!({"camera": "CF93DF9D-22AB-4927"}),
        )];
        assert!(cameras.prepare_flow(&mut other).is_err());

        assert_eq!(cameras.flow_stopped(&flow.id), ["CF93DF9D-22AB-4927"]);
        assert!(cameras.prepare_flow(&mut other).is_ok());
    }

    #[test]
    fn reconcile_drops_stopped_and_failed_starts() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "dev1"}))];
        cameras.prepare_flow(&mut flow).unwrap();
        // A fresh preparation survives while its flow is starting.
        assert!(cameras.reconcile(&|_| false).is_empty());
        cameras.flow_started(&flow.id);
        assert!(cameras.reconcile(&|_| true).is_empty());
        assert_eq!(cameras.reconcile(&|_| false), ["dev1"]);
        assert!(cameras.started_bindings().is_empty());
    }

    #[test]
    fn hosts() {
        assert_eq!(host_without_port("192.0.2.1:8080"), "192.0.2.1");
        assert_eq!(host_without_port("[2001:db8::1]:8080"), "[2001:db8::1]");
        assert_eq!(host_without_port("strom.example.com"), "strom.example.com");
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("192.0.2.1"));
    }

    #[test]
    fn invite_uses_the_configured_host_and_carries_a_token() {
        let cameras = Cameras::new();
        *cameras.settings.write() = CameraSettings {
            public_host: Some("strom.example.com".into()),
            http_port: 8080,
            ..Default::default()
        };
        let invite = cameras.invite(Some("localhost:8080")).unwrap();
        assert!(invite
            .control_url
            .starts_with("ws://strom.example.com:8080/api/cameras/device?token="));
        assert!(invite
            .deep_link
            .starts_with("tallycam://connect?control=ws%3A%2F%2F"));
        assert!(
            !invite.deep_link.contains("&srt="),
            "Strom pushes the SRT URL later"
        );
        assert!(invite.qr_svg.starts_with("<?xml"));

        // Without a configured host, the browser's host is used unless loopback.
        *cameras.settings.write() = CameraSettings {
            http_port: 8080,
            ..Default::default()
        };
        let invite = cameras.invite(Some("192.0.2.5:9999")).unwrap();
        assert!(invite
            .control_url
            .starts_with("ws://192.0.2.5:9999/api/cameras/device?token="));
    }
}
