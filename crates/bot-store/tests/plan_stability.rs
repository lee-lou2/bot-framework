//! 흔들림의 원인과 고침을 같은 연결에서 결정적으로 재현한다. 준비된 문은 연결마다 캐시되고 결과 형식이 열 순서까지 포함하므로, `select *`는 마이그레이션이 열을
//! 더하거나(새 판본과 옛 프로세스가 같은 DB를 쓰는 동안) 열을 지웠다 다시 만들고(`run_origin` 시험) 난 뒤 **같은 연결에서** 다시 쓰이면 PostgreSQL이
//! `cached plan must not change result type`(0A000)으로 거절한다. 풀이 연결을 돌려 쓰는 병렬 실행에서만 가끔 터져 흔들림으로 보였다.
//! 저장소 질의는 열을 명시한다(`src/cols.rs`)이니 같은 DDL 뒤에도 같은 연결에서 그대로 돈다.

mod support;

use bot_core::{id::*, ledger::CallLedger, port::RunStore};
use bot_store::Store;
use sqlx::postgres::PgPoolOptions;
use support::*;

/// 같은 시험 스키마를 연결 하나로만 쓰는 풀(문 캐시가 반드시 같은 연결에서 다시 쓰인다).
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
        .expect_err("같은 연결에서 캐시된 `select *`는 열이 바뀐 뒤 거절된다");
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
    // 연결 하나로 claim(`bot_runs`)·호출 시작(`bot_calls`)·승인 조회(`bot_approvals`)를 해 준비문을 캐시에 올린다
    runs(&db)
        .accept(accept("personal", "r1", "hi"))
        .await
        .unwrap();
    let first = RunStore::claim(&one).await.unwrap().expect("집을 run");
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

    // 새 판본의 마이그레이션이 모든 표에 열을 더한 상황(옛 프로세스의 연결은 그대로 산다)
    for table in TABLES {
        db.sql(&format!("alter table {table} add column zz_probe int"))
            .await;
    }

    // 같은 연결에서 같은 질의가 그대로 돈다
    runs(&db)
        .accept(accept("personal", "r2", "hi"))
        .await
        .unwrap();
    let second = RunStore::claim(&one)
        .await
        .expect("열이 바뀐 뒤에도 claim이 된다")
        .expect("집을 run");
    CallLedger::begin(
        &one,
        second.attempt.clone(),
        call("c2"),
        tool("web.search"),
        [2; 32],
    )
    .await
    .expect("열이 바뀐 뒤에도 호출을 시작한다");
    CallLedger::approval(&one, ApprovalId("n1".into()))
        .await
        .expect("열이 바뀐 뒤에도 승인을 읽는다");
}
