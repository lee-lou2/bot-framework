//! 연결·설정·오류 변환·마이그레이션. 시각은 DB의 `bot_now_ms()`만 쓴다.

use bot_core::port::Error;
use sqlx::{
    PgPool, Postgres, Transaction,
    migrate::{Migration, MigrationType, Migrator},
    postgres::{PgPoolOptions, PgRow},
};
use std::{borrow::Cow, sync::Arc, time::Duration};

pub(crate) type Tx<'a> = Transaction<'a, Postgres>;
pub(crate) type R<T> = Result<T, Fail>;

/// 저장소 내부 오류. 포트 경계에서 `bot_core::port::Error`로 바뀐다.
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
    /// DB 오류는 SQLSTATE 코드만 로그에 남기고 `Unavailable`로 바꾼다(문장·값은 남기지 않는다).
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

/// 저장소 설정. 기본값은 개인 비서 한 대 규모에 맞춘다.
#[derive(Clone, Debug)]
pub struct Config {
    /// 이 프로세스의 소유자 표지(진단용). 판정에는 쓰지 않는다.
    pub worker: String,
    pub lease: Duration,
    /// 실패·리스 만료로 이 횟수를 쓰면 dead letter가 된다.
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// 회신 발송 점유 시간. begin 전에 끝나면 재점유되고, sending에서 끝나면 보류된다.
    pub delivery_lease: Duration,
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
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
    pub(crate) cfg: Arc<Config>,
    pub(crate) activation: Option<bot_core::id::Revision>,
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
    /// 저장소가 쓰는 풀 설정. 반납되는 연결은 `ping`으로 마저 보내지 못한 ROLLBACK을 끝낸다 —
    /// 오류로 버려진 트랜잭션이 연결 안에서 행 잠금을 쥔 채 남지 않게 한다. 밖에서 풀을 만들 때도 이 설정을 쓴다.
    pub fn pool_options() -> PgPoolOptions {
        PgPoolOptions::new()
            .max_connections(10)
            .after_release(|conn, _| {
                Box::pin(async move {
                    use sqlx::Connection;
                    Ok(conn.ping().await.is_ok())
                })
            })
    }

    pub async fn connect(url: &str, cfg: Config) -> R<Store> {
        let pool = Store::pool_options().connect(url).await?;
        Ok(Store::from_pool(pool, cfg))
    }

    pub fn from_pool(pool: PgPool, cfg: Config) -> Store {
        Store {
            pool,
            cfg: Arc::new(cfg),
            activation: None,
        }
    }

    /// 마이그레이션은 검사합이 맞아야 하고, 같은 DB에 동시에 돌려도 한 번만 적용된다.
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

/// 한 열을 문자열로 읽는 줄임. 열 이름 오타는 시험이 잡는다.
pub(crate) fn text(row: &PgRow, col: &str) -> String {
    use sqlx::Row;
    row.get::<String, _>(col)
}
