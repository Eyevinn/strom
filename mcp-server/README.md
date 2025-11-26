# Strom MCP Server

A **working** Model Context Protocol (MCP) server for the Strom GStreamer Flow Engine. This allows AI assistants like Claude to interact with Strom's API to manage GStreamer pipelines through natural language.

## Implementation

This server implements the MCP protocol directly using JSON-RPC 2.0 over stdio, without depending on unstable SDK crates. This provides a reliable, lightweight implementation that's easy to understand and maintain.

## What is MCP?

[Model Context Protocol (MCP)](https://modelcontextprotocol.io/) is Anthropic's standard for connecting AI assistants to external tools and data sources. This MCP server wraps the Strom REST API, enabling Claude and other MCP-compatible AI assistants to:

- Query and manage GStreamer flows
- Start and stop pipelines
- Discover available GStreamer elements
- Configure pipeline properties
- Troubleshoot pipeline issues

## Features

The MCP server provides the following tools:

### Flow Management
- `list_flows` - List all GStreamer flows
- `get_flow` - Get details of a specific flow
- `create_flow` - Create a new flow
- `update_flow` - Update an existing flow
- `delete_flow` - Delete a flow
- `start_flow` - Start a flow's GStreamer pipeline
- `stop_flow` - Stop a running flow

### Element Discovery
- `list_elements` - List available GStreamer elements (with optional category filter)
- `get_element_info` - Get detailed information about a specific element

## Installation

### Prerequisites

1. **Rust 1.75+** with cargo
2. **Strom backend server** running (default: http://localhost:8080)
3. **Claude Desktop** or another MCP-compatible client

### Build

```bash
# From the strom root directory
cargo build --release -p strom-mcp-server

# The binary will be at: target/release/strom-mcp-server
```

## Configuration

The MCP server connects to the Strom API using the following environment variable:

- `STROM_API_URL` - URL of the Strom backend (default: `http://localhost:8080`)

## Usage

### With Claude Desktop

1. **Configure Claude Desktop** to use the MCP server:

Edit your Claude Desktop configuration file:
- macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
- Windows: `%APPDATA%\Claude\claude_desktop_config.json`
- Linux: `~/.config/Claude/claude_desktop_config.json`

Add the following configuration:

```json
{
  "mcpServers": {
    "strom": {
      "command": "/path/to/strom/target/release/strom-mcp-server",
      "env": {
        "STROM_API_URL": "http://localhost:8080"
      }
    }
  }
}
```

Or use cargo to run directly:

```json
{
  "mcpServers": {
    "strom": {
      "command": "cargo",
      "args": [
        "run",
        "--release",
        "--manifest-path",
        "/path/to/strom/mcp-server/Cargo.toml"
      ],
      "env": {
        "STROM_API_URL": "http://localhost:8080"
      }
    }
  }
}
```

2. **Restart Claude Desktop**

3. **Start using natural language** to interact with Strom:

Example prompts:
- "List all my GStreamer flows"
- "Create a new flow called 'RTSP Recorder'"
- "Show me the properties of the x264enc element"
- "Start the flow with ID abc-123"
- "What video sources are available in GStreamer?"

### Standalone Usage

You can also run the MCP server directly for testing:

```bash
# Start the MCP server
STROM_API_URL=http://localhost:8080 cargo run -p strom-mcp-server

# The server will communicate via stdin/stdout using the MCP protocol
```

## Example Interactions

### Creating a Video Recording Pipeline

**User**: "Create a flow that records video from a test source to a file"

**Claude** (using MCP):
1. Calls `create_flow` with name "Test Video Recorder"
2. Calls `list_elements` with category "source"
3. Calls `get_element_info` for "videotestsrc"
4. Calls `update_flow` to add videotestsrc, x264enc, mp4mux, and filesink elements
5. Calls `start_flow` to begin recording

### Troubleshooting

**User**: "Why isn't my RTSP flow working?"

**Claude** (using MCP):
1. Calls `list_flows` to find RTSP flows
2. Calls `get_flow` to inspect the pipeline configuration
3. Calls `get_element_info` for "rtspsrc" to check required properties
4. Provides troubleshooting suggestions based on the configuration

## Architecture

```
┌─────────────────┐
│  Claude Desktop │
│  (MCP Client)   │
└────────┬────────┘
         │
         │ MCP Protocol (stdio)
         │
┌────────▼────────┐
│  Strom MCP      │
│  Server         │
│  (This crate)   │
└────────┬────────┘
         │
         │ HTTP REST API
         │
┌────────▼────────┐
│  Strom Backend  │
│  (Port 8080)    │
└────────┬────────┘
         │
         │ GStreamer API
         │
┌────────▼────────┐
│   GStreamer     │
│   Pipelines     │
└─────────────────┘
```

## Logging

The MCP server uses `tracing` for logging. Set the log level using the `RUST_LOG` environment variable:

```bash
# Info level (default)
RUST_LOG=info cargo run -p strom-mcp-server

# Debug level
RUST_LOG=debug cargo run -p strom-mcp-server

# Trace level (very verbose)
RUST_LOG=trace cargo run -p strom-mcp-server
```

## Development

### Adding New Tools

To add a new MCP tool:

1. Add the tool definition to `list_tools()` in `src/main.rs`
2. Implement the handler method in `StromMcpServer`
3. Add the handler to the match statement in `call_tool()`

Example:

```rust
// In list_tools()
Tool {
    name: "my_new_tool".to_string(),
    description: Some("Description of what it does".to_string()),
    input_schema: json!({
        "type": "object",
        "properties": {
            "param": {
                "type": "string",
                "description": "Parameter description"
            }
        },
        "required": ["param"]
    }),
}

// Add implementation
impl StromMcpServer {
    async fn my_new_tool(&self, param: String) -> Result<Value> {
        // Implementation
    }
}

// Add to call_tool()
"my_new_tool" => {
    let param = arguments["param"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("param is required"))?
        .to_string();
    self.my_new_tool(param).await
}
```

### Testing

```bash
# Run tests (if any)
cargo test -p strom-mcp-server

# Build in release mode
cargo build --release -p strom-mcp-server
```

## Troubleshooting

### Connection Issues

**Problem**: MCP server can't connect to Strom backend

**Solutions**:
- Ensure Strom backend is running: `cargo run -p strom-backend`
- Check the `STROM_API_URL` environment variable
- Verify the backend is accessible: `curl http://localhost:8080/health`

### Claude Desktop Not Detecting Server

**Problem**: Claude doesn't show Strom tools

**Solutions**:
- Verify the `claude_desktop_config.json` path is correct
- Check that the `command` path points to the correct binary
- Restart Claude Desktop completely
- Check Claude Desktop logs for errors

### Permission Errors

**Problem**: Binary not executable

**Solution**:
```bash
chmod +x target/release/strom-mcp-server
```

## Contributing

See the main [CONTRIBUTING.md](../docs/CONTRIBUTING.md) for contribution guidelines.

## License

MIT OR Apache-2.0 (same as parent project)
