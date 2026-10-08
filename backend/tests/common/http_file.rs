//! A minimal HTTP server for one file that cannot seek: it says so
//! (`Accept-Ranges: none`) and answers every request, a range request
//! included, with the whole file. A source reading from it plays from the
//! start and cannot jump anywhere else. Included with `#[path]`.

use std::io::{Read, Write};
use std::net::TcpListener;

/// Serve `body` on 127.0.0.1 until the process ends. Returns the URL.
pub fn serve_without_ranges(body: Vec<u8>, name: &str, content_type: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let content_type = content_type.to_string();
    let body = std::sync::Arc::new(body);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = std::sync::Arc::clone(&body);
            let content_type = content_type.clone();
            std::thread::spawn(move || {
                // Read the request head; its contents do not matter.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n",
                    content_type,
                    body.len()
                );
                // A client that stops reading (a pipeline torn down for a
                // reload) closes the connection; that is not an error here.
                let _ = stream
                    .write_all(reply.as_bytes())
                    .and_then(|()| stream.write_all(&body));
            });
        }
    });
    format!("http://127.0.0.1:{port}/{name}")
}
