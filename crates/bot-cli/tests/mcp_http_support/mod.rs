//! Local HTTP/1.1 wrapper around `bot_broker::testing::FakeMcp` and connection-counting listener for MCP tests.
#![allow(dead_code)]
use bot_broker::testing::{FakeMcp, McpReply, McpRequest};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

pub struct Served {
    pub port: u16,
    pub mcp: Arc<FakeMcp>,
    task: JoinHandle<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Served {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}

pub async fn serve(mcp: FakeMcp) -> Served {
    let mcp = Arc::new(mcp);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::clone(&mcp);
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mcp = Arc::clone(&shared);
            tokio::spawn(async move {
                let _ = answer(stream, mcp).await;
            });
        }
    });
    Served { port, mcp, task }
}

/// Accepts TCP connections and counts them without reading requests (used as a redirect trap).
pub struct Counter {
    pub port: u16,
    accepted: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Counter {
    pub async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            while let Ok((_stream, _)) = listener.accept().await {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        });
        Self {
            port,
            accepted,
            task,
        }
    }

    pub async fn accepted(&self) -> usize {
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Allocates and immediately closes a loopback port so connects fail with connection refused.
pub async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn answer(mut stream: TcpStream, mcp: Arc<FakeMcp>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (
        first.next().unwrap_or_default().to_owned(),
        first.next().unwrap_or_default().to_owned(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let reply: McpReply = mcp.handle(McpRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let mut out = format!("HTTP/1.1 {} Reply\r\n", reply.status);
    for (name, value) in &reply.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    stream.write_all(out.as_bytes()).await?;
    stream.flush().await?;
    // Simulate a hanging server by sending headers only and holding the body open.
    if reply.chunks.first().is_some_and(|c| c == b"HANG") {
        tokio::time::sleep(Duration::from_secs(60)).await;
        return Ok(());
    }
    for chunk in &reply.chunks {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await?;
        stream.write_all(chunk).await?;
        stream.write_all(b"\r\n").await?;
        stream.flush().await?;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.shutdown().await
}

pub fn copy_example_profile(root: &std::path::Path) {
    copy_dir(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles/personal"),
        &root.join("personal"),
    );
}

pub fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

pub fn host_file(dir: &std::path::Path, port: u16, auth: bool) -> std::path::PathBuf {
    let auth = if auth {
        r#","auth":{"kind":"bearer","secret":"weather-token"}"#
    } else {
        ""
    };
    let path = dir.join("host.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"version":1,"servers":{{"weather":{{"transport":"http","url":"http://127.0.0.1:{port}/mcp"{auth}}}}}}}"#
        ),
    )
    .unwrap();
    path
}

pub fn settings(root: &std::path::Path, host: &std::path::Path) -> bot_cli::settings::Settings {
    bot_cli::settings::Settings {
        profiles: Some(root.to_owned()),
        host_config: Some(host.to_owned()),
        database_url: None,
        db_max_connections: None,
        socket: None,
        secrets: bot_cli::settings::SecretsKind::Env,
        secrets_reread: None,
        installs: None,
        admin_token_file: None,
    }
}
