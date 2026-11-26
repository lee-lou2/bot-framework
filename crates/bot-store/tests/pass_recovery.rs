//! F3 넘김의 복구·경합: 커서 커밋 전 강제 종료와 인계, 같은 접수·같은 넘김의 동시 전달, 후보가 아닌 호출의 표식, 이미 움직인 커서.
//! 모두 «호출·답은 한 번, 커서는 한 칸»을 지킨다. 도구·설정은 `support/pass.rs`.

mod support;

use bot_broker::ports::Ran;
use bot_core::{
    command::*,
    id::*,
    ledger::{FinishCall, Outcome},
    pass::direct_call_id,
    port::{Broker, Error},
    slack::{SlackSource, SourceKind},
    state::ConversationKind,
};
use support::{pass::*, *};

/// 커서를 커밋하기 전에 시도가 죽었다(도구가 도는 중에 사라짐): 인계받은 다음 시도는 **같은 후보**에서 다시 시작하고(커서는 그대로),
/// 죽은 시도의 늦은 넘김은 막힌다. 후보 호출 자리는 하나라 다음 후보·기본 대화는 불리지 않는다.
#[tokio::test]
async fn a_crash_before_the_cursor_commit_resumes_at_the_same_candidate_and_the_late_pass_is_fenced()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    let run = accept_chain(
        &db,
        "r1",
        chain(&["agenda.list", "agenda.find"], Some(default_target())),
    )
    .await;
    let dead = runs(&db).claim().await.unwrap().unwrap();
    // 첫 시도의 도구가 도는 중에 프로세스가 사라진다: 실행이 시작된 것을 본 뒤 호출 future를 버린다
    let gate = rig.runner.hold_next();
    let mut inflight = Box::pin(call_at(&rig, &dead));
    let started = async {
        while rig.runner.jobs().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        done = &mut inflight => panic!("붙잡힌 실행이 끝났다: {done:?}"),
        () = started => {}
    }
    drop(inflight);
    drop(gate);

    let alive = db.takeover(&run).await;
    assert_eq!(
        (alive.work.pass_pos, &alive.work.target),
        (Some(0), &tool_target("agenda.list")),
        "커서는 그대로다"
    );
    assert!(alive.attempt.generation.0 > dead.attempt.generation.0);

    // 죽은 시도가 뒤늦게 넘기려 해도 막힌다 — 커서·감사 변화 0
    let late = calls(&db)
        .finish_pass(
            FinishCall {
                attempt: dead.attempt.clone(),
                call: direct_call_id(&run, 0),
                outcome: Outcome::Done(JsonPayload(PLAIN.into())),
            },
            0,
            Default::default(),
        )
        .await;
    assert!(matches!(late, Err(Error::Stale)), "{late:?}");
    assert_eq!(run_state(&db, &run).await.1, Some(0));
    let pass_audits =
        "select count(*) from bot_audit where action in ('call.pass', 'trigger.next')";
    assert_eq!(db.scalar::<i64>(pass_audits).await, 0);

    // 인계 시도는 같은 후보를 읽기로 다시 돌려 넘기고, 커서는 한 칸만 간다(기본 대화는 아직 아니다)
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    assert!(matches!(
        call_at(&rig, &alive).await,
        Ok(CallResult::Passed)
    ));
    assert_eq!(
        run_state(&db, &run).await,
        ("ready".into(), Some(1), "agenda.find".into())
    );
    let actions = audit_actions(&db).await;
    let count = |name: &str| actions.iter().filter(|a| *a == name).count();
    assert_eq!((count("call.pass"), count("trigger.next")), (1, 1));
    assert_eq!(
        rig.runner.jobs().len(),
        2,
        "죽은 시도의 실행과 인계 시도의 재실행"
    );
}

/// 같은 접수가 동시에 두 번 들어오고(재전달) 같은 시도가 같은 넘김을 동시에 두 번 보낸다: run 하나, 넘김 한 번, 커서 한 칸.
#[tokio::test]
async fn concurrent_duplicate_deliveries_pass_once_and_run_the_default_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let make = || chain(&["agenda.list"], Some(default_target()));
    let (a, b) = tokio::join!(
        accept_chain(&db, "r1", make()),
        accept_chain(&db, "r1", make())
    );
    assert_eq!(a, b, "동시에 온 같은 접수는 한 run이다");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);

    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let (x, y) = tokio::join!(call_at(&rig, &ctx), call_at(&rig, &ctx));
    let passed = [&x, &y]
        .iter()
        .filter(|r| matches!(r, Ok(CallResult::Passed)))
        .count();
    assert_eq!(passed, 1, "{x:?} / {y:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'call.pass'")
            .await,
        1
    );
    assert_eq!(
        run_state(&db, &run_of(&db).await).await,
        ("ready".into(), Some(1), "(agent)".into())
    );

    // 기본 대화는 한 번만 집힌다
    let default = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(default.work.target, default_target());
    assert!(
        runs(&db).claim().await.unwrap().is_none(),
        "기본 대화를 두 번 집을 수 없다"
    );
}

async fn run_of(db: &TestDb) -> RunId {
    RunId(
        sqlx::query_scalar("select id from bot_runs limit 1")
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
    )
}

/// 사슬이 있는 run이라도 현재 후보의 직접 호출 ID가 아닌 호출의 표식은 해석하지 않는다: 떼어 내기만 하고 커서·감사는 그대로다.
#[tokio::test]
async fn a_marker_on_a_call_that_is_not_the_current_candidate_is_only_stripped() {
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
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let other = rig
        .broker
        .call(ToolCall {
            attempt: ctx.attempt.clone(),
            call: CallId(format!("{}:not-the-candidate", run.0)),
            tool: ToolId("agenda.list".into()),
            args: JsonPayload("{}".into()),
        })
        .await;
    let Ok(CallResult::Done(JsonPayload(body))) = other else {
        panic!("일반 결과여야 한다: {other:?}");
    };
    assert!(
        !body.contains("bot/pass"),
        "표식은 결과에 남지 않는다: {body}"
    );
    assert_eq!(
        run_state(&db, &run).await,
        ("leased".into(), Some(0), "agenda.list".into()),
        "커서·대상은 그대로(시도는 계속 리스를 쥔다)"
    );
    let pass_audits =
        "select count(*) from bot_audit where action in ('call.pass', 'trigger.next')";
    assert_eq!(db.scalar::<i64>(pass_audits).await, 0);
}

/// 도구가 도는 사이에 커서가 이미 움직였다면 같은 위치의 넘김은 거절된다(앞으로 두 칸 가지 않는다).
#[tokio::test]
async fn a_pass_for_a_cursor_that_already_moved_is_refused() {
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
    let ctx = runs(&db).claim().await.unwrap().unwrap();
    let gate = rig.runner.hold_next();
    let moved = async {
        while rig.runner.jobs().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        db.sql(&format!(
            "update bot_runs set pass_pos = 1 where id = '{}'",
            run.0
        ))
        .await;
        gate.add_permits(1);
    };
    let (result, ()) = tokio::join!(call_at(&rig, &ctx), moved);
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert_eq!(
        run_state(&db, &run).await.1,
        Some(1),
        "커서는 더 가지 않았다"
    );
    let pass_audits =
        "select count(*) from bot_audit where action in ('call.pass', 'trigger.next')";
    assert_eq!(db.scalar::<i64>(pass_audits).await, 0);
}

/// 반응은 후보가 모두 넘기면 기본 에이전트가 고정 형식의 사건 입력을 **정확히 한 번** 받는다: 같은 이벤트가 (바인딩이 바뀐 채) 다시 와도 새 run도 새 기본 호출도 없다.
#[tokio::test]
async fn a_reaction_chain_that_passes_everywhere_calls_the_default_agent_once_even_when_redelivered()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let rig = rig(&db);
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    rig.runner.script(Ran::Done(JsonPayload(PASS.into())));
    let input = r#"{"event":"reaction_added","item_ts":"100.1","reaction":"white_check_mark"}"#;
    let default = Target::Agent {
        agent: AgentId("main".into()),
        instruction: input.into(),
    };
    let built = chain(&["agenda.list", "agenda.find"], Some(default.clone()));
    let source = SlackSource {
        kind: SourceKind::Reaction,
        team: "T1".into(),
        channel: "C1".into(),
        ts: "100.1".into(),
        thread_ts: None,
        event_id: "Ev-R1".into(),
        reaction: Some("white_check_mark".into()),
    };
    let deliver = |binding: &str| {
        let mut command = accept_as(
            "personal",
            "Ev-R1",
            "local:owner",
            ConversationKind::Channel,
            "C1",
            input,
        );
        command.target = built.candidates[0].target.clone();
        command.event.binding = BindingId(binding.into());
        let extras = AcceptExtras {
            source: Some(source.clone()),
            chain: Some(built.clone()),
        };
        (command, extras)
    };
    let (command, extras) = deliver("desk");
    let run = runs(&db).accept_with(command, extras).await.unwrap();

    for _ in 0..2 {
        let ctx = runs(&db).claim().await.unwrap().unwrap();
        assert!(matches!(call_at(&rig, &ctx).await, Ok(CallResult::Passed)));
    }
    // 마지막 후보까지 넘겼다: 기본 에이전트가 고정 형식 입력으로 한 번 불린다
    let last = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(last.work.target, default);
    runs(&db).finish(last.attempt, done("ok")).await.unwrap();
    assert_eq!(run_state(&db, &run).await.0, "succeeded");

    // 같은 이벤트의 재전달(바인딩 이름이 바뀐 뒤): 같은 run이고, 새 접수·새 기본 호출이 없다
    let (command, extras) = deliver("desk2");
    assert_eq!(runs(&db).accept_with(command, extras).await.unwrap(), run);
    assert!(runs(&db).claim().await.unwrap().is_none());
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(rig.runner.jobs().len(), 2, "후보마다 한 번씩만 실행했다");
    let actions = audit_actions(&db).await;
    let count = |name: &str| actions.iter().filter(|a| *a == name).count();
    assert_eq!((count("run.accept"), count("trigger.select")), (1, 1));
}
