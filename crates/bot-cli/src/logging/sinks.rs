//! Late-bound OTLP log/issue tracing layer and panic hook.
use bot_admin::{
    issues::IssueExporter,
    logs::{LogEntry, LogExporter, LogLevel},
};
use bot_core::{
    issue::{IssueEvent, IssueLevel, IssueReporter},
    trace::now_nano,
};
use std::{
    cell::Cell,
    fmt as stdfmt,
    sync::{
        Arc, Once, OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context};

/// Target prefixes skipped by the sink to prevent exporter/HTTP transport logs from recursing back into sinks.
const SKIP_TARGETS: [&str; 9] = [
    "bot_admin::obs",
    "bot_admin::trace",
    "bot_admin::logs",
    "bot_admin::issues",
    "reqwest",
    "hyper",
    "hyper_util",
    "h2",
    "rustls",
];

thread_local! {
    static IN_SINK: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone)]
struct ActiveSinks {
    token: u64,
    logs: Option<Arc<LogExporter>>,
    issues: Option<Arc<IssueExporter>>,
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
static SINKS: OnceLock<RwLock<Option<ActiveSinks>>> = OnceLock::new();
static PANIC_HOOK: Once = Once::new();

fn slot() -> &'static RwLock<Option<ActiveSinks>> {
    SINKS.get_or_init(|| RwLock::new(None))
}

fn current() -> Option<ActiveSinks> {
    slot().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Detaches the attached sinks on drop only if its registration token is still active.
pub struct SinkGuard {
    token: u64,
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        let mut guard = slot().write().unwrap_or_else(|e| e.into_inner());
        if guard
            .as_ref()
            .is_some_and(|active| active.token == self.token)
        {
            *guard = None;
        }
    }
}

pub fn attach_sinks(
    logs: Option<Arc<LogExporter>>,
    issues: Option<Arc<IssueExporter>>,
) -> SinkGuard {
    if issues.as_ref().is_some_and(|e| e.enabled()) {
        install_panic_hook();
    }
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    *slot().write().unwrap_or_else(|e| e.into_inner()) = Some(ActiveSinks {
        token,
        logs,
        issues,
    });
    SinkGuard { token }
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(active) = current()
                && let Some(issues) = &active.issues
                && issues.enabled()
            {
                let msg = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".to_owned());
                let target = info
                    .location()
                    .map(|loc| format!("{}:{}", loc.file(), loc.line()))
                    .unwrap_or_else(|| "panic".to_owned());
                issues.report(IssueEvent::new(IssueLevel::Fatal, "panic", msg, target));
            }
            prev(info);
        }));
    });
}

fn should_skip(target: &str) -> bool {
    SKIP_TARGETS.iter().any(|prefix| {
        target
            .strip_prefix(*prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

fn map_level(level: &Level) -> Option<LogLevel> {
    match *level {
        Level::ERROR => Some(LogLevel::Error),
        Level::WARN => Some(LogLevel::Warn),
        Level::INFO => Some(LogLevel::Info),
        Level::DEBUG => Some(LogLevel::Debug),
        Level::TRACE => None,
    }
}

#[derive(Default)]
struct FieldCollector {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields
                .push((field.name().to_owned(), value.to_owned()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn stdfmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields.push((field.name().to_owned(), rendered));
        }
    }
}

pub(super) struct SinkLayer;

impl<S: Subscriber> Layer<S> for SinkLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let target = meta.target();
        if should_skip(target) {
            return;
        }
        let Some(active) = current() else { return };
        let wants_log = active.logs.as_ref().is_some_and(|e| e.enabled());
        let wants_issue =
            *meta.level() == Level::ERROR && active.issues.as_ref().is_some_and(|e| e.enabled());
        if !wants_log && !wants_issue {
            return;
        }
        // Thread-local reentrancy guard prevents nested tracing events while recording.
        let entered = IN_SINK.with(|flag| {
            if flag.get() {
                false
            } else {
                flag.set(true);
                true
            }
        });
        if !entered {
            return;
        }
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                IN_SINK.with(|flag| flag.set(false));
            }
        }
        let _reset = Reset;

        let mut collector = FieldCollector::default();
        event.record(&mut collector);
        let message = collector.message.unwrap_or_default();
        let now = now_nano();

        if wants_log
            && let Some(level) = map_level(meta.level())
            && let Some(logs) = &active.logs
        {
            logs.record(LogEntry {
                timestamp_nano: now,
                level,
                target: target.to_owned(),
                message: message.clone(),
                fields: collector.fields.clone(),
                trace_id: None,
                span_id: None,
            });
        }
        if wants_issue && let Some(issues) = &active.issues {
            let kind = collector
                .fields
                .iter()
                .find(|(k, v)| k == "code" && !v.is_empty())
                .map(|(_, v)| v.as_str())
                .unwrap_or("log.error");
            let mut ev = IssueEvent::new(IssueLevel::Error, kind, message, target);
            ev.timestamp_nano = now;
            ev.tags = collector.fields;
            issues.report(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_skip_matches_exact_and_child_targets_only() {
        assert!(should_skip("reqwest"));
        assert!(should_skip("reqwest::connect"));
        assert!(should_skip("bot_admin::issues::export"));
        assert!(!should_skip("reqwest_custom"));
        assert!(!should_skip("bot_admin::api"));
        assert!(!should_skip("bot_runtime::worker"));
    }
}
