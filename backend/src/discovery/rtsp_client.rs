//! Minimal RTSP client for fetching SDP via DESCRIBE requests.
//!
//! Used to retrieve SDP from RAVENNA sources discovered via mDNS.

use anyhow::{anyhow, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::debug;

/// Fetch SDP from an RTSP server using DESCRIBE method.
///
/// # Arguments
/// * `url` - The RTSP URL (e.g., "rtsp://192.168.1.100:8554/stream1")
///
/// # Returns
/// The SDP content as a string
pub async fn rtsp_describe(url: &str) -> Result<String> {
    debug!("Fetching SDP from RTSP URL: {}", url);

    // Parse URL
    let parsed = parse_rtsp_url(url)?;

    // Connect to server
    let socket = TcpStream::connect((parsed.host.as_str(), parsed.port)).await?;
    let mut reader = BufReader::new(socket);

    // Send DESCRIBE request
    let request = format!(
        "DESCRIBE {} RTSP/1.0\r\n\
         CSeq: 1\r\n\
         Accept: application/sdp\r\n\
         \r\n",
        url
    );

    debug!("Sending RTSP DESCRIBE request");
    reader.get_mut().write_all(request.as_bytes()).await?;

    // Read response
    let mut lines = Vec::new();
    let mut line = String::new();

    // Read response line
    line.clear();
    reader.read_line(&mut line).await?;
    lines.push(line.clone());

    // Check response code
    if !line.contains("200 OK") {
        return Err(anyhow!("RTSP server returned error: {}", line.trim()));
    }

    // Read headers
    let mut content_length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).await?;
        if line == "\r\n" || line.is_empty() {
            break;
        }

        // Extract Content-Length
        if let Some(len_str) = line.strip_prefix("Content-Length:") {
            content_length = len_str.trim().parse().unwrap_or(0);
        }

        lines.push(line.clone());
    }

    // Read SDP content
    let mut sdp = String::new();
    if content_length > 0 {
        let mut sdp_buf = vec![0u8; content_length];
        reader.read_exact(&mut sdp_buf).await?;
        sdp = String::from_utf8(sdp_buf)?;
    } else {
        // No Content-Length, read until end
        reader.read_to_string(&mut sdp).await?;
    }

    if sdp.is_empty() {
        return Err(anyhow!("No SDP content in RTSP response"));
    }

    debug!("Received SDP ({} bytes)", sdp.len());
    Ok(sdp)
}

/// Parsed RTSP URL components.
struct RtspUrl {
    host: String,
    port: u16,
    #[allow(dead_code)]
    path: String,
}

/// Parse an RTSP URL into components.
fn parse_rtsp_url(url: &str) -> Result<RtspUrl> {
    // rtsp://host:port/path
    let url = url
        .strip_prefix("rtsp://")
        .ok_or_else(|| anyhow!("URL must start with rtsp://"))?;

    // Split host:port and path
    let parts: Vec<&str> = url.splitn(2, '/').collect();
    let host_port = parts[0];
    let path = if parts.len() > 1 {
        format!("/{}", parts[1])
    } else {
        "/".to_string()
    };

    // Split host and port
    let (host, port) = if let Some(colon_pos) = host_port.rfind(':') {
        let host = host_port[..colon_pos].to_string();
        let port = host_port[colon_pos + 1..]
            .parse()
            .map_err(|_| anyhow!("Invalid port number"))?;
        (host, port)
    } else {
        (host_port.to_string(), 8554) // Default RTSP port
    };

    Ok(RtspUrl { host, port, path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rtsp_url() {
        let url = "rtsp://192.168.1.100:8554/stream1";
        let parsed = parse_rtsp_url(url).unwrap();
        assert_eq!(parsed.host, "192.168.1.100");
        assert_eq!(parsed.port, 8554);
        assert_eq!(parsed.path, "/stream1");
    }

    #[test]
    fn test_parse_rtsp_url_no_port() {
        let url = "rtsp://example.com/test";
        let parsed = parse_rtsp_url(url).unwrap();
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 8554);
        assert_eq!(parsed.path, "/test");
    }

    #[test]
    fn test_parse_rtsp_url_no_path() {
        let url = "rtsp://192.168.1.100:554";
        let parsed = parse_rtsp_url(url).unwrap();
        assert_eq!(parsed.host, "192.168.1.100");
        assert_eq!(parsed.port, 554);
        assert_eq!(parsed.path, "/");
    }

    #[test]
    fn test_parse_rtsp_url_invalid_scheme() {
        let url = "http://192.168.1.100:8554/stream";
        let result = parse_rtsp_url(url);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rtsp://"));
    }

    #[test]
    fn test_parse_rtsp_url_no_scheme() {
        let url = "192.168.1.100:8554/stream";
        let result = parse_rtsp_url(url);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_rtsp_url_invalid_port() {
        let url = "rtsp://192.168.1.100:notaport/stream";
        let result = parse_rtsp_url(url);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("port"));
    }

    #[test]
    fn test_parse_rtsp_url_with_nested_path() {
        let url = "rtsp://192.168.1.100:8554/by-name/stream1";
        let parsed = parse_rtsp_url(url).unwrap();
        assert_eq!(parsed.host, "192.168.1.100");
        assert_eq!(parsed.port, 8554);
        assert_eq!(parsed.path, "/by-name/stream1");
    }

    #[test]
    fn test_parse_rtsp_url_hostname() {
        let url = "rtsp://ravenna-device.local:8554/stream";
        let parsed = parse_rtsp_url(url).unwrap();
        assert_eq!(parsed.host, "ravenna-device.local");
        assert_eq!(parsed.port, 8554);
        assert_eq!(parsed.path, "/stream");
    }
}
