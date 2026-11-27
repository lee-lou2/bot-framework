//! Database pool setup, configuration, error mapping, and embedded SQL migrations (all timestamps use DB `bot_now_ms()`).

use bot_core::port::Error;
use sqlx::{
    PgPool, Postgres, Transaction,
    migrate::{Migration, MigrationType, Migrator},
    postgres::{PgPoolOptions, PgRow},
};
use std::{borrow::Cow, sync::Arc, time::Duration};

pub(crate) type Tx<'a> = Transaction<'a, Postgres>;
pub(crate) type R<T> = Result<T, Fail>;

/// Internal store error converted to `bot_core::port::Error` at port boundaries.
#[derive(Debug)]
pub enum Fail {
    Core(Error),
    Sql(sqlx::Error),
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Fail::Sql(e)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Core(e)
    }
}

impl Fail {
    /// Logs only the SQLSTATE (never queries or values) and maps database errors to `Error::Unavailable`.
    pub fn into_core(self) -> Error {
        match self {
            Fail::Core(e) => e,
            Fail::Sql(e) => {
                let code = match &e {
                    sqlx::Error::Database(d) => d.code().map(|c| c.into_owned()),
                    _ => None,
                };
                tracing::warn!(
                    sqlstate = code.as_deref().unwrap_or("-"),
                    "store database error"
                );
                Error::Unavailable
            }
        }
    }

    pub(crate) fn is_unique_violation(&self) -> bool {
        matches!(self, Fail::Sql(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23505"))
    }
}

/// Store runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Diagnostic worker label; not used for fencing decisions.
    pub worker: String,
    pub lease: Duration,
    /// Attempts before a failing or lease-expired run moves to dead-letter.
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Outbox claim lease duration; unbegun expiries reclaim, `sending` expiries hold.
    pub delivery_lease: Duration,
    /// Maximum PG pool connections (clamped to at least 1).
    pub max_connections: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            worker: "worker".into(),
            lease: Duration::from_secs(30),
            max_attempts: 5,
            backoff_base: Duration::from_secs(5),
            backoff_cap: Duration::from_secs(300),
            delivery_lease: Duration::from_secs(300),
            max_connections: 10,
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
    pub(crate) cfg: Arc<Config>,
    pub(crate) activation: Option<Arc<std::sync::RwLock<bot_core::id::Revision>>>,
}

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "runs", include_str!("../migrations/0001_runs.sql")),
    (2, "outbox", include_str!("../migrations/0002_outbox.sql")),
    (
        3,
        "schedule",
        include_str!("../migrations/0003_schedule.sql"),
    ),
    (4, "active", include_str!("../migrations/0004_active.sql")),
    (5, "ledger", include_str!("../migrations/0005_ledger.sql")),
    (6, "memory", include_str!("../migrations/0006_memory.sql")),
    (
        7,
        "http_private",
        include_str!("../migrations/0007_http_private.sql"),
    ),
    (
        8,
        "memory_share",
        include_str!("../migrations/0008_memory_share.sql"),
    ),
    (
        9,
        "memory_share_calls",
        include_str!("../migrations/0009_memory_share_calls.sql"),
    ),
    (
        10,
        "held_steps",
        include_str!("../migrations/0010_held_steps.sql"),
    ),
    (11, "holds", include_str!("../migrations/0011_holds.sql")),
    (
        12,
        "user_schedules",
        include_str!("../migrations/0012_user_schedules.sql"),
    ),
    (
        13,
        "run_origin",
        include_str!("../migrations/0013_run_origin.sql"),
    ),
    (
        14,
        "deliveries",
        include_str!("../migrations/0014_deliveries.sql"),
    ),
    (
        15,
        "claim_set",
        include_str!("../migrations/0015_claim_set.sql"),
    ),
    (16, "unsent", include_str!("../migrations/0016_unsent.sql")),
    (
        17,
        "profile_bundles",
        include_str!("../migrations/0017_profile_bundles.sql"),
    ),
    (
        18,
        "card_jobs",
        include_str!("../migrations/0018_card_jobs.sql"),
    ),
    (
        19,
        "api_runs",
        include_str!("../migrations/0019_api_runs.sql"),
    ),
    (
        20,
        "pass_and_threads",
        include_str!("../migrations/0020_pass_and_threads.sql"),
    ),
    (
        21,
        "preapproval",
        include_str!("../migrations/0021_preapproval.sql"),
    ),
    (
        22,
        "slack_events",
        include_str!("../migrations/0022_slack_events.sql"),
    ),
    (
        23,
        "private pass",
        include_str!("../migrations/0023_private_pass.sql"),
    ),
    (
        24,
        "queue order",
        include_str!("../migrations/0024_queue_order.sql"),
    ),
    (
        25,
        "call final hold",
        include_str!("../migrations/0025_call_final_hold.sql"),
    ),
    (
        26,
        "reply transport",
        include_str!("../migrations/0026_reply_transport.sql"),
    ),
    (
        27,
        "run command receipt",
        include_str!("../migrations/0027_run_command_receipt.sql"),
    ),
    (
        28,
        "hot path indexes",
        include_str!("../migrations/0028_hot_path_indexes.sql"),
    ),
    (
        29,
        "memory hybrid",
        include_str!("../migrations/0029_memory_hybrid.sql"),
    ),
];

fn migrator() -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            MIGRATIONS
                .iter()
                .map(|(version, name, sql)| {
                    Migration::new(
                        *version,
                        Cow::Borrowed(*name),
                        MigrationType::Simple,
                        Cow::Borrowed(*sql),
                        false,
                    )
                })
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

impl Store {
    /// Builds pool options with an `after_release` ping so dropped transactions finish ROLLBACK before releasing row locks.
    pub fn pool_options_with(max_connections: u32) -> PgPoolOptions {
        PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .after_release(|conn, _| {
                Box::pin(async move {
                    use sqlx::Connection;
                    Ok(conn.ping().await.is_ok())
                })
            })
    }

    pub fn pool_options() -> PgPoolOptions {
        Self::pool_options_with(10)
    }

    pub async fn connect(url: &str, cfg: Config) -> R<Store> {
        let pool = Store::pool_options_with(cfg.max_connections)
            .connect(url)
            .await?;
        Ok(Store::from_pool(pool, cfg))
    }

    /// Validates connection URL syntax without connecting, suppressing tracing so sqlx never logs unknown parameter values (e.g. `sslpassword`).
    pub fn check_url(url: &str) -> bool {
        use std::str::FromStr;
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            sqlx::postgres::PgConnectOptions::from_str(url).is_ok()
        })
    }

    pub fn from_pool(pool: PgPool, cfg: Config) -> Store {
        Store {
            pool,
            cfg: Arc::new(cfg),
            activation: None,
        }
    }

    /// Runs checksum-verified migrations serialized across concurrent processes.
    pub async fn migrate(&self) -> R<()> {
        migrator().run(&self.pool).await.map_err(|e| {
            tracing::warn!("store migration failed: {e}");
            Fail::Core(Error::Unavailable)
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub(crate) async fn now_ms(&self) -> R<i64> {
        Ok(sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&self.pool)
            .await?)
    }
}

pub(crate) fn ms_to_time(ms: i64) -> std::time::SystemTime {
    let base = std::time::UNIX_EPOCH;
    if ms >= 0 {
        base + Duration::from_millis(ms as u64)
    } else {
        base - Duration::from_millis(ms.unsigned_abs())
    }
}

pub(crate) fn time_to_ms(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

pub(crate) fn text(row: &PgRow, col: &str) -> String {
    use sqlx::Row;
    row.get::<String, _>(col)
}
