//! Tracing subscriber setup with crate-scoped levels, NDJSON formatting, and sqlx suppression.
//!
//! External transport crates are capped at `WARN` regardless of `BOT_LOG` so `trace` frames never
//! leak private DM bodies or HTTP payloads; `sqlx` targets are disabled entirely because option
//! parsing can log unknown URL parameter values.

mod sinks;

pub use sinks::{SinkGuard, attach_sinks};

use serde_json::{Map, Value};
use std::{
    fmt as stdfmt,
    io::{self, Write},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{
    Layer,
    filter::{LevelFilter, Targets},
    fmt::{self, MakeWriter},
    layer::{Context, SubscriberExt},
    util::{SubscriberInitExt, TryInitError},
};

const OWN: [&str; 9] = [
    "bot",
    "bot_admin",
    "bot_broker",
    "bot_cli",
    "bot_core",
    "bot_gateway",
    "bot_profile",
    "bot_runtime",
    "bot_store",
];

const OUTSIDE: Level = Level::WARN;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

pub fn level_of(text: Option<&str>) -> Level {
    text.and_then(|text| text.parse::<Level>().ok())
        .unwrap_or(Level::WARN)
}

pub fn format_of(text: Option<&str>) -> LogFormat {
    match text.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("json") => LogFormat::Json,
        _ => LogFormat::Text,
    }
}

/// Suppressed unconditionally because sqlx warns with raw values of unrecognized connection URL parameters.
const DATABASE_DRIVERS: [&str; 3] = ["sqlx", "sqlx_core", "sqlx_postgres"];

pub fn filter(level: Level) -> Targets {
    let targets = OWN.iter().fold(
        Targets::new().with_default(level.min(OUTSIDE)),
        |targets, own| targets.with_target(*own, level),
    );
    DATABASE_DRIVERS.iter().fold(targets, |targets, driver| {
        targets.with_target(*driver, LevelFilter::OFF)
    })
}

#[derive(Default)]
struct JsonVisitor {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl Visit for JsonVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields
                .insert(field.name().to_owned(), Value::String(value.to_owned()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn stdfmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields
                .insert(field.name().to_owned(), Value::String(rendered));
        }
    }
}

struct JsonLayer<W> {
    writer: W,
}

impl<S, W> Layer<S> for JsonLayer<W>
where
    S: Subscriber,
    W: for<'w> MakeWriter<'w> + 'static,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0u64, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut obj = Map::new();
        obj.insert("ts_ms".into(), Value::from(ts_ms));
        obj.insert("level".into(), Value::String(meta.level().to_string()));
        obj.insert("target".into(), Value::String(meta.target().to_owned()));
        obj.insert(
            "message".into(),
            Value::String(visitor.message.unwrap_or_default()),
        );
        if !visitor.fields.is_empty() {
            obj.insert("fields".into(), Value::Object(visitor.fields));
        }
        if let Ok(mut line) = serde_json::to_vec(&Value::Object(obj)) {
            line.push(b'\n');
            let mut out = self.writer.make_writer();
            let _ = out.write_all(&line);
        }
    }
}

pub fn init_to<W>(level: Level, writer: W) -> Result<(), TryInitError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    init_with(level, LogFormat::Text, writer)
}

pub fn init_with<W>(level: Level, format: LogFormat, writer: W) -> Result<(), TryInitError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let registry = tracing_subscriber::registry()
        .with(filter(level))
        .with(sinks::SinkLayer);
    match format {
        LogFormat::Text => registry.with(fmt::layer().with_writer(writer)).try_init()?,
        LogFormat::Json => registry.with(JsonLayer { writer }).try_init()?,
    }
    tracing::debug!(%level, format = ?format, "logging initialized");
    Ok(())
}

pub fn init() {
    let level = level_of(std::env::var("BOT_LOG").ok().as_deref());
    let format = format_of(std::env::var("BOT_LOG_FORMAT").ok().as_deref());
    // Ignore already-initialized error when tests install a subscriber first.
    let _ = init_with(level, format, io::stderr);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    #[test]
    fn the_level_and_format_come_from_the_environment_with_safe_defaults() {
        assert_eq!(level_of(None), Level::WARN);
        assert_eq!(level_of(Some("trace")), Level::TRACE);
        assert_eq!(level_of(Some("DEBUG")), Level::DEBUG);
        assert_eq!(level_of(Some("error")), Level::ERROR);
        assert_eq!(level_of(Some("verbose")), Level::WARN);
        assert_eq!(level_of(Some("")), Level::WARN);

        assert_eq!(format_of(None), LogFormat::Text);
        assert_eq!(format_of(Some("")), LogFormat::Text);
        assert_eq!(format_of(Some("text")), LogFormat::Text);
        assert_eq!(format_of(Some("json")), LogFormat::Json);
        assert_eq!(format_of(Some("JSON")), LogFormat::Json);
        assert_eq!(format_of(Some("yaml")), LogFormat::Text);
    }

    #[test]
    fn json_layer_writes_valid_ndjson_with_fields_under_the_same_target_filter() {
        let buf = Buf::default();
        let subscriber = tracing_subscriber::registry()
            .with(filter(Level::INFO))
            .with(JsonLayer {
                writer: buf.clone(),
            });
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "bot_cli::test", run = "r-1", count = 3u64, ok = true, "started");
            tracing::info!(target: "reqwest::connect", "external info is filtered out");
            tracing::warn!(target: "sqlx::query", "database driver warning is filtered out");
        });
        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 1, "{raw}");
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["level"], "INFO");
        assert_eq!(parsed["target"], "bot_cli::test");
        assert_eq!(parsed["message"], "started");
        assert_eq!(parsed["fields"]["run"], "r-1");
        assert_eq!(parsed["fields"]["count"], 3);
        assert_eq!(parsed["fields"]["ok"], true);
        assert!(parsed["ts_ms"].as_u64().unwrap_or(0) > 1_600_000_000_000);
    }

    fn most_verbose(targets: &Targets, target: &str) -> Option<Level> {
        [
            Level::TRACE,
            Level::DEBUG,
            Level::INFO,
            Level::WARN,
            Level::ERROR,
        ]
        .into_iter()
        .find(|level| targets.would_enable(target, level))
    }

    #[test]
    fn only_our_crates_follow_the_requested_level_and_everything_else_stops_at_warn() {
        for level in [Level::TRACE, Level::DEBUG, Level::INFO] {
            let targets = filter(level);
            for own in OWN {
                assert_eq!(most_verbose(&targets, own), Some(level), "{own}");
                let child = format!("{own}::serve::claude");
                assert_eq!(most_verbose(&targets, &child), Some(level), "{child}");
            }
            for outside in [
                "tungstenite::protocol",
                "tokio_tungstenite",
                "hyper_util::client::legacy::pool",
                "hyper::proto::h1::io",
                "h2::codec",
                "reqwest::connect",
                "rustls::client",
                "tokio_util::codec",
                "mio::poll",
                "axum::serve",
                "log",
            ] {
                assert_eq!(
                    most_verbose(&targets, outside),
                    Some(Level::WARN),
                    "{outside}"
                );
            }
        }
        for level in [
            Level::TRACE,
            Level::DEBUG,
            Level::INFO,
            Level::WARN,
            Level::ERROR,
        ] {
            let targets = filter(level);
            for driver in [
                "sqlx",
                "sqlx::query",
                "sqlx_core::pool::inner",
                "sqlx_postgres::options::parse",
            ] {
                assert_eq!(most_verbose(&targets, driver), None, "{level} {driver}");
            }
        }
        let quiet = filter(Level::ERROR);
        assert_eq!(most_verbose(&quiet, "tungstenite"), Some(Level::ERROR));
        assert_eq!(
            most_verbose(&quiet, "bot_gateway::slack"),
            Some(Level::ERROR)
        );
    }
}
