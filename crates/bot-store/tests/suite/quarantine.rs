//! 해독 오류는 행 단위로 격리하며, DB 장애는 현재 트랜잭션을 되돌린다.

use crate::support::*;
use bot_core::{command::Delegate, id::*, port::*};
use serde_json::{Value, json};
use std::time::Duration;

async fn set_target(db: &TestDb, run: &RunId, target: &Value) {
    sqlx::query(
        "update bot_runs set target = $2, not_before_ms = 1 where profile = 'personal' and id = $1",
    )
    .bind(&run.0)
    .bind(target)
    .execute(db.store.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn an_undecodable_target_is_quarantined_and_the_next_run_claims() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let bad = runs(&db)
        .accept(accept("personal", "broken", "원래 입력"))
        .await
        .unwrap();
    let raw = json!({"kind": "agent", "agent": "main", "legacy": "원래 대상"});
    set_target(&db, &bad, &raw).await;
    let healthy = runs(&db)
        .accept(accept("work", "healthy", "작업"))
        .await
        .unwrap();

    let got = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(got.attempt.run, healthy);
    let (state, reason, attempts, target, input): (String, String, i32, Value, String) = sqlx::query_as(
        "select state, error, attempts, target, input from bot_runs where profile = 'personal' and id = $1",
    )
    .bind(&bad.0)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        (state.as_str(), reason.as_str(), attempts),
        ("dead", "invalid_target", 0)
    );
    assert_eq!(target, raw);
    assert_eq!(
        serde_json::from_str::<Value>(&input).unwrap()["text"],
        "원래 입력"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_audit where action = 'run.dead' and verdict = 'invalid_target'").await, 1);

    let resume = || {
        db.store.resume(
            profile_id(),
            bad.clone(),
            PrincipalId("operator".into()),
            "점검".into(),
        )
    };
    assert!(matches!(resume().await, Err(Error::Invalid)));
    assert_eq!(state_of_run(&db, &bad).await, "dead");
    set_target(
        &db,
        &bad,
        &json!({"kind": "agent", "agent": "main", "instruction": "수정"}),
    )
    .await;
    resume().await.unwrap();
    assert_eq!(runs(&db).claim().await.unwrap().unwrap().attempt.run, bad);
}

#[tokio::test]
async fn an_undecodable_child_waits_for_its_parent_lock_without_blocking_other_runs() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let parent = leased(&db, "parent").await.attempt;
    let child = db
        .store
        .delegate(Delegate {
            parent: parent.clone(),
            step: "child".into(),
            child: AgentId("main/child".into()),
            instruction: "조사".into(),
            scopes: scopes(&["run"]),
            checked_revision: Revision("r1".into()),
        })
        .await
        .unwrap();
    set_target(
        &db,
        &child,
        &json!({"kind": "tool", "agent": "main/child", "tool": "fixture.lookup", "args": [1]}),
    )
    .await;
    let healthy = runs(&db)
        .accept(accept("work", "independent", "작업"))
        .await
        .unwrap();
    let mut tx = db.store.pool().begin().await.unwrap();
    sqlx::query("select 1 from bot_runs where profile = 'personal' and id = $1 for update")
        .bind(&parent.run.0)
        .fetch_one(&mut *tx)
        .await
        .unwrap();

    let got = tokio::time::timeout(Duration::from_secs(8), runs(&db).claim())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got.attempt.run, healthy);
    assert_eq!(state_of_run(&db, &child).await, "ready");
    tx.commit().await.unwrap();

    let got = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(got.attempt.run, parent.run);
    assert_eq!(state_of_run(&db, &child).await, "dead");
    assert_eq!(got.work.steps.len(), 1);
    assert_eq!(got.work.steps[0].key, "child");
    assert_eq!(
        serde_json::from_str::<Value>(&got.work.steps[0].body.0).unwrap(),
        json!({"state":"dead","error":"invalid_target"})
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_database_error_during_claim_leaves_the_run_ready() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = runs(&db)
        .accept(accept("personal", "unavailable", "작업"))
        .await
        .unwrap();
    db.sql("alter table bot_steps rename to test_broken_steps")
        .await;

    assert!(matches!(runs(&db).claim().await, Err(Error::Unavailable)));
    assert!(
        db.scalar::<bool>(
            "select state = 'ready' and attempts = 0 and claim_epoch = 0 from bot_runs"
        )
        .await
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.dead'")
            .await,
        0
    );
    db.sql("alter table test_broken_steps rename to bot_steps")
        .await;
    assert_eq!(runs(&db).claim().await.unwrap().unwrap().attempt.run, run);
}
