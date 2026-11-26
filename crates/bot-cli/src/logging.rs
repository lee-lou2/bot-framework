//! 로그 설정. `BOT_LOG`(기본 `warn`)는 **이 프로그램의 크레이트**에만 그 수준으로 적용한다. 그 밖의 라이브러리(웹소켓·HTTP·TLS·런타임)는
//! `BOT_LOG`가 무엇이든 경고까지만 낸다 — 이 라이브러리들은 `trace`에서 오가는 프레임·본문을 그대로 기록하므로(예: 웹소켓 라이브러리의 «Received message …»)
//! 민감 DM 원문이나 요청·응답 본문이 stderr에 남는다. 수준을 올려도 외부 전송 라이브러리의 상세 로그는 나오지 않는다.
//!
//! `BOT_LOG_FORMAT`(`text` 기본 · `json`)으로 출력 형식을 고른다. `json`은 줄마다 하나의 JSON 객체(NDJSON)를 stderr에 쓴다.
//! 호스트 설정에 `admin.logs`나 `admin.issues`가 켜져 있으면 `serve` 시작 시 같은 필터 아래에 OTLP 로그·이슈 트래커 싱크를 붙인다(`sinks`).
//!
//! 데이터베이스 드라이버(sqlx)는 아예 끈다: 접속 주소를 구문 분석하다 모르는 매개변수의 **값**(예: 합성 `sslpassword`)을 경고에 싣는다.
//! 이 프로그램의 크레이트가 남기는 로그는 식별자·분류 낱말만 싣고 본문·인자·자격 값은 싣지 않는다(시험이 trace에서 확인한다).

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

/// 이 프로그램의 로그 타깃(크레이트 이름). 바이너리 `bot`의 타깃도 포함한다.
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

/// 외부 라이브러리가 낼 수 있는 가장 상세한 수준
const OUTSIDE: Level = Level::WARN;

/// 로그 출력 형식(`BOT_LOG_FORMAT`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

/// `BOT_LOG` 값을 수준으로 읽는다. 없거나 알아볼 수 없으면 `warn`.
pub fn level_of(text: Option<&str>) -> Level {
    text.and_then(|text| text.parse::<Level>().ok())
        .unwrap_or(Level::WARN)
}

/// `BOT_LOG_FORMAT` 값을 형식으로 읽는다. `json`(대소문자 무시)이면 `Json`, 그 밖에는 `Text`.
pub fn format_of(text: Option<&str>) -> LogFormat {
    match text.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("json") => LogFormat::Json,
        _ => LogFormat::Text,
    }
}

/// 데이터베이스 드라이버의 기록 타깃. 접속 주소를 구문 분석하다 모르는 매개변수의 **값**을 경고로 남기므로(예: 주소에 든 인증서 비밀번호)
/// 어떤 명령에서도 어떤 수준에서도 이 기록을 받지 않는다.
const DATABASE_DRIVERS: [&str; 3] = ["sqlx", "sqlx_core", "sqlx_postgres"];

/// 타깃별 수준 상한: 이 프로그램의 크레이트는 요청한 수준, 데이터베이스 드라이버는 끔, 나머지는 요청한 수준과 `warn` 중 덜 상세한 쪽.
pub fn filter(level: Level) -> Targets {
    let targets = OWN.iter().fold(
        Targets::new().with_default(level.min(OUTSIDE)),
        |targets, own| targets.with_target(*own, level),
    );
    DATABASE_DRIVERS.iter().fold(targets, |targets, driver| {
        targets.with_target(*driver, LevelFilter::OFF)
    })
}

/// 구조화 로그(NDJSON) 한 줄을 만드는 방문자(외부 `tracing-subscriber/json` 의존 없이 workspace `serde_json`만 쓴다).
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

/// 이 프로세스의 로그를 `writer`로 보내도록 한 번 설치한다(`text` 기본 형식, 기존 호출자 호환).
pub fn init_to<W>(level: Level, writer: W) -> Result<(), TryInitError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    init_with(level, LogFormat::Text, writer)
}

/// 형식(`text`·`json`)을 지정해 전역 로그 구독자를 한 번 설치한다(OTLP 로그·이슈 싱크 레이어 포함).
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
    tracing::debug!(%level, format = ?format, "로그를 시작했어요");
    Ok(())
}

/// 표준 오류로 설치한다. `BOT_LOG`·`BOT_LOG_FORMAT`을 환경에서 읽는다.
pub fn init() {
    let level = level_of(std::env::var("BOT_LOG").ok().as_deref());
    let format = format_of(std::env::var("BOT_LOG_FORMAT").ok().as_deref());
    // 이미 설치돼 있으면(시험) 그대로 둔다
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
            tracing::info!(target: "bot_cli::test", run = "r-1", count = 3u64, ok = true, "시작했어요");
            tracing::info!(target: "reqwest::connect", "외부 info는 필터가 막는다");
            tracing::warn!(target: "sqlx::query", "DB 드라이버 경고도 막는다");
        });
        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 1, "{raw}");
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["level"], "INFO");
        assert_eq!(parsed["target"], "bot_cli::test");
        assert_eq!(parsed["message"], "시작했어요");
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
