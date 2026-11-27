//! 넘김(`bot/pass`, 처리하지 않은 이벤트 넘기기)을 실제 BrokerService·PG 원장·가짜 도구 실행기로 확인한다: PASS는 호출 결론·커서 전진·감사가 한 커밋이고, 후보는 한 번씩만 방문하며,
//! 모든 후보가 넘기면 기본 대화가 정확히 한 번 불리고, 일반 결과·무효 표식·쓰기 흔적·옛 시도는 다음 실행을 만들지 않는다. 강제 종료·동시 전달은 `pass_recovery.rs`.

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

    // 옛 시도는 더 쓸 수 없다(리스·세대가 지났다): 같은 호출의 늦은 보고도 커서를 되돌리거나 다시 넘기지 못한다
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

    // 모든 후보가 넘겼다: 기본 대화(에이전트)가 한 번 불린다. 후보 도구는 더 부르지 않았다
    let last = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(
        (last.work.pass_pos, &last.work.target),
        (Some(2), &default_target())
    );
    assert_eq!(rig.runner.jobs().len(), 2, "후보마다 한 번씩만 실행했다");
    runs(&db).finish(last.attempt, done("hello")).await.unwrap();
    assert_eq!(run_state(&db, &run).await.0, "succeeded");
    assert!(
        runs(&db).claim().await.unwrap().is_none(),
        "넘길 일이 더 없다"
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

/// 접수는 첫 후보(도구)만 보고 `run` 스코프를 요구하지 않는다: 도구만 허용된 요청자는 마지막 후보가 넘겨도 기본 대화(에이전트)를 시작하지 못하고 run은 조용히 끝난다.
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
    // 기본 대화로 가지 않고 끝났다: 대기열에 오른 것이 없고 답(outbox)도 없다
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

    // 대조: `run`이 있는 요청자는 같은 사슬에서 기본 대화가 한 번 불린다
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

/// 접수 뒤에 새 판본이 활성이 돼 요청자의 역할에서 `run`만 철회됐다(도구 스코프는 그대로): 접수가 고정한 스코프에는 `run`이 남아 있어도
/// 마지막 후보가 넘길 때 **현재** 판본에서 다시 확인해 기본 대화를 부르지 않는다. `run`이 그대로인 새 판본이면 기본 대화가 불린다.
#[tokio::test]
async fn a_run_scope_revoked_after_accept_stops_the_default_conversation_at_pass_time() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    // 접수는 r1에 고정되고 요청자는 `run`을 가졌다(저장된 스코프에 `run`이 있다)
    let revoked = accept_chain(&db, "r1", chain(&["agenda.list"], Some(default_target()))).await;
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    // 그 뒤 활성이 된 r2는 `owner` 역할에서 `run`만 뺐다
    activate(&db, &rig, world("r2", &["calendar.read"])).await;
    assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    assert_eq!(run_state(&db, &revoked).await.0, "succeeded");
    assert!(runs(&db).claim().await.unwrap().is_none(), "기본 대화는 0");
    let label: String = sqlx::query_scalar(
        "select verdict from bot_audit where action = 'trigger.next' and target = $1",
    )
    .bind(&revoked.0)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(label, "default_denied@1");

    // 대조: 판본이 바뀌어도 `run`이 그대로면 기본 대화가 한 번 불린다(r3에 고정한 접수, 그 뒤 활성이 된 r4)
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
    // 일반 결과는 첫 일치에서 끝난다: 커서는 그대로이고 다음 후보·기본 대화는 부르지 않는다
    assert_eq!(run_state(&db, &run).await.1, Some(0));
    runs(&db)
        .finish(ctx.attempt, done("handled"))
        .await
        .unwrap();
    assert!(runs(&db).claim().await.unwrap().is_none());
    assert_eq!(rig.runner.jobs().len(), 1);

    // 기본 대화가 없는 사슬(반응)은 마지막 후보가 넘기면 답 없이 조용히 끝난다
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
        // 표식과 함께 온 다른 `_meta` 항목(실행기의 결과 파서가 남기는 표지)
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
        // 읽기 도구라 호출은 거절로 닫힌다. 커서는 그대로이고 다음 후보는 대기열에 오르지 않는다
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

    // 사슬이 없는 호출(모델·위임·HTTP·cron)의 표식은 해석하지 않고 떼어 낸다: 결과는 표식이 없던 것과 같다
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
    // 쓰기 등급인데 단계 원장이 없는 도구: 부작용을 가릴 수 없어 넘기지 않는다(보류)
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
    assert_eq!(rig.runner.jobs().len(), 1, "다음 후보를 부르지 않았다");

    // 단계 원장을 쓰는 도구가 쓰기 단계를 시작한 뒤에는 넘길 수 없다(저장소가 같은 잠금에서 거절한다)
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
        "거절은 아무것도 쓰지 않는다"
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
    // 같은 이벤트가 다시 접수돼도 run은 하나이고 사슬은 최초 것이다
    let again = accept_chain(&db, "r1", chain(&["agenda.find"], None)).await;
    assert_eq!(again, run);
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);

    let ctx = runs(&db).claim().await.unwrap().unwrap();
    // 같은 시도가 같은 넘김을 동시에 두 번 커밋하려 한다: 한 번만 앞으로 간다
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
