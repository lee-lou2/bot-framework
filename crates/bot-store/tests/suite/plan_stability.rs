//! Prepared-statement plan stability across online DDL (PG 0A000).
//!
//! `select *` fails with `cached plan must not change result type` on a reused connection after
//! `ALTER TABLE ADD COLUMN`; store queries select explicit column lists (`src/cols.rs`) and stay valid.

use crate::support::*;
use bot_core::{
    admin::{AdminRead, Page, RunQuery, Viewer},
    id::*,
    ledger::CallLedger,
    port::RunStore,
};
use bot_store::Store;
use sqlx::postgres::PgPoolOptions;

/// Single-connection pool to force prepared statement cache reuse on the same PG backend.
async fn single_connection(db: &TestDb) -> sqlx::PgPool {
    let options = (*db.store.pool().connect_options()).clone();
    PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap()
}

const TABLES: [&str; 9] = [
    "bot_runs",
    "bot_calls",
    "bot_approvals",
    "bot_effects",
    "bot_deliveries",
    "bot_memories",
    "bot_user_schedules",
    "bot_schedule_segments",
    "bot_audit",
];

#[tokio::test]
async fn control_a_star_select_is_refused_on_the_same_connection_after_a_column_is_added() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let pool = single_connection(&db).await;
    sqlx::query("select * from bot_runs limit 1")
        .fetch_all(&pool)
        .await
        .unwrap();
    db.sql("alter table bot_runs add column zz_probe int").await;
    let refused = sqlx::query("select * from bot_runs limit 1")
        .fetch_all(&pool)
        .await
        .expect_err("cached `select *` on the same connection must fail after DDL");
    let code = match &refused {
        sqlx::Error::Database(error) => error.code().map(|c| c.into_owned()),
        _ => None,
    };
    assert_eq!(code.as_deref(), Some("0A000"), "{refused}");
}

#[tokio::test]
async fn store_queries_keep_working_on_the_same_connection_after_columns_are_added() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let one = Store::from_pool(single_connection(&db).await, db.store.config().clone());
    runs(&db)
        .accept(accept("personal", "r1", "hi"))
        .await
        .unwrap();
    let first = RunStore::claim(&one).await.unwrap().expect("run to claim");
    CallLedger::begin(
        &one,
        first.attempt.clone(),
        call("c1"),
        tool("web.search"),
        [1; 32],
    )
    .await
    .unwrap();
    calls(&db)
        .open_approval(open_approval(binding(&first.attempt, "c1", "n1")))
        .await
        .unwrap();
    CallLedger::approval(&one, ApprovalId("n1".into()))
        .await
        .unwrap();
    let admin = Viewer {
        principal: PrincipalId("local:admin".into()),
        operator: true,
    };
    let listed = AdminRead::runs(
        &one,
        &admin,
        RunQuery {
            page: Page {
                limit: 10,
                after: None,
            },
            ..RunQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(listed.items.len(), 1);
    AdminRead::detail(
        &one,
        &admin,
        first.attempt.profile.clone(),
        first.attempt.run.clone(),
    )
    .await
    .unwrap();

    for table in TABLES {
        db.sql(&format!("alter table {table} add column zz_probe int"))
            .await;
    }

    runs(&db)
        .accept(accept("personal", "r2", "hi"))
        .await
        .unwrap();
    let second = RunStore::claim(&one)
        .await
        .expect("claim must keep working after column additions")
        .expect("run to claim");
    CallLedger::begin(
        &one,
        second.attempt.clone(),
        call("c2"),
        tool("web.search"),
        [2; 32],
    )
    .await
    .expect("begin must keep working after column additions");
    CallLedger::approval(&one, ApprovalId("n1".into()))
        .await
        .expect("approval read must keep working after column additions");
    let listed_after = AdminRead::runs(
        &one,
        &admin,
        RunQuery {
            page: Page {
                limit: 10,
                after: None,
            },
            ..RunQuery::default()
        },
    )
    .await
    .expect("AdminRead::runs must succeed without 0A000 after column additions");
    assert_eq!(listed_after.items.len(), 2);
    AdminRead::detail(
        &one,
        &admin,
        second.attempt.profile.clone(),
        second.attempt.run.clone(),
    )
    .await
    .expect("AdminRead::detail must succeed without 0A000 after column additions");
}
