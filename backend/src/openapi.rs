//! OpenAPI documentation configuration.

use crate::api::flows::DynamicPadsResponse;
use crate::api::whep_player::{IceServer, IceServersResponse, WhepStreamInfo, WhepStreamsResponse};
use crate::auth::{AuthStatusResponse, LoginRequest, LoginResponse};
use crate::mcp::handler::JsonRpcRequest;
use crate::version::VersionInfo;
use strom_types::api::{
    CreateDirectoryRequest, CreateFlowRequest, ElementInfoResponse, ElementListResponse,
    ElementPropertiesResponse, ErrorResponse, ExportGstLaunchRequest, ExportGstLaunchResponse,
    FlowDebugInfo, FlowListResponse, FlowResponse, FlowStatsResponse, ListMediaResponse,
    MediaFileEntry, MediaOperationResponse, ParseGstLaunchRequest, ParseGstLaunchResponse,
    RenameMediaRequest, UpdateFlowPropertiesRequest, UpdatePropertyRequest,
};
use strom_types::block::{
    BlockCategoriesResponse, BlockDefinition, BlockInstance, BlockListResponse, BlockResponse,
    CreateBlockRequest, ExposedProperty, ExternalPad, ExternalPads, PropertyMapping, PropertyType,
};
use strom_types::flow::{FlowProperties, GStreamerClockType};
use strom_types::network::{
    Ipv4AddressInfo, Ipv6AddressInfo, NetworkInterfaceInfo, NetworkInterfacesResponse,
};
use strom_types::stats::{BlockStats, StatMetadata, StatValue, Statistic};
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
        crate::api::flows::animate_input,
        crate::api::flows::debug_graph,
        crate::api::flows::get_dynamic_pads,
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
        // MCP endpoints
        crate::api::mcp::mcp_post,
        crate::api::mcp::mcp_get,
        crate::api::mcp::mcp_delete,
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
            VersionInfo,
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
            // Auth types
            LoginRequest,
            LoginResponse,
            AuthStatusResponse,
            // WHEP types
            WhepStreamInfo,
            WhepStreamsResponse,
            IceServersResponse,
            IceServer,
            // MCP types
            JsonRpcRequest,
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
        (name = "mcp", description = "Model Context Protocol (MCP) endpoints"),
        (name = "websocket", description = "WebSocket real-time communication")
    ),
    info(
        title = "Strom GStreamer Flow Engine API",
        version = "0.1.0",
        description = "REST API for managing GStreamer pipelines through a visual flow interface",
        license(
            name = "MIT OR Apache-2.0"
        )
    )
)]
pub struct ApiDoc;
