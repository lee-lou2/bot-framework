//! Process configuration parsed from `BOT_*` environment variables.
use std::{path::PathBuf, time::Duration};

const DEFAULT_REREAD_SECS: u64 = 60;
const MAX_REREAD_SECS: u64 = 86_400;
const MAX_DB_CONNECTIONS: u32 = 200;

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
    pub db_max_connections: Option<u32>,
    pub socket: Option<String>,
    pub secrets: SecretsKind,
    pub secrets_reread: Option<Duration>,
    pub installs: Option<PathBuf>,
    pub admin_token_file: Option<PathBuf>,
}

// Redact `database_url` because it may contain embedded credentials.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("profiles", &self.profiles)
            .field("host_config", &self.host_config)
            .field("database_url", &self.database_url.as_ref().map(|_| "<set>"))
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
            _ => Err("BOT_SECRETS must be env, keychain, or sops:<file>".into()),
        },
    }
}

fn parse_reread(text: &str) -> Result<Option<Duration>, String> {
    match text.parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(secs) if secs <= MAX_REREAD_SECS => Ok(Some(Duration::from_secs(secs))),
        _ => Err(format!(
            "BOT_SECRETS_REREAD_SECS must be 0 (disabled) or an integer between 1 and {MAX_REREAD_SECS} seconds"
        )),
    }
}

fn parse_db_max_connections(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(n) if (1..=MAX_DB_CONNECTIONS).contains(&n) => Ok(n),
        _ => Err(format!(
            "BOT_DB_MAX_CONNECTIONS must be an integer between 1 and {MAX_DB_CONNECTIONS}"
        )),
    }
}

impl Settings {
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
            "empty value uses default"
        );
        assert_eq!(period("5").unwrap(), Some(Duration::from_secs(5)));
        assert_eq!(period("86400").unwrap(), Some(Duration::from_secs(86_400)));
        assert_eq!(period("0").unwrap(), None, "0 disables periodic reread");
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
