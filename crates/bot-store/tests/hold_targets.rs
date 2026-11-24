//! 보류 판정은 발생 ID가 가리키는 정확한 대상에만 적용한다. 구판의 같은 호출 ID(도구·회신 발송)도 종류로 갈린다.

mod support;

use bot_core::{
    admin::{AdminAct, AdminRead, HoldDecision, HoldKind, HoldRef, HoldVerdict},
    id::*,
    ledger::{FinishCall, Outcome},
    port::Error,
    state::ConversationKind,
};
use support::{admin::*, *};

async fn confirm(db: &TestDb, target: HoldRef) -> Result<HoldDecision, Error> {
    db.store
        .resolve_hold(
            operator().principal,
            target,
            HoldVerdict::Confirmed {
                receipt: "receipt-1".into(),
            },
        )
        .await
}

#[tokio::test]
async fn a_legacy_tool_with_a_private_shaped_id_resolves_as_the_listed_tool() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "legacy-tool").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    calls(&db)
        .finish(FinishCall {
            attempt: run.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    // 예약 접두사를 도입하기 전에 저장된 호출을 복원한다(발생 ID가 없던 판에서 올라오는 상황).
    rewind_holds(&db).await;
    db.sql("update bot_calls set call = 'private:' || run")
        .await;
    db.store.migrate().await.unwrap();
    let holds = db.store.holds(&operator(), None).await.unwrap();
    assert_eq!(holds.len(), 1);
    let held = &holds[0];
    assert_eq!(held.kind, HoldKind::Tool);
    let target = hold_ref(&db, HoldKind::Tool, &held.run, &held.call, None).await;
    confirm(&db, target).await.unwrap();
    assert_eq!(state_of_run(&db, &held.run).await, "ready");
    assert_eq!(
        db.scalar::<String>("select receipt from bot_calls").await,
        "receipt-1"
    );
}

#[tokio::test]
async fn a_receipt_is_applied_only_to_the_stored_target_of_that_hold_id() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased_in(&db, "reply", ConversationKind::Direct).await;
    begin_call(&db, &run.attempt, "c1").await;
    runs(&db)
        .finish(run.attempt.clone(), done("answer"))
        .await
        .unwrap();
    db.sql("update bot_outbox set state = 'held'").await;
    let reply = db.scalar::<String>("select call from bot_outbox").await;
    let run = &run.attempt.run;
    let target = hold_ref(&db, HoldKind::Reply, run, &call(&reply), None).await;

    // 발생 ID가 있어도 대상이 다르면(같은 순번을 가리켜도 저장된 호출 ID와 다르다) 아무것도 쓰지 않는다.
    let mut wrong = target.clone();
    wrong.call = call(&format!("{reply}0"));
    assert!(matches!(confirm(&db, wrong).await, Err(Error::Conflict)));
    let mut unknown = target.clone();
    unknown.hold_id = HoldId("hold-none".into());
    assert!(matches!(confirm(&db, unknown).await, Err(Error::Missing)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'hold.resolve'")
            .await,
        0
    );

    // 옛 데이터에서 도구와 회신 호출 ID가 겹치면 발생이 둘이다: 한쪽 판정은 다른 쪽을 건드리지 않는다.
    db.sql("update bot_calls set state = 'held', call = (select call from bot_outbox)")
        .await;
    let tool_target = hold_ref(&db, HoldKind::Tool, run, &call(&reply), None).await;
    assert_ne!(tool_target.hold_id, target.hold_id);
    confirm(&db, target).await.unwrap();
    assert_eq!(
        db.scalar::<String>("select receipt from bot_outbox").await,
        "receipt-1"
    );
    assert!(
        db.scalar::<bool>("select receipt is null from bot_calls")
            .await,
        "회신 판정이 도구 호출에 영수증을 쓰면 안 된다"
    );
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_holds where closed_ms is null and kind = 'tool'"
        )
        .await,
        1,
        "도구 발생은 계속 열려 있다"
    );
    confirm(&db, tool_target).await.unwrap();
    assert_eq!(
        db.scalar::<String>("select receipt from bot_calls").await,
        "receipt-1"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'hold.resolve'")
            .await,
        2
    );
}

#[tokio::test]
async fn upgrading_a_db_with_both_kinds_held_under_one_call_id_opens_two_holds_that_close_independently()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased_in(&db, "both", ConversationKind::Direct).await;
    begin_call(&db, &run.attempt, "c1").await;
    runs(&db)
        .finish(run.attempt.clone(), done("answer"))
        .await
        .unwrap();
    let reply = db.scalar::<String>("select call from bot_outbox").await;
    // 구판 모양: 같은 호출 ID의 도구 호출과 회신 발송이 둘 다 보류이고 발생 ID는 없다
    rewind_holds(&db).await;
    db.sql("update bot_outbox set state = 'held'").await;
    db.sql("update bot_calls set state = 'held', call = (select call from bot_outbox)")
        .await;
    db.store
        .migrate()
        .await
        .expect("종류가 달라 충돌 없이 올라간다");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds where closed_ms is null")
            .await,
        2
    );
    let kinds: Vec<(String, String)> =
        sqlx::query_as("select kind, call from bot_holds order by kind")
            .fetch_all(db.store.pool())
            .await
            .unwrap();
    assert_eq!(
        kinds,
        [
            ("reply".into(), reply.clone()),
            ("tool".into(), reply.clone())
        ]
    );

    // 한쪽 상태가 바뀌어도 다른 종류의 발생은 닫히지 않는다
    db.sql("update bot_outbox set state = 'confirmed', receipt = 'late'")
        .await;
    assert_eq!(
        db.scalar::<String>("select kind from bot_holds where closed_ms is null")
            .await,
        "tool"
    );
    db.sql("update bot_calls set state = 'open'").await;
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds where closed_ms is null")
            .await,
        0
    );
}
