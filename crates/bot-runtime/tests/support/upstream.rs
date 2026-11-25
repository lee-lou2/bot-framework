//! 실제 HTTP 가짜 상류(loopback의 TCP 서버): 받은 요청(경로·머리·본문)을 기록하고 대본대로 답한다. API 직접 호출 실행기의 실제 전송(리다이렉트·시간 한도·취소·연결 끊김)을
//! 시험하려고 진짜 소켓을 쓴다. 요청 하나에 연결 하나(`connection: close`)다.
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

/// 서버가 받은 요청 하나(머리 이름은 소문자)
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
        serde_json::from_slice(&self.body).expect("요청 본문은 JSON")
    }

    /// 요청에 실린 도구 이름들(Anthropic `tools[].name` · OpenAI `tools[].function.name`)
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

    /// 도구 ID(`agenda.list`)가 이 요청에서 모델에게 보인 이름(별칭)
    pub fn alias(&self, tool_id: &str) -> String {
        let stem = tool_id.replace('.', "_");
        self.tool_names()
            .into_iter()
            .find(|name| name == tool_id || name.starts_with(&format!("{stem}_")))
            .unwrap_or_else(|| panic!("요청에 {tool_id} 도구가 없다: {:?}", self.tool_names()))
    }
}

/// 서버의 응답 대본 한 줄
#[derive(Clone)]
pub enum Reply {
    /// 상태와 JSON 본문
    Json(u16, String),
    /// 머리·본문을 직접 정한다
    Raw {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// 302와 `location`
    Redirect(String),
    /// 지연 뒤에 다른 응답
    Delayed(Duration, Box<Reply>),
    /// 받은 요청에 따라 정한다(도구 별칭처럼 요청에서 알 수 있는 값을 응답에 쓸 때)
    With(Arc<dyn Fn(&Seen) -> Reply + Send + Sync>),
    /// 요청을 받고 끝내 답하지 않는다
    Hang,
    /// 요청을 받고 응답 없이 연결을 닫는다
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
            .expect("loopback 바인드");
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

    /// 기준 주소(경로 없음): Anthropic 형식의 `endpoint`
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 다음 요청에 줄 응답을 대본 끝에 더한다.
    pub fn script(&self, reply: Reply) -> &Self {
        lock(&self.state).script.push_back(reply);
        self
    }

    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }

    /// 요청이 `count`건이 될 때까지 기다린다(최대 10초).
    pub async fn wait_for(&self, count: usize) {
        for _ in 0..1000 {
            if self.seen().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("상류가 요청 {count}건을 받지 못했다: {}", self.seen().len());
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
