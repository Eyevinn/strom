//! OpenAPI documentation configuration.

use crate::mcp::handler::JsonRpcRequest;
use strom_types::api::{
    ActivateProbeRequest, ActiveProbesResponse, AnimateInputRequest, AuthStatusResponse,
    AvailableOutput, AvailableSourcesResponse, ClientMessage, CodecStats, CreateDirectoryRequest,
    CreateFlowRequest, DynamicPadsResponse, ElementInfoResponse, ElementListResponse,
    ElementPropertiesResponse, ErrorResponse, ExportGstLaunchRequest, ExportGstLaunchResponse,
    FlowDebugInfo, FlowListResponse, FlowResponse, FlowStatsResponse, IceCandidateStats,
    LatencyResponse, ListMediaResponse, MediaFileEntry, MediaOperationResponse,
    PadPropertiesResponse, ParseGstLaunchRequest, ParseGstLaunchResponse, ProbeInfo, ProbeResponse,
    RenameMediaRequest, RtpStreamStats, ServerMessage, SourceFlowInfo, SystemInfo,
    TransitionResponse, TransportStats, TriggerTransitionRequest, UpdateFlowPropertiesRequest,
    UpdatePadPropertyRequest, UpdatePropertyRequest, WebRtcConnectionStats, WebRtcStats,
    WebRtcStatsResponse,
};
use strom_types::auth::{LoginRequest, LoginResponse};
use strom_types::block::{
    BlockCategoriesResponse, BlockDefinition, BlockInstance, BlockListResponse, BlockResponse,
    CreateBlockRequest, ExposedProperty, ExternalPad, ExternalPads, PropertyMapping, PropertyType,
};
use strom_types::discovery::{
    AnnouncedStreamResponse, DeviceCategory, DeviceCountByCategory, DeviceDiscoveryStatus,
    DeviceResponse, DiscoveredStreamResponse, NdiDiscoveryStatus,
};
use strom_types::events::StromEvent;
use strom_types::flow::{FlowProperties, GStreamerClockType};
use strom_types::mediaplayer::{
    GotoRequest, PlayerAction, PlayerControlRequest, PlayerStateResponse, SeekRequest,
    SetPlaylistRequest,
};
use strom_types::network::{
    Ipv4AddressInfo, Ipv6AddressInfo, NetworkInterfaceInfo, NetworkInterfacesResponse,
};
use strom_types::stats::{BlockStats, StatMetadata, StatValue, Statistic};
use strom_types::whep::{IceServer, IceServersResponse, WhepStreamInfo, WhepStreamsResponse};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::api::flows::list_flows,
        crate::api::flows::create_flow,
        crate::api::flows::get_flow,
        crate::api::flows::update_flow,
        crate::api::flows::update_flow_put,
        crate::api::flows::delete_flow,
        crate::api::flows::start_flow,
        crate::api::flows::stop_flow,
        crate::api::flows::update_flow_properties,
        crate::api::flows::get_flow_rtp_stats,
        crate::api::flows::get_flow_debug_info,
        crate::api::flows::get_element_properties,
        crate::api::flows::update_element_property,
        crate::api::flows::trigger_transition,
        crate::api::flows::select_preview,
        crate::api::flows::toggle_dsk,
        crate::api::flows::fade_to_black,
        crate::api::flows::animate_input,
        crate::api::flows::debug_graph,
        crate::api::flows::get_dynamic_pads,
        crate::api::flows::get_available_sources,
        crate::api::flows::get_block_sdp,
        crate::api::flows::get_flow_latency,
        crate::api::flows::get_webrtc_stats,
        crate::api::flows::get_pad_properties,
        crate::api::flows::update_pad_property,
        crate::api::flows::reset_loudness,
        crate::api::flows::recorder_split_now,
        crate::api::flows::get_block_thumbnail,
        crate::api::elements::list_elements,
        crate::api::elements::get_element_info,
        crate::api::elements::get_element_pad_properties,
        crate::api::blocks::list_blocks,
        crate::api::blocks::get_block,
        crate::api::blocks::create_block,
        crate::api::blocks::update_block,
        crate::api::blocks::delete_block,
        crate::api::blocks::get_categories,
        crate::api::gst_launch::parse_gst_launch,
        crate::api::gst_launch::export_gst_launch,
        crate::api::network::list_interfaces,
        crate::api::version::get_version,
        crate::api::media::list_media,
        crate::api::media::download_file,
        crate::api::media::upload_files,
        crate::api::media::rename_media,
        crate::api::media::delete_file,
        crate::api::media::create_directory,
        crate::api::media::delete_directory,
        // Auth endpoints
        crate::auth::login_handler,
        crate::auth::logout_handler,
        crate::auth::auth_status_handler,
        // WHEP endpoints
        crate::api::whep_player::list_whep_streams,
        crate::api::whep_player::get_ice_servers,
        crate::api::whep_player::whep_endpoint_proxy,
        crate::api::whep_player::whep_endpoint_proxy_options,
        crate::api::whep_player::whep_resource_proxy_patch,
        crate::api::whep_player::whep_resource_proxy_delete,
        crate::api::whep_player::whep_resource_proxy_options,
        // WHIP endpoints
        crate::api::whip_ingest::list_whip_endpoints,
        crate::api::whip_ingest::client_log,
        crate::api::whip_ingest::whip_post,
        crate::api::whip_ingest::whip_options,
        crate::api::whip_ingest::whip_resource_patch,
        crate::api::whip_ingest::whip_resource_delete,
        crate::api::whip_ingest::whip_resource_options,
        // MCP endpoints
        crate::api::mcp::mcp_post,
        crate::api::mcp::mcp_get,
        crate::api::mcp::mcp_delete,
        // Discovery endpoints
        crate::api::discovery::list_streams,
        crate::api::discovery::get_stream,
        crate::api::discovery::get_stream_sdp,
        crate::api::discovery::list_announced,
        crate::api::discovery::device_status,
        crate::api::discovery::list_devices,
        crate::api::discovery::get_device,
        crate::api::discovery::refresh_devices,
        crate::api::discovery::ndi_status,
        crate::api::discovery::list_ndi_sources,
        crate::api::discovery::refresh_ndi_sources,
        // Media player endpoints
        crate::api::mediaplayer::get_player_state,
        crate::api::mediaplayer::set_playlist,
        crate::api::mediaplayer::control_player,
        crate::api::mediaplayer::seek_player,
        crate::api::mediaplayer::goto_file,
        // Probe endpoints
        crate::api::probes::activate_probe,
        crate::api::probes::list_probes,
        crate::api::probes::deactivate_probe,
        // WebSocket endpoint
        crate::api::websocket::websocket_handler,
    ),
    components(
        schemas(
            CreateFlowRequest,
            FlowResponse,
            FlowListResponse,
            FlowProperties,
            GStreamerClockType,
            UpdateFlowPropertiesRequest,
            ElementListResponse,
            ElementInfoResponse,
            ElementPropertiesResponse,
            UpdatePropertyRequest,
            ErrorResponse,
            FlowStatsResponse,
            FlowDebugInfo,
            BlockStats,
            Statistic,
            StatValue,
            StatMetadata,
            BlockDefinition,
            BlockInstance,
            BlockResponse,
            BlockListResponse,
            CreateBlockRequest,
            BlockCategoriesResponse,
            ExposedProperty,
            PropertyMapping,
            PropertyType,
            ExternalPads,
            ExternalPad,
            SystemInfo,
            ParseGstLaunchRequest,
            ParseGstLaunchResponse,
            ExportGstLaunchRequest,
            ExportGstLaunchResponse,
            NetworkInterfacesResponse,
            NetworkInterfaceInfo,
            Ipv4AddressInfo,
            Ipv6AddressInfo,
            ListMediaResponse,
            MediaFileEntry,
            RenameMediaRequest,
            CreateDirectoryRequest,
            MediaOperationResponse,
            // Flow dynamic pads
            DynamicPadsResponse,
            // Flow additional types
            AvailableSourcesResponse,
            AvailableOutput,
            SourceFlowInfo,
            LatencyResponse,
            WebRtcStatsResponse,
            WebRtcStats,
            WebRtcConnectionStats,
            RtpStreamStats,
            IceCandidateStats,
            TransportStats,
            CodecStats,
            UpdatePadPropertyRequest,
            PadPropertiesResponse,
            TriggerTransitionRequest,
            TransitionResponse,
            AnimateInputRequest,
            // Discovery types
            DiscoveredStreamResponse,
            DeviceResponse,
            DeviceCategory,
            AnnouncedStreamResponse,
            DeviceDiscoveryStatus,
            DeviceCountByCategory,
            NdiDiscoveryStatus,
            // Media player types
            PlayerAction,
            PlayerControlRequest,
            SetPlaylistRequest,
            SeekRequest,
            GotoRequest,
            PlayerStateResponse,
            // Auth types
            LoginRequest,
            LoginResponse,
            AuthStatusResponse,
            // WHEP types
            WhepStreamInfo,
            WhepStreamsResponse,
            IceServersResponse,
            IceServer,
            // Probe types
            ActivateProbeRequest,
            ProbeResponse,
            ProbeInfo,
            ActiveProbesResponse,
            // MCP types
            JsonRpcRequest,
            // WebSocket event types
            StromEvent,
            ServerMessage,
            ClientMessage,
        )
    ),
    tags(
        (name = "flows", description = "GStreamer flow management endpoints"),
        (name = "elements", description = "GStreamer element discovery endpoints"),
        (name = "blocks", description = "Reusable block management endpoints"),
        (name = "gst-launch", description = "gst-launch-1.0 import/export endpoints"),
        (name = "Network", description = "Network interface discovery endpoints"),
        (name = "System", description = "System information endpoints"),
        (name = "Media", description = "Media file management endpoints"),
        (name = "auth", description = "Authentication endpoints"),
        (name = "whep", description = "WHEP WebRTC playback endpoints"),
        (name = "whip", description = "WHIP WebRTC ingest endpoints"),
        (name = "mcp", description = "Model Context Protocol (MCP) endpoints"),
        (name = "discovery", description = "AES67 stream and device discovery endpoints"),
        (name = "media_player", description = "Media player control endpoints"),
        (name = "probes", description = "Buffer age probe endpoints"),
        (name = "websocket", description = "WebSocket real-time communication")
    ),
    info(
        title = "Strom API",
        description = "REST and WebSocket API for the Strom pipeline engine",
        license(
            name = "MIT OR Apache-2.0"
        )
    )
)]
pub struct ApiDoc;

/// Build the OpenAPI spec with the version from Cargo.toml.
pub fn openapi_spec() -> utoipa::openapi::OpenApi {
    let mut spec = ApiDoc::openapi();
    spec.info.version = env!("CARGO_PKG_VERSION").to_string();
    spec
}
