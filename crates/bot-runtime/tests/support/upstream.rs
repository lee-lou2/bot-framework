//! Real loopback HTTP fake upstream for testing direct API transport behavior (redirects, timeouts, cancellation, disconnects).
#![allow(dead_code)]

use serde_json::Value;
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body must be JSON")
    }

    pub fn tool_names(&self) -> Vec<String> {
        let body = self.json();
        body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| {
                tool["name"]
                    .as_str()
                    .or_else(|| tool["function"]["name"].as_str())
                    .map(str::to_owned)
            })
            .collect()
    }

    /// Resolves the wire alias presented to the model for `tool_id`.
    pub fn alias(&self, tool_id: &str) -> String {
        let stem = tool_id.replace('.', "_");
        self.tool_names()
            .into_iter()
            .find(|name| name == tool_id || name.starts_with(&format!("{stem}_")))
            .unwrap_or_else(|| {
                panic!(
                    "tool {tool_id} missing from request: {:?}",
                    self.tool_names()
                )
            })
    }
}

#[derive(Clone)]
pub enum Reply {
    Json(u16, String),
    Raw {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Redirect(String),
    Delayed(Duration, Box<Reply>),
    With(Arc<dyn Fn(&Seen) -> Reply + Send + Sync>),
    Hang,
    Close,
}

struct State {
    script: VecDeque<Reply>,
    seen: Vec<Seen>,
}

pub struct FakeApi {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FakeApi {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            script: VecDeque::new(),
            seen: Vec::new(),
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, shared).await;
                });
            }
        });
        Self { addr, state, task }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn script(&self, reply: Reply) -> &Self {
        lock(&self.state).script.push_back(reply);
        self
    }

    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }

    pub async fn wait_for(&self, count: usize) {
        for _ in 0..1000 {
            if self.seen().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "upstream did not receive {count} request(s): got {}",
            self.seen().len()
        );
    }
}

impl Drop for FakeApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at + 4;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 || buf.len() > 1 << 20 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines
        .next()
        .unwrap_or_default()
        .split(' ')
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[head_end..(head_end + length).min(buf.len())].to_vec();
    let seen = Seen {
        path,
        headers,
        body,
    };
    let reply = {
        let mut state = lock(&state);
        state.seen.push(seen.clone());
        state.script.pop_front()
    };
    let reply = reply.unwrap_or_else(|| Reply::Json(500, r#"{"error":"no script"}"#.into()));
    respond(&mut stream, reply, &seen).await
}

async fn respond(stream: &mut TcpStream, mut reply: Reply, seen: &Seen) -> std::io::Result<()> {
    loop {
        match reply {
            Reply::Delayed(delay, inner) => {
                tokio::time::sleep(delay).await;
                reply = *inner;
            }
            Reply::With(decide) => reply = decide(seen),
            Reply::Hang => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                return Ok(());
            }
            Reply::Close => return Ok(()),
            Reply::Json(status, body) => {
                let headers = vec![("content-type".to_owned(), "application/json".to_owned())];
                return write(stream, status, headers, body.into_bytes()).await;
            }
            Reply::Redirect(location) => {
                return write(
                    stream,
                    302,
                    vec![("location".to_owned(), location)],
                    Vec::new(),
                )
                .await;
            }
            Reply::Raw {
                status,
                headers,
                body,
            } => return write(stream, status, headers, body).await,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        529 => "Overloaded",
        _ => "Status",
    }
}

async fn write(
    stream: &mut TcpStream,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await
}
