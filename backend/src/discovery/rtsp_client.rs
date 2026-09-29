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
#[derive(Debug)]
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
        // (url, host, port, path)
        let cases = [
            (
                "rtsp://192.0.2.10:8554/stream1",
                "192.0.2.10",
                8554,
                "/stream1",
            ),
            // Missing port defaults to 8554
            ("rtsp://example.com/test", "example.com", 8554, "/test"),
            // Missing path defaults to "/"
            ("rtsp://192.0.2.10:554", "192.0.2.10", 554, "/"),
            (
                "rtsp://192.0.2.10:8554/by-name/stream1",
                "192.0.2.10",
                8554,
                "/by-name/stream1",
            ),
            (
                "rtsp://ravenna-device.local:8554/stream",
                "ravenna-device.local",
                8554,
                "/stream",
            ),
        ];
        for (url, host, port, path) in cases {
            let parsed = parse_rtsp_url(url).unwrap_or_else(|e| panic!("{}: {}", url, e));
            assert_eq!(
                (parsed.host.as_str(), parsed.port, parsed.path.as_str()),
                (host, port, path),
                "{}",
                url
            );
        }
    }

    #[test]
    fn test_parse_rtsp_url_rejects() {
        // (url, text the error must contain)
        let cases = [
            ("http://192.0.2.10:8554/stream", "rtsp://"),
            ("192.0.2.10:8554/stream", "rtsp://"),
            ("rtsp://192.0.2.10:notaport/stream", "port"),
        ];
        for (url, expected) in cases {
            let err = parse_rtsp_url(url)
                .err()
                .unwrap_or_else(|| panic!("{} was accepted", url));
            assert!(
                err.to_string().contains(expected),
                "{}: error {:?} should mention {:?}",
                url,
                err.to_string(),
                expected
            );
        }
    }
}
