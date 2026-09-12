# MCP (Model Context Protocol) Integration

> Code is the source of truth — this may have drifted; read the code for the current implementation.

Strom supports the [Model Context Protocol](https://modelcontextprotocol.io/) for AI assistant
integration, enabling tools like Claude to inspect and control GStreamer pipelines
programmatically.

MCP is served by the backend itself over Streamable HTTP at `/api/mcp` — there is no separate
binary to install or keep in version sync. Point any MCP client at that URL.

## Endpoint

```
/api/mcp
```

| Method | Purpose |
|--------|---------|
| `POST` | Send JSON-RPC requests |
| `GET` | Open SSE stream for server-initiated messages |
| `DELETE` | Terminate a session |

## Session Management

Sessions are managed via the `Mcp-Session-Id` header:

1. Client sends `initialize` request (no session ID required)
2. Server responds with `Mcp-Session-Id` header containing a UUID
3. Client includes this header in subsequent requests

## Client Configuration

### Claude Code

```bash
claude mcp add --transport http strom http://localhost:8080/api/mcp
```

Or in `.mcp.json`:

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

For a remote server with authentication enabled:

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

### Clients that only speak stdio

Bridge them to the HTTP endpoint with [`mcp-remote`](https://www.npmjs.com/package/mcp-remote):

```json
{
  "mcpServers": {
    "strom": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://localhost:8080/api/mcp"]
    }
  }
}
```

## Examples

### Initialize

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
    "serverInfo": { "name": "strom", "version": "0.6.8" }
  }
}
```

Response headers include:
```
Mcp-Session-Id: <uuid>
```

### List Tools

```bash
curl -X POST http://localhost:8080/api/mcp \
  -H "Content-Type: application/json" \
  -H "Mcp-Session-Id: <session-id>" \
  -d '{"jsonrpc": "2.0", "id": 2, "method": "tools/list"}'
```

### Call a Tool

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

## Available Tools

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

- **Origin validation**: browser-style cross-origin requests are rejected (DNS rebinding
  protection).
- **Session isolation**: each session has independent state.
- **API key authentication**: when authentication is enabled on the server, requests must
  include either an `X-API-Key: <key>` header (recommended) or
  `Authorization: Bearer <key>`. See [AUTHENTICATION.md](AUTHENTICATION.md).
