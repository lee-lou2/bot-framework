//! Loopback HTTP control endpoint for external OpenAI SDK tests (`POST /__ctl/script`, `GET /__ctl/stats`).
//! Counts come directly from PostgreSQL and FakeUpstream: `runs`, `active`, `model_calls`, `tool_runs`, `tool_reads`, and `queued`.
use super::plan;
use crate::claude_support::Rig;
use bot_broker::testing::FakeUpstream;
use serde_json::{Value, json};
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

const RUNS: &str = "select count(*) from {s}.bot_requests where api_endpoint is not null";
const WRITES: &str =
    "select count(*) from {s}.bot_calls where tool like '%remove' and state in ('started', 'done')";
const READS: &str =
    "select count(*) from {s}.bot_calls where tool like '%list' and state in ('started', 'done')";
const ACTIVE: &str = "select count(*) from {s}.bot_runs where api_endpoint is not null and state in ('ready', 'leased', 'waiting', 'retry')";

struct State {
    upstream: Arc<FakeUpstream>,
    base: String,
    schema: String,
    pushed: AtomicUsize,
}

pub struct Control {
    pub port: u16,
    task: JoinHandle<()>,
    state: Arc<State>,
}

impl Control {
    pub async fn start(rig: &Rig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(State {
            upstream: rig.upstream.clone(),
            base: rig.schema.base.clone(),
            schema: rig.schema.name.clone(),
            pushed: AtomicUsize::new(0),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(handle(socket, state.clone()));
            }
        });
        Self {
            port,
            task,
            state: shared,
        }
    }

    pub fn script(&self, plan: &Value) {
        let added = plan::push(&self.state.upstream, plan);
        self.state.pushed.fetch_add(added, Ordering::SeqCst);
    }

    pub async fn stats(&self) -> Value {
        stats(&self.state).await
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/__ctl", self.port)
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn count(state: &State, sql: &str) -> u64 {
    let out = Command::new("psql")
        .args([&state.base, "-Atc", &sql.replace("{s}", &state.schema)])
        .output()
        .expect("psql");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("query failed: {}", String::from_utf8_lossy(&out.stderr)))
}

async fn stats(state: &Arc<State>) -> Value {
    let probe = state.clone();
    let (runs, active, tool_runs, tool_reads) = tokio::task::spawn_blocking(move || {
        (
            count(&probe, RUNS),
            count(&probe, ACTIVE),
            count(&probe, WRITES),
            count(&probe, READS),
        )
    })
    .await
    .unwrap();
    let model_calls = state.upstream.requests().len();
    let queued = state
        .pushed
        .load(Ordering::SeqCst)
        .saturating_sub(model_calls);
    json!({"runs": runs, "active": active, "model_calls": model_calls, "tool_runs": tool_runs, "tool_reads": tool_reads, "queued": queued})
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut line = head.lines().next()?.split(' ');
    let (method, path) = (line.next()?.to_owned(), line.next()?.to_owned());
    let length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    let mut body = buf[end..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some((method, path, body))
}

async fn handle(mut socket: TcpStream, state: Arc<State>) {
    let Some((method, path, body)) = read_request(&mut socket).await else {
        return;
    };
    let (status, reply) = match (method.as_str(), path.as_str()) {
        ("GET", "/__ctl/stats") => (200, stats(&state).await),
        ("POST", "/__ctl/script") => match serde_json::from_slice::<Value>(&body) {
            Ok(plan) => {
                let added = plan::push(&state.upstream, &plan);
                state.pushed.fetch_add(added, Ordering::SeqCst);
                (200, json!({"queued": added}))
            }
            Err(_) => (400, json!({"error": "request body is not JSON"})),
        },
        _ => (404, json!({})),
    };
    let text = reply.to_string();
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        text.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(text.as_bytes()).await;
    let _ = socket.shutdown().await;
}
