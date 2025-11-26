# Integration Options

This document covers integration possibilities for Strom, including MCP (Model Context Protocol) and API documentation with OpenAPI/Swagger.

## MCP (Model Context Protocol) Integration

### What is MCP?

MCP (Model Context Protocol) is Anthropic's standard for connecting AI assistants like Claude to external data sources and tools. It allows Claude to interact with your application's data and functionality in a structured way.

### MCP for Strom

Integrating MCP with Strom would enable Claude to:

1. **Query flows** - Ask Claude "What flows do I have?" or "Show me the RTSP recorder flow"
2. **Create/modify flows** - Tell Claude "Create a new flow that records from an RTSP camera to a file"
3. **Manage pipelines** - "Start the recording flow" or "Stop all flows"
4. **Inspect elements** - "What properties does the x264enc element have?"
5. **Troubleshoot** - "Why isn't my flow working?" with Claude able to read flow configurations and states

### Implementation Approaches

#### Option 1: MCP Server Wrapper (Recommended)

Create a separate MCP server that wraps the Strom REST API.

**Pros:**
- Clean separation of concerns
- No changes needed to Strom backend
- Can be developed independently
- Easy to version and deploy separately

**Implementation:**
```rust
// mcp-server/src/main.rs
use mcp_sdk::Server;

#[tokio::main]
async fn main() {
    let server = Server::new("strom-mcp");

    // Register tools
    server.add_tool("list_flows", list_flows_handler);
    server.add_tool("create_flow", create_flow_handler);
    server.add_tool("start_flow", start_flow_handler);
    // ... more tools

    server.serve().await;
}

async fn list_flows_handler(_params: Value) -> Result<Value> {
    // Call Strom API at http://localhost:8080/api/flows
    let response = reqwest::get("http://localhost:8080/api/flows").await?;
    Ok(response.json().await?)
}
```

**MCP Tools to Implement:**
- `list_flows` - Get all flows
- `get_flow` - Get specific flow details
- `create_flow` - Create a new flow
- `update_flow` - Modify a flow
- `delete_flow` - Remove a flow
- `start_flow` - Start a pipeline
- `stop_flow` - Stop a pipeline
- `list_elements` - Get available GStreamer elements
- `get_element_info` - Get element properties/capabilities

#### Option 2: Built-in MCP Server

Integrate MCP server directly into the Strom backend.

**Pros:**
- Single deployment
- Direct access to internal state
- No network overhead for internal calls

**Cons:**
- Tighter coupling
- Backend becomes more complex
- Harder to update MCP independently

#### Option 3: MCP via REST API Proxy

Use a generic MCP-to-REST bridge.

**Pros:**
- Zero custom code
- Works with existing API

**Cons:**
- Less semantic understanding
- May require API adjustments
- Limited to REST capabilities

### Testing MCP Integration

```bash
# Install Claude Desktop or use MCP CLI tools
# Configure MCP server in Claude Desktop settings:
{
  "mcpServers": {
    "strom": {
      "command": "cargo",
      "args": ["run", "--manifest-path", "/path/to/strom-mcp/Cargo.toml"]
    }
  }
}

# Test with Claude
"Claude, list all my GStreamer flows"
"Claude, create a flow that transcodes video using x264enc"
```

---

## OpenAPI / Swagger Documentation

### Why OpenAPI?

OpenAPI (formerly Swagger) provides:
- **Interactive API documentation** - Try endpoints in browser
- **Client generation** - Auto-generate API clients for any language
- **Validation** - Ensure requests/responses match spec
- **Standards compliance** - Industry-standard API documentation

### Implementation Options for Axum

#### Option 1: `utoipa` (Recommended)

`utoipa` is a Rust library for generating OpenAPI docs from code annotations.

**Add to workspace dependencies:**
```toml
[workspace.dependencies]
utoipa = { version = "4.0", features = ["axum_extras", "chrono", "uuid"] }
utoipa-swagger-ui = { version = "6.0", features = ["axum"] }
```

**Example implementation:**
```rust
// backend/src/main.rs
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

#[derive(OpenApi)]
#[openapi(
    paths(
        api::flows::list_flows,
        api::flows::get_flow,
        api::flows::create_flow,
        api::flows::update_flow,
        api::flows::delete_flow,
        api::flows::start_flow,
        api::flows::stop_flow,
    ),
    components(
        schemas(Flow, Element, Link, PropertyValue, PipelineState,
                CreateFlowRequest, FlowResponse, FlowListResponse)
    ),
    tags(
        (name = "flows", description = "Flow management endpoints"),
        (name = "elements", description = "GStreamer element discovery")
    ),
    info(
        title = "Strom API",
        version = "0.1.0",
        description = "GStreamer Flow Engine REST API",
        contact(
            name = "API Support",
            email = "support@example.com"
        )
    )
)]
struct ApiDoc;

// In main():
let app = Router::new()
    .merge(SwaggerUi::new("/swagger-ui")
        .url("/api-docs/openapi.json", ApiDoc::openapi()))
    .nest("/api", api_router)
    // ... rest of routes
```

**Annotate handlers:**
```rust
/// List all flows
#[utoipa::path(
    get,
    path = "/api/flows",
    responses(
        (status = 200, description = "List of all flows", body = FlowListResponse)
    ),
    tag = "flows"
)]
pub async fn list_flows(State(state): State<AppState>) -> Json<FlowListResponse> {
    // ... implementation
}

/// Create a new flow
#[utoipa::path(
    post,
    path = "/api/flows",
    request_body = CreateFlowRequest,
    responses(
        (status = 201, description = "Flow created successfully", body = FlowResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse)
    ),
    tag = "flows"
)]
pub async fn create_flow(
    State(state): State<AppState>,
    Json(req): Json<CreateFlowRequest>,
) -> (StatusCode, Json<FlowResponse>) {
    // ... implementation
}
```

**Annotate types:**
```rust
// types/src/flow.rs
use utoipa::ToSchema;

/// A complete GStreamer pipeline definition
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Flow {
    /// Unique identifier for this flow
    pub id: FlowId,
    /// Human-readable name
    #[schema(example = "RTSP Camera Recorder")]
    pub name: String,
    /// Whether to automatically start this flow on server startup
    #[serde(default)]
    pub auto_start: bool,
    // ... fields
}
```

**Access documentation:**
- Swagger UI: `http://localhost:8080/swagger-ui`
- OpenAPI JSON: `http://localhost:8080/api-docs/openapi.json`

#### Option 2: `aide`

Another Rust OpenAPI library with good Axum support.

```toml
aide = { version = "0.13", features = ["axum"] }
```

#### Option 3: Manual OpenAPI YAML

Write OpenAPI spec manually (not recommended - gets out of sync).

### Testing API with OpenAPI

Once OpenAPI is set up:

1. **Interactive testing** via Swagger UI
2. **Generate clients:**
   ```bash
   # Generate Python client
   openapi-generator-cli generate -i openapi.json -g python -o python-client/

   # Generate TypeScript client
   openapi-generator-cli generate -i openapi.json -g typescript-axios -o ts-client/
   ```

3. **API validation:**
   ```bash
   # Validate spec
   npx @redocly/cli lint openapi.json
   ```

---

## Testing Strategy

### Manual API Testing

**Current approach** (no tools needed):
```bash
curl http://localhost:8080/api/flows
curl -X POST http://localhost:8080/api/flows \
  -H "Content-Type: application/json" \
  -d '{"name":"Test","auto_start":false}'
```

### With OpenAPI

**Swagger UI** - Interactive browser-based testing
- Try all endpoints
- See request/response schemas
- Validate inputs

### Automated Testing

**Integration tests** in `backend/tests/`:
```rust
#[tokio::test]
async fn test_create_flow() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Test Flow","auto_start":false}"#))
                .unwrap()
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
}
```

### Frontend/GUI Testing

For egui WASM app:
1. **Manual testing** - Run `trunk serve`, test in browser
2. **wasm-pack test** - Run tests in headless browsers
3. **Screenshot testing** - Capture UI states for visual regression

**Test structure:**
```rust
// frontend/tests/app_tests.rs
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test]
fn test_app_creation() {
    let app = StromApp::default();
    assert_eq!(app.flows.len(), 0);
}
```

---

## Recommended Implementation Order

1. **OpenAPI first** (easier, immediate value)
   - Add `utoipa` and `utoipa-swagger-ui` to dependencies
   - Annotate existing handlers
   - Test via Swagger UI at `/swagger-ui`
   - Estimated time: 2-3 hours

2. **MCP integration** (enables AI assistance)
   - Create separate `mcp-server` crate
   - Implement MCP tools as wrappers around API
   - Test with Claude Desktop
   - Estimated time: 4-6 hours

3. **Automated testing**
   - Add integration tests for API
   - Add unit tests for core logic
   - Set up CI pipeline
   - Estimated time: 4-8 hours

---

## Next Steps

To implement OpenAPI right now:

```bash
# Add to workspace Cargo.toml
# Then update backend/Cargo.toml to include utoipa dependencies
# Annotate handlers and types
# Add SwaggerUi to router
# Test at http://localhost:8080/swagger-ui
```

To implement MCP:

```bash
# Create new crate: mcp-server
# Add mcp-sdk dependency (when available)
# Implement tool handlers
# Configure in Claude Desktop
# Test with natural language queries
```
