//! Phone cameras, the running side: going live with the flow, tally from the
//! vision mixers. The bindings and addresses are in `crate::cameras`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use strom_types::cameras::CameraTally;
use strom_types::{FlowId, StromEvent};
use tallycam_hub::hub::DEFAULT_CALL_TIMEOUT;
use tallycam_hub::protocol::{ControllerEvent, TallyState, Values};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use super::AppState;
use crate::blocks::builtin::vision_mixer::overlay;
use crate::cameras::tally::{self, MixerBuses};
use crate::cameras::{redact_srt_uri, to_hub_tally, ActiveBinding, CameraSettings, Cameras};

/// How often the bindings and tallies are checked without an event (a PiP
/// zone change, a flow that stopped on an error).
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
            let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    ev = events.recv() => match ev {
                        Ok(StromEvent::FlowStarted { flow_id }) => {
                            for (_, binding) in cameras.flow_started(&flow_id) {
                                spawn_go_live(&cameras, binding, Duration::ZERO);
                            }
                            state.update_camera_tally().await;
                        }
                        Ok(StromEvent::FlowStopped { flow_id }) => {
                            for device in cameras.flow_stopped(&flow_id) {
                                spawn_stop(&cameras, device);
                            }
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
                                    spawn_go_live(&cameras, binding, AFTER_HELLO_DELAY);
                                }
                            }
                        }
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    },
                    _ = tick.tick() => {
                        let running = state.running_flow_ids().await;
                        for device in cameras.reconcile(&|f| running.contains(f)) {
                            spawn_stop(&cameras, device);
                        }
                        state.update_camera_tally().await;
                    }
                }
            }
            debug!("Camera service stopped");
        });
    }

    /// Set the tally of every bound camera from the running vision mixers.
    /// The hub only sends changes, so calling this often is cheap.
    pub(crate) async fn update_camera_tally(&self) {
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

/// Tell a bound phone where to send and put it live, after `delay`.
fn spawn_go_live(cameras: &Arc<Cameras>, binding: ActiveBinding, delay: Duration) {
    let cameras = cameras.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        go_live(&cameras, &binding).await;
    });
}

async fn go_live(cameras: &Cameras, binding: &ActiveBinding) {
    let hub = cameras.hub();
    let device = &binding.device;
    if !hub.is_online(device) {
        info!(
            "Camera {} is offline; it goes live when it connects",
            device
        );
        return;
    }
    let Some(host) = cameras.srt_host_for(device) else {
        warn!(
            "Camera {}: no address it can send SRT to; set cameras.public_host",
            device
        );
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
    }
    if let Err(e) = set(&hub, device, "stream_url", Value::String(url)).await {
        warn!("Camera {}: could not set the stream URL: {}", device, e);
        return;
    }
    if let Err(e) = set(&hub, device, "live", Value::Bool(true)).await {
        warn!("Camera {}: could not go live: {}", device, e);
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
