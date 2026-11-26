//! 보류 판정은 발생 ID가 가리키는 정확한 대상에만 적용한다. 구판의 같은 호출 ID(도구·회신 발송)도 종류로 갈린다.

#[path = "hold_targets/final_result.rs"]
mod final_result;
mod support;

use bot_core::{
    admin::{AdminAct, AdminRead, HoldDecision, HoldKind, HoldRef, HoldVerdict},
    id::*,
    ledger::{FinishCall, Outcome},
    port::{Error, RunAdmin},
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
    db.sql("update bot_runs set state = 'waiting', wait_kind = 'hold', finished_ms = null")
        .await;
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

async fn held_steps(db: &TestDb, upgrade: bool, pending: bool) -> Attempt {
    use bot_core::{
        command::{EffectRequest, JsonPayload},
        state::EffectState,
    };
    let at = leased(db, "steps").await.attempt;
    begin_call(db, &at, "c1").await;
    calls(db).start(start_call(&at, "c1", None)).await.unwrap();
    for name in if pending {
        &["first", "next"][..]
    } else {
        &["first"][..]
    } {
        let p = effects(db)
            .request(EffectRequest {
                attempt: at.clone(),
                call: call("c1"),
                step: (*name).into(),
                args: JsonPayload("{}".into()),
            })
            .await
            .unwrap();
        if *name == "first" {
            let mut status = p.status;
            status.state = EffectState::Confirmed;
            status.receipt = Some("step-receipt".into());
            effects(db)
                .record(at.clone(), call("c1"), status)
                .await
                .unwrap();
        }
    }
    if upgrade {
        // 0024 당시의 호출 트리거로 종결한다. 0025가 기존 고립 행을 실제로 보정해야 한다.
        let sql = include_str!("../migrations/0011_holds.sql");
        let func = sql
            .split("create function bot_calls_hold()")
            .nth(1)
            .unwrap()
            .split("create trigger bot_calls_hold")
            .next()
            .unwrap();
        db.sql(&format!(
            "create or replace function bot_calls_hold(){func}"
        ))
        .await;
        db.sql("drop trigger if exists bot_effects_call_hold on bot_effects")
            .await;
        db.sql("delete from _sqlx_migrations where version=25")
            .await;
    }
    calls(db)
        .finish(FinishCall {
            attempt: at.clone(),
            call: call("c1"),
            outcome: Outcome::Held("response_unknown".into()),
        })
        .await
        .unwrap();
    at
}

#[tokio::test]
async fn upgrading_an_orphaned_held_call_opens_one_final_decision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = held_steps(&db, true, false).await;
    assert_eq!(db.scalar::<i64>("select count(*) from bot_holds").await, 0);
    db.store.migrate().await.unwrap();
    let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), None).await;
    db.store.migrate().await.unwrap();
    assert_eq!(db.scalar::<i64>("select count(*) from bot_holds").await, 1);
    assert_eq!(confirm(&db, target).await.unwrap(), HoldDecision::Applied);
}

#[tokio::test]
async fn upgrading_terminal_runs_closes_old_tool_holds_without_opening_new_ones() {
    for state in ["cancelled", "dead", "succeeded"] {
        let Some(db) = TestDb::new().await else {
            return;
        };
        let at = held_steps(&db, true, true).await;
        // 구판의 미실행 단계에는 보류 ID가 열려 있고, 호출 전체에는 보류 ID가 없다.
        db.sql("update bot_effects set resolution = 'not_executed' where state = 'held'")
            .await;
        let finished = db
            .scalar::<i64>(&format!(
                "update bot_runs set state = '{state}', wait_kind = null, finished_ms = bot_now_ms() returning finished_ms"
            ))
            .await;
        assert_eq!(
            db.scalar::<i64>(
                "select count(*) from bot_holds where effect is not null and closed_ms is null"
            )
            .await,
            1
        );

        db.store.migrate().await.unwrap();

        assert_eq!(state_of_run(&db, &at.run).await, state);
        assert_eq!(
            db.scalar::<i64>(
                "select count(*) from bot_holds where kind = 'tool' and effect is null"
            )
            .await,
            0,
            "{state}"
        );
        assert_eq!(
            db.scalar::<Option<i64>>("select closed_ms from bot_holds where effect is not null")
                .await,
            Some(finished),
            "{state}"
        );
        assert_eq!(
            db.scalar::<String>("select receipt from bot_effects where step = 'first'")
                .await,
            "step-receipt",
            "{state}"
        );
        assert!(db.store.holds(&operator(), None).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn upgrading_preserves_the_recorded_verdict_of_a_terminal_call() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    held_steps(&db, true, false).await;
    db.sql("update bot_calls set resolution = 'not_executed'")
        .await;
    db.sql("update bot_runs set state = 'cancelled', wait_kind = null, finished_ms = 42")
        .await;

    db.store.migrate().await.unwrap();

    assert_eq!(
        db.scalar::<Option<String>>("select resolution from bot_calls")
            .await
            .as_deref(),
        Some("not_executed")
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_holds").await, 0);
}

#[tokio::test]
async fn upgrading_retracts_a_legacy_whole_not_executed_mark_and_parks_the_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = held_steps(&db, true, true).await;
    // 구판 관리 판정의 커밋 결과를 재현한다. 마지막 단계만 미실행이고 첫 단계에는 이미 영수증이 있다.
    db.sql("update bot_holds set closed_ms=bot_now_ms(), operator='local:owner', verdict='not_executed', evidence_sha256=decode(repeat('00',32),'hex') where effect is not null").await;
    db.sql("update bot_effects set resolution='not_executed' where state='held'")
        .await;
    db.sql("update bot_calls set resolution='not_executed'")
        .await;
    db.sql("update bot_runs set state='ready', wait_kind=null, keep_generation=true")
        .await;
    assert_eq!(state_of_run(&db, &at.run).await, "ready");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds where closed_ms is null")
            .await,
        0
    );
    db.store.migrate().await.unwrap();
    assert_eq!(state_of_run(&db, &at.run).await, "waiting");
    let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), None).await;
    let view = db
        .store
        .holds(&operator(), None)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        view.steps
            .iter()
            .find(|s| s.step == "first")
            .unwrap()
            .receipt
            .as_deref(),
        Some("step-receipt")
    );
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_holds where operator='local:owner' and verdict='not_executed'"
        )
        .await,
        1
    );
    assert_eq!(confirm(&db, target).await.unwrap(), HoldDecision::Applied);
    assert_eq!(state_of_run(&db, &at.run).await, "ready");
}

#[tokio::test]
async fn settling_the_last_started_step_opens_the_call_hold() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = held_steps(&db, false, true).await;
    // 구판과 섞인 보류에서 단계가 아직 started인 상태를 재현한다.
    db.sql("update bot_effects set state = 'started' where step = 'next'")
        .await;
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds where closed_ms is null")
            .await,
        0
    );
    db.sql(
        "update bot_effects set state = 'confirmed', receipt = 'late-receipt' where step = 'next'",
    )
    .await;
    let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), None).await;
    assert_eq!(confirm(&db, target).await.unwrap(), HoldDecision::Applied);
}

#[tokio::test]
async fn cancelling_a_held_run_closes_its_tool_holds_and_keeps_receipts() {
    for pending in [false, true] {
        let Some(db) = TestDb::new().await else {
            return;
        };
        let at = held_steps(&db, false, pending).await;
        let mut views = db.store.holds(&operator(), None).await.unwrap();
        let view = views.pop().unwrap();
        let step = view
            .steps
            .iter()
            .find(|s| s.step == "next")
            .map(|s| &s.effect);
        let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), step).await;
        db.store
            .cancel(
                at.profile.clone(),
                at.run.clone(),
                operator().principal,
                "종료".into(),
            )
            .await
            .unwrap();

        assert_eq!(state_of_run(&db, &at.run).await, "cancelled");
        assert!(db.store.holds(&operator(), None).await.unwrap().is_empty());
        assert_eq!(
            db.scalar::<i64>("select count(*) from bot_holds where closed_ms is null")
                .await,
            0
        );
        assert_eq!(
            db.scalar::<String>("select receipt from bot_effects where step = 'first'")
                .await,
            "step-receipt"
        );
        assert!(matches!(confirm(&db, target).await, Err(Error::Conflict)));
    }
}

#[tokio::test]
async fn a_legacy_admin_cannot_commit_a_whole_call_as_not_executed_after_a_confirmed_step() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = held_steps(&db, false, true).await;
    let mut old = db.store.pool().begin().await.unwrap();
    // 구판 관리자가 마지막 단계와 발생을 판정한 뒤 호출 전체까지 미실행으로 쓴다.
    sqlx::query("update bot_holds set closed_ms=bot_now_ms() where effect is not null")
        .execute(&mut *old)
        .await
        .unwrap();
    sqlx::query("update bot_effects set resolution='not_executed' where state='held'")
        .execute(&mut *old)
        .await
        .unwrap();
    let error = sqlx::query("update bot_calls set resolution='not_executed'")
        .execute(&mut *old)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    old.rollback().await.unwrap();

    let view = db
        .store
        .holds(&operator(), None)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let step = view.steps.iter().find(|s| s.step == "next").unwrap();
    let step = hold_ref(
        &db,
        HoldKind::Tool,
        &at.run,
        &call("c1"),
        Some(&step.effect),
    )
    .await;
    let verdict = HoldVerdict::NotExecuted {
        evidence: "수신처 기록 없음".into(),
    };
    assert_eq!(
        db.store
            .resolve_hold(operator().principal, step, verdict)
            .await
            .unwrap(),
        HoldDecision::Applied
    );
    assert_eq!(state_of_run(&db, &at.run).await, "waiting");
    let target = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), None).await;
    assert_eq!(confirm(&db, target).await.unwrap(), HoldDecision::Applied);
}

#[tokio::test]
async fn a_mixed_call_exposes_its_final_decision_only_after_its_last_pending_step() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = held_steps(&db, false, true).await;
    let h = db
        .store
        .holds(&operator(), None)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(h.hold_id.is_none());
    let last = h.steps.iter().find(|s| s.step == "next").unwrap();
    let step = hold_ref(
        &db,
        HoldKind::Tool,
        &at.run,
        &call("c1"),
        Some(&last.effect),
    )
    .await;
    let verdict = HoldVerdict::NotExecuted {
        evidence: "수신처에 다음 단계 기록 없음".into(),
    };
    assert_eq!(
        db.store
            .resolve_hold(operator().principal, step.clone(), verdict.clone())
            .await
            .unwrap(),
        HoldDecision::Applied
    );
    assert_eq!(state_of_run(&db, &at.run).await, "waiting");
    let whole = hold_ref(&db, HoldKind::Tool, &at.run, &call("c1"), None).await;
    assert_ne!(whole.hold_id, step.hold_id);
    assert_eq!(
        db.store
            .resolve_hold(operator().principal, step, verdict)
            .await
            .unwrap(),
        HoldDecision::Repeated
    );
    assert_eq!(confirm(&db, whole).await.unwrap(), HoldDecision::Applied);
    assert_eq!(state_of_run(&db, &at.run).await, "ready");
}
