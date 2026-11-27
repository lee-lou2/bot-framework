//! Process-wide observability setup: OTLP trace/log and Sentry issue exporters with live secret redaction.
use super::watched::{Pace, Watched, declared, read_round};
use crate::{host::HostFile, logging, secrets::env::EnvStore};
use bot_admin::{
    Exporter, Fence, IssueExporter, LogExporter,
    issues::IssueOptions,
    trace::{Options, Redactor},
};
use bot_core::{
    issue::{IssueEvent, IssueLevel, IssueReporter},
    port::SecretStore,
    trace::{SpanRecord, Tracer, Val, key},
};
use bot_profile::Registry;
use std::{sync::Arc, time::Duration};

// Expected control-flow or policy-denial outcomes that should not open Sentry issues.
const BENIGN_SPAN_ERRORS: [&str; 10] = [
    "cancelled",
    "denied",
    "forbidden",
    "invalid",
    "lost",
    "no_route",
    "parked",
    "rate_limited",
    "too_large",
    "unregistered",
];

struct ReportingTracer {
    inner: Arc<Exporter>,
    issues: Arc<IssueExporter>,
}

impl Tracer for ReportingTracer {
    fn record(&self, span: SpanRecord) {
        if let Some(error_class) = span.error
            && !BENIGN_SPAN_ERRORS.contains(&error_class)
        {
            let private = span
                .attrs
                .iter()
                .any(|(k, v)| *k == key::BOT_PRIVATE && *v == Val::Bool(true));
            let mut ev = IssueEvent::new(
                IssueLevel::Error,
                error_class,
                format!("{} failed ({error_class})", span.name),
                span.name.clone(),
            )
            .profile(span.profile.clone())
            .trace_id(span.ids.trace)
            .span_id(span.ids.span);
            ev.timestamp_nano = span.end_nano;
            if !private {
                for tag_key in [key::BOT_RUN_ID, key::BOT_AGENT, key::GEN_AI_TOOL_NAME] {
                    if let Some(val) = span.str(tag_key) {
                        ev = ev.tag(tag_key, val);
                    }
                }
            }
            self.issues.report(ev);
        }
        self.inner.record(span);
    }
}

pub struct Observe {
    pub exporter: Arc<Exporter>,
    pub logs: Arc<LogExporter>,
    pub issues: Arc<IssueExporter>,
    /// Shared live redactor fed by every secret read through `secrets`.
    pub redactor: Redactor,
    /// Single secret store passed to all roles so rotated values are masked before use.
    pub secrets: Arc<Watched>,
    _sinks: Option<logging::SinkGuard>,
}

impl Observe {
    pub fn tracer(&self) -> Arc<dyn Tracer> {
        if self.issues.enabled() {
            Arc::new(ReportingTracer {
                inner: self.exporter.clone(),
                issues: self.issues.clone(),
            })
        } else {
            self.exporter.clone()
        }
    }

    pub fn set_gate(&self, check: Fence, timeout: Duration) {
        self.exporter.set_gate(check.clone(), timeout);
        self.logs.set_gate(check.clone(), timeout);
        self.issues.set_gate(check, timeout);
    }

    pub async fn flush(&self) {
        let timeout = Duration::from_secs(5);
        tokio::join!(
            self.exporter.flush(timeout),
            self.logs.flush(timeout),
            self.issues.flush(timeout),
        );
    }
}

/// Placeholder secret store used by `bot doctor` when checking model alias configuration offline.
pub(super) fn inert_secrets() -> Arc<Watched> {
    Arc::new(Watched::new(
        Arc::new(EnvStore::default()),
        Redactor::default(),
    ))
}

pub async fn start(
    host: &HostFile,
    registry: &Registry,
    secrets: Arc<dyn SecretStore>,
) -> Result<Observe, String> {
    let redactor = Redactor::default();
    let secrets = Arc::new(Watched::new(secrets, redactor.clone()));
    read_round(
        &secrets,
        &declared(host, registry),
        Pace::new(Duration::ZERO),
    )
    .await;
    let store = Some(secrets.clone() as Arc<dyn SecretStore>);
    let exporter = Exporter::start(
        host.admin.trace.clone(),
        store.clone(),
        redactor.clone(),
        Options::default(),
    )?;
    let logs = LogExporter::start(
        host.admin.logs.clone(),
        store.clone(),
        redactor.clone(),
        Options::default(),
    )?;
    let issues = IssueExporter::start(
        host.admin.issues.clone(),
        store,
        redactor.clone(),
        IssueOptions::default(),
    )?;
    let sinks = (logs.enabled() || issues.enabled())
        .then(|| logging::attach_sinks(Some(logs.clone()), Some(issues.clone())));
    Ok(Observe {
        exporter,
        logs,
        issues,
        redactor,
        secrets,
        _sinks: sinks,
    })
}

#[cfg(test)]
mod tests;
