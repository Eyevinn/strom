# Integration Options

This document covers Strom's integration capabilities, including the MCP (Model Context Protocol) server and OpenAPI/Swagger documentation.

## MCP (Model Context Protocol) Integration

### What is MCP?

MCP (Model Context Protocol) is Anthropic's standard for connecting AI assistants like Claude to external data sources and tools. It allows Claude to interact with your application's data and functionality in a structured way.

### MCP Server - Already Implemented ✅

Strom includes a fully functional MCP server (`strom-mcp-server`) that enables Claude to:

1. **Query flows** - Ask Claude "What flows do I have?" or "Show me the RTSP recorder flow"
2. **Create/modify flows** - Tell Claude "Create a new flow that records from an RTSP camera to a file"
3. **Manage pipelines** - "Start the recording flow" or "Stop all flows"
4. **Inspect elements** - "What properties does the x264enc element have?"
5. **Troubleshoot** - "Why isn't my flow working?" with Claude able to read flow configurations and states

### Using the MCP Server

The MCP server is implemented as a standalone binary (`strom-mcp-server`) in the workspace.

```bash
# Install Claude Desktop or use MCP CLI tools
# Configure MCP server in Claude Desktop settings:
{
  "mcpServers": {
    "strom": {
      "command": "/path/to/strom-mcp-server",
      "env": {
        "STROM_API_URL": "http://localhost:8080"
      }
    }
  }
}

# Test with Claude
"Claude, list all my GStreamer flows"
"Claude, create a flow that transcodes video using x264enc"
```

For detailed setup instructions, see `mcp-server/README.md`.

---

## OpenAPI / Swagger Documentation - Already Implemented ✅

### Current Implementation

Strom includes full OpenAPI/Swagger documentation using `utoipa`:

**Access the documentation:**
- **Swagger UI**: `http://localhost:8080/swagger-ui`
- **OpenAPI JSON**: `http://localhost:8080/api-docs/openapi.json`

### Features

The API documentation provides:
- **Interactive API testing** - Try all endpoints directly in your browser
- **Comprehensive schema documentation** - All request/response types documented
- **Client generation** - Export OpenAPI spec to generate clients for any language
- **Standards compliance** - Industry-standard OpenAPI 3.0 specification

### Generating API Clients

Export the OpenAPI spec and generate clients for any language:

```bash
# Download the OpenAPI spec
curl http://localhost:8080/api-docs/openapi.json > strom-api.json

# Generate Python client
openapi-generator-cli generate -i strom-api.json -g python -o python-client/

# Generate TypeScript client
openapi-generator-cli generate -i strom-api.json -g typescript-axios -o ts-client/

# Generate Go client
openapi-generator-cli generate -i strom-api.json -g go -o go-client/
```

### API Validation

Validate the OpenAPI specification:

```bash
# Download spec
curl http://localhost:8080/api-docs/openapi.json > strom-api.json

# Validate with Redocly CLI
npx @redocly/cli lint strom-api.json
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

## Additional Integration Possibilities

### Future Enhancements

While MCP and OpenAPI are fully implemented, here are potential future integrations:

1. **Prometheus Metrics** - Export pipeline statistics and system metrics
2. **Grafana Dashboards** - Visual monitoring of pipeline performance
3. **Webhook Notifications** - Send events to external services (Slack, Discord, etc.)
4. **MQTT Integration** - Publish pipeline state changes to MQTT brokers
5. **gRPC API** - Alternative to REST for high-performance integrations
6. **GraphQL API** - Flexible query language for complex data requirements

### Community Contributions Welcome

If you're interested in adding new integrations, please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.
