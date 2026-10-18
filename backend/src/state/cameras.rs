//! Phone cameras, the running side: going live with the flow, tally from the
//! vision mixers. The bindings and addresses are in `crate::cameras`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use strom_types::cameras::CameraTally;
use strom_types::ports::{PortPoolStatus, PortState};
use strom_types::{Flow, FlowId, PropertyValue, StromEvent};
use tallycam_hub::hub::DEFAULT_CALL_TIMEOUT;
use tallycam_hub::protocol::{ControllerEvent, TallyState, Values};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error, info, warn};

use super::AppState;
use crate::blocks::builtin::vision_mixer::overlay;
use crate::cameras::labels;
use crate::cameras::tally::{self, MixerBuses};
use crate::cameras::{
    redact_srt_uri, to_hub_tally, ActiveBinding, CameraSettings, Cameras, PortHolder,
};
use crate::gst::PipelineError;

/// How often the bindings and tallies are checked without an event, for
/// changes nothing announces (a flow that died without a teardown).
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
/// A phone sends its full `state` right after `hello`; wait for it before
/// comparing its stream URL with ours.
const AFTER_HELLO_DELAY: Duration = Duration::from_millis(500);

impl AppState {
    pub fn cameras(&self) -> &Arc<Cameras> {
        &self.inner.cameras
    }

    /// Keep known cameras in `data_dir` and apply `settings`. Called once from
    /// main, before the server accepts connections.
    pub fn configure_cameras(&self, data_dir: &Path, settings: CameraSettings) {
        self.inner.cameras.configure(data_dir, settings);
    }

    /// Bind the cameras of `flow` before it is built (see
    /// [`Cameras::prepare_flow`]). Returns the copy of the flow to build when
    /// it has cameras.
    pub(super) async fn prepare_cameras(&self, flow: &Flow) -> Result<Option<Flow>, PipelineError> {
        let pool = self.port_pool_status().await;
        let taken = self.srt_ports_taken(flow.id, &pool).await;
        let build = self.inner.cameras.prepare_flow(flow, &taken).map_err(|e| {
            error!("Cannot start flow {}: {}", flow.id, e);
            PipelineError::InvalidFlow(e)
        })?;
        // A free pool port can still be handed to a reservation later, and
        // nothing would then notice the clash.
        for listener in crate::cameras::listeners(flow).iter().filter(|l| l.camera) {
            if pool
                .ports
                .iter()
                .any(|s| (s.first..=s.last).contains(&listener.port))
            {
                warn!(
                    "Camera block {} of flow {} listens on port {}, which is in the port pool; \
                     pick a port outside the pool so no reservation is given it",
                    listener.block_id, flow.id, listener.port
                );
            }
        }
        Ok(build)
    }

    /// The SRT listener ports of running flows other than `starting`, and the
    /// ports the port pool holds for someone other than `starting`.
    async fn srt_ports_taken(
        &self,
        starting: FlowId,
        pool: &PortPoolStatus,
    ) -> HashMap<u16, PortHolder> {
        let mut taken = HashMap::new();
        for entry in pool.entries.iter().cloned() {
            let holder = match (entry.state, entry.flow_id) {
                (PortState::Assigned, Some(f)) if f == starting => continue,
                (PortState::Blocked, _) => continue,
                (PortState::Assigned, Some(f)) => {
                    format!("the port pool (assigned to flow {f})")
                }
                _ => format!(
                    "the port pool (reserved by '{}')",
                    entry.owner_id.unwrap_or_default()
                ),
            };
            taken.insert(
                entry.port,
                PortHolder {
                    description: holder,
                    camera: false,
                },
            );
        }
        let running = self.running_flow_ids().await;
        let flows = self.inner.flows.read().await;
        for flow in flows
            .values()
            .filter(|f| f.id != starting && running.contains(&f.id))
        {
            for listener in crate::cameras::listeners(flow) {
                if taken
                    .get(&listener.port)
                    .is_some_and(|h: &PortHolder| h.camera)
                {
                    continue;
                }
                taken.insert(
                    listener.port,
                    PortHolder {
                        description: format!(
                            "block '{}' of running flow '{}'",
                            listener.block_id, flow.name
                        ),
                        camera: listener.camera,
                    },
                );
            }
        }
        taken
    }

    /// The flow is torn down (stopped, or its start failed): its phones stop
    /// streaming and their tally goes off. Done in the teardown rather than
    /// on `FlowStopped`, which arrives later and could undo a quick restart.
    pub(super) fn release_cameras(&self, flow_id: &FlowId) {
        for device in self.inner.cameras.flow_stopped(flow_id) {
            spawn_stop(&self.inner.cameras, device);
        }
    }

    async fn running_flow_ids(&self) -> HashSet<FlowId> {
        self.inner.pipelines.read().await.keys().copied().collect()
    }

    /// Start the task that puts bound phones live with their flows and keeps
    /// their tally in step with the vision mixers.
    pub fn start_camera_service(&self) {
        let state = self.clone();
        let mut events = self.inner.events.subscribe();
        let mut hub_events = self.inner.cameras.hub().subscribe();
        tokio::spawn(async move {
            let cameras = state.inner.cameras.clone();
            // The names the cameras have now are not changes.
            cameras.note_names();
            let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    ev = events.recv() => match ev {
                        Ok(StromEvent::FlowStarted { flow_id }) => {
                            let running = state.running_flow_ids().await;
                            for (_, binding) in cameras.mark_started(&|f| *f == flow_id && running.contains(f)) {
                                spawn_go_live(&cameras, binding, Duration::ZERO, false);
                            }
                            state.update_camera_tally().await;
                            state.relabel_camera_inputs(Some(flow_id), None).await;
                        }
                        Ok(StromEvent::VisionMixerStateChanged { .. }) => {
                            state.update_camera_tally().await;
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => state.update_camera_tally().await,
                        Err(RecvError::Closed) => break,
                    },
                    ev = hub_events.recv() => match ev {
                        Ok(frame) => {
                            if let ControllerEvent::DeviceHello { device, .. } = &*frame.event {
                                if let Some((_, binding)) = cameras.started_binding_of(device) {
                                    spawn_go_live(&cameras, binding, AFTER_HELLO_DELAY, false);
                                }
                                let renamed = cameras.note_names();
                                if !renamed.is_empty() {
                                    state.relabel_camera_inputs(None, Some(&renamed)).await;
                                }
                            }
                        }
                        // A missed hello: offer every live binding again;
                        // go_live leaves a phone that is already right alone.
                        Err(RecvError::Lagged(_)) => {
                            for (_, binding) in cameras.started_bindings() {
                                spawn_go_live(&cameras, binding, Duration::ZERO, false);
                            }
                        }
                        Err(RecvError::Closed) => break,
                    },
                    _ = tick.tick() => {
                        let running = state.running_flow_ids().await;
                        // A FlowStarted we missed.
                        let missed: HashSet<FlowId> = cameras
                            .mark_started(&|f| running.contains(f))
                            .into_iter()
                            .map(|((flow_id, _), _)| flow_id)
                            .collect();
                        for device in cameras.reconcile(&|f| running.contains(f)) {
                            spawn_stop(&cameras, device);
                        }
                        // Every live binding, so a failed go-live is tried
                        // again; a phone that is already right is left alone.
                        for (_, binding) in cameras.started_bindings() {
                            spawn_go_live(&cameras, binding, Duration::ZERO, true);
                        }
                        state.update_camera_tally().await;
                        for flow_id in missed {
                            state.relabel_camera_inputs(Some(flow_id), None).await;
                        }
                    }
                }
            }
            debug!("Camera service stopped");
        });
    }

    /// Set the tally of every bound camera from the running vision mixers.
    /// The hub only sends changes, so calling this often is cheap.
    async fn update_camera_tally(&self) {
        let bindings = self.inner.cameras.started_bindings();
        if bindings.is_empty() {
            return;
        }
        let running = self.running_flow_ids().await;
        let mut by_flow: HashMap<FlowId, HashMap<String, String>> = HashMap::new();
        let mut result: HashMap<String, CameraTally> = HashMap::new();
        for ((flow_id, block_id), binding) in bindings {
            result.insert(binding.device.clone(), CameraTally::Off);
            by_flow
                .entry(flow_id)
                .or_default()
                .insert(block_id, binding.device);
        }
        {
            let flows = self.inner.flows.read().await;
            for (flow_id, cameras) in &by_flow {
                let Some(flow) = flows.get(flow_id).filter(|_| running.contains(flow_id)) else {
                    continue;
                };
                let mixers: Vec<(String, MixerBuses)> = flow
                    .blocks
                    .iter()
                    .filter(|b| b.block_definition_id == "builtin.vision_mixer")
                    .filter_map(|b| {
                        overlay::get_overlay_state(flow_id, &b.id)
                            .map(|s| (b.id.clone(), MixerBuses::from_overlay(&s)))
                    })
                    .collect();
                for (device, t) in tally::resolve_flow(flow, cameras, &mixers) {
                    let entry = result.entry(device).or_default();
                    *entry = tally::combine(*entry, t);
                }
            }
        }
        let hub = self.inner.cameras.hub();
        for (device, t) in result {
            // An unknown device (bound but never connected) has no tally.
            let _ = hub.set_tally(&device, to_hub_tally(t));
        }
    }
}

impl AppState {
    /// Label the vision mixer inputs of running flows that show a camera with
    /// the camera's display name, where the label is not the operator's own
    /// (see [`labels::should_relabel`]). Limited to `only_flow` and to
    /// `only_devices` when given. Persisted like any label change; a changed
    /// flow is announced with `FlowUpdated` so clients reload it.
    async fn relabel_camera_inputs(
        &self,
        only_flow: Option<FlowId>,
        only_devices: Option<&[String]>,
    ) {
        let cameras = &self.inner.cameras;
        let names = cameras.display_names();
        let camera_names = cameras.camera_names();
        let running = self.running_flow_ids().await;
        let mut changes: Vec<(FlowId, String, HashMap<String, PropertyValue>)> = Vec::new();
        {
            let flows = self.inner.flows.read().await;
            for flow in flows
                .values()
                .filter(|f| running.contains(&f.id) && only_flow.is_none_or(|id| id == f.id))
            {
                let by_mixer = labels::relabels(flow, &names, &camera_names, only_devices);
                changes.extend(by_mixer.into_iter().map(|(m, p)| (flow.id, m, p)));
            }
        }
        let mut updated = HashSet::new();
        for (flow_id, mixer, properties) in changes {
            let wanted = properties.len();
            match self
                .update_block_properties(&flow_id, &mixer, properties, None, None)
                .await
            {
                Ok((_, rejected)) => {
                    for (name, reason) in &rejected {
                        warn!(
                            "Camera label {} of block {} in flow {} not applied: {}",
                            name, mixer, flow_id, reason
                        );
                    }
                    if rejected.len() < wanted {
                        info!(
                            "Labelled {} input(s) of vision mixer {} in flow {} with camera names",
                            wanted - rejected.len(),
                            mixer,
                            flow_id
                        );
                        updated.insert(flow_id);
                    }
                }
                Err(e) => warn!(
                    "Camera labels of block {} in flow {} not applied: {}",
                    mixer, flow_id, e
                ),
            }
        }
        for flow_id in updated {
            self.inner
                .events
                .broadcast(StromEvent::FlowUpdated { flow_id });
        }
    }
}

/// Tell a bound phone where to send and put it live, after `delay`. Skipped
/// while an attempt for the phone is in flight; a `retry` also waits a while
/// after the last attempt (see [`Cameras::begin_go_live`]).
fn spawn_go_live(cameras: &Arc<Cameras>, binding: ActiveBinding, delay: Duration, retry: bool) {
    let cameras = cameras.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if !cameras.begin_go_live(&binding.device, retry) {
            return;
        }
        go_live(&cameras, &binding, retry).await;
        cameras.end_go_live(&binding.device);
    });
}

/// A `retry` reports a phone it cannot reach at debug level only: the first
/// attempt already said so.
async fn go_live(cameras: &Cameras, binding: &ActiveBinding, retry: bool) {
    let hub = cameras.hub();
    let device = &binding.device;
    // A binding of an earlier run must not touch the phone: its URL carries
    // the old passphrase. Checked again after every call that waits.
    if !cameras.is_current(binding) {
        return;
    }
    if !hub.is_online(device) {
        if !retry {
            info!(
                "Camera {} is offline; it goes live when it connects",
                device
            );
        }
        return;
    }
    let Some(host) = cameras.srt_host_for(device) else {
        if retry {
            debug!("Camera {}: still no address it can send SRT to", device);
        } else {
            warn!(
                "Camera {}: no address it can send SRT to; set cameras.public_host",
                device
            );
        }
        return;
    };
    let url = binding.stream_url(&host);
    let state = hub.device_state(device);
    let live = state.get("live") == Some(&Value::Bool(true));
    let same_url = state.get("stream_url").and_then(Value::as_str) == Some(url.as_str());
    if live && same_url {
        debug!("Camera {} is already live to its flow", device);
        return;
    }
    info!("Camera {}: streaming to {}", device, redact_srt_uri(&url));
    // A new URL applies on the next Go Live, so a phone live elsewhere stops
    // first.
    if live {
        if let Err(e) = set(&hub, device, "live", Value::Bool(false)).await {
            warn!("Camera {}: could not stop the old stream: {}", device, e);
            return;
        }
        if !cameras.is_current(binding) {
            return;
        }
    }
    if let Err(e) = set(&hub, device, "stream_url", Value::String(url)).await {
        warn!("Camera {}: could not set the stream URL: {}", device, e);
        return;
    }
    // The flow can stop while the calls above wait on the phone; going live
    // then would leave it streaming to a closed port.
    if !cameras.is_current(binding) {
        debug!("Camera {}: its flow stopped while going live", device);
        return;
    }
    if let Err(e) = set(&hub, device, "live", Value::Bool(true)).await {
        warn!("Camera {}: could not go live: {}", device, e);
    }
    // A stop that raced the call above may have landed first.
    if !cameras.is_current(binding) {
        info!(
            "Camera {}: flow stopped while going live, stopping its stream",
            device
        );
        if let Err(e) = set(&hub, device, "live", Value::Bool(false)).await {
            warn!("Camera {}: could not stop streaming: {}", device, e);
        }
    }
}

/// The phone's flow stopped: stop streaming, tally off.
fn spawn_stop(cameras: &Arc<Cameras>, device: String) {
    let hub = cameras.hub();
    let _ = hub.set_tally(&device, TallyState::Off);
    tokio::spawn(async move {
        if !hub.is_online(&device) {
            return;
        }
        info!("Camera {}: flow stopped, stopping its stream", device);
        if let Err(e) = set(&hub, &device, "live", Value::Bool(false)).await {
            warn!("Camera {}: could not stop streaming: {}", device, e);
        }
    });
}

async fn set(
    hub: &tallycam_hub::Hub,
    device: &str,
    control: &str,
    value: Value,
) -> Result<(), tallycam_hub::hub::CallError> {
    let mut values = Values::new();
    values.insert(control.to_string(), value);
    hub.set(device, values, DEFAULT_CALL_TIMEOUT)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::JsonFileStorage;
    use strom_types::BlockInstance;
    use tempfile::TempDir;

    fn new_state(dir: &TempDir) -> AppState {
        gstreamer::init().unwrap();
        AppState::new(
            JsonFileStorage::new(dir.path().join("flows.json")),
            dir.path().join("blocks.json"),
            dir.path().join("media"),
            vec![],
            "all".to_string(),
            vec![],
            false,
            false,
        )
    }

    fn block(id: &str, definition: &str, properties: serde_json::Value) -> BlockInstance {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "block_definition_id": definition,
            "properties": properties,
            "position": {"x": 0.0, "y": 0.0},
        }))
        .unwrap()
    }

    fn free_udp_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn add_flow(state: &AppState, name: &str, blocks: Vec<BlockInstance>) -> FlowId {
        let mut flow = Flow::new(name);
        flow.blocks = blocks;
        let id = flow.id;
        state.upsert_flow(flow).await.unwrap();
        id
    }

    fn camera_block(port: u16) -> BlockInstance {
        block(
            "cam",
            crate::cameras::SRT_INPUT_BLOCK,
            serde_json::json!({
                "camera": "phone-1",
                "srt_uri": format!("srt://:{port}?mode=listener"),
            }),
        )
    }

    /// The stored flow (memory, flow API, flows.json) never holds the
    /// passphrase, while the camera binding does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn started_camera_flow_stores_no_passphrase() {
        let dir = TempDir::new().unwrap();
        let state = new_state(&dir);
        let port = free_udp_port();
        let id = add_flow(&state, "cams", vec![camera_block(port)]).await;

        state.start_flow(&id).await.expect("camera flow starts");
        let (_, binding) = state
            .cameras()
            .mark_started(&|f| *f == id)
            .pop()
            .expect("camera bound");
        assert!(!binding.passphrase.is_empty());

        let stored = serde_json::to_string(&state.get_flow(&id).await.unwrap()).unwrap();
        assert!(!stored.contains(&binding.passphrase), "{stored}");
        assert!(!stored.contains("passphrase"), "{stored}");
        let file = std::fs::read_to_string(dir.path().join("flows.json")).unwrap();
        assert!(!file.contains(&binding.passphrase), "{file}");
        assert!(!file.contains("passphrase"), "{file}");

        state.stop_flow(&id).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn camera_port_used_by_a_running_flow_or_the_pool_is_refused() {
        let dir = TempDir::new().unwrap();
        let state = new_state(&dir);

        // A running flow listens on the port.
        let port = free_udp_port();
        let plain = add_flow(
            &state,
            "Studio",
            vec![block(
                "srt",
                crate::cameras::SRT_INPUT_BLOCK,
                serde_json::json!({"srt_uri": format!("srt://:{port}?mode=listener")}),
            )],
        )
        .await;
        state
            .start_flow(&plain)
            .await
            .expect("plain SRT flow starts");
        let cams = add_flow(&state, "cams", vec![camera_block(port)]).await;
        let err = state.start_flow(&cams).await.unwrap_err().to_string();
        assert!(
            err.contains(&format!("SRT port {port}"))
                && err.contains("block 'srt' of running flow 'Studio'"),
            "{err}"
        );
        assert!(state.get_flow_state(&cams).await.is_none());
        state.stop_flow(&plain).await.unwrap();

        // The port pool holds it for someone else.
        let pool_port = free_udp_port();
        state
            .configure_port_pool(
                std::collections::BTreeSet::from([pool_port]),
                crate::ports::PortReservationStore::new(dir.path()),
                600,
                false,
            )
            .await
            .unwrap();
        state
            .reserve_ports("other", 1, None)
            .await
            .unwrap()
            .unwrap();
        let pooled = add_flow(&state, "pooled", vec![camera_block(pool_port)]).await;
        let err = state.start_flow(&pooled).await.unwrap_err().to_string();
        assert!(err.contains("the port pool (reserved by 'other')"), "{err}");
    }

    /// Tally follows `VisionMixerStateChanged`, so a PiP change that alters
    /// what a bus shows must broadcast it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pip_changes_on_a_bus_broadcast_the_mixer_state() {
        let dir = TempDir::new().unwrap();
        let state = new_state(&dir);
        crate::gpu::detect_gpu_capabilities();
        let id = add_flow(
            &state,
            "mixer",
            vec![block(
                "vm",
                "builtin.vision_mixer",
                serde_json::json!({
                    "compositor_preference": "cpu",
                    "num_inputs": 2,
                    "num_pips": 2,
                }),
            )],
        )
        .await;
        state
            .start_flow(&id)
            .await
            .expect("vision mixer flow starts");
        let mut events = state.inner.events.subscribe();
        let mut next_state = || {
            let mut found = None;
            while let Ok(ev) = events.try_recv() {
                if let StromEvent::VisionMixerStateChanged { preview_pip, .. } = ev {
                    found = Some(preview_pip);
                }
            }
            found
        };

        state
            .select_vision_mixer_pip_for_preview(&id, "vm", 0)
            .await
            .unwrap();
        assert_eq!(next_state(), Some(Some(0)), "PiP selected on preview");

        let zone = |sources: Vec<usize>| strom_types::vision_mixer::Zone {
            rect: None,
            capacity: None,
            sources,
            border: None,
        };
        state
            .apply_vision_mixer_pip_config(
                &id,
                "vm",
                0,
                Some(0),
                vec![zone(vec![1])],
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(next_state(), Some(Some(0)), "PiP on preview changed");

        state
            .apply_vision_mixer_pip_config(
                &id,
                "vm",
                1,
                Some(1),
                vec![zone(vec![0])],
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(next_state(), None, "PiP on no bus: nothing visible changed");

        state.stop_flow(&id).await.unwrap();
    }
}
