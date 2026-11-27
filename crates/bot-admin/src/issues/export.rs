//! Async issue tracker exporter with bounded queue, cooldown throttling, sampling, circuit breaker, and redaction.
//! `IssueReporter::report` is sync, non-blocking, and infallible; export errors only update counters to avoid recursion.

use super::sentry::{self, ENVELOPE_CONTENT_TYPE};
use crate::{
    fence::Fence,
    obs::{
        SecretRef,
        runtime::{
            Breaker, Counters, Door, Fail, Options, Stats, resolve_secret_ref, validate_endpoint,
        },
        transport::{Client, Scheme, Target},
    },
    trace::{Redactor, sample_hit},
};
use bot_core::{
    issue::{IssueEvent, IssueReporter},
    port::SecretStore,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::Ordering::Relaxed},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_COOLDOWN_KEYS: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueKind {
    Sentry,
}

impl IssueKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sentry => "sentry",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sentry" => Some(Self::Sentry),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IssueConfig {
    pub kind: IssueKind,
    pub name: String,
    pub endpoint: String,
    /// SecretStore reference for the Sentry public key; optional only on loopback `http://`.
    pub key: Option<SecretRef>,
    pub environment: Option<String>,
    pub release: Option<String>,
    pub sample: f64,
    pub remote: bool,
}

impl IssueConfig {
    pub fn check(&self) -> Result<(), String> {
        validate(self).map(|_| ())
    }
}

#[derive(Clone, Debug)]
pub struct IssueOptions {
    pub runtime: Options,
    /// Minimum interval between consecutive sends for the same `(kind, target)` (0 disables).
    pub cooldown: Duration,
}

impl Default for IssueOptions {
    fn default() -> Self {
        Self {
            runtime: Options {
                queue: 512,
                batch: 1,
                ..Options::default()
            },
            cooldown: Duration::from_secs(5),
        }
    }
}

fn short_tag_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
}

fn validate(cfg: &IssueConfig) -> Result<Target, String> {
    if !(0.0..=1.0).contains(&cfg.sample) {
        return Err("admin.issues.sample: must be between 0.0 and 1.0".to_owned());
    }
    let target = validate_endpoint(
        "admin.issues",
        &cfg.endpoint,
        "/api/0/envelope/",
        cfg.remote,
        "admin.issues.remote",
    )?;
    if cfg.key.is_none() && !(target.scheme == Scheme::Http && target.is_loopback()) {
        return Err(
            "admin.issues.key: auth key secret is required unless targeting loopback http://"
                .to_owned(),
        );
    }
    if cfg.environment.as_deref().is_some_and(|e| !short_tag_ok(e)) {
        return Err(
            "admin.issues.environment: must be 1..=128 chars without control characters".to_owned(),
        );
    }
    if cfg.release.as_deref().is_some_and(|r| !short_tag_ok(r)) {
        return Err(
            "admin.issues.release: must be 1..=128 chars without control characters".to_owned(),
        );
    }
    Ok(target)
}

struct Shared {
    cfg: Option<IssueConfig>,
    target: Option<Target>,
    c: Counters,
}

pub struct IssueExporter {
    tx: Option<mpsc::Sender<IssueEvent>>,
    sample: f64,
    cooldown: Duration,
    seen: Mutex<HashMap<(String, String), Instant>>,
    shared: Arc<Shared>,
    door: Arc<Door>,
}

struct Worker {
    door: Arc<Door>,
    shared: Arc<Shared>,
    secrets: Option<Arc<dyn SecretStore>>,
    red: Arc<Redactor>,
    client: Client,
    opts: Options,
}

impl Worker {
    async fn send(&self, event: IssueEvent) -> Result<(), Fail> {
        let (Some(cfg), Some(target)) = (&self.shared.cfg, &self.shared.target) else {
            return Err(Fail::Class("config"));
        };
        let mut headers = Vec::new();
        if let Some(secret) = &cfg.key {
            let key = resolve_secret_ref(&self.secrets, secret)
                .await
                .map_err(Fail::Class)?;
            headers.push(sentry::auth_header(&key).map_err(Fail::Class)?);
        }
        let body = match cfg.kind {
            IssueKind::Sentry => sentry::encode_envelope(cfg, &event, &self.red),
        };
        if !self.door.open().await {
            return Err(Fail::Stale);
        }
        self.client
            .post(
                target,
                ENVELOPE_CONTENT_TYPE,
                &headers,
                &body,
                self.opts.timeout,
            )
            .await
            .map_err(Fail::Class)
    }

    async fn run(self, mut rx: mpsc::Receiver<IssueEvent>) {
        let mut breaker = Breaker::new(self.opts.clone());
        let c = &self.shared.c;
        while let Some(event) = rx.recv().await {
            c.inflight.fetch_add(1, Relaxed);
            c.queued.fetch_sub(1, Relaxed);
            let now = Instant::now();
            if !self.door.open().await {
                c.dropped.fetch_add(1, Relaxed);
                c.set_error("stale");
            } else if !breaker.allow(now) {
                c.dropped.fetch_add(1, Relaxed);
            } else {
                match self.send(event).await {
                    Ok(()) => {
                        c.sent.fetch_add(1, Relaxed);
                        breaker.ok();
                        c.open.store(false, Relaxed);
                    }
                    Err(Fail::Stale) => {
                        c.dropped.fetch_add(1, Relaxed);
                        c.set_error("stale");
                    }
                    Err(Fail::Class(class)) => {
                        c.failed.fetch_add(1, Relaxed);
                        c.set_error(class);
                        if breaker.fail(Instant::now()) {
                            c.open.store(true, Relaxed);
                        }
                    }
                }
            }
            c.inflight.fetch_sub(1, Relaxed);
        }
    }
}

impl IssueExporter {
    pub fn start(
        config: Option<IssueConfig>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: IssueOptions,
    ) -> Result<Arc<Self>, String> {
        Self::start_with_client(config, secrets, redactor, opts, None)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn start_with_root_pem(
        config: Option<IssueConfig>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: IssueOptions,
        pem: &[u8],
    ) -> Result<Arc<Self>, String> {
        let client = Client::with_root_pem(pem)?;
        Self::start_with_client(config, secrets, redactor, opts, Some(client))
    }

    fn start_with_client(
        config: Option<IssueConfig>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: IssueOptions,
        custom_client: Option<Client>,
    ) -> Result<Arc<Self>, String> {
        let target = match &config {
            Some(cfg) => Some(validate(cfg)?),
            None => None,
        };
        let runtime = tokio::runtime::Handle::try_current();
        if runtime.is_err() && target.is_some() {
            return Err("issue exporter must be started inside a tokio runtime".to_owned());
        }
        let client = if let Some(t) = &target {
            match custom_client {
                Some(client) => Some(client),
                None => Some(Client::for_targets(std::iter::once(t))?),
            }
        } else {
            None
        };
        let sample = config.as_ref().map_or(0.0, |c| c.sample);
        let door = Arc::new(Door::new());
        let (tx, rx) = if target.is_some() {
            let (tx, rx) = mpsc::channel(opts.runtime.queue.max(1));
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let shared = Arc::new(Shared {
            cfg: config,
            target,
            c: Counters::default(),
        });
        if let (Some(rx), Ok(handle), Some(client)) = (rx, &runtime, client) {
            let worker = Worker {
                door: door.clone(),
                shared: shared.clone(),
                secrets,
                red: Arc::new(redactor),
                client,
                opts: opts.runtime,
            };
            handle.spawn(worker.run(rx));
        }
        Ok(Arc::new(Self {
            tx,
            sample,
            cooldown: opts.cooldown,
            seen: Mutex::new(HashMap::new()),
            shared,
            door,
        }))
    }

    pub fn enabled(&self) -> bool {
        self.tx.is_some()
    }

    pub fn set_gate(&self, check: Fence, timeout: Duration) -> bool {
        self.door.set(check, timeout)
    }

    pub fn stats(&self) -> Stats {
        let backend = match (&self.shared.cfg, &self.shared.target) {
            (Some(cfg), Some(target)) => Some((cfg.name.as_str(), cfg.kind.as_str(), target)),
            _ => None,
        };
        self.shared.c.snapshot(backend)
    }

    pub async fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let busy = self.shared.c.queued.load(Relaxed) + self.shared.c.inflight.load(Relaxed);
            if busy == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn throttled(&self, kind: &str, target: &str) -> bool {
        if self.cooldown.is_zero() {
            return false;
        }
        let now = Instant::now();
        let mut map = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let key = (kind.to_owned(), target.to_owned());
        if let Some(prev) = map.get(&key)
            && now.saturating_duration_since(*prev) < self.cooldown
        {
            return true;
        }
        if !map.contains_key(&key) && map.len() >= MAX_COOLDOWN_KEYS {
            map.retain(|_, at| now.saturating_duration_since(*at) < self.cooldown);
            if map.len() >= MAX_COOLDOWN_KEYS
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(key, now);
        false
    }
}

/// SHA-256 whitens RFC 4122 v4 variant bits (`id[8]`) so deterministic sampling stays uniform.
fn issue_sample_hit(event_id: &[u8; 16], rate: f64) -> bool {
    let digest = Sha256::digest(event_id);
    let mut uniform = [0u8; 16];
    uniform.copy_from_slice(&digest[..16]);
    sample_hit(&uniform, rate)
}

impl IssueReporter for IssueExporter {
    fn report(&self, event: IssueEvent) {
        let Some(tx) = &self.tx else { return };
        if !issue_sample_hit(&event.event_id, self.sample) {
            return;
        }
        let c = &self.shared.c;
        let weight = event.kind.len()
            + event.message.len()
            + event.target.len()
            + event
                .tags
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>();
        if weight > MAX_EVENT_BYTES || self.throttled(&event.kind, &event.target) {
            c.dropped.fetch_add(1, Relaxed);
            return;
        }
        c.queued.fetch_add(1, Relaxed);
        if tx.try_send(event).is_err() {
            c.queued.fetch_sub(1, Relaxed);
            c.dropped.fetch_add(1, Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::issue::new_event_id;

    #[test]
    fn issue_sampling_is_uniform_across_rfc4122_v4_event_ids() {
        let mut hits_quarter = 0usize;
        for _ in 0..4000 {
            let id = new_event_id();
            assert!(!issue_sample_hit(&id, 0.0));
            assert!(issue_sample_hit(&id, 1.0));
            if issue_sample_hit(&id, 0.25) {
                hits_quarter += 1;
            }
        }
        assert!(
            (800..=1200).contains(&hits_quarter),
            "sample=0.25 hit rate skewed: {hits_quarter}/4000"
        );
    }

    #[test]
    fn cooldown_map_is_strictly_bounded_by_max_cooldown_keys() {
        let exporter = IssueExporter {
            tx: None,
            sample: 1.0,
            cooldown: Duration::from_secs(60),
            seen: Mutex::new(HashMap::new()),
            shared: Arc::new(Shared {
                cfg: None,
                target: None,
                c: Counters::default(),
            }),
            door: Arc::new(Door::new()),
        };
        for i in 0..(MAX_COOLDOWN_KEYS + 200) {
            assert!(!exporter.throttled("panic", &format!("file.rs:{i}")));
        }
        let len = exporter.seen.lock().unwrap().len();
        assert!(
            len <= MAX_COOLDOWN_KEYS,
            "cooldown map exceeded limit: {len}"
        );
    }
}
