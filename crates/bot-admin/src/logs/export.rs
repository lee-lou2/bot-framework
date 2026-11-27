//! Background OTLP log exporter with bounded queue, batching, circuit breaker, activation gate, and redaction.
//! Export errors update counters only and never emit `tracing` events to avoid recursion.

use super::otlp::{self, Out};
use crate::{
    fence::Fence,
    obs::{
        HostHeaderValue,
        runtime::{
            Breaker, Counters, Door, Fail, Options, Stats, check_host_headers, instance_id,
            resolve_host_headers, validate_endpoint,
        },
        transport::{Client, Target},
    },
    trace::Redactor,
};
use bot_core::{port::SecretStore, trace::Val};
use std::{
    sync::{Arc, atomic::Ordering::Relaxed},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const OTLP_CONTENT_TYPE: &str = "application/x-protobuf";
const DEFAULT_LOGS_PATH: &str = "/v1/logs";
const MAX_BODY_CHARS: usize = 2048;
const MAX_FIELD_CHARS: usize = 256;
const MAX_ATTRS: usize = 32;
const MAX_ENTRY_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum LogLevel {
    Debug,
    Info,
    #[default]
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    pub const fn severity_text(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogConfig {
    pub name: String,
    pub endpoint: String,
    pub headers: Vec<(String, HostHeaderValue)>,
    pub level: LogLevel,
    pub remote: bool,
}

impl LogConfig {
    pub fn check(&self) -> Result<(), String> {
        validate(self).map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp_nano: u64,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
}

fn validate(cfg: &LogConfig) -> Result<Target, String> {
    let target = validate_endpoint(
        "admin.logs",
        &cfg.endpoint,
        DEFAULT_LOGS_PATH,
        cfg.remote,
        "admin.logs.remote",
    )?;
    check_host_headers("admin.logs", &cfg.headers)?;
    Ok(target)
}

fn cap(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_owned()
    } else {
        s.chars().take(limit).collect()
    }
}

fn sanitize(entry: LogEntry, red: &Redactor) -> Out {
    let body = cap(&red.mask(&entry.message), MAX_BODY_CHARS);
    let mut attrs = Vec::with_capacity(entry.fields.len().min(MAX_ATTRS) + 1);
    let mut dropped = 0u32;
    attrs.push((
        "code.namespace".to_owned(),
        cap(&red.mask(&entry.target), MAX_FIELD_CHARS),
    ));
    for (k, v) in entry.fields {
        if attrs.len() >= MAX_ATTRS {
            dropped += 1;
            continue;
        }
        let key_ok = !k.is_empty()
            && k.len() <= 64
            && k.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
        if !key_ok || red.leaks_meta(&k) {
            dropped += 1;
            continue;
        }
        attrs.push((k, cap(&red.mask(&v), MAX_FIELD_CHARS)));
    }
    Out {
        timestamp_nano: entry.timestamp_nano,
        level: entry.level,
        body,
        attrs,
        dropped,
        trace_id: entry.trace_id,
        span_id: entry.span_id,
    }
}

struct Shared {
    cfg: Option<LogConfig>,
    target: Option<Target>,
    c: Counters,
}

pub struct LogExporter {
    tx: Option<mpsc::Sender<LogEntry>>,
    min_level: LogLevel,
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
    instance: String,
}

impl Worker {
    async fn send(&self, batch: Vec<LogEntry>) -> Result<(), Fail> {
        let (Some(cfg), Some(target)) = (&self.shared.cfg, &self.shared.target) else {
            return Err(Fail::Class("config"));
        };
        let hs = resolve_host_headers(&cfg.headers, &self.secrets)
            .await
            .map_err(Fail::Class)?;
        let records: Vec<Out> = batch
            .into_iter()
            .map(|entry| sanitize(entry, &self.red))
            .collect();
        let resource = [
            ("service.name", Val::Str("bot".into())),
            (
                "service.version",
                Val::Str(env!("CARGO_PKG_VERSION").into()),
            ),
            ("service.instance.id", Val::Str(self.instance.clone())),
        ];
        let body = otlp::request(&resource, ("bot", env!("CARGO_PKG_VERSION")), &records);
        if !self.door.open().await {
            return Err(Fail::Stale);
        }
        self.client
            .post(target, OTLP_CONTENT_TYPE, &hs, &body, self.opts.timeout)
            .await
            .map_err(Fail::Class)
    }

    async fn run(self, mut rx: mpsc::Receiver<LogEntry>) {
        let mut breaker = Breaker::new(self.opts.clone());
        let c = &self.shared.c;
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            let deadline = tokio::time::Instant::now() + self.opts.interval;
            while batch.len() < self.opts.batch {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(item)) => batch.push(item),
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

impl LogExporter {
    pub fn start(
        config: Option<LogConfig>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: Options,
    ) -> Result<Arc<Self>, String> {
        Self::start_with_client(config, secrets, redactor, opts, None)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn start_with_root_pem(
        config: Option<LogConfig>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: Options,
        pem: &[u8],
    ) -> Result<Arc<Self>, String> {
        let client = Client::with_root_pem(pem)?;
        Self::start_with_client(config, secrets, redactor, opts, Some(client))
    }

    fn start_with_client(
        config: Option<LogConfig>,
        secrets: Option<Arc<dyn SecretStore>>,
        redactor: Redactor,
        opts: Options,
        custom_client: Option<Client>,
    ) -> Result<Arc<Self>, String> {
        let target = match &config {
            Some(cfg) => Some(validate(cfg)?),
            None => None,
        };
        let runtime = tokio::runtime::Handle::try_current();
        if runtime.is_err() && target.is_some() {
            return Err("log exporter must be started inside a tokio runtime".to_owned());
        }
        let client = if let Some(t) = &target {
            match custom_client {
                Some(client) => Some(client),
                None => Some(Client::for_targets(std::iter::once(t))?),
            }
        } else {
            None
        };
        let min_level = config.as_ref().map_or(LogLevel::Error, |c| c.level);
        let door = Arc::new(Door::new());
        let (tx, rx) = if target.is_some() {
            let (tx, rx) = mpsc::channel(opts.queue.max(1));
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
                opts,
                instance: instance_id(),
            };
            handle.spawn(worker.run(rx));
        }
        Ok(Arc::new(Self {
            tx,
            min_level,
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
            (Some(cfg), Some(target)) => Some((cfg.name.as_str(), "otlp-logs", target)),
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

    pub fn record(&self, entry: LogEntry) {
        let Some(tx) = &self.tx else { return };
        if entry.level < self.min_level {
            return;
        }
        let weight = entry.target.len()
            + entry.message.len()
            + entry
                .fields
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>();
        let c = &self.shared.c;
        if weight > MAX_ENTRY_BYTES {
            c.dropped.fetch_add(1, Relaxed);
            return;
        }
        c.queued.fetch_add(1, Relaxed);
        if tx.try_send(entry).is_err() {
            c.queued.fetch_sub(1, Relaxed);
            c.dropped.fetch_add(1, Relaxed);
        }
    }
}
