//! Phone cameras (TallyCam) in Strom.
//!
//! Strom embeds the `tallycam-hub` library: phones connect to
//! `/api/cameras/device` with the TallyCam control protocol, operator UIs to
//! `/api/cameras/control`. A phone is put in a flow through the `camera`
//! property of an MPEG-TS/SRT input block:
//!
//! - when the flow starts, the block becomes an SRT listener with a fresh
//!   passphrase ([`Cameras::prepare_flow`]), and the phone is told where to
//!   send (`stream_url`) and to go live. The passphrase lives only in memory:
//!   never in the flow, so never in `flows.json` or the flow API;
//! - a bound phone that connects while its flow runs is put live too;
//! - when the flow stops, the phone stops streaming and its tally goes off;
//! - while the flow runs, its vision mixers drive the phone's tally
//!   ([`tally`]).
//!
//! The running side (event loop, pushes, tally) is in `state/cameras.rs`,
//! next to the flow state it reads.

pub mod display;
pub mod labels;
pub mod settings;
pub mod store;
pub mod tally;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
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

pub use settings::{CameraSettings, CamerasConfig};
use store::CameraStore;

/// The SRT input block that has a camera mode.
pub const SRT_INPUT_BLOCK: &str = "builtin.mpegtssrt_input";
/// Where phones connect.
pub const DEVICE_PATH: &str = "/api/cameras/device";
/// Where operator UIs (LCP controllers) connect.
pub const CONTROL_PATH: &str = "/api/cameras/control";
/// Build-time-only block property carrying a camera block's passphrase from
/// [`Cameras::prepare_flow`] to the block builder. It is set on the copy of
/// the flow that is built, never on the stored flow.
pub const PASSPHRASE_PROPERTY: &str = "_camera_passphrase";
/// The SRT key length that goes with the passphrase, in bytes.
pub const PBKEYLEN: u32 = 16;
/// Safety net for a binding whose flow is not running and was not torn down
/// either. Every failed start tears down, so this only bounds a leak; it is
/// long so a slow start keeps its binding.
const PREPARED_TIMEOUT: Duration = Duration::from_secs(600);
/// Least time between two go-live attempts for one camera, so a phone that
/// keeps refusing is not asked every reconcile tick.
const GO_LIVE_RETRY: Duration = Duration::from_secs(5);

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
            "srt://{host}:{}?streamid={}&latency={}&passphrase={}&pbkeylen={PBKEYLEN}",
            self.port, self.stream_id, self.latency_ms, self.passphrase
        )
    }
}

/// The URI a camera block's `srtsrc` listens with. The passphrase is set on
/// the element's write-only `passphrase` property instead, so it cannot be
/// read back from the running pipeline either.
pub fn listener_uri(port: u16) -> String {
    format!("srt://:{port}?mode=listener")
}

/// An SRT listener of a flow: a block or element that binds a local UDP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub block_id: String,
    pub port: u16,
    /// A camera-bound SRT input (always a listener, whatever its URI says).
    pub camera: bool,
}

/// Who already uses a port, as an operator would look for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHolder {
    /// For the error, e.g. "block 'x' of running flow 'y'".
    pub description: String,
    /// A camera block. Plain SRT listeners sharing a port with each other are
    /// not refused here; only clashes involving a camera are.
    pub camera: bool,
}

/// Key of a binding: block ids are unique only within their flow.
pub type BindingKey = (FlowId, String);

/// The camera hub and the bindings of running flows.
pub struct Cameras {
    hub: RwLock<Arc<Hub>>,
    settings: RwLock<CameraSettings>,
    bindings: Mutex<HashMap<BindingKey, ActiveBinding>>,
    names: Mutex<labels::NameHistory>,
    /// Go-live attempts by device: in flight, and when the last one began.
    go_live: Mutex<HashMap<String, (bool, Instant)>>,
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
            names: Mutex::new(labels::NameHistory::default()),
            go_live: Mutex::new(HashMap::new()),
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

    /// Bind the cameras of `flow`, about to start: each camera block gets a
    /// fresh passphrase, kept here. Returns the copy of the flow to build,
    /// with the passphrases on it ([`PASSPHRASE_PROPERTY`]), or `None` when
    /// the flow has no cameras. `flow` itself is not changed, so the stored
    /// flow never holds a passphrase.
    ///
    /// Refused, with what to change: a camera already in another flow, and an
    /// SRT port clash involving a camera, against `taken` (other running flows
    /// and the port pool), other flows being started, and this flow itself.
    pub fn prepare_flow(
        &self,
        flow: &Flow,
        taken: &HashMap<u16, PortHolder>,
    ) -> Result<Option<Flow>, String> {
        let mut bindings = self.bindings.lock();
        bindings.retain(|(f, _), _| *f != flow.id);

        let listeners = listeners(flow);
        for listener in &listeners {
            check_port(flow, listener, &listeners, taken, &bindings)?;
        }

        let mut build = flow.clone();
        let mut prepared: Vec<(String, ActiveBinding)> = Vec::new();
        for block in build
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
                    "Camera {} (block '{}') is already in use by block '{other_block}' \
                     of flow {other_flow}. Stop that flow, or bind another camera to this block.",
                    self.label(&device),
                    block.id
                ));
            }
            if let Some((other_block, _)) = prepared.iter().find(|(_, b)| b.device == device) {
                return Err(format!(
                    "Camera {} is bound to both block '{other_block}' and block '{}' \
                     of this flow. Bind it to one block only.",
                    self.label(&device),
                    block.id
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
                PASSPHRASE_PROPERTY.to_string(),
                PropertyValue::String(binding.passphrase.clone()),
            );
            info!(
                "Camera {} bound to block {} of flow {}: SRT listener on port {}",
                binding.device, block.id, flow.id, binding.port
            );
            prepared.push((block.id.clone(), binding));
        }
        if prepared.is_empty() {
            return Ok(None);
        }
        for (block_id, binding) in prepared {
            bindings.insert((flow.id, block_id), binding);
        }
        Ok(Some(build))
    }

    /// Mark the prepared bindings of `running` flows live and return them.
    /// Only a running flow's pipeline is built, so its listeners are up. Called
    /// on `FlowStarted` and on every reconcile tick (for a missed event); a
    /// late `FlowStarted` of an earlier run cannot mark a restart that is
    /// still building, since that flow is not running yet.
    pub fn mark_started(
        &self,
        running: &dyn Fn(&FlowId) -> bool,
    ) -> Vec<(BindingKey, ActiveBinding)> {
        let mut bindings = self.bindings.lock();
        bindings
            .iter_mut()
            .filter(|((f, _), b)| !b.started && running(f))
            .map(|(k, b)| {
                b.started = true;
                (k.clone(), b.clone())
            })
            .collect()
    }

    /// `binding` is still the live binding of its device: its flow was not
    /// stopped or restarted since (a restart gives a new passphrase).
    pub fn is_current(&self, binding: &ActiveBinding) -> bool {
        self.bindings
            .lock()
            .values()
            .any(|b| b.started && b.device == binding.device && b.passphrase == binding.passphrase)
    }

    /// The flow was torn down (stopped, or its start failed): forget its
    /// bindings. Returns the devices that went live with it and are no
    /// longer bound anywhere.
    pub fn flow_stopped(&self, flow_id: &FlowId) -> Vec<String> {
        let mut bindings = self.bindings.lock();
        let mut removed = Vec::new();
        bindings.retain(|(f, _), b| {
            if f != flow_id {
                return true;
            }
            removed.push((b.device.clone(), b.started));
            false
        });
        let released = unbound(removed, &bindings);
        drop(bindings);
        self.released(released)
    }

    /// Drop bindings whose flow is not running: started ones whose flow
    /// stopped without an event we saw, and prepared ones left behind for
    /// [`PREPARED_TIMEOUT`]. Returns, like [`Self::flow_stopped`], only the
    /// devices that went live and are no longer bound anywhere: a phone that
    /// never went live with the flow is not ours to stop.
    pub fn reconcile(&self, running: &dyn Fn(&FlowId) -> bool) -> Vec<String> {
        let mut bindings = self.bindings.lock();
        let mut removed = Vec::new();
        bindings.retain(|(f, _), b| {
            if running(f) || (!b.started && b.prepared_at.elapsed() < PREPARED_TIMEOUT) {
                return true;
            }
            removed.push((b.device.clone(), b.started));
            false
        });
        let released = unbound(removed, &bindings);
        drop(bindings);
        self.released(released)
    }

    /// Devices whose last binding ended: each follows the hub's production
    /// again. Returns those that went live, for the caller to stop.
    ///
    /// Strom gives no camera a production of its own yet: the flow has no
    /// mains or shutter setting. Clearing it here keeps a camera from keeping
    /// a flow's production once that flow lets go of it.
    /// TODO: set the flow's production on the bound cameras when a flow gets
    /// one (tallycam docs/SHADING.md D6: cameras bound to a block in a flow
    /// get the flow's settings; an unbound camera gets Strom's default).
    fn released(&self, released: Vec<(String, bool)>) -> Vec<String> {
        let hub = self.hub();
        released
            .into_iter()
            .filter_map(|(device, went_live)| {
                // An unknown device (bound, never connected) has none.
                let _ = hub.set_device_production(&device, None);
                went_live.then_some(device)
            })
            .collect()
    }

    /// Claim a go-live attempt for `device`: false while one is in flight,
    /// and, for a `retry`, within [`GO_LIVE_RETRY`] of the last one. Pair
    /// with [`Self::end_go_live`].
    pub fn begin_go_live(&self, device: &str, retry: bool) -> bool {
        let mut attempts = self.go_live.lock();
        if let Some((in_flight, last)) = attempts.get(device) {
            if *in_flight || (retry && last.elapsed() < GO_LIVE_RETRY) {
                return false;
            }
        }
        attempts.insert(device.to_string(), (true, Instant::now()));
        true
    }

    pub fn end_go_live(&self, device: &str) {
        if let Some((in_flight, _)) = self.go_live.lock().get_mut(device) {
            *in_flight = false;
        }
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

    /// Every known camera as a device list entry (the camera picker).
    pub fn device_list(&self) -> Vec<strom_types::discovery::DeviceResponse> {
        let mut devices: Vec<_> = self
            .hub()
            .devices()
            .iter()
            .map(display::device_response)
            .collect();
        devices.sort_by_cached_key(|d| d.name.to_lowercase());
        devices
    }

    /// The display name of every known camera, by device id.
    pub fn display_names(&self) -> HashMap<String, String> {
        self.hub()
            .devices()
            .iter()
            .map(|d| (d.id.clone(), display::snapshot_name(d)))
            .collect()
    }

    /// Record the current display names of the known cameras. Returns the
    /// devices whose name is new (first seen, or changed).
    pub fn note_names(&self) -> Vec<String> {
        let current = self.display_names();
        let mut names = self.names.lock();
        current
            .into_iter()
            .filter(|(device, name)| names.note(device, name))
            .map(|(device, _)| device)
            .collect()
    }

    /// Every name a camera has or had in this run: a label that is one of
    /// them was set by Strom, not by an operator.
    pub fn camera_names(&self) -> std::collections::HashSet<String> {
        let mut all = self.names.lock().all();
        all.extend(self.display_names().into_values());
        all
    }

    /// A camera as an operator knows it: "'<display name>' (<device id>)", or
    /// the device id of a camera Strom does not know.
    fn label(&self, device: &str) -> String {
        self.hub()
            .devices()
            .iter()
            .find(|d| d.id == device)
            .map(|d| format!("'{}' ({device})", display::snapshot_name(d)))
            .unwrap_or_else(|| device.to_string())
    }
}

/// The removed `(device, went live)` that no binding in `bindings` uses any
/// more, once per device (went live if any of its removed bindings did).
fn unbound(
    removed: Vec<(String, bool)>,
    bindings: &HashMap<BindingKey, ActiveBinding>,
) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for (device, went_live) in removed {
        if bindings.values().any(|b| b.device == device) {
            continue;
        }
        match out.iter_mut().find(|(d, _)| *d == device) {
            Some((_, live)) => *live |= went_live,
            None => out.push((device, went_live)),
        }
    }
    out
}

fn summary(d: DeviceSnapshot, bound: &mut HashMap<String, Vec<CameraBinding>>) -> CameraSummary {
    CameraSummary {
        bindings: bound.remove(&d.id).unwrap_or_default(),
        name: display::snapshot_name(&d),
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
        key_fingerprint: d.key_fingerprint,
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

/// The SRT listeners of `flow`: camera blocks, and any block or element with
/// an `srt://` URI property in listener mode.
pub fn listeners(flow: &Flow) -> Vec<Listener> {
    let defaults = default_srt_listener_uris();
    let blocks = flow.blocks.iter().map(|b| {
        let camera = b.block_definition_id == SRT_INPUT_BLOCK && camera_of(&b.properties).is_some();
        // A property left at its default is not stored, so a block that
        // listens on its default URI has no URI among its properties.
        let default = defaults
            .get(&b.block_definition_id)
            .into_iter()
            .flatten()
            .filter(|(name, _)| !b.properties.contains_key(name))
            .find_map(|(_, uri)| uri_port(uri));
        (&b.id, &b.properties, camera, default)
    });
    let elements = flow
        .elements
        .iter()
        .map(|e| (&e.id, &e.properties, false, None));
    blocks
        .chain(elements)
        .filter_map(|(id, properties, camera, default)| {
            let port = if camera {
                Some(srt_port(properties))
            } else {
                properties
                    .values()
                    .find_map(|v| match v {
                        PropertyValue::String(uri) if is_srt_listener(uri) => uri_port(uri),
                        _ => None,
                    })
                    .or(default)
            }?;
            Some(Listener {
                block_id: id.clone(),
                port,
                camera,
            })
        })
        .collect()
}

/// The SRT listener URIs that built-in blocks use by default, by block
/// definition id: (property name, default URI).
fn default_srt_listener_uris() -> &'static HashMap<String, Vec<(String, String)>> {
    static DEFAULTS: OnceLock<HashMap<String, Vec<(String, String)>>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        crate::blocks::builtin::get_all_builtin_blocks()
            .into_iter()
            .filter_map(|def| {
                let uris: Vec<(String, String)> = def
                    .exposed_properties
                    .iter()
                    .filter_map(|p| match &p.default_value {
                        Some(PropertyValue::String(uri)) if is_srt_listener(uri) => {
                            Some((p.name.clone(), uri.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                (!uris.is_empty()).then_some((def.id, uris))
            })
            .collect()
    })
}

/// `srt://` with `mode=listener`, or with no host and no mode (which srtsrc
/// and srtsink treat as a listener).
fn is_srt_listener(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("srt://") else {
        return false;
    };
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mode = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("mode="))
        .map(str::to_ascii_lowercase);
    match mode.as_deref() {
        Some("listener") => true,
        Some(_) => false,
        None => authority.starts_with(':'),
    }
}

/// Refuse a port clash that involves a camera.
fn check_port(
    flow: &Flow,
    listener: &Listener,
    listeners: &[Listener],
    taken: &HashMap<u16, PortHolder>,
    bindings: &HashMap<BindingKey, ActiveBinding>,
) -> Result<(), String> {
    let port = listener.port;
    let same_flow = listeners.iter().find(|other| {
        other.port == port
            && other.block_id != listener.block_id
            && (listener.camera || other.camera)
    });
    let in_taken = || {
        taken
            .get(&port)
            .filter(|h| listener.camera || h.camera)
            .map(|h| h.description.clone())
    };
    // Bindings also cover a flow that is starting and not yet running.
    let in_bindings = || {
        bindings
            .iter()
            .find(|((f, _), b)| *f != flow.id && b.port == port)
            .map(|((f, block), _)| format!("camera block '{block}' of flow {f}"))
    };
    let holder = same_flow
        .map(|other| format!("block '{}' of this flow", other.block_id))
        .or_else(in_taken)
        .or_else(in_bindings);
    match holder {
        Some(holder) => Err(format!(
            "{} block '{}' listens on SRT port {port}, which {holder} already uses. \
             Change the port in the SRT URI of one of them.",
            if listener.camera { "Camera" } else { "SRT" },
            listener.block_id
        )),
        None => Ok(()),
    }
}

/// The port of the block's `srt_uri` (listener or caller form), or the
/// default URI's port.
pub fn srt_port(properties: &HashMap<String, PropertyValue>) -> u16 {
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

    /// A camera-capable SRT input. Every test that finds listeners uses one,
    /// and finding them reads the built-in block definitions, which need
    /// GStreamer: initialized here, not left to whichever test ran first.
    fn srt_block(id: &str, props: serde_json::Value) -> BlockInstance {
        gstreamer::init().unwrap();
        serde_json::from_value(serde_json::json!({
            "id": id,
            "block_definition_id": SRT_INPUT_BLOCK,
            "properties": props,
            "position": {"x": 0.0, "y": 0.0},
        }))
        .unwrap()
    }

    fn free() -> HashMap<u16, PortHolder> {
        HashMap::new()
    }

    fn uri(block: &BlockInstance) -> &str {
        match &block.properties["srt_uri"] {
            PropertyValue::String(s) => s,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn prepare_keeps_the_passphrase_out_of_the_stored_flow() {
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
        let stored = serde_json::to_string(&flow).unwrap();
        let build = cameras.prepare_flow(&flow, &free()).unwrap().unwrap();

        let started = cameras.mark_started(&|f| *f == flow.id);
        assert_eq!(started.len(), 1);
        let b = &started[0].1;
        assert_eq!(b.device, "CF93DF9D-22AB-4927");
        assert_eq!(b.port, 7001);
        assert_eq!(b.stream_id, "cam-cf93df9d");

        assert_eq!(
            serde_json::to_string(&flow).unwrap(),
            stored,
            "flow untouched"
        );
        assert!(
            matches!(&build.blocks[0].properties[PASSPHRASE_PROPERTY],
                PropertyValue::String(p) if *p == b.passphrase),
            "the built copy carries it to the block"
        );
        assert_eq!(uri(&build.blocks[0]), "srt://:7001?mode=listener");
        assert_eq!(uri(&build.blocks[1]), "srt://192.0.2.1:7002?mode=caller");
        assert!(!build.blocks[1].properties.contains_key(PASSPHRASE_PROPERTY));

        assert_eq!(
            b.stream_url("192.0.2.10"),
            format!(
                "srt://192.0.2.10:7001?streamid=cam-cf93df9d&latency=80&passphrase={}&pbkeylen=16",
                b.passphrase
            )
        );
        assert!(!redact_srt_uri(&b.stream_url("192.0.2.10")).contains(&b.passphrase));

        // A restart gets a new passphrase.
        cameras.flow_stopped(&flow.id);
        cameras.prepare_flow(&flow, &free()).unwrap();
        assert_ne!(
            cameras.mark_started(&|f| *f == flow.id)[0].1.passphrase,
            b.passphrase
        );
    }

    #[test]
    fn a_flow_without_cameras_is_built_as_it_is() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("plain");
        flow.blocks = vec![srt_block("in", serde_json::json!({}))];
        assert!(cameras.prepare_flow(&flow, &free()).unwrap().is_none());
    }

    #[test]
    fn a_camera_is_in_one_running_flow_at_a_time() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "dev1"}))];
        cameras.prepare_flow(&flow, &free()).unwrap();
        cameras.mark_started(&|f| *f == flow.id);
        let mut other = Flow::new("other");
        other.blocks = vec![srt_block(
            "cam",
            serde_json::json!({"camera": "dev1", "srt_uri": "srt://:7300?mode=listener"}),
        )];
        let err = cameras.prepare_flow(&other, &free()).unwrap_err();
        assert!(err.contains("already in use"), "{err}");
        assert_eq!(cameras.flow_stopped(&flow.id), ["dev1"]);
        assert!(cameras.prepare_flow(&other, &free()).is_ok());
    }

    #[test]
    fn a_camera_port_clash_is_refused_with_who_holds_it() {
        let camera = |port: u16| {
            srt_block(
                "cam",
                serde_json::json!({
                    "camera": "dev1",
                    "srt_uri": format!("srt://:{port}?mode=listener"),
                }),
            )
        };

        // Another running flow, or the port pool.
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![camera(7110)];
        let taken = HashMap::from([(
            7110,
            PortHolder {
                description: "block 'srt' of running flow 'Studio'".into(),
                camera: false,
            },
        )]);
        let err = cameras.prepare_flow(&flow, &taken).unwrap_err();
        assert!(err.contains("SRT port 7110"), "{err}");
        assert!(
            err.contains("block 'srt' of running flow 'Studio'"),
            "{err}"
        );
        assert!(
            cameras.started_bindings().is_empty()
                && cameras.mark_started(&|f| *f == flow.id).is_empty()
        );

        // Another listener in the same flow.
        flow.blocks.push(srt_block(
            "listener",
            serde_json::json!({"srt_uri": "srt://:7110"}),
        ));
        let err = cameras.prepare_flow(&flow, &free()).unwrap_err();
        assert!(err.contains("block 'listener' of this flow"), "{err}");

        // A camera of a flow still starting, and the other way round: a plain
        // listener starting on a camera's port.
        let mut first = Flow::new("first");
        first.blocks = vec![camera(7120)];
        cameras.prepare_flow(&first, &free()).unwrap();
        let mut second = Flow::new("second");
        second.blocks = vec![srt_block(
            "in",
            serde_json::json!({"srt_uri": "srt://0.0.0.0:7120?mode=listener"}),
        )];
        let err = cameras.prepare_flow(&second, &free()).unwrap_err();
        assert!(err.contains("camera block 'cam' of flow"), "{err}");

        // Plain listeners sharing a port are not this module's business, and
        // a caller does not bind the port.
        let plain = HashMap::from([(
            7130,
            PortHolder {
                description: "x".into(),
                camera: false,
            },
        )]);
        second.blocks = vec![srt_block(
            "in",
            serde_json::json!({"srt_uri": "srt://:7130?mode=listener"}),
        )];
        assert!(cameras.prepare_flow(&second, &plain).is_ok());
        second.blocks = vec![srt_block(
            "in",
            serde_json::json!({"srt_uri": "srt://192.0.2.1:7120?mode=caller"}),
        )];
        assert!(cameras.prepare_flow(&second, &free()).is_ok());
    }

    #[test]
    fn listeners_are_found_in_blocks_and_elements() {
        let mut flow = Flow::new("l");
        flow.blocks = vec![
            srt_block(
                "cam",
                serde_json::json!({"camera": "d", "srt_uri": "srt://192.0.2.1:7001?mode=caller"}),
            ),
            srt_block("caller", serde_json::json!({})),
        ];
        flow.elements = vec![serde_json::from_value(serde_json::json!({
            "id": "sink",
            "element_type": "srtsink",
            "properties": {"uri": "srt://:7002"},
            "position": [0.0, 0.0],
        }))
        .unwrap()];
        let ports: Vec<(String, u16, bool)> = listeners(&flow)
            .into_iter()
            .map(|l| (l.block_id, l.port, l.camera))
            .collect();
        assert_eq!(
            ports,
            [("cam".into(), 7001, true), ("sink".into(), 7002, false)]
        );
        assert!(is_srt_listener("srt://192.0.2.1:7000?mode=Listener"));
        assert!(!is_srt_listener("srt://:7000?mode=rendezvous"));
        assert!(!is_srt_listener("udp://:7000"));
    }

    /// An SRT output left at its default URI listens on the default port,
    /// which a camera without an `srt_uri` also takes.
    #[test]
    fn a_default_srt_output_listener_clashes_with_a_default_camera() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("defaults");
        let mut output = srt_block("out", serde_json::json!({}));
        output.block_definition_id = "builtin.mpegtssrt_output".into();
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "d"})), output];
        let ports: Vec<(String, u16)> = listeners(&flow)
            .into_iter()
            .map(|l| (l.block_id, l.port))
            .collect();
        assert_eq!(ports, [("cam".into(), 5000), ("out".into(), 5000)]);
        let err = cameras.prepare_flow(&flow, &free()).unwrap_err();
        assert!(err.contains("block 'out' of this flow"), "{err}");

        // A URI set to a caller is not the default listener.
        flow.blocks[1].properties.insert(
            "srt_uri".into(),
            PropertyValue::String("srt://192.0.2.1:5000?mode=caller".into()),
        );
        assert_eq!(listeners(&flow).len(), 1);
    }

    #[test]
    fn stopping_releases_the_flow_and_stops_only_phones_that_went_live() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "dev1"}))];
        cameras.prepare_flow(&flow, &free()).unwrap();
        assert!(cameras.flow_stopped(&flow.id).is_empty(), "start failed");
        assert!(
            cameras.mark_started(&|f| *f == flow.id).is_empty(),
            "and is forgotten"
        );

        cameras.prepare_flow(&flow, &free()).unwrap();
        cameras.mark_started(&|f| *f == flow.id);
        assert_eq!(cameras.flow_stopped(&flow.id), ["dev1"]);
        assert!(cameras.started_bindings().is_empty());
    }

    #[test]
    fn reconcile_drops_stopped_and_failed_starts() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "dev1"}))];
        cameras.prepare_flow(&flow, &free()).unwrap();
        // A fresh preparation survives while its flow is starting.
        assert!(cameras.reconcile(&|_| false).is_empty());
        cameras.mark_started(&|f| *f == flow.id);
        assert!(cameras.reconcile(&|_| true).is_empty());
        assert_eq!(cameras.reconcile(&|_| false), ["dev1"]);
        assert!(cameras.started_bindings().is_empty());
    }

    /// One go-live per phone at a time; a retry also waits after the last.
    #[test]
    fn go_live_attempts_do_not_overlap_and_retries_wait() {
        let cameras = Cameras::new();
        assert!(cameras.begin_go_live("dev1", false));
        assert!(!cameras.begin_go_live("dev1", false), "in flight");
        assert!(cameras.begin_go_live("dev2", true), "another phone");
        cameras.end_go_live("dev1");
        assert!(!cameras.begin_go_live("dev1", true), "retry too soon");
        assert!(cameras.begin_go_live("dev1", false), "an event goes now");
    }

    /// A missed `FlowStarted` is made up for once the flow is seen running,
    /// and each binding is handed out to go live once.
    #[test]
    fn a_running_flow_goes_live_without_its_started_event() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "dev1"}))];
        cameras.prepare_flow(&flow, &free()).unwrap();
        assert!(cameras.mark_started(&|_| false).is_empty());
        let started = cameras.mark_started(&|f| *f == flow.id);
        assert_eq!(started.len(), 1);
        assert!(cameras.is_current(&started[0].1));
        assert!(
            cameras.mark_started(&|f| *f == flow.id).is_empty(),
            "already live"
        );
        assert!(cameras.mark_started(&|_| true).is_empty());
    }

    /// A binding stops being current when its flow stops or restarts, so a
    /// go-live still in flight does not leave the phone streaming.
    #[test]
    fn a_stopped_or_restarted_binding_is_not_current() {
        let cameras = Cameras::new();
        let mut flow = Flow::new("cams");
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "dev1"}))];
        cameras.prepare_flow(&flow, &free()).unwrap();
        let (_, old) = cameras.mark_started(&|f| *f == flow.id).pop().unwrap();
        cameras.flow_stopped(&flow.id);
        assert!(!cameras.is_current(&old));
        cameras.prepare_flow(&flow, &free()).unwrap();
        cameras.mark_started(&|f| *f == flow.id);
        assert!(!cameras.is_current(&old), "a restart has a new passphrase");
    }

    /// Cameras with `dev1` known (offline), kept in `dir`.
    fn cameras_knowing_dev1(dir: &std::path::Path) -> Cameras {
        std::fs::write(
            dir.join("cameras.json"),
            r#"{"version":1,"cameras":[{"info":{"id":"dev1","name":"Cam 1"}}]}"#,
        )
        .unwrap();
        let cameras = Cameras::new();
        cameras.configure(dir, CameraSettings::default());
        cameras
    }

    fn own_production(
        cameras: &Cameras,
        device: &str,
    ) -> Option<tallycam_hub::protocol::Production> {
        cameras
            .hub()
            .devices()
            .into_iter()
            .find(|d| d.id == device)
            .and_then(|d| d.production)
    }

    /// A camera's own production ends with its binding: when the flow stops
    /// (whether or not the phone went live) and when reconcile drops it.
    #[test]
    fn a_camera_follows_the_hub_production_once_its_binding_ends() {
        let dir = tempfile::tempdir().unwrap();
        let cameras = cameras_knowing_dev1(dir.path());
        let hub = cameras.hub();
        let mut flow_production = hub.production();
        flow_production.mains_hz = if flow_production.mains_hz == 60 {
            50
        } else {
            60
        };
        let mut flow = Flow::new("cams");
        flow.blocks = vec![srt_block("cam", serde_json::json!({"camera": "dev1"}))];
        let give = || {
            hub.set_device_production("dev1", Some(flow_production.clone()))
                .unwrap();
            assert!(own_production(&cameras, "dev1").is_some());
        };

        // Stopped after going live.
        cameras.prepare_flow(&flow, &free()).unwrap();
        cameras.mark_started(&|f| *f == flow.id);
        give();
        assert_eq!(cameras.flow_stopped(&flow.id), ["dev1"]);
        assert_eq!(own_production(&cameras, "dev1"), None, "flow stopped");
        assert_eq!(hub.device_production("dev1"), Some(hub.production()));

        // A start that failed before the phone went live.
        cameras.prepare_flow(&flow, &free()).unwrap();
        give();
        assert!(cameras.flow_stopped(&flow.id).is_empty());
        assert_eq!(own_production(&cameras, "dev1"), None, "start failed");

        // A flow that stopped without an event.
        cameras.prepare_flow(&flow, &free()).unwrap();
        cameras.mark_started(&|f| *f == flow.id);
        give();
        assert!(cameras.reconcile(&|_| true).is_empty());
        assert!(own_production(&cameras, "dev1").is_some(), "still running");
        assert_eq!(cameras.reconcile(&|_| false), ["dev1"]);
        assert_eq!(own_production(&cameras, "dev1"), None, "reconciled");
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
