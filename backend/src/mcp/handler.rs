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

/// Why a `tools/call` did not produce a result.
///
/// The MCP spec splits these: a call the server could not make sense of is a
/// JSON-RPC error, while a tool that ran and failed returns a normal result
/// carrying `isError: true`.
enum ToolError {
    /// Unknown tool, missing argument, unparseable argument.
    BadRequest(String),
    /// The tool ran and failed.
    Execution(anyhow::Error),
}

impl From<anyhow::Error> for ToolError {
    fn from(e: anyhow::Error) -> Self {
        ToolError::Execution(e)
    }
}

impl From<crate::gst::pipeline::PipelineError> for ToolError {
    fn from(e: crate::gst::pipeline::PipelineError) -> Self {
        ToolError::Execution(e.into())
    }
}

/// Build the `isError` result for a tool that ran and failed.
fn tool_error_result(error: &anyhow::Error) -> Value {
    json!({
        "isError": true,
        "content": [{
            "type": "text",
            "text": format!("{:#}", error)
        }]
    })
}

/// Read a required string argument.
fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    args[name]
        .as_str()
        .ok_or_else(|| ToolError::BadRequest(format!("{} is required and must be a string", name)))
}

/// Read a required flow id argument.
fn required_flow_id(args: &Value) -> Result<strom_types::FlowId, ToolError> {
    let raw = required_str(args, "flow_id")?;
    raw.parse().map_err(|_| {
        ToolError::BadRequest(format!(
            "flow_id must be a UUID (got '{}') - use list_flows to find valid ids",
            raw
        ))
    })
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
            "ping" => Some(JsonRpcResponse::success(id, json!({}))),
            "tools/list" => Some(Self::handle_list_tools(id)),
            "tools/call" => {
                let result =
                    Self::handle_call_tool(state, request.params.unwrap_or(json!({}))).await;
                match result {
                    Ok(value) => Some(JsonRpcResponse::success(id, value)),
                    // A tool that ran and failed is a result, not a transport
                    // failure: the caller gets isError so the model can read
                    // the reason and try something else. Only a malformed call
                    // is a JSON-RPC error.
                    Err(ToolError::Execution(e)) => {
                        error!("MCP: Tool execution failed: {:#}", e);
                        Some(JsonRpcResponse::success(id, tool_error_result(&e)))
                    }
                    Err(ToolError::BadRequest(message)) => {
                        error!("MCP: Invalid tool call: {}", message);
                        Some(JsonRpcResponse::error(id, -32602, message))
                    }
                }
            }
            // Notifications carry no id and MUST NOT be answered — a response to
            // one is an unmatchable message on the client's side. Anything under
            // notifications/* is swallowed, which also covers the notifications
            // this server has not heard of yet.
            //
            // "initialized" (no prefix) is not a real MCP method; it is kept
            // because this server used to answer only that spelling, so a client
            // built against the old behaviour may still send it.
            method if method.starts_with("notifications/") || method == "initialized" => {
                debug!("MCP: Ignoring notification: {}", method);
                None
            }
            _ if id.is_none() => {
                // An unknown method with no id is still a notification.
                debug!("MCP: Ignoring unknown notification: {}", request.method);
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
    async fn handle_call_tool(state: &AppState, params: Value) -> Result<Value, ToolError> {
        let tool_params: ToolCallParams = serde_json::from_value(params)
            .map_err(|e| ToolError::BadRequest(format!("Invalid tools/call params: {}", e)))?;
        let args = tool_params.arguments.unwrap_or(json!({}));

        let result = match tool_params.name.as_str() {
            "list_flows" => {
                info!("MCP: Listing all flows");
                let flows = state.get_flows().await;
                json!({ "flows": flows })
            }

            "get_flow" => {
                let flow_id = required_flow_id(&args)?;
                info!("MCP: Getting flow {}", flow_id);
                let flow = state
                    .get_flow(&flow_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Flow not found: {}", flow_id))?;
                json!({ "flow": flow })
            }

            "create_flow" => {
                let name = required_str(&args, "name")?;
                info!("MCP: Creating flow '{}'", name);
                let flow = Flow::new(name.to_string());
                state.upsert_flow(flow.clone()).await?;
                json!({ "flow": flow })
            }

            "update_flow" => {
                let flow_id = required_flow_id(&args)?;
                let flow: Flow = serde_json::from_value(args["flow"].clone())
                    .map_err(|e| ToolError::BadRequest(format!("Invalid flow object: {}", e)))?;
                info!("MCP: Updating flow {}", flow_id);
                state.upsert_flow(flow.clone()).await?;
                json!({ "flow": flow })
            }

            "delete_flow" => {
                let flow_id = required_flow_id(&args)?;
                info!("MCP: Deleting flow {}", flow_id);
                let deleted = state.delete_flow(&flow_id).await?;
                if !deleted {
                    return Err(anyhow::anyhow!("Flow not found: {}", flow_id).into());
                }
                json!({ "success": true, "message": format!("Flow {} deleted", flow_id) })
            }

            "start_flow" => {
                let flow_id = required_flow_id(&args)?;
                info!("MCP: Starting flow {}", flow_id);
                let _state = state.start_flow(&flow_id).await?;
                json!({ "success": true, "message": format!("Flow {} started", flow_id) })
            }

            "stop_flow" => {
                let flow_id = required_flow_id(&args)?;
                info!("MCP: Stopping flow {}", flow_id);
                let _state = state.stop_flow(&flow_id).await?;
                json!({ "success": true, "message": format!("Flow {} stopped", flow_id) })
            }

            "update_flow_properties" => {
                let flow_id = required_flow_id(&args)?;
                info!("MCP: Updating properties for flow {}", flow_id);

                // Get current flow
                let mut flow = state
                    .get_flow(&flow_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Flow not found: {}", flow_id))?;

                // Update description if provided
                if let Some(desc) = args["description"].as_str() {
                    flow.properties.description = Some(desc.to_string());
                }

                // Update clock_type if provided
                if let Some(clock_type_str) = args["clock_type"].as_str() {
                    flow.properties.clock_type = clock_type_str
                        .parse::<GStreamerClockType>()
                        .map_err(ToolError::BadRequest)?;
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
                let element_name = required_str(&args, "element_name")?;
                info!("MCP: Getting info for element '{}'", element_name);
                let info = state
                    .get_element_info_with_properties(element_name)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Element not found: {}", element_name))?;
                serde_json::to_value(&info).map_err(anyhow::Error::from)?
            }

            "get_element_properties" => {
                let flow_id = required_flow_id(&args)?;
                let element_id = required_str(&args, "element_id")?;
                info!(
                    "MCP: Getting properties for element {} in flow {}",
                    element_id, flow_id
                );
                let properties = state.get_element_properties(&flow_id, element_id).await?;
                serde_json::to_value(&properties).map_err(anyhow::Error::from)?
            }

            "update_element_property" => {
                let flow_id = required_flow_id(&args)?;
                let element_id = required_str(&args, "element_id")?;
                let property_name = required_str(&args, "property_name")?;

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
                            return Err(ToolError::BadRequest(format!(
                                "value {} is not a representable number",
                                n
                            )));
                        }
                    }
                    Value::Bool(b) => PropertyValue::Bool(*b),
                    other => {
                        return Err(ToolError::BadRequest(format!(
                            "value must be a string, number or boolean (got {})",
                            other
                        )))
                    }
                };

                info!(
                    "MCP: Updating property {}.{} = {:?} in flow {}",
                    element_id, property_name, value, flow_id
                );
                state
                    .update_element_property(&flow_id, element_id, property_name, value, None)
                    .await?;
                json!({
                    "success": true,
                    "message": format!("Property {}.{} updated successfully", element_id, property_name)
                })
            }

            unknown => {
                return Err(ToolError::BadRequest(format!(
                    "Unknown tool: {} - call tools/list for the available tools",
                    unknown
                )));
            }
        };

        // Wrap result in MCP content format
        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&result).map_err(anyhow::Error::from)?
            }]
        }))
    }
}
