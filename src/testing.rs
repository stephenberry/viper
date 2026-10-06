//! Test support: a scripted HTTP server that records requests and replays canned responses.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct MockResponse {
    pub status: u16,
    pub body: String,
}

impl MockResponse {
    pub fn sse(events: &[Value]) -> MockResponse {
        let body = events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        MockResponse { status: 200, body }
    }

    /// Anthropic-style SSE with `event:` lines.
    pub fn anthropic(events: &[Value]) -> MockResponse {
        let body = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("")))
            .collect::<String>();
        MockResponse { status: 200, body }
    }

    pub fn error(status: u16, body: &str) -> MockResponse {
        MockResponse { status, body: body.to_string() }
    }
}

pub struct MockServer {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

impl MockServer {
    /// Serve `responses` in order, one per request.
    pub async fn start(responses: Vec<MockResponse>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let body_start = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = find(&buf, b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                    .unwrap_or(0);
                while buf.len() < body_start + length {
                    let n = socket.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body: Value = serde_json::from_slice(&buf[body_start..body_start + length]).unwrap_or(Value::Null);
                recorded.lock().unwrap().push(body);
                let content_type = if response.status == 200 { "text/event-stream" } else { "application/json" };
                let reply = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            }
        });
        MockServer { url, requests }
    }
}
