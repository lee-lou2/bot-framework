//! Per-call egress CONNECT proxy enforcing credentials, exact host:port allowlists, and public IPs.
//!
//! Opens HTTP `CONNECT` tunnels only after checking per-call Basic credentials, exact `host:port`
//! policy, and resolved public IP addresses (pinning the connection to the vetted IP). The only
//! plain-HTTP exception is `LOCAL_HOST` for sandbox effect-ledger callbacks handled in-process.

use crate::egress::{EgressPolicy, parse_target, vet_resolved};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bot_core::port::{Error, Reply};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
    time::{sleep, timeout},
};

/// Reserved `.invalid` host intercepted locally for effect-ledger callbacks; never routed externally.
pub const LOCAL_HOST: &str = "bot-effects.invalid";
const MAX_LOCAL_BODY: usize = 64 * 1024;

pub trait LocalService: Send + Sync {
    fn handle(&self, frame: Vec<u8>) -> Reply<'_, Vec<u8>>;
}

const MAX_HEAD: usize = 8 * 1024;

pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

pub trait Resolver: Send + Sync {
    fn resolve(&self, host: &str) -> Reply<'_, Vec<IpAddr>>;
}

pub trait Connector: Send + Sync {
    fn connect(&self, addr: SocketAddr) -> Reply<'_, Box<dyn Duplex>>;
}

pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str) -> Reply<'_, Vec<IpAddr>> {
        let host = host.to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|_| Error::Unavailable)?;
            Ok(found.map(|addr| addr.ip()).collect())
        })
    }
}

pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect(&self, addr: SocketAddr) -> Reply<'_, Box<dyn Duplex>> {
        Box::pin(async move {
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|_| Error::Unavailable)?;
            Ok(Box::new(stream) as Box<dyn Duplex>)
        })
    }
}

#[derive(Clone)]
pub struct ProxyConfig {
    pub policy: EgressPolicy,
    pub allow_private: bool,
    /// Operator-pinned private/local `(host, port) -> SocketAddr` overrides for policy-allowed targets.
    pub private_routes: std::collections::BTreeMap<(String, u16), SocketAddr>,
    pub user: String,
    pub password: String,
    pub head_timeout: Duration,
    pub connect_timeout: Duration,
    pub tunnel_limit: Duration,
    pub max_connections: usize,
    pub local: Option<Arc<dyn LocalService>>,
}

impl ProxyConfig {
    pub fn new(policy: EgressPolicy, user: String, password: String) -> Self {
        Self {
            policy,
            allow_private: false,
            private_routes: std::collections::BTreeMap::new(),
            user,
            password,
            head_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            tunnel_limit: Duration::from_secs(600),
            max_connections: 32,
            local: None,
        }
    }
}

#[derive(Default)]
struct Counters {
    tunnels: AtomicU64,
    auth: AtomicU64,
    policy: AtomicU64,
    address: AtomicU64,
    failed: AtomicU64,
    malformed: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub tunnels: u64,
    pub denied_auth: u64,
    pub denied_policy: u64,
    pub denied_address: u64,
    pub failed: u64,
    pub malformed: u64,
}

struct Shared {
    cfg: ProxyConfig,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
    counters: Counters,
}

pub struct Proxy {
    addr: SocketAddr,
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    accept: JoinHandle<()>,
}

impl Proxy {
    pub async fn start(
        cfg: ProxyConfig,
        resolver: Arc<dyn Resolver>,
        connector: Arc<dyn Connector>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let slots = Arc::new(Semaphore::new(cfg.max_connections));
        let shared = Arc::new(Shared {
            cfg,
            resolver,
            connector,
            counters: Counters::default(),
        });
        let (stop, signal) = watch::channel(false);
        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&shared), slots, signal));
        Ok(Self {
            addr,
            shared,
            stop,
            accept,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> Stats {
        let c = &self.shared.counters;
        Stats {
            tunnels: c.tunnels.load(Ordering::Relaxed),
            denied_auth: c.auth.load(Ordering::Relaxed),
            denied_policy: c.policy.load(Ordering::Relaxed),
            denied_address: c.address.load(Ordering::Relaxed),
            failed: c.failed.load(Ordering::Relaxed),
            malformed: c.malformed.load(Ordering::Relaxed),
        }
    }

    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.accept.await;
    }
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            accepted = listener.accept() => {
                let Ok((client, _)) = accepted else { continue };
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else { continue };
                let (shared, stop) = (Arc::clone(&shared), stop.clone());
                tokio::spawn(async move {
                    serve(client, &shared, stop).await;
                    drop(permit);
                });
            }
        }
    }
}

async fn reply(client: &mut TcpStream, status: &str, extra: &str) {
    let text =
        format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n{extra}\r\n");
    let _ = client.write_all(text.as_bytes()).await;
    let _ = client.shutdown().await;
}

struct Head {
    target: String,
    authorization: Option<String>,
    content_length: Option<usize>,
    chunked: bool,
    leftover: Vec<u8>,
}

/// Reads request headers while preserving already-buffered post-header bytes (e.g., TLS ClientHello).
async fn read_head(client: &mut TcpStream) -> Option<Head> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
        let n = client.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if end + 4 > MAX_HEAD {
        return None;
    }
    let leftover = buf[end + 4..].to_vec();
    let text = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next()?.split(' ');
    let (method, target, version) = (request.next()?, request.next()?, request.next()?);
    if request.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }
    let mut authorization = None;
    let (mut content_length, mut chunked) = (None, false);
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some()
                || value.trim().is_empty()
                || !value.trim().bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            content_length = Some(value.trim().parse().ok()?);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = true;
        }
        if name.eq_ignore_ascii_case("proxy-authorization") {
            if authorization.is_some() {
                return None;
            }
            authorization = Some(value.trim().to_owned());
        }
    }
    // Defer method dispatch to the caller so well-formed unsupported methods return 405.
    Some(Head {
        target: format!("{method} {target}"),
        authorization,
        content_length,
        chunked,
        leftover,
    })
}

async fn serve(mut client: TcpStream, shared: &Shared, mut stop: watch::Receiver<bool>) {
    let c = &shared.counters;
    let Ok(Some(head)) = timeout(shared.cfg.head_timeout, read_head(&mut client)).await else {
        c.malformed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "400 Bad Request", "").await;
    };
    let local_path = head
        .target
        .strip_prefix("POST http://")
        .and_then(|rest| rest.strip_prefix(LOCAL_HOST))
        .map(|rest| rest.strip_prefix(":80").unwrap_or(rest).to_owned());
    if let (Some(local), Some(path)) = (&shared.cfg.local, local_path) {
        return serve_local(client, shared, local, &path, head).await;
    }
    let Some(target) = head.target.strip_prefix("CONNECT ") else {
        c.malformed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "405 Method Not Allowed", "Allow: CONNECT\r\n").await;
    };
    if !credentials_match(head.authorization.as_deref(), &shared.cfg) {
        c.auth.fetch_add(1, Ordering::Relaxed);
        return reply(
            &mut client,
            "407 Proxy Authentication Required",
            "Proxy-Authenticate: Basic realm=\"egress\"\r\n",
        )
        .await;
    }
    let Some((host, port)) = parse_target(target) else {
        c.malformed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "400 Bad Request", "").await;
    };
    if !shared.cfg.policy.permits(&host, port) {
        c.policy.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "403 Forbidden", "").await;
    }
    let dest = if let Some(&addr) = shared.cfg.private_routes.get(&(host.clone(), port)) {
        addr
    } else {
        let Ok(resolved) = shared.resolver.resolve(&host).await else {
            c.failed.fetch_add(1, Ordering::Relaxed);
            return reply(&mut client, "502 Bad Gateway", "").await;
        };
        let Some(ip) = vet_resolved(&resolved, shared.cfg.allow_private) else {
            c.address.fetch_add(1, Ordering::Relaxed);
            return reply(&mut client, "403 Forbidden", "").await;
        };
        SocketAddr::new(ip, port)
    };
    let connecting = shared.connector.connect(dest);
    let Ok(Ok(mut upstream)) = timeout(shared.cfg.connect_timeout, connecting).await else {
        c.failed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "502 Bad Gateway", "").await;
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
        || (!head.leftover.is_empty() && upstream.write_all(&head.leftover).await.is_err())
    {
        c.failed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    c.tunnels.fetch_add(1, Ordering::Relaxed);
    tokio::select! {
        _ = copy_bidirectional(&mut client, &mut upstream) => {}
        _ = stop.changed() => {}
        _ = sleep(shared.cfg.tunnel_limit) => {}
    }
    let _ = client.shutdown().await;
}

async fn serve_local(
    mut client: TcpStream,
    shared: &Shared,
    local: &Arc<dyn LocalService>,
    path: &str,
    head: Head,
) {
    let c = &shared.counters;
    if !credentials_match(head.authorization.as_deref(), &shared.cfg) {
        c.auth.fetch_add(1, Ordering::Relaxed);
        return reply(
            &mut client,
            "407 Proxy Authentication Required",
            "Proxy-Authenticate: Basic realm=\"egress\"\r\n",
        )
        .await;
    }
    if path != "/effect" {
        c.malformed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "404 Not Found", "").await;
    }
    let Some(length) = head
        .content_length
        .filter(|n| *n <= MAX_LOCAL_BODY && !head.chunked)
    else {
        c.malformed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "411 Length Required", "").await;
    };
    let mut body = head.leftover;
    let read = timeout(shared.cfg.head_timeout, async {
        let mut chunk = [0u8; 4096];
        while body.len() < length {
            let want = (length - body.len()).min(chunk.len());
            match client.read(&mut chunk[..want]).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        body.len() == length
    })
    .await;
    if !matches!(read, Ok(true)) {
        c.malformed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "400 Bad Request", "").await;
    }
    let Ok(answer) = local.handle(body).await else {
        c.failed.fetch_add(1, Ordering::Relaxed);
        return reply(&mut client, "502 Bad Gateway", "").await;
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        answer.len()
    );
    let _ = client.write_all(head.as_bytes()).await;
    let _ = client.write_all(&answer).await;
    let _ = client.shutdown().await;
}

fn credentials_match(header: Option<&str>, cfg: &ProxyConfig) -> bool {
    let Some(encoded) = header.and_then(|h| h.strip_prefix("Basic ")) else {
        return false;
    };
    let Some(decoded) = base64_decode(encoded.trim()) else {
        return false;
    };
    constant_time_eq(
        &decoded,
        format!("{}:{}", cfg.user, cfg.password).as_bytes(),
    )
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn base64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}

#[cfg(test)]
mod tests;
