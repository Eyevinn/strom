//! WebSocket endpoint for real-time bidirectional updates.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::{sink::SinkExt, stream::StreamExt};
use std::time::Duration;
use strom_types::StromEvent;
use tokio::select;
use tokio::time::interval;
use tracing::{debug, error, info, trace};

use crate::state::AppState;

/// WebSocket endpoint for real-time bidirectional communication.
///
/// This endpoint establishes a WebSocket connection and:
/// - Streams events to the client whenever flows are created, updated, deleted, started, or stopped
/// - Sends ping messages every 15 seconds to keep the connection alive
/// - Handles pong responses from the client
///
/// Example usage from JavaScript:
/// ```javascript
/// const ws = new WebSocket('ws://localhost:3000/api/ws');
/// ws.onmessage = (event) => {
///     const data = JSON.parse(event.data);
///     console.log('Received event:', data);
/// };
/// ws.onclose = () => console.log('Disconnected');
/// ws.onerror = (error) => console.error('WebSocket error:', error);
/// ```
#[utoipa::path(
    get,
    path = "/api/ws",
    tag = "websocket",
    responses(
        (status = 101, description = "WebSocket connection upgraded"),
        (status = 401, description = "Authentication required (use auth_token query param)")
    )
)]
pub async fn websocket_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    info!(
        "New WebSocket client connecting (total subscribers: {})",
        state.events().subscriber_count() + 1
    );
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Handle an individual WebSocket connection.
async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to the event broadcaster
    let mut rx = state.events().subscribe();

    // Ping interval for keep-alive
    let mut ping_interval = interval(Duration::from_secs(15));

    // System stats interval (send every 1 second)
    let mut stats_interval = interval(Duration::from_secs(1));

    info!("WebSocket client connected");

    // Send a Ping event immediately to confirm connection
    // This is a valid StromEvent that the frontend can parse
    let welcome_event = StromEvent::Ping;
    if let Err(e) = send_event(&mut sender, welcome_event).await {
        error!("Failed to send welcome message: {}", e);
        return;
    }

    // Handle the WebSocket connection
    loop {
        select! {
            // Event from the broadcaster
            event_result = rx.recv() => {
                match event_result {
                    Ok(event) => {
                        if let Err(e) = send_event(&mut sender, event).await {
                            error!("Failed to send event to client: {}", e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("Client is lagging, skipped {} events", skipped);
                        // Try to continue despite lag
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("Event broadcaster closed, disconnecting client");
                        break;
                    }
                }
            }

            // Ping interval for keep-alive
            _ = ping_interval.tick() => {
                trace!("Sending ping to client");
                if let Err(e) = sender.send(Message::Ping(vec![].into())).await {
                    debug!("Failed to send ping, client likely disconnected: {}", e);
                    break;
                }
            }

            // System stats interval (also sends PTP stats and thread stats)
            _ = stats_interval.tick() => {
                // Send system stats
                let stats = state.get_system_stats().await;
                let event = StromEvent::SystemStats(stats);
                if let Err(e) = send_event(&mut sender, event).await {
                    debug!("Failed to send system stats, client likely disconnected: {}", e);
                    break;
                }

                // Send thread stats (CPU per GStreamer streaming thread)
                // Always send, even when empty, so frontend clears stale data
                let thread_stats = state.get_thread_stats();
                let event = StromEvent::ThreadStats(thread_stats);
                if let Err(e) = send_event(&mut sender, event).await {
                    debug!("Failed to send thread stats, client likely disconnected: {}", e);
                    break;
                }

                // Send PTP stats for flows with PTP clocks
                let ptp_events = state.get_ptp_stats_events().await;
                for ptp_event in ptp_events {
                    if let Err(e) = send_event(&mut sender, ptp_event).await {
                        debug!("Failed to send PTP stats, client likely disconnected: {}", e);
                        break;
                    }
                }
            }

            // Message from the client
            message = receiver.next() => {
                match message {
                    Some(Ok(Message::Pong(_))) => {
                        trace!("Received pong from client");
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Client sent close message");
                        break;
                    }
                    Some(Ok(Message::Text(text))) => {
                        debug!("Received text message from client: {}", text);
                        // Handle ping messages from client
                        if text.trim() == "ping" {
                            debug!("Received ping from client, sending pong");
                            if let Err(e) = sender.send(Message::Text("pong".into())).await {
                                error!("Failed to send pong: {}", e);
                                break;
                            }
                        }
                        // For future: handle other client commands here
                    }
                    Some(Ok(_)) => {
                        debug!("Received other message type from client");
                    }
                    Some(Err(e)) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        info!("Client disconnected");
                        break;
                    }
                }
            }
        }
    }

    info!("WebSocket client disconnected");
}

/// Send an event to the client as a JSON message.
async fn send_event(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    event: StromEvent,
) -> Result<(), axum::Error> {
    trace!("Sending event to client: {}", event.description());

    match serde_json::to_string(&event) {
        Ok(json) => {
            sender.send(Message::Text(json.into())).await?;
            Ok(())
        }
        Err(e) => {
            error!("Failed to serialize event: {}", e);
            Err(axum::Error::new(e))
        }
    }
}
