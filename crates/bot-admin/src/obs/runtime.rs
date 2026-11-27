//! 관측 내보내기 공통 런타임: 설정 옵션(`Options`), 상태 통계(`Stats`), 원자 카운터(`Counters`),
//! 활성화 판본 문(`Door`), 지수 백오프 회로 차단기(`Breaker`), 비밀 헤더 조회, 끝점 검증.

use super::{
    HostHeaderValue, SecretRef,
    transport::{Scheme, Target, header_ok},
};
use crate::fence::Fence;
use bot_core::port::SecretStore;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Options {
    pub queue: usize,
    pub batch: usize,
    pub interval: Duration,
    pub timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub trip_after: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            queue: 2048,
            batch: 256,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(5),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            trip_after: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub enabled: bool,
    pub queued: u64,
    pub sent: u64,
    pub dropped: u64,
    pub failed: u64,
    pub circuit: &'static str,
    pub last_error: Option<&'static str>,
    /// (이름, 종류, 출처 `호스트:포트`, 상태). 주소의 경로·헤더는 싣지 않는다.
    pub backends: Vec<(String, &'static str, String, &'static str)>,
}

impl Stats {
    pub fn json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "queued": self.queued,
            "sent": self.sent,
            "dropped": self.dropped,
            "failed": self.failed,
            "circuit": self.circuit,
            "last_error": self.last_error,
            "backends": self.backends.iter().map(|(name, kind, origin, state)| {
                json!({"name": name, "kind": kind, "origin": origin, "state": state})
            }).collect::<Vec<_>>(),
        })
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            queued: 0,
            sent: 0,
            dropped: 0,
            failed: 0,
            circuit: "closed",
            last_error: None,
            backends: Vec::new(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Counters {
    pub queued: AtomicU64,
    pub inflight: AtomicU64,
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
    pub failed: AtomicU64,
    pub open: AtomicBool,
    pub last_error: Mutex<Option<&'static str>>,
}

impl Counters {
    pub(crate) fn set_error(&self, class: &'static str) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(class);
    }

    pub(crate) fn snapshot(&self, backend: Option<(&str, &'static str, &Target)>) -> Stats {
        let open = self.open.load(Relaxed);
        let state = if open { "open" } else { "closed" };
        let backends = match backend {
            Some((name, kind, target)) => vec![(name.to_owned(), kind, target.origin(), state)],
            None => Vec::new(),
        };
        Stats {
            enabled: backend.is_some(),
            queued: self.queued.load(Relaxed),
            sent: self.sent.load(Relaxed),
            dropped: self.dropped.load(Relaxed),
            failed: self.failed.load(Relaxed),
            circuit: state,
            last_error: *self.last_error.lock().unwrap_or_else(|e| e.into_inner()),
            backends,
        }
    }
}

/// 내보내기 문: 호스트가 «이 프로세스의 활성화 판본이 지금도 현재인가»에 이은 확인(`set_gate`).
/// 확인하지 못해도(저장소 장애·시간 초과) 닫힌 것으로 본다. 문이 달리기 전(시작 중)과 문을 쓰지 않는 구성(시험)은 열려 있다.
pub(crate) struct Door {
    check: OnceLock<(Fence, Duration)>,
}

impl Door {
    pub(crate) fn new() -> Self {
        Self {
            check: OnceLock::new(),
        }
    }

    pub(crate) fn set(&self, check: Fence, timeout: Duration) -> bool {
        self.check.set((check, timeout)).is_ok()
    }

    pub(crate) async fn open(&self) -> bool {
        let Some((check, timeout)) = self.check.get() else {
            return true;
        };
        tokio::time::timeout(*timeout, check())
            .await
            .unwrap_or(false)
    }
}

/// 한 배치를 보내지 못한 이유. `Stale`은 전송 직전 문이 닫혀 의도하고 버린 것(실패·차단기와 무관)이다.
pub(crate) enum Fail {
    Stale,
    Class(&'static str),
}

pub(crate) struct Breaker {
    fails: u32,
    trips: u32,
    until: Option<Instant>,
    opts: Options,
}

impl Breaker {
    pub(crate) fn new(opts: Options) -> Self {
        Self {
            fails: 0,
            trips: 0,
            until: None,
            opts,
        }
    }

    pub(crate) fn allow(&self, now: Instant) -> bool {
        self.until.is_none_or(|t| now >= t)
    }

    pub(crate) fn ok(&mut self) {
        (self.fails, self.trips, self.until) = (0, 0, None);
    }

    /// 열렸으면 참. 열린 뒤 첫 시도(반열림)가 실패하면 곧바로 더 길게 다시 연다.
    pub(crate) fn fail(&mut self, now: Instant) -> bool {
        self.fails += 1;
        if self.fails < self.opts.trip_after {
            return false;
        }
        self.trips += 1;
        let factor = 1u32 << (self.trips - 1).min(16);
        let wait = self
            .opts
            .backoff_min
            .saturating_mul(factor)
            .min(self.opts.backoff_max);
        self.until = Some(now + wait);
        true
    }
}

pub(crate) fn instance_id() -> String {
    bot_core::random::random_hex(6)
}

/// 끝점 검사: `https://`는 명시 없이 허용하고, 평문 `http://`는 `remote: true`가 없으면 루프백·사설 주소·`localhost`만 받는다.
pub(crate) fn validate_endpoint(
    owner: &str,
    endpoint: &str,
    default_path: &str,
    remote: bool,
    remote_hint: &str,
) -> Result<Target, String> {
    let target =
        Target::parse_with_default(endpoint, default_path).map_err(|e| format!("{owner}: {e}"))?;
    if target.scheme == Scheme::Http && !remote && !target.stays_local() {
        return Err(format!(
            "{owner}: 수집기 호스트 «{host}»이(가) 루프백·사설 주소가 아니다 — 평문 http라 헤더 비밀과 내용이 사설망 밖으로 나갈 수 있다. \
             https://나 사설 IP(또는 localhost)로 적거나, 정말 평문으로 보내려면 {remote_hint}를 true로 명시한다",
            host = target.host
        ));
    }
    Ok(target)
}

pub(crate) fn check_host_headers(
    owner: &str,
    headers: &[(String, HostHeaderValue)],
) -> Result<(), String> {
    for (name, value) in headers {
        let plain = match value {
            HostHeaderValue::Plain(v) => v.as_str(),
            HostHeaderValue::Secret(_) => "",
        };
        if !header_ok(name, plain) {
            return Err(format!("{owner}: 잘못된 헤더 이름 또는 값"));
        }
    }
    Ok(())
}

pub(crate) async fn resolve_secret_ref(
    secrets: &Option<Arc<dyn SecretStore>>,
    secret: &SecretRef,
) -> Result<String, &'static str> {
    let store = secrets.as_ref().ok_or("secret")?;
    let handle = store
        .for_profile(secret.profile.clone())
        .await
        .map_err(|_| "secret")?;
    let value = handle
        .resolve(secret.name.clone())
        .await
        .map_err(|_| "secret")?;
    let text = String::from_utf8(value.0).map_err(|_| "secret")?;
    let trimmed = text.trim().to_owned();
    if trimmed.is_empty() {
        return Err("secret");
    }
    Ok(trimmed)
}

pub(crate) async fn resolve_host_headers(
    headers: &[(String, HostHeaderValue)],
    secrets: &Option<Arc<dyn SecretStore>>,
) -> Result<Vec<(String, String)>, &'static str> {
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let text = match value {
            HostHeaderValue::Plain(v) => {
                if !header_ok(name, v) {
                    return Err("config");
                }
                v.clone()
            }
            HostHeaderValue::Secret(secret) => {
                let resolved = resolve_secret_ref(secrets, secret).await?;
                if !header_ok(name, &resolved) {
                    return Err("secret");
                }
                resolved
            }
        };
        out.push((name.clone(), text));
    }
    Ok(out)
}
