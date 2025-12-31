//! MCP session management.
//!
//! Manages session lifecycle for MCP Streamable HTTP connections.
//! Sessions are identified by cryptographically secure UUIDs and
//! support SSE streaming for server-initiated messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info};
use uuid::Uuid;

/// Events that can be sent to MCP clients via SSE.
#[derive(Clone, Debug)]
pub enum McpEvent {
    /// A JSON-RPC message to send to the client.
    JsonRpc(String),
}

/// An MCP session.
#[derive(Debug)]
pub struct McpSession {
    /// Unique session identifier.
    pub id: String,
    /// When the session was created.
    pub created_at: Instant,
    /// Broadcast sender for SSE events.
    pub event_tx: broadcast::Sender<McpEvent>,
    /// Whether the session has been initialized (received InitializeResult).
    pub initialized: bool,
}

impl McpSession {
    /// Create a new session with a unique ID.
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(100);
        Self {
            id: Uuid::new_v4().to_string(),
            created_at: Instant::now(),
            event_tx,
            initialized: false,
        }
    }

    /// Subscribe to session events for SSE streaming.
    pub fn subscribe(&self) -> broadcast::Receiver<McpEvent> {
        self.event_tx.subscribe()
    }

    /// Send an event to all SSE subscribers.
    pub fn send(&self, event: McpEvent) -> Result<usize, broadcast::error::SendError<McpEvent>> {
        self.event_tx.send(event)
    }

    /// Get the session age in seconds.
    pub fn age_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
}

impl Default for McpSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Manager for MCP sessions.
#[derive(Clone)]
pub struct McpSessionManager {
    sessions: Arc<RwLock<HashMap<String, McpSession>>>,
}

impl McpSessionManager {
    /// Create a new session manager.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new session and return its ID.
    pub async fn create_session(&self) -> String {
        let session = McpSession::new();
        let id = session.id.clone();
        let mut sessions = self.sessions.write().await;
        sessions.insert(id.clone(), session);
        info!("Created MCP session: {}", id);
        id
    }

    /// Get a session by ID.
    pub async fn get_session(&self, id: &str) -> Option<McpSession> {
        let sessions = self.sessions.read().await;
        sessions.get(id).map(|s| McpSession {
            id: s.id.clone(),
            created_at: s.created_at,
            event_tx: s.event_tx.clone(),
            initialized: s.initialized,
        })
    }

    /// Check if a session exists.
    pub async fn session_exists(&self, id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions.contains_key(id)
    }

    /// Mark a session as initialized.
    pub async fn mark_initialized(&self, id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            session.initialized = true;
            debug!("MCP session {} marked as initialized", id);
            true
        } else {
            false
        }
    }

    /// Check if a session is initialized.
    pub async fn is_initialized(&self, id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions.get(id).map(|s| s.initialized).unwrap_or(false)
    }

    /// Subscribe to a session's event stream.
    pub async fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<McpEvent>> {
        let sessions = self.sessions.read().await;
        sessions.get(id).map(|s| s.subscribe())
    }

    /// Send an event to a session's subscribers.
    pub async fn send_event(&self, id: &str, event: McpEvent) -> bool {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(id) {
            session.send(event).is_ok()
        } else {
            false
        }
    }

    /// Terminate a session.
    pub async fn terminate(&self, id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        if sessions.remove(id).is_some() {
            info!("Terminated MCP session: {}", id);
            true
        } else {
            false
        }
    }

    /// Get the number of active sessions.
    pub async fn session_count(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions.len()
    }

    /// Clean up stale sessions older than the given age in seconds.
    pub async fn cleanup_stale(&self, max_age_secs: u64) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|id, session| {
            let keep = session.age_secs() < max_age_secs;
            if !keep {
                info!(
                    "Cleaning up stale MCP session: {} (age: {}s)",
                    id,
                    session.age_secs()
                );
            }
            keep
        });
        before - sessions.len()
    }
}

impl Default for McpSessionManager {
    fn default() -> Self {
        Self::new()
    }
}
