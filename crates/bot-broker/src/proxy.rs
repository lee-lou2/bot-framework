//! egress 프록시: 도구 서버가 외부로 나가는 유일한 길. 호출마다 하나를 열고 호출이 끝나면 닫는다.
//!
//! HTTP `CONNECT` 터널만 연다. 요청마다 다음을 순서대로 확인한다.
//! 1. 호출 전용 자격(`Proxy-Authorization: Basic`): 같은 호스트의 다른 프로세스가 이 허용 목록을 빌려 쓰지 못하게 한다.
//! 2. 정확한 `호스트:포트`가 허용 목록(`egress::EgressPolicy`)에 있는가(와일드카드·접미사 일치 없음).
//! 3. 이름을 풀어 얻은 주소가 모두 공개 주소인가(`egress::vet_resolved`). 연결은 검사한 주소에 고정한다.
//!
//! 리디렉션은 도구가 새 `CONNECT`로 따르므로 요청마다 같은 검사를 다시 받는다.
//! 평문 HTTP 프록시(절대 URI 요청)는 열지 않는다. 예외는 브로커가 직접 받는 전용 주소(`LOCAL_HOST`) 하나다:
//! 표준 입출력 밖의 통로(FD)를 줄 수 없는 샌드박스의 도구가 단계 원장 콜백(`effect.request`·`effect.status`)을
//! 같은 호출 전용 자격으로 보낸다. 이 주소는 허용 목록과 무관하게 바깥으로 나가지 않고 `LocalService`가 처리한다.

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

/// 브로커가 직접 처리하는 전용 호스트 이름. 예약된 최상위 도메인(`.invalid`)이라 실제 이름 풀이는 어디서도 성공하지 않는다.
pub const LOCAL_HOST: &str = "bot-effects.invalid";
/// 전용 주소의 요청 본문 상한(단계 원장 프레임 한도와 같다).
const MAX_LOCAL_BODY: usize = 64 * 1024;

/// 전용 주소로 온 프레임 한 줄을 처리해 응답 한 줄을 돌려준다.
pub trait LocalService: Send + Sync {
    fn handle(&self, frame: Vec<u8>) -> Reply<'_, Vec<u8>>;
}

/// 요청 머리 상한.
const MAX_HEAD: usize = 8 * 1024;

pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

pub trait Resolver: Send + Sync {
    fn resolve(&self, host: &str) -> Reply<'_, Vec<IpAddr>>;
}

pub trait Connector: Send + Sync {
    fn connect(&self, addr: SocketAddr) -> Reply<'_, Box<dyn Duplex>>;
}

/// 운영체제 이름 해석.
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

/// 일반 TCP 연결.
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
    /// 운영자가 사설망을 명시적으로 열었을 때만 true.
    pub allow_private: bool,
    /// 운영자가 호스트 설정에 명시한 사설/로컬 고정 목적지 `(host, port) -> SocketAddr`.
    /// `policy`를 통과한 정확한 대상에만 적용되며, 공개 도메인의 DNS 재바인딩 보호는 그대로 유지된다.
    pub private_routes: std::collections::BTreeMap<(String, u16), SocketAddr>,
    pub user: String,
    pub password: String,
    pub head_timeout: Duration,
    pub connect_timeout: Duration,
    /// 터널 하나의 최대 수명.
    pub tunnel_limit: Duration,
    pub max_connections: usize,
    /// 전용 주소 처리기. 없으면 전용 주소도 평문 프록시 요청으로 보고 거절한다.
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

/// 거절·터널 수. 시험과 운영 관측용이다.
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
    /// 루프백의 빈 포트에 연다.
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

    /// 새 연결을 받지 않고 열린 터널도 끊는다.
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

/// 요청 머리를 읽는다. 머리 뒤에 이미 도착한 바이트(TLS 첫 메시지 등)는 `leftover`로 보존한다.
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
                return None; // 같은 머리가 둘이면 어느 쪽이 옳은지 모호하다
            }
            authorization = Some(value.trim().to_owned());
        }
    }
    // 메서드는 호출한 쪽이 다시 가른다(405를 돌려주려면 머리가 정상이어야 한다).
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

/// 전용 주소(`POST http://bot-effects.invalid/effect`)의 요청 하나. 같은 호출 전용 자격을 요구하고 바깥으로 나가지 않는다.
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

/// 길이가 같을 때 모든 바이트를 본다(비교 시간이 맞은 접두어 길이를 흘리지 않는다).
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
