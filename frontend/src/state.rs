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
    /// Element properties loading failed (element_type, error)
    ElementPropertiesError(String, String),

    /// Element pad properties loaded (lazy loading)
    ElementPadPropertiesLoaded(ElementInfo),
    /// Element pad properties loading failed (element_type, error)
    ElementPadPropertiesError(String, String),

    /// Event received from backend via WebSocket
    Event(StromEvent),

    /// WebSocket connection state changed
    ConnectionStateChanged(ConnectionState),

    /// Single flow fetched (for updating after WebSocket events)
    FlowFetched(Box<Flow>),

    /// Request full refresh of flows
    RefreshNeeded,

    /// System information loaded from backend
    SystemInfoLoaded(crate::api::SystemInfo),

    /// Authentication status loaded
    AuthStatusLoaded(crate::api::AuthStatusResponse),
    /// Logout completed
    LogoutComplete,
    /// Session expired (received 401 from API)
    SessionExpired,

    /// WebRTC stats loaded for a flow
    WebRtcStatsLoaded {
        flow_id: strom_types::FlowId,
        stats: strom_types::api::WebRtcStats,
    },

    /// SRT stats loaded for a flow
    SrtStatsLoaded {
        flow_id: strom_types::FlowId,
        stats: strom_types::api::SrtStats,
    },

    /// Flow operation completed successfully
    FlowOperationSuccess(String),
    /// Flow operation failed
    FlowOperationError(String),

    /// Flow created successfully (includes flow ID to navigate to)
    FlowCreated(strom_types::FlowId),

    /// Latency loaded for a flow
    LatencyLoaded {
        flow_id: String,
        latency: crate::api::LatencyResponse,
    },
    /// Latency loading failed (flow not running)
    LatencyNotAvailable(String),

    /// RTP statistics loaded for a flow (jitterbuffer stats from AES67 Input blocks)
    RtpStatsLoaded {
        flow_id: String,
        rtp_stats: strom_types::api::FlowStatsResponse,
    },
    /// RTP statistics not available (flow not running or no RTP blocks)
    RtpStatsNotAvailable(String),

    /// Dynamic pads loaded for a running flow (element_id -> pad_name -> tee_name)
    DynamicPadsLoaded {
        flow_id: String,
        pads: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    },

    /// gst-launch export completed successfully (pipeline string, flow_name)
    GstLaunchExported { pipeline: String, flow_name: String },
    /// gst-launch export failed
    GstLaunchExportError(String),

    /// Network interfaces loaded from API
    NetworkInterfacesLoaded(Vec<strom_types::NetworkInterfaceInfo>),

    /// System clock info loaded from backend
    SystemClockLoaded(crate::api::SystemClockInfo),
    /// System clock info not supported on this platform (backend returned 501)
    SystemClockUnsupported,

    /// Available inter channels loaded from API
    AvailableChannelsLoaded(Vec<strom_types::api::AvailableOutput>),

    /// Discovered streams loaded from API
    DiscoveredStreamsLoaded(Vec<crate::discovery::DiscoveredStream>),
    /// Announced streams loaded from API
    AnnouncedStreamsLoaded(Vec<crate::discovery::AnnouncedStream>),
    /// NDI sources loaded from API
    NdiSourcesLoaded {
        available: bool,
        sources: Vec<crate::discovery::NdiSource>,
    },
    /// SDP loaded for a discovered stream
    StreamSdpLoaded { stream_id: String, sdp: String },
    /// SDP loaded from stream picker (for updating AES67 Input block)
    StreamPickerSdpLoaded { block_id: String, sdp: String },

    /// Media directory listing loaded
    MediaListLoaded(strom_types::api::ListMediaResponse),
    /// Media operation completed successfully
    MediaSuccess(String),
    /// Media operation failed
    MediaError(String),
    /// Request media page refresh
    MediaRefresh,

    /// Log level loaded from API
    LogLevelLoaded { current: String, default: String },
    /// Log level update failed
    LogLevelError(String),

    /// GStreamer debug level loaded from API
    GstLogLevelLoaded { current: String, default: String },
    /// GStreamer debug level update failed
    GstLogLevelError(String),
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
