//! Hold resolutions target the exact `HoldId` occurrence; legacy colliding tool/reply call IDs stay separated by kind.

#[path = "hold_targets/final_result.rs"]
mod final_result;

use crate::support::{admin::*, *};
use bot_core::{
    admin::{AdminAct, AdminRead, HoldDecision, HoldKind, HoldRef, HoldVerdict},
    id::*,
    ledger::{FinishCall, Outcome},
    port::{Error, RunAdmin},
    state::ConversationKind,
};

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
    // Simulate upgrading a pre-0011 DB containing a tool call ID with the reserved `private:` prefix.
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

    // Colliding tool and reply call IDs produce distinct holds that resolve independently.
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
        "reply resolution must not write a receipt to bot_calls"
    );
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_holds where closed_ms is null and kind = 'tool'"
        )
        .await,
        1,
        "tool hold must remain open"
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
    rewind_holds(&db).await;
    db.sql("update bot_runs set state = 'waiting', wait_kind = 'hold', finished_ms = null")
        .await;
    db.sql("update bot_outbox set state = 'held'").await;
    db.sql("update bot_calls set state = 'held', call = (select call from bot_outbox)")
        .await;
    db.store
        .migrate()
        .await
        .expect("distinct kinds must migrate without conflict");
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
        // Restore the pre-0025 call trigger so migration 0025 repairs existing orphaned rows.
        let sql = include_str!("../../migrations/0011_holds.sql");
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
    // Reproduce a legacy admin verdict that marked the whole call not_executed despite a confirmed first step.
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
                "stop".into(),
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
    // Simulate a legacy admin trying to mark the whole call not_executed after resolving the last step.
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
        evidence: "no record at destination".into(),
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
        evidence: "no record of next step at destination".into(),
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
