//! Database check for `bot doctor`: validates URL syntax by default and tests connectivity only with `--check-db`.
//! Never prints the database URL (which may contain credentials) or raw connection errors.

use super::{Item, State};
use crate::settings::Settings;
use bot_store::{Config, Store};
use std::time::Duration;

const SETTING: &str = "Database";
const CONNECTION: &str = "Database connection";
const CONNECT_LIMIT: Duration = Duration::from_secs(5);

/// Classifies the URL target as local or remote without returning the hostname.
fn target(url: &str) -> Result<&'static str, &'static str> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or("URL must start with postgres:// or postgresql://")?;
    let (head, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = head.split('/').next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(inner) => inner.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    let local = |name: &str| {
        name.is_empty()
            || name.starts_with('/')
            || matches!(name, "localhost" | "127.0.0.1" | "::1")
    };
    // Query `host=` / `hostaddr=` parameters override the authority host in libpq/sqlx.
    let redirected = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| matches!(key, "host" | "hostaddr") && !local(value));
    Ok(if local(host) && !redirected {
        "local"
    } else {
        "remote"
    })
}

pub async fn check(settings: &Settings, check_db: bool, items: &mut Vec<Item>) {
    let Some(url) = settings.database_url.as_deref() else {
        items.push(
            Item::new(State::Fail, SETTING, "BOT_DATABASE_URL is not set").with([
                "A PostgreSQL URL (postgres://user@host/db) is required for serve, chat, and intake"
                    .to_owned(),
            ]),
        );
        items.push(Item::skipped(CONNECTION, "skipped because URL is missing"));
        return;
    };
    // Parse with the same validator as `Store::connect`, emitting fixed text so URL fragments never leak.
    let kind = match target(url) {
        Ok(kind) if Store::check_url(url) => kind,
        _ => {
            items.push(
                Item::new(State::Fail, SETTING, "BOT_DATABASE_URL has invalid format").with([
                    "Expected postgres://user@host:port/db (URL is not printed)".to_owned(),
                ]),
            );
            items.push(Item::skipped(
                CONNECTION,
                "skipped because URL format is invalid",
            ));
            return;
        }
    };
    items.push(Item::new(
        State::Pass,
        SETTING,
        format!("URL configured (valid format, target: {kind}, URL not printed)"),
    ));
    if !check_db {
        items.push(Item::skipped(
            CONNECTION,
            "not checked — pass `--check-db` for a brief connectivity check (no writes or migrations)",
        ));
        return;
    }
    // Discard raw connection error text (may contain credentials); timeout indicates unreachable DB.
    let attempt = tokio::time::timeout(CONNECT_LIMIT, Store::connect(url, Config::default())).await;
    let hint = "Check the URL, credentials, and network (raw error omitted to avoid leaking URL)"
        .to_owned();
    items.push(match attempt {
        Ok(Ok(store)) => {
            store.pool().close().await;
            Item::new(
                State::Pass,
                CONNECTION,
                "connected (closed immediately — no writes or migrations)",
            )
        }
        Ok(Err(_)) => Item::new(State::Fail, CONNECTION, "failed to connect").with([hint]),
        Err(_) => Item::new(
            State::Fail,
            CONNECTION,
            format!("timed out after {}s", CONNECT_LIMIT.as_secs()),
        )
        .with([hint]),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_target_without_returning_a_host() {
        for local in [
            "postgres://u:pw@localhost:5432/db",
            "postgresql://127.0.0.1/db",
            "postgres://u@[::1]:5432/db",
            "postgres:///db?host=/var/run/postgresql",
        ] {
            assert_eq!(target(local), Ok("local"), "{local}");
        }
        for remote in [
            "postgres://u:pw@db.internal.example:5432/db",
            "postgres:///db?host=db.internal.example",
            "postgres://u@localhost/db?hostaddr=10.0.0.5",
        ] {
            assert_eq!(target(remote), Ok("remote"), "{remote}");
        }
        for bad in ["mysql://u@h/db", "localhost", ""] {
            assert!(target(bad).is_err(), "{bad}");
        }
    }
}
