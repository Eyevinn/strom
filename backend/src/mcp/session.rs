//! MCP session management.
//!
//! Manages session lifecycle for MCP Streamable HTTP connections.
//! Sessions are identified by cryptographically secure UUIDs and
//! support SSE streaming for server-initiated messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info};
use uuid::Uuid;

/// How long a session may sit idle before it is collected.
///
/// The MCP spec lets a client terminate with `DELETE`, but does not require it,
/// and clients in practice just walk away — so nothing but this reclaims a
/// session. Long enough that an assistant idling between questions keeps its
/// session; short enough that abandoned ones do not accumulate for the life of
/// the process.
const SESSION_MAX_IDLE: Duration = Duration::from_secs(60 * 60);

/// How often to sweep for idle sessions.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

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
    /// When the session was last used by its client.
    pub last_seen: Instant,
    /// Broadcast sender for SSE events.
    pub event_tx: broadcast::Sender<McpEvent>,
}

impl McpSession {
    /// Create a new session with a unique ID.
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(100);
        let now = Instant::now();
        Self {
            id: Uuid::new_v4().to_string(),
            created_at: now,
            last_seen: now,
            event_tx,
        }
    }

    /// Subscribe to session events for SSE streaming.
    pub fn subscribe(&self) -> broadcast::Receiver<McpEvent> {
        self.event_tx.subscribe()
    }

    /// Get the session age in seconds.
    pub fn age_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }

    /// How long the session has been idle.
    pub fn idle(&self) -> Duration {
        self.last_seen.elapsed()
    }

    /// Whether an SSE stream is currently attached to this session.
    pub fn has_subscribers(&self) -> bool {
        self.event_tx.receiver_count() > 0
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

    /// Check if a session exists.
    pub async fn session_exists(&self, id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions.contains_key(id)
    }

    /// Mark a session as used, so the idle sweep leaves it alone.
    pub async fn touch(&self, id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            session.last_seen = Instant::now();
        }
    }

    /// Subscribe to a session's event stream.
    pub async fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<McpEvent>> {
        let sessions = self.sessions.read().await;
        sessions.get(id).map(|s| s.subscribe())
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

    /// Drop sessions that have been idle longer than `max_idle`.
    ///
    /// A session with a live SSE subscriber is never collected, however idle:
    /// the client is sitting on the stream waiting for notifications, which
    /// produces no requests of its own to refresh `last_seen`.
    ///
    /// Returns the number of sessions removed.
    pub async fn cleanup_idle(&self, max_idle: Duration) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|id, session| {
            let keep = session.idle() < max_idle || session.has_subscribers();
            if !keep {
                info!(
                    "Cleaning up idle MCP session: {} (idle: {}s, age: {}s)",
                    id,
                    session.idle().as_secs(),
                    session.age_secs()
                );
            }
            keep
        });
        before - sessions.len()
    }

    /// Start the background task that collects idle sessions.
    /// Should be called once at startup.
    pub fn start_cleanup_task(&self) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
            // The first tick fires immediately; skip it so startup does not
            // sweep an empty map.
            interval.tick().await;
            loop {
                interval.tick().await;
                let removed = manager.cleanup_idle(SESSION_MAX_IDLE).await;
                if removed > 0 {
                    debug!(
                        "MCP session sweep removed {} idle session(s), {} remaining",
                        removed,
                        manager.session_count().await
                    );
                }
            }
        });
        info!(
            "Started MCP session cleanup task ({}s interval, {}s max idle)",
            CLEANUP_INTERVAL.as_secs(),
            SESSION_MAX_IDLE.as_secs()
        );
    }
}

impl Default for McpSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_sessions_are_collected_and_touch_saves_them() {
        let manager = McpSessionManager::new();
        let stale = manager.create_session().await;
        let active = manager.create_session().await;
        assert_eq!(manager.session_count().await, 2);

        // Nothing is idle yet, so a sweep with a generous window is a no-op.
        assert_eq!(manager.cleanup_idle(Duration::from_secs(3600)).await, 0);
        assert_eq!(manager.session_count().await, 2);

        // Let both age, then touch only `active`.
        tokio::time::sleep(Duration::from_millis(200)).await;
        manager.touch(&active).await;

        // A window shorter than `stale`'s idle time but comfortably longer
        // than `active`'s collects exactly the abandoned one.
        assert_eq!(manager.cleanup_idle(Duration::from_millis(100)).await, 1);
        assert_eq!(manager.session_count().await, 1);
        assert!(manager.session_exists(&active).await);
        assert!(!manager.session_exists(&stale).await);
    }

    #[tokio::test]
    async fn a_session_with_a_live_sse_stream_is_never_collected() {
        let manager = McpSessionManager::new();
        let watching = manager.create_session().await;
        let abandoned = manager.create_session().await;

        let stream = manager.subscribe(&watching).await;
        assert!(stream.is_some());

        // Zero idle window: everything is stale, but the subscriber holds on.
        assert_eq!(manager.cleanup_idle(Duration::ZERO).await, 1);
        assert!(manager.session_exists(&watching).await);
        assert!(!manager.session_exists(&abandoned).await);

        // Once the client disconnects, the session becomes collectable.
        drop(stream);
        assert_eq!(manager.cleanup_idle(Duration::ZERO).await, 1);
        assert_eq!(manager.session_count().await, 0);
    }

    #[tokio::test]
    async fn terminate_removes_only_the_named_session() {
        let manager = McpSessionManager::new();
        let a = manager.create_session().await;
        let b = manager.create_session().await;

        assert!(manager.terminate(&a).await);
        // Terminating twice reports that there was nothing to do.
        assert!(!manager.terminate(&a).await);
        assert!(manager.session_exists(&b).await);
        assert_eq!(manager.session_count().await, 1);
    }
}
