//! End-to-end `bot/pass` candidate chain behavior on real BrokerService and PostgreSQL.

use crate::support::{pass::*, *};
use bot_broker::ports::Ran;
use bot_core::{
    command::*,
    id::*,
    ledger::{FinishCall, Outcome},
    pass::{PassNext, direct_call_id},
    port::{Broker, Error},
};

#[tokio::test]
async fn passes_walk_the_candidates_once_each_and_the_default_runs_exactly_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let run = accept_chain(
        &db,
        "r1",
        chain(&["agenda.list", "agenda.find"], Some(default_target())),
    )
    .await;
    assert_eq!(
        run_state(&db, &run).await,
        ("ready".into(), Some(0), "agenda.list".into())
    );

    let first = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(first.work.pass_pos, Some(0));
    assert!(matches!(
        call_at(&rig, &first).await,
        Ok(CallResult::Passed)
    ));
    assert_eq!(
        run_state(&db, &run).await,
        ("ready".into(), Some(1), "agenda.find".into())
    );

    let late = calls(&db)
        .finish_pass(
            FinishCall {
                attempt: first.attempt.clone(),
                call: direct_call_id(&run, 0),
                outcome: Outcome::Done(JsonPayload(PLAIN.into())),
            },
            0,
            Default::default(),
        )
        .await;
    assert!(matches!(late, Err(Error::Stale)), "{late:?}");
    assert!(matches!(call_at(&rig, &first).await, Err(Error::Stale)));

    let second = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(
        (second.work.pass_pos, &second.work.target),
        (Some(1), &tool_target("agenda.find"))
    );
    assert!(matches!(
        call_at(&rig, &second).await,
        Ok(CallResult::Passed)
    ));
    assert_eq!(
        run_state(&db, &run).await,
        ("ready".into(), Some(2), "(agent)".into())
    );

    let last = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(
        (last.work.pass_pos, &last.work.target),
        (Some(2), &default_target())
    );
    assert_eq!(rig.runner.jobs().len(), 2, "each candidate runs once");
    runs(&db).finish(last.attempt, done("hello")).await.unwrap();
    assert_eq!(run_state(&db, &run).await.0, "succeeded");
    assert!(
        runs(&db).claim().await.unwrap().is_none(),
        "no more candidates should remain"
    );

    let actions = audit_actions(&db).await;
    let count = |name: &str| actions.iter().filter(|a| *a == name).count();
    assert_eq!(
        (
            count("trigger.select"),
            count("call.pass"),
            count("trigger.next")
        ),
        (1, 2, 2)
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_calls where state = 'done'")
            .await,
        2
    );
}

#[tokio::test]
async fn a_requester_without_the_run_scope_never_gets_the_default_conversation() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let tool_only = scopes(&["calendar.read"]);
    let denied = accept_chain_scoped(
        &db,
        "no-run",
        chain(&["agenda.list"], Some(default_target())),
        tool_only,
    )
    .await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    assert_eq!(run_state(&db, &denied).await.0, "succeeded");
    assert!(runs(&db).claim().await.unwrap().is_none());
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 0);
    let actions = audit_actions(&db).await;
    assert!(actions.iter().any(|a| a == "trigger.next"));
    let label: String = sqlx::query_scalar(
        "select verdict from bot_audit where action = 'trigger.next' and target = $1",
    )
    .bind(&denied.0)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(label, "default_denied@1");

    let allowed = accept_chain(
        &db,
        "with-run",
        chain(&["agenda.list"], Some(default_target())),
    )
    .await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    assert_eq!(
        run_state(&db, &allowed).await,
        ("ready".into(), Some(1), "(agent)".into())
    );
    let last = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(last.work.target, default_target());
}

#[tokio::test]
async fn a_run_scope_revoked_after_accept_stops_the_default_conversation_at_pass_time() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let revoked = accept_chain(&db, "r1", chain(&["agenda.list"], Some(default_target()))).await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    // Revoke `run` in the newly active revision while keeping the tool scope.
    activate(&db, &rig, world("r2", &["calendar.read"])).await;
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    assert_eq!(run_state(&db, &revoked).await.0, "succeeded");
    assert!(
        runs(&db).claim().await.unwrap().is_none(),
        "default conversation must not run"
    );
    let label: String = sqlx::query_scalar(
        "select verdict from bot_audit where action = 'trigger.next' and target = $1",
    )
    .bind(&revoked.0)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(label, "default_denied@1");

    activate(&db, &rig, world("r3", &["run", "calendar.read"])).await;
    let kept = accept_chain_at(
        &db,
        "r2",
        chain(&["agenda.list"], Some(default_target())),
        run_scopes(),
        "r3",
    )
    .await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    activate(&db, &rig, world("r4", &["run", "calendar.read"])).await;
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    assert_eq!(
        run_state(&db, &kept).await,
        ("ready".into(), Some(1), "(agent)".into())
    );
}

#[tokio::test]
async fn an_ordinary_result_stops_the_chain_and_a_chain_without_a_default_ends_quietly() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PLAIN.into())));
    let run = accept_chain(
        &db,
        "r1",
        chain(&["agenda.list", "agenda.find"], Some(default_target())),
    )
    .await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Done(_))));
    assert_eq!(run_state(&db, &run).await.1, Some(0));
    runs(&db)
        .finish(ctx.attempt, done("handled"))
        .await
        .unwrap();
    assert!(runs(&db).claim().await.unwrap().is_none());
    assert_eq!(rig.runner.jobs().len(), 1);

    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let run = accept_chain(&db, "r2", chain(&["agenda.list"], None)).await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    assert_eq!(run_state(&db, &run).await.0, "succeeded");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 0);
    assert!(runs(&db).claim().await.unwrap().is_none());
}

#[tokio::test]
async fn invalid_markers_and_markers_outside_a_chain_never_start_another_candidate() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    for (i, bad) in [
        r#"{"_meta":{"bot/pass":true},"content":[{"text":"x","type":"text"}],"isError":false}"#,
        r#"{"_meta":{"bot/pass":true},"content":[],"isError":true}"#,
        r#"{"_meta":{"bot/pass":true},"content":[],"isError":false,"structuredContent":{}}"#,
        r#"{"_meta":{"bot/pass":"true"},"content":[],"isError":false}"#,
        r#"{"_meta":{"bot/pass":true,"bot/wake":{"at":"2999-01-01T00:00:00Z","instruction":"x"}},"content":[],"isError":false}"#,
        r#"{"_meta":{"bot/extra":true,"bot/pass":true},"content":[],"isError":false}"#,
    ]
    .into_iter()
    .enumerate()
    {
        rig.runner.script(Ran::Done(JsonPayload(bad.into())));
        let run = accept_chain(
            &db,
            &format!("bad{i}"),
            chain(&["agenda.list", "agenda.find"], Some(default_target())),
        )
        .await;
        let ctx = runs(&db).claim().await.unwrap().unwrap();
        assert!(matches!(call_at(&rig, &ctx).await, Err(Error::Denied)), "{bad}");
        let (_, pos, _) = run_state(&db, &run).await;
        assert_eq!(pos, Some(0), "{bad}");
        runs(&db)
            .fail(ctx.attempt, Failure::Permanent("denied"))
            .await
            .unwrap();
        assert!(runs(&db).claim().await.unwrap().is_none(), "{bad}");
    }
    assert!(
        audit_actions(&db)
            .await
            .iter()
            .any(|a| a == "call.pass_refused")
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_calls where state = 'done'")
            .await,
        0
    );

    // Outside a chain, `bot/pass` metadata is stripped instead of advancing.
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let mut plain = accept("personal", "plain", "hi");
    plain.target = tool_target("agenda.list");
    runs(&db).accept(plain).await.unwrap();
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(ctx.work.pass_pos, None);
    let result = rig
        .broker
        .call(ToolCall {
            attempt: ctx.attempt.clone(),
            call: direct_call_id(&ctx.attempt.run, 0),
            tool: ToolId("agenda.list".into()),
            args: JsonPayload("{}".into()),
        })
        .await;
    let Ok(CallResult::Done(payload)) = result else {
        panic!("{result:?}");
    };
    assert_eq!(payload.0, r#"{"content":[],"isError":false}"#);
}

#[tokio::test]
async fn effects_or_unverifiable_writes_cannot_be_passed() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    // Write tool without an effect journal cannot verify side effects, so passing holds the call.
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let run = accept_chain(
        &db,
        "w",
        chain(&["agenda.write", "agenda.find"], Some(default_target())),
    )
    .await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let held = call_at(&rig, &ctx).await;
    assert!(matches!(held, Ok(CallResult::Held(_))), "{held:?}");
    assert_eq!(run_state(&db, &run).await.1, Some(0));
    assert_eq!(rig.runner.jobs().len(), 1, "next candidate must not run");

    // Once an effect step is recorded, passing is refused under the same lock.
    let run = accept_chain(
        &db,
        "e",
        chain(&["agenda.post", "agenda.find"], Some(default_target())),
    )
    .await;
    runs(&db)
        .fail(ctx.attempt.clone(), Failure::Permanent("x"))
        .await
        .ok();
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(ctx.attempt.run, run);
    let call = direct_call_id(&run, 0);
    calls(&db)
        .begin(
            ctx.attempt.clone(),
            call.clone(),
            ToolId("agenda.post".into()),
            [1; 32],
        )
        .await
        .unwrap();
    calls(&db)
        .start(start_call(&ctx.attempt, &call.0, None))
        .await
        .unwrap();
    effects(&db)
        .request(EffectRequest {
            attempt: ctx.attempt.clone(),
            call: call.clone(),
            step: "post".into(),
            args: JsonPayload("{}".into()),
        })
        .await
        .unwrap();
    let refused = calls(&db)
        .finish_pass(
            FinishCall {
                attempt: ctx.attempt.clone(),
                call,
                outcome: Outcome::Done(JsonPayload(r#"{"content":[],"isError":false}"#.into())),
            },
            0,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(refused, PassNext::Refused("pass_after_effect"));
    assert_eq!(run_state(&db, &run).await.1, Some(0));
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_calls where run = '{}' and state = 'done'",
            run.0
        ))
        .await,
        0,
        "refused pass must write nothing"
    );
}

#[tokio::test]
async fn two_deliveries_of_the_same_pass_move_the_cursor_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let run = accept_chain(
        &db,
        "r1",
        chain(&["agenda.list", "agenda.find"], Some(default_target())),
    )
    .await;
    let again = accept_chain(&db, "r1", chain(&["agenda.find"], None)).await;
    assert_eq!(again, run);
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);

    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let finish = |outcome: &str| FinishCall {
        attempt: ctx.attempt.clone(),
        call: direct_call_id(&run, 0),
        outcome: Outcome::Done(JsonPayload(outcome.into())),
    };
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    let twice = calls(&db)
        .finish_pass(
            finish(r#"{"content":[],"isError":false}"#),
            0,
            Default::default(),
        )
        .await;
    assert!(matches!(twice, Err(Error::Stale)), "{twice:?}");
    assert_eq!(run_state(&db, &run).await.1, Some(1));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'call.pass'")
            .await,
        1
    );
}
