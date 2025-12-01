//! WebSocket client for real-time bidirectional updates.
//!
//! Supports both WASM (using gloo-net) and native (using tokio-tungstenite) platforms.

use std::sync::mpsc::Sender;
use strom_types::StromEvent;

use crate::state::{AppMessage, ConnectionState};

/// WebSocket client for connecting to the backend event stream.
pub struct WebSocketClient {
    url: String,
    connected: bool,
    /// Optional auth token for authentication
    auth_token: Option<String>,
}

impl WebSocketClient {
    /// Create a new WebSocket client with the given URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            connected: false,
            auth_token: None,
        }
    }

    /// Create a new WebSocket client with auth token.
    pub fn new_with_auth(url: impl Into<String>, auth_token: Option<String>) -> Self {
        Self {
            url: url.into(),
            connected: false,
            auth_token,
        }
    }

    /// Connect to the WebSocket and start listening for events.
    /// Automatically reconnects on disconnect with exponential backoff.
    ///
    /// The `tx` sender will be used to send messages to the main UI thread.
    /// The `ctx` is used to request repaints when events are received.
    pub fn connect(&mut self, tx: Sender<AppMessage>, ctx: egui::Context) {
        tracing::info!("Connecting to WebSocket: {}", self.url);

        // Build URL with auth token as query parameter if present
        let url = if let Some(ref token) = self.auth_token {
            if self.url.contains('?') {
                format!("{}&auth_token={}", self.url, token)
            } else {
                format!("{}?auth_token={}", self.url, token)
            }
        } else {
            self.url.clone()
        };

        #[cfg(target_arch = "wasm32")]
        {
            use wasm_bindgen_futures::spawn_local;
            spawn_local(async move {
                Self::wasm_connection_loop(url, tx, ctx).await;
            });
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            tokio::spawn(async move {
                Self::native_connection_loop(url, tx, ctx).await;
            });
        }

        self.connected = true;
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Disconnect from the WebSocket.
    pub fn disconnect(&mut self) {
        tracing::info!("Disconnecting from WebSocket");
        self.connected = false;
        // The actual WebSocket will be dropped when the async task completes
    }
}

// ============================================================================
// WASM Implementation (using gloo-net)
// ============================================================================

#[cfg(target_arch = "wasm32")]
impl WebSocketClient {
    async fn wasm_connection_loop(url: String, tx: Sender<AppMessage>, ctx: egui::Context) {
        use futures_util::stream::StreamExt;
        use gloo_net::websocket::{futures::WebSocket, Message};
        use gloo_timers::future::sleep;
        use std::time::Duration;

        let mut attempt = 1u32;

        loop {
            // Notify that we're attempting to connect
            let _ = tx.send(AppMessage::ConnectionStateChanged(
                ConnectionState::Reconnecting { attempt },
            ));
            ctx.request_repaint();

            tracing::info!("WebSocket connection attempt {} to: {}", attempt, url);

            match WebSocket::open(&url) {
                Ok(mut ws) => {
                    tracing::info!(
                        "WebSocket handshake initiated, waiting for stream readiness..."
                    );

                    // Track if we've marked as connected
                    let mut marked_connected = false;

                    // Read messages from the WebSocket
                    while let Some(msg) = ws.next().await {
                        match msg {
                            Ok(Message::Text(text)) => {
                                // Only mark as connected after we receive a valid text message
                                if !marked_connected {
                                    tracing::info!(
                                        "Received text message from backend - connection confirmed"
                                    );
                                    let _ = tx.send(AppMessage::ConnectionStateChanged(
                                        ConnectionState::Connected,
                                    ));
                                    ctx.request_repaint();
                                    marked_connected = true;
                                    attempt = 1;
                                }

                                tracing::trace!("Received WebSocket message: {}", text);

                                // Parse the event
                                match serde_json::from_str::<StromEvent>(&text) {
                                    Ok(event) => {
                                        tracing::trace!(
                                            "Parsed WebSocket event: {}",
                                            event.description()
                                        );
                                        let _ = tx.send(AppMessage::Event(event));
                                        ctx.request_repaint();
                                    }
                                    Err(err) => {
                                        tracing::error!("Failed to parse WebSocket event: {}", err);
                                    }
                                }
                            }
                            Ok(Message::Bytes(_)) => {
                                tracing::trace!("Received binary message (ignored)");
                            }
                            Err(e) => {
                                tracing::error!("WebSocket error: {:?}", e);
                                break;
                            }
                        }
                    }

                    // Connection closed
                    if marked_connected {
                        tracing::warn!("WebSocket connection lost, will attempt to reconnect...");
                    } else {
                        tracing::warn!("WebSocket connection attempt failed, will retry...");
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to open WebSocket: {:?}", e);
                }
            }

            // Wait before reconnecting (exponential backoff with max 10 seconds)
            let delay_ms = (1000u64 * 2u64.pow(attempt.min(4) - 1)).min(10000);
            tracing::info!("Waiting {}ms before reconnection attempt...", delay_ms);
            sleep(Duration::from_millis(delay_ms)).await;

            attempt += 1;
        }
    }
}

// ============================================================================
// Native Implementation (using tokio-tungstenite)
// ============================================================================

#[cfg(not(target_arch = "wasm32"))]
impl WebSocketClient {
    async fn native_connection_loop(url: String, tx: Sender<AppMessage>, ctx: egui::Context) {
        use futures_util::stream::StreamExt;
        use tokio::time::{sleep, Duration};
        use tokio_tungstenite::{connect_async, tungstenite::Message};

        let mut attempt = 1u32;

        loop {
            // Notify that we're attempting to connect
            let _ = tx.send(AppMessage::ConnectionStateChanged(
                ConnectionState::Reconnecting { attempt },
            ));
            ctx.request_repaint();

            tracing::info!("WebSocket connection attempt {} to: {}", attempt, url);

            match connect_async(&url).await {
                Ok((mut ws_stream, _)) => {
                    tracing::info!("WebSocket connected successfully");

                    // Track if we've marked as connected
                    let mut marked_connected = false;

                    // Read messages from the WebSocket
                    while let Some(msg_result) = ws_stream.next().await {
                        match msg_result {
                            Ok(Message::Text(text)) => {
                                // Only mark as connected after we receive a valid text message
                                if !marked_connected {
                                    tracing::info!(
                                        "Received text message from backend - connection confirmed"
                                    );
                                    let _ = tx.send(AppMessage::ConnectionStateChanged(
                                        ConnectionState::Connected,
                                    ));
                                    ctx.request_repaint();
                                    marked_connected = true;
                                    attempt = 1;
                                }

                                tracing::trace!("Received WebSocket message: {}", text);

                                // Parse the event
                                match serde_json::from_str::<StromEvent>(&text) {
                                    Ok(event) => {
                                        tracing::trace!(
                                            "Parsed WebSocket event: {}",
                                            event.description()
                                        );
                                        let _ = tx.send(AppMessage::Event(event));
                                        ctx.request_repaint();
                                    }
                                    Err(err) => {
                                        tracing::error!("Failed to parse WebSocket event: {}", err);
                                    }
                                }
                            }
                            Ok(Message::Binary(_)) => {
                                tracing::trace!("Received binary message (ignored)");
                            }
                            Ok(Message::Ping(_)) => {
                                // Pong is automatically handled by tokio-tungstenite
                                tracing::trace!("Received ping");
                            }
                            Ok(Message::Pong(_)) => {
                                tracing::trace!("Received pong");
                            }
                            Ok(Message::Close(_)) => {
                                tracing::info!("WebSocket closed by server");
                                break;
                            }
                            Ok(Message::Frame(_)) => {
                                // Raw frames are not used
                            }
                            Err(e) => {
                                tracing::error!("WebSocket error: {:?}", e);
                                break;
                            }
                        }
                    }

                    // Connection closed
                    if marked_connected {
                        tracing::warn!("WebSocket connection lost, will attempt to reconnect...");
                    } else {
                        tracing::warn!("WebSocket connection attempt failed, will retry...");
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to connect WebSocket: {:?}", e);
                }
            }

            // Wait before reconnecting (exponential backoff with max 10 seconds)
            let delay_ms = (1000u64 * 2u64.pow(attempt.min(4) - 1)).min(10000);
            tracing::info!("Waiting {}ms before reconnection attempt...", delay_ms);
            sleep(Duration::from_millis(delay_ms)).await;

            attempt += 1;
        }
    }
}
