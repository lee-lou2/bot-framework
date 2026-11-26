//! OTLP 추적 내보내기: 프로필마다 독립된 한정 큐·작업·차단기를 둔다. `record`는 동기·무대기·무실패다.
//! `http://`(루프백·사설망, 외부는 `remote: true`)와 `https://`(Langfuse·LangSmith·원격 OTLP 수집기)를 모두 지원한다.
//! 수집기가 느리거나 죽어도 실행은 영향이 없다: 큐가 차면 버리고(`dropped`), 연속 실패하면 차단기가 열려 배치를 버린다.

use super::{
    Ids, Redactor, SpanRecord, Tracer, Val,
    otlp::{self, Out},
    policy::{self, Policy},
    sample_hit,
};
use crate::{
    fence::Fence,
    obs::{
        runtime::{Breaker, Counters, Door, Fail, instance_id, validate_endpoint},
        transport::{Client, Target, header_ok},
    },
};
use bot_core::{
    id::{ProfileId, SecretName},
    port::SecretStore,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering::Relaxed},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub use crate::obs::runtime::{Options, Stats};

const OTLP_CONTENT_TYPE: &str = "application/x-protobuf";
const DEFAULT_TRACE_PATH: &str = "/v1/traces";

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
    /// `http(s)://호스트[:포트][/경로]`. Langfuse·LangSmith 등 표준 OTLP/HTTP 끝점을 그대로 받는다.
    pub endpoint: String,
    pub headers: Vec<(String, HeaderValue)>,
    /// 화면 링크 틀. `{trace_id}`를 추적 ID로 바꾼다(`http://`·`https://`).
    pub link: Option<String>,
    /// 사설망 밖일 수 있는 수집기로 **평문** `http://` 전송을 해도 된다는 운영자의 명시(`https://`는 필요 없다).
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
    /// 시작하지 않고 설정만 검사한다(`Exporter::start`와 같은 규칙: 주소·헤더·링크 틀·표본 범위).
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

struct Shared {
    cfg: ProfileTrace,
    target: Option<Target>,
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

fn validate(p: &ProfileId, t: &ProfileTrace) -> Result<Option<Target>, String> {
    if !(0.0..=1.0).contains(&t.sample) {
        return Err(format!("{}: sample은 0.0~1.0이어야 한다", p.0));
    }
    let Some(b) = &t.backend else { return Ok(None) };
    let hint = format!("admin.trace.{}.remote", p.0);
    let target = validate_endpoint(&p.0, &b.endpoint, DEFAULT_TRACE_PATH, b.remote, &hint)?;
    for (name, value) in &b.headers {
        let plain = match value {
            HeaderValue::Plain(v) => v.as_str(),
            HeaderValue::Secret(_) => "",
        };
        if !header_ok(name, plain) {
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
            HeaderValue::Plain(v) => {
                if !header_ok(name, v) {
                    return Err("config");
                }
                v.clone()
            }
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
                let raw = String::from_utf8(v.0).map_err(|_| "secret")?;
                let trimmed = raw.trim().to_owned();
                if trimmed.is_empty() || !header_ok(name, &trimmed) {
                    return Err("secret");
                }
                trimmed
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
    client: Client,
    opts: Options,
    instance: String,
}

impl Worker {
    /// 순서가 중요하다: ① 헤더 준비(교체된 키가 가림 목록에 들어간다) → ② 그때의 가림 목록으로 본문 생성 →
    /// ③ 전송 직전 문 확인 → ④ 전송.
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
        self.client
            .post(target, OTLP_CONTENT_TYPE, &hs, &body, self.opts.timeout)
            .await
            .map_err(Fail::Class)
    }

    async fn run(self, mut rx: mpsc::Receiver<SpanRecord>) {
        let mut breaker = Breaker::new(self.opts.clone());
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
                c.dropped.fetch_add(n, Relaxed);
                c.set_error("stale");
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
                        c.dropped.fetch_add(n, Relaxed);
                        c.set_error("stale");
                    }
                    Err(Fail::Class(class)) => {
                        c.failed.fetch_add(n, Relaxed);
                        c.set_error(class);
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
        Self::start_with_client(config, secrets, redactor, opts, None)
    }

    /// 시험용: 로컬 HTTPS 자체 서명 인증서를 신뢰하는 클라이언트로 시작한다.
    #[cfg(any(test, feature = "testing"))]
    pub fn start_with_root_pem(
        config: BTreeMap<ProfileId, ProfileTrace>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: Options,
        pem: &[u8],
    ) -> Result<Arc<Self>, String> {
        let client = Client::with_root_pem(pem)?;
        Self::start_with_client(config, secrets, redactor, opts, Some(client))
    }

    fn start_with_client(
        config: BTreeMap<ProfileId, ProfileTrace>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: Options,
        custom_client: Option<Client>,
    ) -> Result<Arc<Self>, String> {
        let mut targets = BTreeMap::new();
        for (p, t) in &config {
            targets.insert(p.clone(), validate(p, t)?);
        }
        let has_active = targets.values().any(Option::is_some);
        let runtime = tokio::runtime::Handle::try_current();
        if runtime.is_err() && has_active {
            return Err("추적 내보내기는 tokio 런타임 안에서 시작해야 한다".to_owned());
        }
        let client = if has_active {
            match custom_client {
                Some(client) => Some(client),
                None => Some(Client::for_targets(targets.values().flatten())?),
            }
        } else {
            None
        };
        let (red, instance) = (Arc::new(redactor), instance_id());
        let door = Arc::new(Door::new());
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
            if let (Some(rx), Ok(handle), Some(client)) = (rx, &runtime, client.clone()) {
                let w = Worker {
                    profile: profile.clone(),
                    door: door.clone(),
                    shared,
                    secrets: secrets.clone(),
                    red: red.clone(),
                    client,
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
    pub fn set_gate(&self, check: Fence, timeout: Duration) -> bool {
        self.door.set(check, timeout)
    }

    pub fn stats(&self, profile: &ProfileId) -> Option<Stats> {
        let lane = &self.lanes.get(profile)?.shared;
        let backend = match (&lane.cfg.backend, &lane.target) {
            (Some(b), Some(t)) => Some((b.name.as_str(), "otlp-http", t)),
            _ => None,
        };
        Some(lane.c.snapshot(backend))
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
