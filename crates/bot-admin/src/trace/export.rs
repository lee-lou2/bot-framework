//! OTLP 내보내기: 프로필마다 독립된 한정 큐·작업·차단기를 둔다. `record`는 동기·무대기·무실패다.
//! 수집기가 느리거나 죽어도 실행은 영향이 없다: 큐가 차면 버리고(`dropped`), 연속 실패하면 차단기가 열려 배치를 버린다.
//! 한 프로필의 스팬은 그 프로필의 목적지로만 간다(미등록·꺼진 프로필은 어디로도 가지 않는다).

use super::{
    Ids, Redactor, SpanRecord, Tracer, Val, http,
    otlp::{self, Out},
    policy::{self, Policy},
    sample_hit,
};
use crate::fence::Fence;
use bot_core::{
    id::{ProfileId, SecretName},
    port::SecretStore,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// 헤더 값: 평문(비밀이 아닌 것) 또는 프로필 비밀 이름(보낼 때마다 SecretStore에서 받는다).
#[derive(Clone, PartialEq)]
pub enum HeaderValue {
    Plain(String),
    Secret(SecretName),
}

/// 진단 출력에 헤더 값을 싣지 않는다(실수로 평문에 넣은 토큰이 로그에 남지 않게).
impl std::fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("Plain(..)"),
            Self::Secret(name) => write!(f, "Secret({})", name.as_str()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Backend {
    pub name: String,
    /// `http://호스트:포트/경로`. 평문만 지원한다(TLS는 수집기에서).
    pub endpoint: String,
    pub headers: Vec<(String, HeaderValue)>,
    /// 화면 링크 틀. `{trace_id}`를 추적 ID로 바꾼다(`http://`·`https://`).
    pub link: Option<String>,
    /// 사설망 밖일 수 있는 수집기로 **평문** http 전송을 해도 된다는 운영자의 명시(관리 화면의 `remote`와 같은 방식). 꺼져 있으면 루프백·사설 주소·`localhost`만 받는다
    /// (`validate`·`serve`가 시작 전에 거절한다) — 평문이라 헤더 비밀과 내용이 그대로 나간다.
    pub remote: bool,
}

/// 프로필별 추적 설정. `backend`가 없으면 그 프로필은 내보내지 않는다.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileTrace {
    pub backend: Option<Backend>,
    pub ids: Ids,
    /// 내용 속성(프롬프트·인자·결과)을 가림 처리해 내보낼지. 기본 꺼짐.
    pub content: bool,
    /// 0.0..=1.0. 추적 ID 값으로 정하므로 모든 프로세스가 같은 결론이다.
    pub sample: f64,
}

impl ProfileTrace {
    /// 시작하지 않고 설정만 검사한다(`Exporter::start`와 같은 규칙: http 평문 주소·헤더·링크 틀·표본 범위).
    pub fn check(&self, profile: &ProfileId) -> Result<(), String> {
        validate(profile, self).map(|_| ())
    }
}

impl Default for ProfileTrace {
    fn default() -> Self {
        Self {
            backend: None,
            ids: Ids::Hash,
            content: false,
            sample: 1.0,
        }
    }
}

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
            "enabled": self.enabled, "queued": self.queued, "sent": self.sent, "dropped": self.dropped,
            "failed": self.failed, "circuit": self.circuit, "last_error": self.last_error,
            "backends": self.backends.iter().map(|(name, kind, origin, state)| json!({"name": name, "kind": kind, "origin": origin, "state": state})).collect::<Vec<_>>(),
        })
    }
}

#[derive(Default)]
struct Counters {
    queued: AtomicU64,
    inflight: AtomicU64,
    sent: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
    open: AtomicBool,
    last_error: Mutex<Option<&'static str>>,
}

/// 내보내기 문: 호스트가 «이 프로세스의 활성화 판본이 지금도 현재인가»에 이은 확인(`Exporter::set_gate`). 수집 설정은 활성화 지문에 들어 있어
/// 수집을 거두거나 바꾸면 옛 프로세스는 구판이 되고, 그 프로세스의 문은 닫힌다: 큐에 있던 것까지 보내지 않고 버린다(`dropped`, 분류 `stale`).
/// 확인하지 못해도(저장소 장애·시간 초과) 닫힌 것으로 본다 — 거둔 것을 확인할 수 없는 동안 옛 허락으로 내보내지 않는다.
/// 문이 달리기 전(시작 중)과 문을 쓰지 않는 구성(시험)은 열려 있다.
///
/// 배치마다 두 번 본다. ① 배치를 집자마자(구판은 수집 자격조차 읽지 않는다) ② **전송 직전**(헤더 자격을 읽고 본문을 만든 뒤) — 자격을 읽는 동안(보관소가
/// 느릴 수 있다) 수집이 거두어져도 옛 목적지로 가지 않는다. ②를 통과해 `http::post`를 시작한 요청은 거둘 수 없다: 그 배치는 `dropped`가 아니라
/// `sent`(또는 네트워크 실패면 `failed`)로 세고, 그다음 배치부터 문이 막는다.
struct Door {
    check: OnceLock<(Fence, Duration)>,
}

impl Door {
    async fn open(&self) -> bool {
        let Some((check, timeout)) = self.check.get() else {
            return true;
        };
        tokio::time::timeout(*timeout, check())
            .await
            .unwrap_or(false)
    }
}

/// 한 배치를 보내지 못한 이유. `Stale`은 전송 직전 문이 닫혀 의도하고 버린 것(실패·차단기와 무관)이다.
enum Fail {
    Stale,
    Class(&'static str),
}

/// 작업이 함께 쥐는 것. 송신기는 여기에 두지 않는다: 작업이 송신기를 쥐면 `Exporter`를 놓아도 채널이 닫히지 않는다.
struct Shared {
    cfg: ProfileTrace,
    target: Option<http::Target>,
    c: Counters,
}

struct Lane {
    tx: Option<mpsc::Sender<SpanRecord>>,
    shared: Arc<Shared>,
}

/// `Exporter`를 모두 놓으면 송신기가 닫히고, 작업은 남은 큐를 비운 뒤 끝나며 SecretStore 참조를 놓는다.
pub struct Exporter {
    lanes: BTreeMap<ProfileId, Lane>,
    door: Arc<Door>,
}

struct Breaker {
    fails: u32,
    trips: u32,
    until: Option<Instant>,
    opts: Options,
}

impl Breaker {
    fn allow(&self, now: Instant) -> bool {
        self.until.is_none_or(|t| now >= t)
    }

    fn ok(&mut self) {
        (self.fails, self.trips, self.until) = (0, 0, None);
    }

    /// 열렸으면 참. 열린 뒤 첫 시도(반열림)가 실패하면 곧바로 더 길게 다시 연다.
    fn fail(&mut self, now: Instant) -> bool {
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

fn validate(p: &ProfileId, t: &ProfileTrace) -> Result<Option<http::Target>, String> {
    if !(0.0..=1.0).contains(&t.sample) {
        return Err(format!("{}: sample은 0.0~1.0이어야 한다", p.0));
    }
    let Some(b) = &t.backend else { return Ok(None) };
    let target = http::Target::parse(&b.endpoint).map_err(|e| format!("{}: {e}", p.0))?;
    if !b.remote && !target.stays_local() {
        return Err(format!(
            "{0}: 수집기 호스트 «{1}»이(가) 루프백·사설 주소가 아니다 — 평문 http라 헤더 비밀과 내용이 사설망 밖으로 나갈 수 있다. \
             사설 IP(또는 localhost)로 적거나, 정말 그렇게 보내려면 admin.trace.{0}.remote를 true로 명시한다",
            p.0, target.host
        ));
    }
    for (name, value) in &b.headers {
        let plain = match value {
            HeaderValue::Plain(v) => v.as_str(),
            HeaderValue::Secret(_) => "",
        };
        if !http::header_ok(name, plain) {
            return Err(format!("{}: 잘못된 헤더 이름 또는 값", p.0));
        }
    }
    let link_ok = |l: &str| {
        (l.starts_with("http://") || l.starts_with("https://"))
            && l.contains("{trace_id}")
            && !l.contains(char::is_whitespace)
    };
    if b.link.as_deref().is_some_and(|l| !link_ok(l)) {
        return Err(format!(
            "{}: 링크 틀은 http(s)://…{{trace_id}}… 모양이어야 한다",
            p.0
        ));
    }
    Ok(Some(target))
}

fn instance_id() -> String {
    let now = super::now_nano();
    let d = Sha256::digest(format!("{}:{now}", std::process::id()));
    crate::json::hex(&d[..6])
}

async fn headers(
    cfg: &ProfileTrace,
    secrets: &Option<Arc<dyn SecretStore>>,
    profile: &ProfileId,
) -> Result<Vec<(String, String)>, &'static str> {
    let mut out = Vec::new();
    let Some(b) = &cfg.backend else {
        return Ok(out);
    };
    let mut handle = None;
    for (name, value) in &b.headers {
        let text = match value {
            HeaderValue::Plain(v) => v.clone(),
            HeaderValue::Secret(secret) => {
                let store = secrets.as_ref().ok_or("secret")?;
                if handle.is_none() {
                    handle = Some(
                        store
                            .for_profile(profile.clone())
                            .await
                            .map_err(|_| "secret")?,
                    );
                }
                let h = handle.as_ref().ok_or("secret")?;
                let v = h.resolve(secret.clone()).await.map_err(|_| "secret")?;
                String::from_utf8(v.0).map_err(|_| "secret")?
            }
        };
        out.push((name.clone(), text));
    }
    Ok(out)
}

struct Worker {
    profile: ProfileId,
    door: Arc<Door>,
    shared: Arc<Shared>,
    secrets: Option<Arc<dyn SecretStore>>,
    red: Arc<Redactor>,
    opts: Options,
    instance: String,
}

impl Worker {
    /// 순서가 중요하다: ① 헤더 준비(보관소를 읽는 동안 새로 알게 된 값 — 교체된 키 — 이 가림 목록에 들어간다) → ② **그때의** 가림 목록으로 본문 생성 →
    /// ③ 전송 직전 문 확인(읽는 동안 수집이 거두어졌으면 보내지 않는다) → ④ 전송. 본문을 먼저 만들면 ①이 알게 된 값이 본문에서 가려지지 않고, 문을 ① 앞에서만
    /// 보면 ① 도중의 철회를 놓친다.
    async fn send(&self, batch: Vec<SpanRecord>) -> Result<(), Fail> {
        let (Some(target), policy) = (
            &self.shared.target,
            Policy {
                ids: self.shared.cfg.ids,
                content: self.shared.cfg.content,
                private: false,
            },
        ) else {
            return Err(Fail::Class("config"));
        };
        let hs = headers(&self.shared.cfg, &self.secrets, &self.profile)
            .await
            .map_err(Fail::Class)?;
        let spans: Vec<Out> = batch
            .into_iter()
            .map(|s| policy::apply(s, &policy, &self.red))
            .collect();
        let resource = [
            ("service.name", Val::Str("bot".into())),
            (
                "service.version",
                Val::Str(env!("CARGO_PKG_VERSION").into()),
            ),
            ("service.instance.id", Val::Str(self.instance.clone())),
            (super::key::BOT_PROFILE, Val::Str(self.profile.0.clone())),
        ];
        let body = otlp::request(&resource, ("bot", env!("CARGO_PKG_VERSION")), &spans);
        if !self.door.open().await {
            return Err(Fail::Stale);
        }
        http::post(target, &hs, &body, self.opts.timeout)
            .await
            .map_err(Fail::Class)
    }

    async fn run(self, mut rx: mpsc::Receiver<SpanRecord>) {
        let mut breaker = Breaker {
            fails: 0,
            trips: 0,
            until: None,
            opts: self.opts.clone(),
        };
        let c = &self.shared.c;
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            let deadline = tokio::time::Instant::now() + self.opts.interval;
            while batch.len() < self.opts.batch {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(s)) => batch.push(s),
                    _ => break,
                }
            }
            let n = batch.len() as u64;
            c.inflight.fetch_add(n, Relaxed);
            c.queued.fetch_sub(n, Relaxed);
            let now = Instant::now();
            if !self.door.open().await {
                // 구판이 되었거나 현재성을 확인하지 못했다: 자격도 읽지 않고 보내지 않는다(차단기·실패 횟수와 무관한 의도한 버림)
                c.dropped.fetch_add(n, Relaxed);
                *c.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some("stale");
            } else if !breaker.allow(now) {
                c.dropped.fetch_add(n, Relaxed);
            } else {
                match self.send(batch).await {
                    Ok(()) => {
                        c.sent.fetch_add(n, Relaxed);
                        breaker.ok();
                        c.open.store(false, Relaxed);
                    }
                    Err(Fail::Stale) => {
                        // 자격을 읽는 동안 구판이 되었다: 전송 직전 문이 막았다(시작한 전송이 아니다)
                        c.dropped.fetch_add(n, Relaxed);
                        *c.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some("stale");
                    }
                    Err(Fail::Class(class)) => {
                        c.failed.fetch_add(n, Relaxed);
                        *c.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(class);
                        if breaker.fail(Instant::now()) {
                            c.open.store(true, Relaxed);
                        }
                    }
                }
            }
            c.inflight.fetch_sub(n, Relaxed);
        }
    }
}

impl Exporter {
    /// 프로필마다 작업을 띄운다(tokio 런타임 안에서 불러야 한다). 설정이 잘못되면 시작하지 않는다.
    pub fn start(
        config: BTreeMap<ProfileId, ProfileTrace>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: Options,
    ) -> Result<Arc<Self>, String> {
        let mut targets = BTreeMap::new();
        for (p, t) in &config {
            targets.insert(p.clone(), validate(p, t)?);
        }
        let runtime = tokio::runtime::Handle::try_current();
        if runtime.is_err() && targets.values().any(Option::is_some) {
            return Err("추적 내보내기는 tokio 런타임 안에서 시작해야 한다".to_owned());
        }
        let (red, instance) = (Arc::new(redactor), instance_id());
        let door = Arc::new(Door {
            check: OnceLock::new(),
        });
        let mut lanes = BTreeMap::new();
        for (profile, cfg) in config {
            let target = targets.remove(&profile).flatten();
            let (tx, rx) = if target.is_some() {
                let (tx, rx) = mpsc::channel(opts.queue.max(1));
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            let shared = Arc::new(Shared {
                cfg,
                target,
                c: Counters::default(),
            });
            let lane = Lane {
                tx,
                shared: shared.clone(),
            };
            if let (Some(rx), Ok(handle)) = (rx, &runtime) {
                let w = Worker {
                    profile: profile.clone(),
                    door: door.clone(),
                    shared,
                    secrets: secrets.clone(),
                    red: red.clone(),
                    opts: opts.clone(),
                    instance: instance.clone(),
                };
                handle.spawn(w.run(rx));
            }
            lanes.insert(profile, lane);
        }
        Ok(Arc::new(Self { lanes, door }))
    }

    /// 내보내기 문을 단다(한 번만). 참을 돌려주는 동안만 보내고, 거짓이거나 `timeout` 안에 답이 없으면 그 배치를 버린다.
    /// 호스트는 «이 프로세스의 활성화 판본이 저장소의 활성 판본과 같은가»를 잇는다. 이미 달려 있으면 거짓이다(바꾸지 않는다).
    pub fn set_gate(&self, check: Fence, timeout: Duration) -> bool {
        self.door.check.set((check, timeout)).is_ok()
    }

    pub fn stats(&self, profile: &ProfileId) -> Option<Stats> {
        let lane = &self.lanes.get(profile)?.shared;
        let c = &lane.c;
        let open = c.open.load(Relaxed);
        let backends = match (&lane.cfg.backend, &lane.target) {
            (Some(b), Some(t)) => vec![(
                b.name.clone(),
                "otlp-http",
                t.origin(),
                if open { "open" } else { "closed" },
            )],
            _ => Vec::new(),
        };
        Some(Stats {
            enabled: lane.target.is_some(),
            queued: c.queued.load(Relaxed),
            sent: c.sent.load(Relaxed),
            dropped: c.dropped.load(Relaxed),
            failed: c.failed.load(Relaxed),
            circuit: if open { "open" } else { "closed" },
            last_error: *c.last_error.lock().unwrap_or_else(|e| e.into_inner()),
            backends,
        })
    }

    /// 화면 링크. 틀이 설정된 프로필만 만든다.
    pub fn link(&self, profile: &ProfileId, trace: &[u8; 16]) -> Option<String> {
        let template = self
            .lanes
            .get(profile)?
            .shared
            .cfg
            .backend
            .as_ref()?
            .link
            .as_ref()?;
        Some(template.replace("{trace_id}", &crate::json::hex(trace)))
    }

    /// 큐와 전송 중인 것이 모두 끝날 때까지 기다린다(종료·시험용). 제한 시간 안에 비면 참.
    pub async fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let busy = self
                .lanes
                .values()
                .any(|l| l.shared.c.queued.load(Relaxed) + l.shared.c.inflight.load(Relaxed) > 0);
            if !busy {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// 스팬 하나가 큐에서 차지하는 대략의 바이트. 호출자가 거대한 값을 넣어도 큐 메모리가 한없이 늘지 않게 한다.
const MAX_SPAN_BYTES: usize = 256 * 1024;

fn weight(s: &SpanRecord) -> usize {
    let val = |v: &Val| match v {
        Val::Str(t) => t.len(),
        _ => 8,
    };
    let attrs =
        |list: &[(super::Key, Val)]| list.iter().map(|(k, v)| k.len() + val(v)).sum::<usize>();
    s.name.len()
        + attrs(&s.attrs)
        + s.events
            .iter()
            .map(|e| e.name.len() + attrs(&e.attrs))
            .sum::<usize>()
        + s.links.len() * 24
}

impl Tracer for Exporter {
    fn record(&self, span: SpanRecord) {
        let Some(lane) = self.lanes.get(&span.profile) else {
            return;
        };
        let Some(tx) = &lane.tx else { return };
        let (cfg, c) = (&lane.shared.cfg, &lane.shared.c);
        if !sample_hit(&span.ids.trace, cfg.sample) {
            return;
        }
        if weight(&span) > MAX_SPAN_BYTES {
            c.dropped.fetch_add(1, Relaxed);
            return;
        }
        c.queued.fetch_add(1, Relaxed);
        if tx.try_send(span).is_err() {
            c.queued.fetch_sub(1, Relaxed);
            c.dropped.fetch_add(1, Relaxed);
        }
    }
}
