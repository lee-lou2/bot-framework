//! 운영자 인증: Host 확인(421) → 실패 한도(429) → 비밀 비교(401). 화면·조회·판정이 같은 길을 지난다.
//! 비밀은 해시로만 쥐고 모든 항목을 상수 시간으로 견주며 값은 어디에도 싣지 않는다.
//! 바꾸는 요청(POST)은 추가로 CSRF 방어를 거친다: 교차 출처 거절, 서버가 발급한 토큰 헤더, JSON 본문.

use crate::{port::Viewer, resp::reply};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::Response,
};
use bot_core::id::PrincipalId;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// 비밀 후보 → 신원. 구현은 모든 후보를 같은 시간에 처리해야 한다(조기 반환 금지).
pub trait Authenticator: Send + Sync {
    fn identify(&self, secret: &[u8]) -> Option<Viewer>;

    /// 받아 줄 신원이 하나도 없으면 거짓이다. 거짓이면 관리 라우트를 만들지 않는다(닫힌 채 시작).
    fn enabled(&self) -> bool {
        true
    }
}

pub fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// 해시한 뒤 XOR 누적으로 견준다(길이와 내용에 걸리는 시간이 같다).
fn same(candidate: &[u8; 32], stored: &[u8; 32]) -> bool {
    candidate
        .iter()
        .zip(stored)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// 시작할 때 정한 고정 비밀 표. 빈 비밀은 받지 않는다.
#[derive(Default)]
pub struct StaticTokens {
    entries: Vec<([u8; 32], Viewer)>,
}

impl StaticTokens {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, secret: &str, viewer: Viewer) -> Self {
        if !secret.is_empty() {
            self.entries.push((digest(secret.as_bytes()), viewer));
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Authenticator for StaticTokens {
    fn enabled(&self) -> bool {
        !self.is_empty()
    }

    fn identify(&self, secret: &[u8]) -> Option<Viewer> {
        let candidate = digest(secret);
        let mut found = None;
        for (stored, viewer) in &self.entries {
            if same(&candidate, stored) && found.is_none() {
                found = Some(viewer.clone());
            }
        }
        found
    }
}

struct Limiter {
    fails: VecDeque<Instant>,
    blocked_until: Option<Instant>,
    max: usize,
    window: Duration,
    block: Duration,
}

impl Limiter {
    fn new(max: usize, window: Duration, block: Duration) -> Self {
        Self {
            fails: VecDeque::new(),
            blocked_until: None,
            max,
            window,
            block,
        }
    }

    /// 차단 중이면 남은 시간.
    fn check(&mut self, now: Instant) -> Result<(), Duration> {
        match self.blocked_until {
            Some(t) if now < t => Err(t - now),
            Some(_) => {
                self.blocked_until = None;
                self.fails.clear();
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// 아직 기억해 둘 이유가 있는가(차단 중이거나 집계 창 안에 실패가 남음).
    fn active(&self, now: Instant) -> bool {
        self.blocked_until.is_some_and(|t| now < t)
            || self
                .fails
                .back()
                .is_some_and(|t| now.duration_since(*t) <= self.window)
    }

    fn fail(&mut self, now: Instant) {
        self.fails.push_back(now);
        while self
            .fails
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            self.fails.pop_front();
        }
        if self.fails.len() >= self.max {
            self.blocked_until = Some(now + self.block);
        }
    }
}

/// 접속 출처(피어 IP)마다 따로 세는 실패 예산: 한 출처의 실패가 다른 출처의 정상 운영자를 막지 않는다.
/// 피어 주소를 알 수 없는 경로(`ConnectInfo` 없음)는 한 칸을 같이 쓴다. 기억하는 출처 수는 한정한다.
struct Limiters {
    by: HashMap<Option<IpAddr>, Limiter>,
    max: usize,
    window: Duration,
    block: Duration,
}

const MAX_SOURCES: usize = 4096;

impl Limiters {
    fn new(max: usize, window: Duration, block: Duration) -> Self {
        Self {
            by: HashMap::new(),
            max,
            window,
            block,
        }
    }

    fn check(&mut self, src: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        self.by.get_mut(&src).map_or(Ok(()), |l| l.check(now))
    }

    fn fail(&mut self, src: Option<IpAddr>, now: Instant) {
        if !self.by.contains_key(&src) && self.by.len() >= MAX_SOURCES {
            self.by.retain(|_, l| l.active(now));
            if self.by.len() >= MAX_SOURCES
                && let Some(k) = self.by.keys().next().copied()
            {
                self.by.remove(&k);
            }
        }
        let (max, window, block) = (self.max, self.window, self.block);
        self.by
            .entry(src)
            .or_insert_with(|| Limiter::new(max, window, block))
            .fail(now);
    }
}

/// 허용하는 `Host` 값(소문자, 포트 포함): 바인드 주소와 루프백 이름, 운영자가 더한 값.
pub fn hosts_for(bind: SocketAddr, extra: &[String]) -> Vec<String> {
    let port = bind.port();
    let mut hosts = vec![
        bind.to_string(),
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    hosts.extend(extra.iter().map(|h| h.trim().to_ascii_lowercase()));
    hosts
}

pub struct Guard {
    auth: Arc<dyn Authenticator>,
    hosts: HashSet<String>,
    limiters: Mutex<Limiters>,
    csrf_key: [u8; 32],
}

fn random_key() -> [u8; 32] {
    use std::{hash::BuildHasher, io::Read};
    let mut key = [0u8; 32];
    if std::fs::File::open("/dev/urandom").is_ok_and(|mut f| f.read_exact(&mut key).is_ok())
        && key != [0; 32]
    {
        return key;
    }
    // 운영체제가 무작위 씨앗을 준 `RandomState`를 섞어 쓴다.
    for (i, chunk) in key.chunks_mut(8).enumerate() {
        let state = std::collections::hash_map::RandomState::new();
        let word = state.hash_one((i, std::process::id(), Instant::now()));
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    key
}

/// 새 운영자 비밀: 무작위 32바이트의 16진 64자(운영체제 난수). 시작할 때 만들어 메모리에만 두고 안내한다.
pub fn new_token() -> String {
    crate::json::hex(&random_key())
}

impl Guard {
    pub fn new(
        auth: Arc<dyn Authenticator>,
        hosts: &[String],
        max_failures: usize,
        window: Duration,
        block: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            auth,
            hosts: hosts.iter().map(|h| h.to_ascii_lowercase()).collect(),
            limiters: Mutex::new(Limiters::new(max_failures.max(1), window, block)),
            csrf_key: random_key(),
        })
    }

    /// 신원에 묶인 CSRF 토큰. 인증된 GET으로만 알 수 있어 다른 출처의 페이지는 읽을 수 없다.
    pub fn csrf(&self, viewer: &Viewer) -> String {
        let mut h = Sha256::new();
        h.update(self.csrf_key);
        h.update(viewer.principal.0.as_bytes());
        h.update([0, u8::from(viewer.operator)]);
        crate::json::hex(&h.finalize()[..16])
    }

    fn host_ok(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| self.hosts.contains(&h.to_ascii_lowercase()))
    }

    fn csrf_ok(&self, headers: &HeaderMap, viewer: &Viewer) -> bool {
        let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        if text("sec-fetch-site").is_some_and(|s| !matches!(s, "same-origin" | "none")) {
            return false;
        }
        if let Some(origin) = text("origin") {
            let host = origin
                .strip_prefix("http://")
                .or_else(|| origin.strip_prefix("https://"));
            if !host.is_some_and(|h| self.hosts.contains(&h.to_ascii_lowercase())) {
                return false;
            }
        }
        let json = text("content-type").is_some_and(|c| {
            c.split(';')
                .next()
                .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
        });
        let token = text("x-bot-csrf").map(|t| digest(t.as_bytes()));
        let want = digest(self.csrf(viewer).as_bytes());
        json && token.is_some_and(|t| same(&t, &want))
    }
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let bytes = s.trim().as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i == bytes.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for c in &chunk[..4 - pad] {
            n = n << 6 | u32::from(digit(*c)?);
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

/// `Authorization`에서 비밀 후보를 꺼낸다: `Bearer <값>`(CLI·시험) 또는 `Basic base64(<이름>:<값>)`(브라우저, 이름은 무시).
fn credential(h: &HeaderMap) -> Option<Vec<u8>> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(t) = v.strip_prefix("Bearer ") {
        return Some(t.trim().as_bytes().to_vec());
    }
    let decoded = b64_decode(v.strip_prefix("Basic ")?)?;
    let pos = decoded.iter().position(|b| *b == b':')?;
    Some(decoded[pos + 1..].to_vec())
}

fn unauthorized() -> Response {
    let mut r = reply(StatusCode::UNAUTHORIZED, "unauthorized");
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"bot\", charset=\"UTF-8\""),
    );
    r
}

/// Host 확인(421) → 실패 한도(429) → 비밀 비교(401) → 바꾸는 요청의 CSRF(403). 통과하면 `Viewer`를 요청에 싣는다.
pub async fn guard(State(g): State<Arc<Guard>>, mut req: Request, next: Next) -> Response {
    if !g.host_ok(req.headers()) {
        return reply(StatusCode::MISDIRECTED_REQUEST, "host");
    }
    let now = Instant::now();
    let source = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_canonical());
    let blocked = g
        .limiters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .check(source, now);
    if let Err(wait) = blocked {
        let mut r = reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
        if let Ok(v) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return r;
    }
    // 자격 헤더가 아예 없는 요청은 추측이 아니라 브라우저의 첫 요청(로그인 창을 띄우는 401)이므로 실패로 세지 않는다.
    // 헤더가 있는데 맞지 않거나 읽을 수 없으면 추측으로 보고 센다.
    let offered = req.headers().contains_key(header::AUTHORIZATION);
    let viewer = credential(req.headers()).and_then(|c| g.auth.identify(&c));
    let Some(viewer) = viewer else {
        if offered {
            g.limiters
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fail(source, now);
        }
        return unauthorized();
    };
    if !matches!(*req.method(), Method::GET | Method::HEAD) && !g.csrf_ok(req.headers(), &viewer) {
        return reply(StatusCode::FORBIDDEN, "csrf");
    }
    req.extensions_mut().insert(viewer);
    next.run(req).await
}

pub fn viewer(principal: &str, operator: bool) -> Viewer {
    Viewer {
        principal: PrincipalId(principal.to_owned()),
        operator,
    }
}

#[cfg(test)]
mod tests;
