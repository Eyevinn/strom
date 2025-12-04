//! Application state and channel-based IPC for async operations.

use std::sync::mpsc::{channel, Receiver, Sender};
use strom_types::element::ElementInfo;
use strom_types::{BlockDefinition, Flow, StromEvent};

/// Messages sent from async operations to the main UI thread.
#[derive(Debug)]
pub enum AppMessage {
    /// Flows loaded from API
    FlowsLoaded(Vec<Flow>),
    /// Flows loading failed
    FlowsError(String),

    /// Elements loaded from API
    ElementsLoaded(Vec<ElementInfo>),
    /// Elements loading failed
    ElementsError(String),

    /// Blocks loaded from API
    BlocksLoaded(Vec<BlockDefinition>),
    /// Blocks loading failed
    BlocksError(String),

    /// Element properties loaded (lazy loading)
    ElementPropertiesLoaded(ElementInfo),
    /// Element properties loading failed
    ElementPropertiesError(String),

    /// Element pad properties loaded (lazy loading)
    ElementPadPropertiesLoaded(ElementInfo),
    /// Element pad properties loading failed
    ElementPadPropertiesError(String),

    /// SDP loaded for a block
    SdpLoaded {
        flow_id: String,
        block_id: String,
        sdp: String,
    },
    /// SDP loading failed
    SdpError(String),

    /// Event received from backend via WebSocket
    Event(StromEvent),

    /// WebSocket connection state changed
    ConnectionStateChanged(ConnectionState),

    /// Single flow fetched (for updating after WebSocket events)
    FlowFetched(Flow),

    /// Request full refresh of flows
    RefreshNeeded,

    /// Version information loaded from backend
    VersionLoaded(crate::api::VersionInfo),

    /// Authentication status loaded
    AuthStatusLoaded(crate::api::AuthStatusResponse),
    /// Login result received
    LoginResult(crate::api::LoginResponse),
    /// Logout completed
    LogoutComplete,

    /// WebRTC stats loaded for a flow
    WebRtcStatsLoaded {
        flow_id: strom_types::FlowId,
        stats: strom_types::api::WebRtcStats,
    },
    /// WebRTC stats loading failed
    WebRtcStatsError(String),

    /// Flow operation completed successfully
    FlowOperationSuccess(String),
    /// Flow operation failed
    FlowOperationError(String),

    /// Flow created successfully (includes flow ID to navigate to)
    FlowCreated(strom_types::FlowId),

    /// Latency loaded for a flow
    LatencyLoaded {
        flow_id: String,
        latency: crate::api::LatencyInfo,
    },
    /// Latency loading failed (flow not running)
    LatencyNotAvailable(String),

    /// Statistics loaded for a flow
    StatsLoaded {
        flow_id: String,
        stats: crate::api::FlowStatsInfo,
    },
    /// Statistics loading failed (flow not running)
    StatsNotAvailable(String),

    /// gst-launch export completed successfully (pipeline string, flow_name)
    GstLaunchExported { pipeline: String, flow_name: String },
    /// gst-launch export failed
    GstLaunchExportError(String),

    /// Network interfaces loaded from API
    NetworkInterfacesLoaded(Vec<strom_types::NetworkInterfaceInfo>),
}

/// WebSocket connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connected to backend
    Connected,
    /// Disconnected from backend
    Disconnected,
    /// Attempting to reconnect
    Reconnecting { attempt: u32 },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    pub fn description(&self) -> &'static str {
        match self {
            ConnectionState::Connected => "Connected",
            ConnectionState::Disconnected => "Disconnected",
            ConnectionState::Reconnecting { .. } => "Reconnecting",
        }
    }
}

/// Application state with channel-based communication.
pub struct AppStateChannels {
    /// Sender for app messages (cloned for each async operation)
    pub tx: Sender<AppMessage>,
    /// Receiver for app messages (owned by main UI thread)
    pub rx: Receiver<AppMessage>,
}

impl AppStateChannels {
    /// Create new application state channels.
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx }
    }

    /// Get a clone of the sender for use in async operations.
    pub fn sender(&self) -> Sender<AppMessage> {
        self.tx.clone()
    }
}

impl Default for AppStateChannels {
    fn default() -> Self {
        Self::new()
    }
}
