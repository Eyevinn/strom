//! Minimal RTSP server for serving SDP via DESCRIBE requests.
//!
//! This server only implements the DESCRIBE method and is used to
//! announce RAVENNA streams via mDNS.

use crate::discovery::DiscoveryService;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

/// RTSP server configuration.
pub struct RtspServerConfig {
    /// Bind address (e.g., "0.0.0.0:8554")
    pub bind_addr: String,
}

impl Default for RtspServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8554".to_string(),
        }
    }
}

/// Run the RTSP server.
///
/// This server handles DESCRIBE requests and returns SDP for announced streams.
/// Stream IDs are extracted from the URL path (e.g., rtsp://host:port/stream_id).
pub async fn run_rtsp_server(config: RtspServerConfig, discovery: DiscoveryService) -> Result<()> {
    let listener = TcpListener::bind(&config.bind_addr).await?;
    info!("RTSP server listening on {}", config.bind_addr);

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                debug!("RTSP connection from {}", addr);
                let discovery = discovery.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_rtsp_connection(socket, discovery).await {
                        warn!("RTSP connection error from {}: {}", addr, e);
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept RTSP connection: {}", e);
            }
        }
    }
}

/// Handle a single RTSP connection.
async fn handle_rtsp_connection(socket: TcpStream, discovery: DiscoveryService) -> Result<()> {
    let mut reader = BufReader::new(socket);
    let mut lines = Vec::new();
    let mut line = String::new();

    // Read request
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line == "\r\n" {
            break;
        }
        lines.push(line.clone());
    }

    if lines.is_empty() {
        return Ok(());
    }

    // Parse request line
    let parts: Vec<&str> = lines[0].split_whitespace().collect();
    if parts.len() < 3 {
        send_error_response(&mut reader, 400, "Bad Request", "1").await?;
        return Ok(());
    }

    let method = parts[0];
    let url = parts[1];

    // Extract CSeq header
    let cseq = lines
        .iter()
        .find(|l| l.to_lowercase().starts_with("cseq:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "1".to_string());

    debug!("RTSP {} request for {}", method, url);

    // Handle DESCRIBE
    if method == "DESCRIBE" {
        handle_describe(url, &cseq, &mut reader, discovery).await?;
    } else if method == "OPTIONS" {
        handle_options(&cseq, &mut reader).await?;
    } else {
        send_error_response(&mut reader, 501, "Not Implemented", &cseq).await?;
    }

    Ok(())
}

/// Handle DESCRIBE request.
async fn handle_describe(
    url: &str,
    cseq: &str,
    reader: &mut BufReader<TcpStream>,
    discovery: DiscoveryService,
) -> Result<()> {
    // Extract stream ID from URL: rtsp://host:port/stream_id
    let stream_id = url.split('/').next_back().unwrap_or("").trim();

    if stream_id.is_empty() {
        send_error_response(reader, 404, "Not Found", cseq).await?;
        return Ok(());
    }

    debug!("Looking up SDP for stream ID: {}", stream_id);

    // Get SDP for this stream
    if let Some(sdp) = discovery.get_stream_sdp(stream_id).await {
        let response = format!(
            "RTSP/1.0 200 OK\r\n\
             CSeq: {}\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {}\r\n\
             \r\n\
             {}",
            cseq,
            sdp.len(),
            sdp
        );

        reader.get_mut().write_all(response.as_bytes()).await?;
        debug!("Sent SDP ({} bytes) for stream {}", sdp.len(), stream_id);
    } else {
        debug!("Stream not found: {}", stream_id);
        send_error_response(reader, 404, "Not Found", cseq).await?;
    }

    Ok(())
}

/// Handle OPTIONS request.
async fn handle_options(cseq: &str, reader: &mut BufReader<TcpStream>) -> Result<()> {
    let response = format!(
        "RTSP/1.0 200 OK\r\n\
         CSeq: {}\r\n\
         Public: DESCRIBE, OPTIONS\r\n\
         \r\n",
        cseq
    );

    reader.get_mut().write_all(response.as_bytes()).await?;
    Ok(())
}

/// Send an error response.
async fn send_error_response(
    reader: &mut BufReader<TcpStream>,
    code: u16,
    reason: &str,
    cseq: &str,
) -> Result<()> {
    let response = format!(
        "RTSP/1.0 {} {}\r\n\
         CSeq: {}\r\n\
         \r\n",
        code, reason, cseq
    );

    reader.get_mut().write_all(response.as_bytes()).await?;
    Ok(())
}
