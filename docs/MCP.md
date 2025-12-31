# MCP (Model Context Protocol) Integration

Strom supports the [Model Context Protocol](https://modelcontextprotocol.io/) for AI assistant integration, enabling tools like Claude to interact with GStreamer pipelines programmatically.

## Transport Options

Strom provides two MCP transport options:

| Transport | Endpoint | Use Case |
|-----------|----------|----------|
| **Streamable HTTP** | `POST/GET/DELETE /api/mcp` | Remote access, web clients, multiple concurrent sessions |
| **stdio** | `strom-mcp-server` binary | Local CLI tools (Claude Code, etc.) |

## Streamable HTTP Transport (Recommended)

The integrated HTTP transport implements the [MCP 2025-03-26 specification](https://modelcontextprotocol.io/specification/2025-03-26/basic/transports) and is the recommended approach for most use cases.

### Endpoint

```
/api/mcp
```

### Methods

| Method | Purpose |
|--------|---------|
| `POST` | Send JSON-RPC requests |
| `GET` | Open SSE stream for server-initiated messages |
| `DELETE` | Terminate a session |

### Session Management

Sessions are managed via the `Mcp-Session-Id` header:

1. Client sends `initialize` request (no session ID required)
2. Server responds with `Mcp-Session-Id` header containing a UUID
3. Client includes this header in all subsequent requests

### Example: Initialize

```bash
curl -X POST http://localhost:8081/api/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}'
```

Response:
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2025-03-26",
    "capabilities": { "tools": {} },
    "serverInfo": { "name": "strom", "version": "0.3.5" }
  }
}
```

Response headers include:
```
Mcp-Session-Id: <uuid>
```

### Example: List Tools

```bash
curl -X POST http://localhost:8081/api/mcp \
  -H "Content-Type: application/json" \
  -H "Mcp-Session-Id: <session-id>" \
  -d '{"jsonrpc": "2.0", "id": 2, "method": "tools/list"}'
```

### Example: Call a Tool

```bash
curl -X POST http://localhost:8081/api/mcp \
  -H "Content-Type: application/json" \
  -H "Mcp-Session-Id: <session-id>" \
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "tools/call",
    "params": {
      "name": "create_flow",
      "arguments": { "name": "My New Flow" }
    }
  }'
```

### SSE Stream (Server-Sent Events)

Connect to receive real-time notifications:

```bash
curl -N http://localhost:8081/api/mcp \
  -H "Accept: text/event-stream" \
  -H "Mcp-Session-Id: <session-id>"
```

Events include:
- `notifications/strom/flowCreated`
- `notifications/strom/flowUpdated`
- `notifications/strom/flowDeleted`
- `notifications/strom/flowStarted`
- `notifications/strom/flowStopped`
- `notifications/strom/pipelineError`
- `notifications/strom/pipelineWarning`

### Terminate Session

```bash
curl -X DELETE http://localhost:8081/api/mcp \
  -H "Mcp-Session-Id: <session-id>"
```

## stdio Transport

The standalone `strom-mcp-server` binary provides stdio transport for local CLI tools.

### Configuration

Add to your Claude Code MCP configuration (`.mcp.json`):

```json
{
  "mcpServers": {
    "strom": {
      "command": "/path/to/strom-mcp-server",
      "env": {
        "STROM_API_URL": "http://localhost:8081"
      }
    }
  }
}
```

### How It Works

The stdio server acts as a proxy:

```
Claude Code <--stdio--> strom-mcp-server <--HTTP--> Strom Backend
```

## Comparison

| Feature | Streamable HTTP | stdio |
|---------|-----------------|-------|
| **Latency** | Direct (< 1ms) | HTTP round-trip (~5ms) |
| **Deployment** | Single binary | Requires separate binary |
| **Remote access** | Yes | No (local only) |
| **Multiple clients** | Yes | One per process |
| **Real-time events** | SSE streaming | Not supported |
| **Session management** | Built-in | N/A |
| **Browser support** | Yes | No |

### When to Use Each

**Use Streamable HTTP when:**
- Building web-based AI integrations
- Need real-time event streaming
- Connecting from remote machines
- Running multiple AI clients concurrently

**Use stdio when:**
- Using Claude Code CLI
- Need simplest possible local setup
- Running in environments where HTTP isn't practical

## Available Tools

Both transports provide the same 12 tools:

| Tool | Description |
|------|-------------|
| `list_flows` | List all GStreamer flows |
| `get_flow` | Get details of a specific flow |
| `create_flow` | Create a new flow |
| `update_flow` | Update flow elements, links, and properties |
| `delete_flow` | Delete a flow |
| `start_flow` | Start a flow's GStreamer pipeline |
| `stop_flow` | Stop a running flow |
| `update_flow_properties` | Update flow description, clock type |
| `list_elements` | List available GStreamer elements |
| `get_element_info` | Get detailed element information |
| `get_element_properties` | Get properties from a running element |
| `update_element_property` | Update a property on a running element |

## Security

### Streamable HTTP

- **Origin validation**: Requests are validated against allowed origins (localhost by default)
- **Session isolation**: Each session has independent state
- **No authentication bypass**: MCP endpoint respects server authentication settings

### stdio

- **Local only**: Only accessible from the local machine
- **Process isolation**: Each invocation is independent

## Architecture

### Streamable HTTP (Integrated)

```
┌─────────────────────────────────────────┐
│           Strom Backend                  │
├─────────────────────────────────────────┤
│  /api/flows      - REST API             │
│  /api/elements   - REST API             │
│  /api/ws         - WebSocket            │
│  /api/mcp        - MCP Streamable HTTP  │ ← Direct state access
└─────────────────────────────────────────┘
```

### stdio (Proxy)

```
┌──────────────┐     ┌─────────────────┐     ┌──────────────┐
│ Claude Code  │────▶│ strom-mcp-server│────▶│ Strom Backend│
│   (stdio)    │◀────│    (proxy)      │◀────│  (HTTP API)  │
└──────────────┘     └─────────────────┘     └──────────────┘
```

## Protocol Version

- **Streamable HTTP**: `2025-03-26`
- **stdio**: `2024-11-05`

The Streamable HTTP transport uses the newer protocol version which includes session management and SSE streaming support.
