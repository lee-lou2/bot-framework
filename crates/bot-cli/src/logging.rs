//! 로그 설정. `BOT_LOG`(기본 `warn`)는 **이 프로그램의 크레이트**에만 그 수준으로 적용한다. 그 밖의 라이브러리(웹소켓·HTTP·TLS·런타임)는
//! `BOT_LOG`가 무엇이든 경고까지만 낸다 — 이 라이브러리들은 `trace`에서 오가는 프레임·본문을 그대로 기록하므로(예: 웹소켓 라이브러리의 «Received message …»)
//! 민감 DM 원문이나 요청·응답 본문이 stderr에 남는다. 수준을 올려도 외부 전송 라이브러리의 상세 로그는 나오지 않는다.
//!
//! 데이터베이스 드라이버(sqlx)는 아예 끈다: 접속 주소를 구문 분석하다 모르는 매개변수의 **값**(예: 합성 `sslpassword`)을 경고에 싣는다.
//!
//! 이 프로그램의 크레이트가 남기는 로그는 식별자·분류 낱말만 싣고 본문·인자·자격 값은 싣지 않는다(시험이 trace에서 확인한다).
use std::io;
use tracing::Level;
use tracing_subscriber::{
    filter::{LevelFilter, Targets},
    fmt::{self, MakeWriter},
    layer::SubscriberExt,
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

/// `BOT_LOG` 값을 수준으로 읽는다. 없거나 알아볼 수 없으면 `warn`.
pub fn level_of(text: Option<&str>) -> Level {
    text.and_then(|text| text.parse::<Level>().ok())
        .unwrap_or(Level::WARN)
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

/// 이 프로세스의 로그를 `writer`로 보내도록 한 번 설치한다(`log` 크레이트를 쓰는 라이브러리의 기록도 같은 필터를 지난다).
pub fn init_to<W>(level: Level, writer: W) -> Result<(), TryInitError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    // 필터는 **전역** 층으로 둔다(모든 층에 같은 판정이 적용되고 호출 지점마다 한 번 정해진다)
    tracing_subscriber::registry()
        .with(filter(level))
        .with(fmt::layer().with_writer(writer))
        .try_init()?;
    // 이 프로그램의 크레이트가 요청한 수준으로 켜졌다는 표지(수준이 `debug` 이상일 때만 보인다)
    tracing::debug!(%level, "로그를 시작했어요");
    Ok(())
}

/// 표준 오류로 설치한다. `BOT_LOG`는 환경에서 읽는다.
pub fn init() {
    let level = level_of(std::env::var("BOT_LOG").ok().as_deref());
    // 이미 설치돼 있으면(시험) 그대로 둔다
    let _ = init_to(level, io::stderr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_comes_from_the_environment_and_defaults_to_warn() {
        assert_eq!(level_of(None), Level::WARN);
        assert_eq!(level_of(Some("trace")), Level::TRACE);
        assert_eq!(level_of(Some("DEBUG")), Level::DEBUG);
        assert_eq!(level_of(Some("error")), Level::ERROR);
        assert_eq!(level_of(Some("verbose")), Level::WARN);
        assert_eq!(level_of(Some("")), Level::WARN);
    }

    /// 타깃마다 켜진 가장 상세한 수준(필터가 실제로 판정한 값)
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
                // 하위 모듈의 타깃도 같은 수준이다(`bot_cli::serve`는 `bot_cli`의 것)
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
        // 데이터베이스 드라이버는 어떤 수준을 요청해도 아무것도 내지 않는다(타깃 이름이 `sqlx`로 시작하든 `sqlx_postgres::…`이든)
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
        // 낮은 수준을 요청하면 외부도 그보다 상세하지 않다
        let quiet = filter(Level::ERROR);
        assert_eq!(most_verbose(&quiet, "tungstenite"), Some(Level::ERROR));
        assert_eq!(
            most_verbose(&quiet, "bot_gateway::slack"),
            Some(Level::ERROR)
        );
    }
}
