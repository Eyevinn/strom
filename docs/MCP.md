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
curl -X POST http://localhost:8080/api/mcp \
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
curl -X POST http://localhost:8080/api/mcp \
  -H "Content-Type: application/json" \
  -H "Mcp-Session-Id: <session-id>" \
  -d '{"jsonrpc": "2.0", "id": 2, "method": "tools/list"}'
```

### Example: Call a Tool

```bash
curl -X POST http://localhost:8080/api/mcp \
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
curl -N http://localhost:8080/api/mcp \
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
curl -X DELETE http://localhost:8080/api/mcp \
  -H "Mcp-Session-Id: <session-id>"
```

### Claude Code Configuration

Add to your Claude Code MCP configuration (`.mcp.json`):

```json
{
  "mcpServers": {
    "strom": {
      "type": "http",
      "url": "http://localhost:8080/api/mcp"
    }
  }
}
```

For remote servers with authentication:

```json
{
  "mcpServers": {
    "strom": {
      "type": "http",
      "url": "https://strom.example.com/api/mcp",
      "headers": {
        "X-API-Key": "your-api-key-here"
      }
    }
  }
}
```

## stdio Transport

The standalone `strom-mcp-server` binary provides stdio transport for local CLI tools like Claude Code.

### How It Works

The stdio server acts as a proxy between the MCP client and Strom's REST API:

```
Claude Code <--stdio--> strom-mcp-server <--HTTP REST API--> Strom Backend
```

This architecture enables **remote communication**: while the MCP server runs locally alongside Claude Code, it can connect to a Strom instance running anywhere accessible via HTTP. This is useful for:
- Controlling remote Strom instances from your local machine
- Managing multiple Strom deployments from a single Claude Code session
- Accessing Strom servers in Docker containers, VMs, or cloud instances

### Configuration

Add to your Claude Code MCP configuration (`.mcp.json`):

```json
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
```

#### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `STROM_API_URL` | URL of the Strom server | `http://localhost:8080` |
| `STROM_API_KEY` | API key for authentication (if enabled on server) | None |

#### Remote Connection Example

To connect to a remote Strom server:

```json
{
  "mcpServers": {
    "strom-production": {
      "command": "/path/to/strom-mcp-server",
      "env": {
        "STROM_API_URL": "https://strom.example.com:8080",
        "STROM_API_KEY": "your-api-key-here"
      }
    }
  }
}
```

## Comparison

| Feature | Streamable HTTP | stdio |
|---------|-----------------|-------|
| **Latency** | Direct (< 1ms) | HTTP round-trip (~5ms) |
| **Deployment** | Single binary | Requires separate binary |
| **Remote Strom access** | Yes | Yes (via HTTP proxy) |
| **Multiple clients** | Yes | One per process |
| **Real-time events** | SSE streaming | Not supported |
| **Session management** | Built-in | N/A |
| **Browser support** | Yes | No |
| **Authentication** | API key header | API key via env var |

### When to Use Each

**Use Streamable HTTP when:**
- Building web-based AI integrations
- Need real-time event streaming
- Connecting from remote machines
- Running multiple AI clients concurrently

**Use stdio when:**
- Using Claude Code CLI
- Controlling local or remote Strom instances from your development machine
- Need simplest possible setup for CLI tools

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
- **API key authentication**: When `STROM_API_KEY` is set on the server, requests must include either:
  - `X-API-Key: <key>` header (recommended)
  - `Authorization: Bearer <key>` header

### stdio

- **Local process**: The MCP server binary runs locally alongside Claude Code
- **Process isolation**: Each invocation is independent
- **Remote authentication**: When connecting to a remote Strom server with authentication enabled, the `STROM_API_KEY` environment variable must be set
- **Inherits server security**: All API key validation is performed by the Strom server, so security policies are enforced regardless of transport

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
