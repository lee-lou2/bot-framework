//! Admin API, embedded UI, and OTLP/Sentry observability exporters (`bot serve --role admin`).
//!
//! Store contracts live in `bot_core::admin`; approvals go through `Broker::decide` and hold/run actions through `AdminAct`.

pub mod api;
pub mod assets;
pub mod auth;
pub mod fence;
pub mod issues;
pub mod json;
pub mod logs;
pub mod map;
pub mod obs;
pub mod preapproval;
pub mod resp;
mod run_control;
pub mod settings;
pub mod trace;

#[cfg(any(test, feature = "testing"))]
pub mod fake;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware::from_fn_with_state,
    routing::{get, post},
};
use bot_core::{
    admin::{AdminAct, AdminRead},
    id::ProfileId,
    port::Broker,
    profile::ProfileCatalog,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

pub use auth::{Authenticator, StaticTokens, hosts_for};
pub use fence::{Fence, FenceFuture};
pub use issues::IssueExporter;
pub use logs::LogExporter;
pub use resp::LeakCheck;
pub use settings::AdminSettings;
pub use trace::Exporter;

/// Mixes enabled observability settings (`trace`, `logs`, `issues`) into the host activation fingerprint.
/// Disabled sections are skipped so existing deployment fingerprints remain unchanged.
pub fn obs_fingerprint(base: [u8; 32], settings: &AdminSettings) -> [u8; 32] {
    let base = match trace::fingerprint(&settings.trace) {
        None => base,
        Some(fp) => {
            let mut hash = Sha256::new();
            hash.update(b"bot-authority-trace-v1\0");
            hash.update(base);
            hash.update(fp);
            hash.finalize().into()
        }
    };
    let base = match logs::fingerprint(&settings.logs) {
        None => base,
        Some(fp) => {
            let mut hash = Sha256::new();
            hash.update(b"bot-authority-logs-v1\0");
            hash.update(base);
            hash.update(fp);
            hash.finalize().into()
        }
    };
    match issues::fingerprint(&settings.issues) {
        None => base,
        Some(fp) => {
            let mut hash = Sha256::new();
            hash.update(b"bot-authority-issues-v1\0");
            hash.update(base);
            hash.update(fp);
            hash.finalize().into()
        }
    }
}

fn holds_secret(red: &trace::Redactor, v: &Value) -> bool {
    match v {
        Value::String(s) => red.has_value(s),
        Value::Array(a) => a.iter().any(|x| holds_secret(red, x)),
        Value::Object(o) => o
            .iter()
            .any(|(k, x)| red.has_value(k) || holds_secret(red, x)),
        _ => false,
    }
}

pub struct AdminConfig {
    /// Allowed `Host` header values (with port) to block DNS rebinding.
    pub hosts: Vec<String>,
    pub max_failures: usize,
    pub window: Duration,
    pub block: Duration,
    pub leak_check: Option<LeakCheck>,
    pub traces: Option<Arc<Exporter>>,
    pub logs: Option<Arc<LogExporter>>,
    pub issues: Option<Arc<IssueExporter>>,
    pub fence: Option<Fence>,
    pub fence_timeout: Duration,
}

impl AdminConfig {
    pub fn new(bind: SocketAddr) -> Self {
        Self {
            hosts: hosts_for(bind, &[]),
            max_failures: 5,
            window: Duration::from_secs(60),
            block: Duration::from_secs(60),
            leak_check: None,
            traces: None,
            logs: None,
            issues: None,
            fence: None,
            fence_timeout: fence::DEFAULT_TIMEOUT,
        }
    }

    pub fn fence(mut self, check: impl Fn() -> FenceFuture + Send + Sync + 'static) -> Self {
        self.fence = Some(Arc::new(check));
        self
    }

    pub fn fence_timeout(mut self, timeout: Duration) -> Self {
        self.fence_timeout = timeout;
        self
    }

    /// Blocks responses containing known secret values by scanning decoded JSON strings (avoiding escape bypasses).
    pub fn leak_values(self, values: impl IntoIterator<Item = String>) -> Self {
        self.leak_redactor(trace::Redactor::new(values))
    }

    /// Like `leak_values`, sharing a live `Redactor` so rotated secrets added later are caught immediately.
    pub fn leak_redactor(mut self, red: trace::Redactor) -> Self {
        self.leak_check = Some(Arc::new(move |v| !holds_secret(&red, v)));
        self
    }

    pub fn traces(mut self, exporter: Arc<Exporter>) -> Self {
        self.traces = Some(exporter);
        self
    }

    pub fn logs(mut self, exporter: Arc<LogExporter>) -> Self {
        self.logs = Some(exporter);
        self
    }

    pub fn issues(mut self, exporter: Arc<IssueExporter>) -> Self {
        self.issues = Some(exporter);
        self
    }

    pub fn host(mut self, host: &str) -> Self {
        self.hosts.push(host.trim().to_ascii_lowercase());
        self
    }
}

pub struct AdminDeps {
    pub read: Arc<dyn AdminRead>,
    pub act: Arc<dyn AdminAct>,
    pub broker: Arc<dyn Broker>,
    pub auth: Arc<dyn Authenticator>,
    pub profiles: Arc<dyn ProfileCatalog>,
}

#[derive(Default)]
pub struct Extra {
    pub(crate) exporter: Option<Arc<Exporter>>,
    pub(crate) logs: Option<Arc<LogExporter>>,
    pub(crate) issues: Option<Arc<IssueExporter>>,
}

impl Extra {
    pub(crate) fn obs(&self, profile: &ProfileId) -> Value {
        match self.exporter.as_ref().and_then(|e| e.stats(profile)) {
            Some(stats) => stats.json(),
            None => serde_json::json!({"enabled": false}),
        }
    }

    pub(crate) fn logs_obs(&self) -> Value {
        match &self.logs {
            Some(exporter) if exporter.enabled() => exporter.stats().json(),
            _ => serde_json::json!({"enabled": false}),
        }
    }

    pub(crate) fn issues_obs(&self) -> Value {
        match &self.issues {
            Some(exporter) if exporter.enabled() => exporter.stats().json(),
            _ => serde_json::json!({"enabled": false}),
        }
    }

    fn link(&self, profile: &str, trace_hex: &str) -> Option<String> {
        let bytes: Vec<u8> = (0..trace_hex.len() / 2)
            .map_while(|i| u8::from_str_radix(trace_hex.get(i * 2..i * 2 + 2)?, 16).ok())
            .collect();
        let trace: [u8; 16] = bytes.try_into().ok()?;
        self.exporter
            .as_ref()?
            .link(&ProfileId(profile.to_owned()), &trace)
    }

    pub(crate) fn decorate(&self, v: &mut Value) {
        match v {
            Value::Object(o) => {
                let link = match (o.get("profile"), o.get("trace_id")) {
                    (Some(Value::String(p)), Some(Value::String(t))) => self.link(p, t),
                    _ => None,
                };
                if let Some(link) = link {
                    o.insert("trace_link".into(), Value::String(link));
                }
                o.values_mut().for_each(|x| self.decorate(x));
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.decorate(x)),
            _ => {}
        }
    }

    pub(crate) fn decision(
        &self,
        at: &bot_core::id::Attempt,
        tag: &str,
        action: &'static str,
        verdict: &'static str,
        risk: Option<&'static str>,
    ) {
        use trace::{SpanIds, SpanRecord, key};
        let Some(e) = &self.exporter else { return };
        // Mix action and timestamp into the span ID to avoid collisions on repeated decisions without embedding nonces.
        let ids =
            SpanIds::attempt(at).child("admin", &format!("{action}:{tag}:{}", trace::now_nano()));
        let mut span = SpanRecord::start(&at.profile, format!("admin {action}"), ids)
            .attr(key::BOT_ADMIN_ACTION, action)
            .attr(key::BOT_ADMIN_VERDICT, verdict)
            .attr(key::BOT_RUN_ID, at.run.0.clone());
        if let Some(r) = risk {
            span = span.attr(key::BOT_APPROVAL_RISK, r);
        }
        span.end(e.as_ref());
    }
}

pub fn router(cfg: AdminConfig, deps: AdminDeps) -> Option<Router> {
    if !deps.auth.enabled() {
        return None;
    }
    let guard = auth::Guard::new(
        deps.auth,
        &cfg.hosts,
        cfg.max_failures,
        cfg.window,
        cfg.block,
    );
    let st = Arc::new(api::St {
        read: deps.read,
        act: deps.act,
        broker: deps.broker,
        profiles: deps.profiles,
        guard: guard.clone(),
        leak: cfg.leak_check,
        started: Instant::now(),
        extra: Extra {
            exporter: cfg.traces,
            logs: cfg.logs,
            issues: cfg.issues,
        },
    });
    let gate = cfg.fence.map(|check| {
        Arc::new(fence::Gate {
            check,
            timeout: cfg.fence_timeout,
        })
    });
    let app = Router::new()
        .route("/v1/admin/me", get(api::me))
        .route("/v1/admin/runs", get(api::runs))
        .route("/v1/admin/runs/{profile}/{run}", get(api::run_detail))
        .route(
            "/v1/admin/runs/{profile}/{run}/resume",
            post(run_control::resume),
        )
        .route(
            "/v1/admin/runs/{profile}/{run}/cancel",
            post(run_control::cancel),
        )
        .route("/v1/admin/approvals", get(api::approvals))
        .route("/v1/admin/approvals/{nonce}/decide", post(api::decide))
        .route("/v1/admin/holds", get(api::holds))
        .route(
            "/v1/admin/holds/{profile}/{run}/{call}/resolve",
            post(api::resolve),
        )
        .route("/v1/admin/preapprovals", get(preapproval::list))
        .route(
            "/v1/admin/preapprovals/{profile}/{agent}/{trigger}/pause",
            post(preapproval::pause),
        )
        .route(
            "/v1/admin/preapprovals/{profile}/{agent}/{trigger}/resume",
            post(preapproval::resume),
        )
        .route("/v1/admin/audit", get(api::audit))
        .route("/v1/admin/audit/verify", get(api::audit_verify))
        .route("/v1/admin/status", get(api::status))
        .route("/v1/admin/map", get(api::map))
        .route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/ui") }),
        )
        .route("/ui", get(assets::index))
        .route("/ui/", get(assets::index))
        .route("/ui/{*path}", get(assets::asset))
        .fallback(api::not_found)
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(st);
    // Keep fence inside auth so only authenticated requests trigger active-set checks.
    let app = match gate {
        Some(g) => app.layer(from_fn_with_state(g, fence::layer)),
        None => app,
    };
    Some(app.layer(from_fn_with_state(guard, auth::guard)))
}

pub fn check_bind(addr: SocketAddr, remote_allowed: bool) -> Result<(), String> {
    if addr.ip().is_loopback() || remote_allowed {
        Ok(())
    } else {
        Err("remote must be enabled to bind admin UI on a non-loopback address".to_owned())
    }
}

pub async fn bind(
    addr: SocketAddr,
    remote_allowed: bool,
) -> std::io::Result<tokio::net::TcpListener> {
    check_bind(addr, remote_allowed)
        .map_err(|m| std::io::Error::new(std::io::ErrorKind::InvalidInput, m))?;
    tokio::net::TcpListener::bind(addr).await
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Attach peer ConnectInfo for per-IP auth rate limiting.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_binding_needs_an_explicit_flag() {
        let local: SocketAddr = "127.0.0.1:8080".parse().expect("addr");
        let v6: SocketAddr = "[::1]:8080".parse().expect("addr");
        let any: SocketAddr = "0.0.0.0:8080".parse().expect("addr");
        assert!(check_bind(local, false).is_ok() && check_bind(v6, false).is_ok());
        assert!(check_bind(any, false).is_err());
        assert!(check_bind(any, true).is_ok());
    }
}
