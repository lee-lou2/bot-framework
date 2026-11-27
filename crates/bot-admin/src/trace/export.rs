//! Per-profile OTLP trace exporter with bounded queues, circuit breakers, activation gate, and non-blocking `Tracer::record`.

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

#[derive(Clone, PartialEq)]
pub enum HeaderValue {
    Plain(String),
    Secret(SecretName),
}

/// Redacts plain header values in debug output so accidentally placed tokens never leak into logs.
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
    pub endpoint: String,
    pub headers: Vec<(String, HeaderValue)>,
    pub link: Option<String>,
    /// Allows plaintext `http://` to non-private hosts (`https://` does not require this).
    pub remote: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProfileTrace {
    pub backend: Option<Backend>,
    pub ids: Ids,
    pub content: bool,
    pub sample: f64,
}

impl ProfileTrace {
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

/// Dropping all `Exporter` handles closes lane channels so background workers drain and release `SecretStore`.
pub struct Exporter {
    lanes: BTreeMap<ProfileId, Lane>,
    door: Arc<Door>,
}

fn validate(p: &ProfileId, t: &ProfileTrace) -> Result<Option<Target>, String> {
    if !(0.0..=1.0).contains(&t.sample) {
        return Err(format!("{}: sample must be between 0.0 and 1.0", p.0));
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
            return Err(format!("{}: invalid header name or value", p.0));
        }
    }
    let link_ok = |l: &str| {
        (l.starts_with("http://") || l.starts_with("https://"))
            && l.contains("{trace_id}")
            && !l.contains(char::is_whitespace)
    };
    if b.link.as_deref().is_some_and(|l| !link_ok(l)) {
        return Err(format!(
            "{}: link template must match http(s)://...{{trace_id}}...",
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
    /// Order matters: resolve headers first (registering rotated secrets in `Redactor`), then encode body, check gate, and POST.
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
    pub fn start(
        config: BTreeMap<ProfileId, ProfileTrace>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: Options,
    ) -> Result<Arc<Self>, String> {
        Self::start_with_client(config, secrets, redactor, opts, None)
    }

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
            return Err("trace exporter must be started inside a tokio runtime".to_owned());
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
