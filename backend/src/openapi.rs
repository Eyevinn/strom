//! OpenAPI documentation configuration.

use crate::mcp::handler::JsonRpcRequest;
use strom_types::api::{
    ActivateProbeRequest, ActiveProbesResponse, AnimateInputRequest, AuthStatusResponse,
    AvailableOutput, AvailableSourcesResponse, BlockPropertiesResponse, ClientMessage, CodecStats,
    CreateDirectoryRequest, DskToggleRequest, DskToggleResponse, DynamicPadsResponse,
    ElementInfoResponse, ElementListResponse, ElementPropertiesResponse, ErrorResponse,
    ExportGstLaunchRequest, ExportGstLaunchResponse, FadeToBlackRequest, FadeToBlackResponse,
    FlowDebugInfo, FlowListResponse, FlowResponse, FlowStatsResponse, GstLogLevelResponse,
    IceCandidateStats, LatencyResponse, ListMediaResponse, LogLevelResponse, MediaFileEntry,
    MediaOperationResponse, OverlayAlphaRequest, OverlayAlphaResponse, PadPropertiesResponse,
    ParseGstLaunchRequest, ParseGstLaunchResponse, PipState, ProbeInfo, ProbeResponse,
    RenameMediaRequest, RtpStreamStats, SelectPreviewRequest, SelectPreviewResponse, ServerMessage,
    SetGstLogLevelRequest, SetLogLevelRequest, SourceFlowInfo, SrtCallerStats, SrtConnectionStats,
    SrtMode, SrtRole, SrtStats, SrtStatsResponse, SystemClockInfo, SystemInfo, TransitionResponse,
    TransportStats, TriggerTransitionRequest, UpdateBlockPropertiesRequest,
    UpdateFlowPropertiesRequest, UpdatePadPropertyRequest, UpdatePipConfigRequest,
    UpdatePipConfigResponse, UpdatePropertyRequest, VisionMixerState, WebRtcConnectionStats,
    WebRtcStats, WebRtcStatsResponse,
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
use utoipa::openapi::schema::{Discriminator, Schema};
use utoipa::openapi::RefOr;
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
        crate::api::flows::get_pip_config,
        crate::api::flows::update_pip_config,
        crate::api::flows::get_vision_mixer_state,
        crate::api::flows::set_overlay_alpha,
        crate::api::flows::toggle_dsk,
        crate::api::flows::fade_to_black,
        crate::api::flows::set_vision_mixer_effect,
        crate::api::vision_mixer_page::get_multiview_endpoint,
        crate::api::flows::animate_input,
        crate::api::flows::debug_graph,
        crate::api::flows::get_flow_pad_caps,
        crate::api::flows::get_dynamic_pads,
        crate::api::flows::get_available_sources,
        crate::api::flows::get_block_sdp,
        crate::api::flows::get_flow_latency,
        crate::api::flows::get_webrtc_stats,
        crate::api::flows::get_srt_stats,
        crate::api::flows::get_pad_properties,
        crate::api::flows::update_pad_property,
        crate::api::flows::reset_loudness,
        crate::api::flows::recorder_split_now,
        crate::api::flows::get_block_properties,
        crate::api::flows::update_block_properties,
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
        crate::api::system_clock::get_system_clock,
        crate::api::logging::get_log_level,
        crate::api::logging::set_log_level,
        crate::api::logging::get_gst_log_level,
        crate::api::logging::set_gst_log_level,
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
            SystemClockInfo,
            LogLevelResponse,
            SetLogLevelRequest,
            GstLogLevelResponse,
            SetGstLogLevelRequest,
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
            SrtStatsResponse,
            SrtStats,
            SrtConnectionStats,
            SrtCallerStats,
            SrtRole,
            SrtMode,
            UpdatePadPropertyRequest,
            PadPropertiesResponse,
            UpdateBlockPropertiesRequest,
            BlockPropertiesResponse,
            TriggerTransitionRequest,
            TransitionResponse,
            AnimateInputRequest,
            // Vision mixer types
            SelectPreviewRequest,
            SelectPreviewResponse,
            UpdatePipConfigRequest,
            UpdatePipConfigResponse,
            VisionMixerState,
            PipState,
            strom_types::vision_mixer::InputResolution,
            strom_types::vision_mixer::NormRect,
            strom_types::vision_mixer::Source,
            strom_types::vision_mixer::SourceCrop,
            strom_types::vision_mixer::Zone,
            strom_types::vision_mixer::ZoneBorder,
            strom_types::api::MultiviewEndpointResponse,
            OverlayAlphaRequest,
            OverlayAlphaResponse,
            DskToggleRequest,
            DskToggleResponse,
            FadeToBlackRequest,
            FadeToBlackResponse,
            strom_types::effects::VideoEffect,
            strom_types::effects::EffectTarget,
            strom_types::effects::SetVideoEffectRequest,
            strom_types::effects::SetVideoEffectResponse,
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

    // Add discriminator hints for tagged enums so TypeScript generators
    // produce proper discriminated union types.
    add_discriminator(&mut spec, "StromEvent", "type");
    add_discriminator(&mut spec, "ServerMessage", "type");
    add_discriminator(&mut spec, "ClientMessage", "type");

    spec
}

/// Add an OpenAPI `discriminator` to a `oneOf` schema, enabling TypeScript
/// generators to emit proper discriminated union types.
fn add_discriminator(spec: &mut utoipa::openapi::OpenApi, schema_name: &str, property: &str) {
    let Some(schemas) = spec.components.as_mut().map(|c| &mut c.schemas) else {
        return;
    };
    let Some(RefOr::T(Schema::OneOf(one_of))) = schemas.get_mut(schema_name) else {
        return;
    };
    one_of.discriminator = Some(Discriminator::new(property));
}
