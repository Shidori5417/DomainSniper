//! A tiny in-process HTTP server for sender tests: records requests, replies from a script.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// One scripted response.
#[derive(Clone)]
pub struct Canned {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Canned {
    pub fn status(status: u16) -> Canned {
        Canned {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    pub fn json(status: u16, body: &str) -> Canned {
        Canned::status(status)
            .header("Content-Type", "application/json")
            .with_body(body)
    }

    pub fn header(mut self, name: &str, value: &str) -> Canned {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn with_body(mut self, body: &str) -> Canned {
        self.body = body.into();
        self
    }
}

pub struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockServer {
    /// Starts listening on a free local port. Request `n` gets `script[n]`; the last entry
    /// repeats once the script runs out.
    pub async fn start(script: Vec<Canned>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(stream).await;
                let Some((mut stream, request)) = request else {
                    continue;
                };
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(request);
                    log.len() - 1
                };
                let canned = &script[index.min(script.len() - 1)];
                let _ = stream.write_all(&encode(canned)).await;
                let _ = stream.shutdown().await;
            }
        });
        MockServer {
            addr,
            requests,
            task,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

fn encode(canned: &Canned) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", canned.status);
    for (name, value) in &canned.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\n\r\n{}",
        canned.body.len(),
        canned.body
    ));
    head.into_bytes()
}

async fn read_request(mut stream: TcpStream) -> Option<(TcpStream, Request)> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().ok())
        })
        .flatten()
        .unwrap_or(0usize);
    while data.len() < header_end + content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..n]);
    }
    let mut first = head.lines().next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let body = String::from_utf8_lossy(&data[header_end..header_end + content_length]).to_string();
    Some((stream, Request { method, path, body }))
}
