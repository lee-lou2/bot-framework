//! 프로세스 설정: `BOT_` 환경 변수. 값 자체는 비밀이 아니지만(경로·주소) 데이터베이스 주소에는 비밀번호가 들어 있을 수 있어
//! 어디에도 출력하지 않는다.
//!
//! - `BOT_PROFILES` — 프로필 루트 폴더
//! - `BOT_HOST_CONFIG` — 호스트 등록부(JSON) 경로. 없으면 내장 데모 호스트(README 5분 시작용)
//! - `BOT_DATABASE_URL` — PostgreSQL 접속 주소(저장소를 쓰는 역할)
//! - `BOT_DB_MAX_CONNECTIONS` — PostgreSQL 연결 풀 최대 개수(1~200, 생략하면 기본 10)
//! - `BOT_SOCKET` — CLI 소켓 경로(생략하면 호스트 설정 또는 기본 위치)
//! - `BOT_SECRETS` — 자격 보관소: `env`(기본, 개발용) · `keychain`(macOS) · `sops:<파일>`
//! - `BOT_SECRETS_REREAD_SECS` — 선언된 비밀 이름 전체를 다시 읽어 가림 목록에 더하는 주기(초, 기본 60, 0이면 끈다). 읽을 때마다 키체인은
//!   `security`를, SOPS는 30초 기억이 끝났을 때 `sops`를 부른다.
//! - `BOT_ADMIN_TOKEN_FILE` — 관리 화면 운영자 비밀을 받을 0600 파일(절대 경로). 터미널이 없는 기동의 전달 수단이다.
//! - `BOT_INSTALLS` — 도구 서버 읽기 전용 설치본을 두는 폴더(절대 경로, 생략하면 `$HOME/.local/state/bot/installs`)
use std::{path::PathBuf, time::Duration};

/// 선언된 비밀을 다시 읽는 기본 주기(초)
const DEFAULT_REREAD_SECS: u64 = 60;
/// 주기의 상한(초): 하루
const MAX_REREAD_SECS: u64 = 86_400;
/// DB 풀 최대 연결 수 상한
const MAX_DB_CONNECTIONS: u32 = 200;

/// 자격 보관소 종류. 종류마다 달라지는 사실(보호할 파일·누락 힌트·복호화 여부)은 `secrets/mod.rs`의 `impl SecretsKind`에 있다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretsKind {
    Env,
    Keychain,
    Sops(PathBuf),
}

#[derive(Clone)]
pub struct Settings {
    pub profiles: Option<PathBuf>,
    pub host_config: Option<PathBuf>,
    pub database_url: Option<String>,
    /// DB 풀 최대 연결 수(None이면 저장소 기본값 10)
    pub db_max_connections: Option<u32>,
    pub socket: Option<String>,
    pub secrets: SecretsKind,
    /// 선언된 비밀 이름 전체를 다시 읽는 주기. `None`이면 다시 읽지 않는다(시작 때와 쓰는 순간에만 읽는다).
    pub secrets_reread: Option<Duration>,
    pub installs: Option<PathBuf>,
    pub admin_token_file: Option<PathBuf>,
}

/// 비밀번호가 든 주소는 찍지 않는다.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("profiles", &self.profiles)
            .field("host_config", &self.host_config)
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "<설정됨>"),
            )
            .field("db_max_connections", &self.db_max_connections)
            .field("socket", &self.socket)
            .field("secrets", &self.secrets)
            .field("secrets_reread", &self.secrets_reread)
            .field("installs", &self.installs)
            .field("admin_token_file", &self.admin_token_file)
            .finish()
    }
}

fn parse_secrets(text: &str) -> Result<SecretsKind, String> {
    match text {
        "env" => Ok(SecretsKind::Env),
        "keychain" => Ok(SecretsKind::Keychain),
        other => match other.strip_prefix("sops:") {
            Some(path) if !path.is_empty() => Ok(SecretsKind::Sops(PathBuf::from(path))),
            _ => Err("BOT_SECRETS는 env · keychain · sops:<파일> 중 하나여야 해요".into()),
        },
    }
}

/// 0은 «끈다», 그 밖에는 1초 이상 하루 이하의 정수(초)만 받는다.
fn parse_reread(text: &str) -> Result<Option<Duration>, String> {
    match text.parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(secs) if secs <= MAX_REREAD_SECS => Ok(Some(Duration::from_secs(secs))),
        _ => Err(format!(
            "BOT_SECRETS_REREAD_SECS는 0(끔) 또는 1~{MAX_REREAD_SECS} 사이의 정수(초)여야 해요"
        )),
    }
}

/// DB 풀 최대 연결 수: 1~200 정수만 받는다.
fn parse_db_max_connections(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(n) if (1..=MAX_DB_CONNECTIONS).contains(&n) => Ok(n),
        _ => Err(format!(
            "BOT_DB_MAX_CONNECTIONS는 1~{MAX_DB_CONNECTIONS} 사이의 정수여야 해요"
        )),
    }
}

impl Settings {
    /// `get`으로 환경을 읽는다(시험에서는 가짜를 준다). 빈 값은 없는 것으로 본다.
    pub fn read(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let pick = |name: &str| get(name).filter(|value| !value.is_empty());
        Ok(Self {
            profiles: pick("BOT_PROFILES").map(PathBuf::from),
            host_config: pick("BOT_HOST_CONFIG").map(PathBuf::from),
            database_url: pick("BOT_DATABASE_URL"),
            db_max_connections: match pick("BOT_DB_MAX_CONNECTIONS") {
                Some(text) => Some(parse_db_max_connections(&text)?),
                None => None,
            },
            socket: pick("BOT_SOCKET"),
            installs: pick("BOT_INSTALLS").map(PathBuf::from),
            admin_token_file: pick("BOT_ADMIN_TOKEN_FILE").map(PathBuf::from),
            secrets: match pick("BOT_SECRETS") {
                Some(text) => parse_secrets(&text)?,
                None => SecretsKind::Env,
            },
            secrets_reread: match pick("BOT_SECRETS_REREAD_SECS") {
                Some(text) => parse_reread(&text)?,
                None => Some(Duration::from_secs(DEFAULT_REREAD_SECS)),
            },
        })
    }

    pub fn from_process() -> Result<Self, String> {
        Self::read(&|name| std::env::var(name).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &'static [(&'static str, &'static str)]) -> Result<Settings, String> {
        Settings::read(&|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        })
    }

    #[test]
    fn reads_the_documented_variables_and_ignores_empty_ones() {
        let settings = read(&[
            ("BOT_PROFILES", "/srv/profiles"),
            ("BOT_HOST_CONFIG", ""),
            ("BOT_DATABASE_URL", "postgres://u:pw@localhost/bot"),
            ("BOT_SECRETS", "sops:/srv/secrets.sops.json"),
            ("BOT_INSTALLS", "/srv/installs"),
        ])
        .unwrap();
        assert_eq!(settings.installs, Some(PathBuf::from("/srv/installs")));
        assert_eq!(read(&[]).unwrap().installs, None);
        assert_eq!(settings.profiles, Some(PathBuf::from("/srv/profiles")));
        assert_eq!(settings.host_config, None);
        assert_eq!(
            settings.secrets,
            SecretsKind::Sops("/srv/secrets.sops.json".into())
        );
        assert_eq!(read(&[]).unwrap().secrets, SecretsKind::Env);
        assert_eq!(
            read(&[("BOT_SECRETS", "keychain")]).unwrap().secrets,
            SecretsKind::Keychain
        );
    }

    #[test]
    fn rejects_unknown_secret_backends_and_never_prints_the_database_url() {
        for bad in ["vault", "sops:", "ENV", "sops"] {
            let result = Settings::read(&|name| (name == "BOT_SECRETS").then(|| bad.to_string()));
            assert!(result.is_err(), "{bad}");
        }
        let settings = read(&[("BOT_DATABASE_URL", "postgres://user:hunter2@db/bot")]).unwrap();
        assert!(!format!("{settings:?}").contains("hunter2"));
    }

    #[test]
    fn the_secret_reread_period_defaults_to_a_minute_and_zero_turns_it_off() {
        let period = |text: &'static str| {
            Settings::read(&|name| (name == "BOT_SECRETS_REREAD_SECS").then(|| text.to_string()))
                .map(|settings| settings.secrets_reread)
        };
        assert_eq!(
            read(&[]).unwrap().secrets_reread,
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            period("").unwrap(),
            Some(Duration::from_secs(60)),
            "빈 값은 없는 것"
        );
        assert_eq!(period("5").unwrap(), Some(Duration::from_secs(5)));
        assert_eq!(period("86400").unwrap(), Some(Duration::from_secs(86_400)));
        assert_eq!(period("0").unwrap(), None, "0은 끈다");
        for bad in ["abc", "-1", "1.5", "86401", "60s", " 5"] {
            let error = period(bad).unwrap_err();
            assert!(error.contains("BOT_SECRETS_REREAD_SECS"), "{bad}: {error}");
        }
    }

    #[test]
    fn db_max_connections_accepts_bounded_integers_only() {
        let pool = |text: &'static str| {
            Settings::read(&|name| (name == "BOT_DB_MAX_CONNECTIONS").then(|| text.to_string()))
                .map(|settings| settings.db_max_connections)
        };
        assert_eq!(read(&[]).unwrap().db_max_connections, None);
        assert_eq!(pool("").unwrap(), None);
        assert_eq!(pool("1").unwrap(), Some(1));
        assert_eq!(pool("32").unwrap(), Some(32));
        assert_eq!(pool("200").unwrap(), Some(200));
        for bad in ["0", "201", "-1", "abc", " 10"] {
            let error = pool(bad).unwrap_err();
            assert!(error.contains("BOT_DB_MAX_CONNECTIONS"), "{bad}: {error}");
        }
    }
}
