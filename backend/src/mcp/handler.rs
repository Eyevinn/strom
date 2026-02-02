//! MCP JSON-RPC request handler.
//!
//! Handles MCP protocol methods and tool calls with direct AppState access.

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use strom_types::element::PropertyValue;
use strom_types::flow::GStreamerClockType;
use strom_types::Flow;
use tracing::{debug, error, info};

/// MCP protocol version we support.
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// JSON-RPC 2.0 Request.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 Response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Create a success response.
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Create an error response with data.
    pub fn error_with_data(
        id: Option<Value>,
        code: i32,
        message: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: Some(data),
            }),
        }
    }
}

/// JSON-RPC 2.0 Error.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Tool call parameters from MCP.
#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

/// MCP request handler with direct AppState access.
pub struct McpHandler;

impl McpHandler {
    /// Handle an MCP JSON-RPC request.
    pub async fn handle_request(
        state: &AppState,
        request: JsonRpcRequest,
    ) -> Option<JsonRpcResponse> {
        let id = request.id.clone();
        debug!("MCP: Handling method: {}", request.method);

        match request.method.as_str() {
            "initialize" => Some(Self::handle_initialize(id)),
            "initialized" => {
                // Notification, no response needed
                None
            }
            "ping" => Some(JsonRpcResponse::success(id, json!({}))),
            "tools/list" => Some(Self::handle_list_tools(id)),
            "tools/call" => {
                let result =
                    Self::handle_call_tool(state, request.params.unwrap_or(json!({}))).await;
                match result {
                    Ok(value) => Some(JsonRpcResponse::success(id, value)),
                    Err(e) => Some(JsonRpcResponse::error(
                        id,
                        -32603,
                        format!("Tool call failed: {}", e),
                    )),
                }
            }
            "notifications/cancelled" => {
                // Client cancelled a request - acknowledge
                None
            }
            _ => Some(JsonRpcResponse::error(
                id,
                -32601,
                format!("Method not found: {}", request.method),
            )),
        }
    }

    /// Handle the initialize request.
    fn handle_initialize(id: Option<Value>) -> JsonRpcResponse {
        JsonRpcResponse::success(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "strom",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
    }

    /// Handle the tools/list request.
    fn handle_list_tools(id: Option<Value>) -> JsonRpcResponse {
        JsonRpcResponse::success(
            id,
            json!({
                "tools": [
                    {
                        "name": "list_flows",
                        "description": "List all GStreamer flows",
                        "inputSchema": {
                            "type": "object",
                            "properties": {},
                            "required": []
                        }
                    },
                    {
                        "name": "get_flow",
                        "description": "Get details of a specific flow by ID",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the flow"
                                }
                            },
                            "required": ["flow_id"]
                        }
                    },
                    {
                        "name": "create_flow",
                        "description": "Create a new flow",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "name": {
                                    "type": "string",
                                    "description": "Name for the new flow"
                                }
                            },
                            "required": ["name"]
                        }
                    },
                    {
                        "name": "update_flow",
                        "description": "Update a flow's elements, links, and properties",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the flow to update"
                                },
                                "flow": {
                                    "type": "object",
                                    "description": "Complete flow object with id, name, elements, links, blocks, and state"
                                }
                            },
                            "required": ["flow_id", "flow"]
                        }
                    },
                    {
                        "name": "delete_flow",
                        "description": "Delete a flow",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the flow to delete"
                                }
                            },
                            "required": ["flow_id"]
                        }
                    },
                    {
                        "name": "start_flow",
                        "description": "Start a flow's GStreamer pipeline",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the flow to start"
                                }
                            },
                            "required": ["flow_id"]
                        }
                    },
                    {
                        "name": "stop_flow",
                        "description": "Stop a running flow",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the flow to stop"
                                }
                            },
                            "required": ["flow_id"]
                        }
                    },
                    {
                        "name": "update_flow_properties",
                        "description": "Update flow properties like description and GStreamer clock type",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the flow"
                                },
                                "description": {
                                    "type": "string",
                                    "description": "Optional human-readable description (multiline supported)"
                                },
                                "clock_type": {
                                    "type": "string",
                                    "enum": ["monotonic", "realtime", "ptp", "ntp"],
                                    "description": "Optional GStreamer clock type. Default is 'monotonic'."
                                }
                            },
                            "required": ["flow_id"]
                        }
                    },
                    {
                        "name": "list_elements",
                        "description": "List available GStreamer elements, optionally filtered by category",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "category": {
                                    "type": "string",
                                    "description": "Optional category filter (e.g., 'source', 'codec', 'sink')"
                                }
                            },
                            "required": []
                        }
                    },
                    {
                        "name": "get_element_info",
                        "description": "Get detailed information about a specific GStreamer element",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "element_name": {
                                    "type": "string",
                                    "description": "Name of the GStreamer element (e.g., 'videotestsrc', 'x264enc')"
                                }
                            },
                            "required": ["element_name"]
                        }
                    },
                    {
                        "name": "get_element_properties",
                        "description": "Get current property values from a running pipeline element. The flow must be started for this to work.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the running flow"
                                },
                                "element_id": {
                                    "type": "string",
                                    "description": "The element instance ID (e.g., 'src', 'encoder', 'sink')"
                                }
                            },
                            "required": ["flow_id", "element_id"]
                        }
                    },
                    {
                        "name": "update_element_property",
                        "description": "Update a property on a running pipeline element. Allows live modification of properties like bitrate, volume, brightness, etc. Only properties marked as mutable in the current pipeline state can be updated. Check element info to see which properties support live updates (mutable_in_playing flag).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "flow_id": {
                                    "type": "string",
                                    "description": "The UUID of the running flow"
                                },
                                "element_id": {
                                    "type": "string",
                                    "description": "The element instance ID"
                                },
                                "property_name": {
                                    "type": "string",
                                    "description": "The name of the property to update"
                                },
                                "value": {
                                    "description": "The new property value (can be string, number, or boolean)"
                                }
                            },
                            "required": ["flow_id", "element_id", "property_name", "value"]
                        }
                    }
                ]
            }),
        )
    }

    /// Handle a tools/call request.
    async fn handle_call_tool(state: &AppState, params: Value) -> anyhow::Result<Value> {
        let tool_params: ToolCallParams = serde_json::from_value(params)?;
        let args = tool_params.arguments.unwrap_or(json!({}));

        let result = match tool_params.name.as_str() {
            "list_flows" => {
                info!("MCP: Listing all flows");
                let flows = state.get_flows().await;
                json!({ "flows": flows })
            }

            "get_flow" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                info!("MCP: Getting flow {}", flow_id);
                let flow_uuid: strom_types::FlowId = flow_id.parse()?;
                let flow = state
                    .get_flow(&flow_uuid)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Flow not found: {}", flow_id))?;
                json!({ "flow": flow })
            }

            "create_flow" => {
                let name = args["name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("name is required"))?;
                info!("MCP: Creating flow '{}'", name);
                let flow = Flow::new(name.to_string());
                state.upsert_flow(flow.clone()).await?;
                json!({ "flow": flow })
            }

            "update_flow" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                let flow: Flow = serde_json::from_value(args["flow"].clone())
                    .map_err(|e| anyhow::anyhow!("Invalid flow object: {}", e))?;
                info!("MCP: Updating flow {}", flow_id);
                state.upsert_flow(flow.clone()).await?;
                json!({ "flow": flow })
            }

            "delete_flow" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                info!("MCP: Deleting flow {}", flow_id);
                let flow_uuid: strom_types::FlowId = flow_id.parse()?;
                let deleted = state.delete_flow(&flow_uuid).await?;
                if !deleted {
                    return Err(anyhow::anyhow!("Flow not found: {}", flow_id));
                }
                json!({ "success": true, "message": format!("Flow {} deleted", flow_id) })
            }

            "start_flow" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                info!("MCP: Starting flow {}", flow_id);
                let flow_uuid: strom_types::FlowId = flow_id.parse()?;
                let _state = state.start_flow(&flow_uuid).await?;
                json!({ "success": true, "message": format!("Flow {} started", flow_id) })
            }

            "stop_flow" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                info!("MCP: Stopping flow {}", flow_id);
                let flow_uuid: strom_types::FlowId = flow_id.parse()?;
                let _state = state.stop_flow(&flow_uuid).await?;
                json!({ "success": true, "message": format!("Flow {} stopped", flow_id) })
            }

            "update_flow_properties" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                info!("MCP: Updating properties for flow {}", flow_id);
                let flow_uuid: strom_types::FlowId = flow_id.parse()?;

                // Get current flow
                let mut flow = state
                    .get_flow(&flow_uuid)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Flow not found: {}", flow_id))?;

                // Update description if provided
                if let Some(desc) = args["description"].as_str() {
                    flow.properties.description = Some(desc.to_string());
                }

                // Update clock_type if provided
                if let Some(clock_type_str) = args["clock_type"].as_str() {
                    flow.properties.clock_type = match clock_type_str {
                        "monotonic" => GStreamerClockType::Monotonic,
                        "realtime" => GStreamerClockType::Realtime,
                        "ptp" => GStreamerClockType::Ptp,
                        "ntp" => GStreamerClockType::Ntp,
                        _ => return Err(anyhow::anyhow!("Invalid clock_type: {}", clock_type_str)),
                    };
                }

                state.upsert_flow(flow.clone()).await?;
                json!({ "flow": flow })
            }

            "list_elements" => {
                let category = args["category"].as_str().map(|s| s.to_string());
                info!("MCP: Listing elements (category: {:?})", category);
                let elements = state.discover_elements().await;
                let filtered: Vec<_> = if let Some(cat) = category {
                    elements
                        .into_iter()
                        .filter(|e| e.category.to_lowercase().contains(&cat.to_lowercase()))
                        .collect()
                } else {
                    elements
                };
                json!({ "elements": filtered })
            }

            "get_element_info" => {
                let element_name = args["element_name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("element_name is required"))?;
                info!("MCP: Getting info for element '{}'", element_name);
                let info = state
                    .get_element_info_with_properties(element_name)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Element not found: {}", element_name))?;
                serde_json::to_value(&info)?
            }

            "get_element_properties" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                let element_id = args["element_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("element_id is required"))?;
                info!(
                    "MCP: Getting properties for element {} in flow {}",
                    element_id, flow_id
                );
                let flow_uuid: strom_types::FlowId = flow_id.parse()?;
                let properties = state.get_element_properties(&flow_uuid, element_id).await?;
                serde_json::to_value(&properties)?
            }

            "update_element_property" => {
                let flow_id = args["flow_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("flow_id is required"))?;
                let element_id = args["element_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("element_id is required"))?;
                let property_name = args["property_name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("property_name is required"))?;

                // Parse property value from JSON value
                let value: PropertyValue = match &args["value"] {
                    Value::String(s) => PropertyValue::String(s.clone()),
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            PropertyValue::Int(i)
                        } else if let Some(u) = n.as_u64() {
                            PropertyValue::UInt(u)
                        } else if let Some(f) = n.as_f64() {
                            PropertyValue::Float(f)
                        } else {
                            return Err(anyhow::anyhow!("Invalid number value"));
                        }
                    }
                    Value::Bool(b) => PropertyValue::Bool(*b),
                    _ => return Err(anyhow::anyhow!("Invalid property value type")),
                };

                info!(
                    "MCP: Updating property {}.{} = {:?} in flow {}",
                    element_id, property_name, value, flow_id
                );
                let flow_uuid: strom_types::FlowId = flow_id.parse()?;
                state
                    .update_element_property(&flow_uuid, element_id, property_name, value)
                    .await?;
                json!({
                    "success": true,
                    "message": format!("Property {}.{} updated successfully", element_id, property_name)
                })
            }

            _ => {
                error!("MCP: Unknown tool: {}", tool_params.name);
                return Err(anyhow::anyhow!("Unknown tool: {}", tool_params.name));
            }
        };

        // Wrap result in MCP content format
        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&result)?
            }]
        }))
    }
}
