//! MCP (Model Context Protocol) Streamable HTTP support.
//!
//! This module implements the MCP 2025-03-26 Streamable HTTP transport,
//! allowing AI assistants like Claude to interact with Strom via a standard
//! HTTP endpoint with optional SSE streaming for server-initiated messages.
//!
//! ## Endpoints
//!
//! - `POST /api/mcp` - Send JSON-RPC requests
//! - `GET /api/mcp` - Open SSE stream for server messages
//! - `DELETE /api/mcp` - Terminate session
//!
//! ## Session Management
//!
//! Sessions are identified by the `Mcp-Session-Id` header, assigned during
//! initialization and required for subsequent requests.

pub mod handler;
pub mod session;

pub use handler::McpHandler;
pub use session::{McpSession, McpSessionManager};
