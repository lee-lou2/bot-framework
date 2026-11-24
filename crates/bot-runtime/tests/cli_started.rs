//! 실제 `claude` CLI+PG+실제 프런트·브로커로 확인하는 «시작 뒤 실행 여부 불명»: 되돌릴 수 없는 쓰기가 실제 `call_start`로 PG에 `Started`로 기록된 뒤
//! 결과를 남기기 전에 워커가 사라졌다. 재개한 새 워커는 그 호출을 **다시 실행하지 않고**(재실행 0), 확정 거절 표식도 남기지 않으며(표식 0),
//! 호출을 `Held`(운영자 판정 대기)로 두고 run을 멈춘다(저장소 시험 `a_held_call_parks_the_run_and_waits_for_an_operator_verdict`의 규약).
//! - 운영자가 «실행 안 됨»으로 판정하면 같은 호출 ID를 현재 권한·**새 승인**으로 한 번만 실행한다(처음 승인은 시작 때 소비됐다).
//! - 운영자가 «실행됨»(영수증)으로 판정하면 실행 없이 그 영수증으로 호출을 끝내고 CLI가 그 결과를 본다.
//!
//! 판정은 실제 운영자 경로(`AdminRead::holds`·`AdminAct::resolve_hold`)로 한다. 워커의 소멸은 도구 실행을 붙잡아 둔 채 그 실행 future를 버려 흉내 낸다
//! (실행 기록은 `Started`로 남고 결과는 남지 않는다). 모델 호출은 0이다.
//!
//! 실행: `BOT_CLAUDE_BIN=<claude 경로> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test cli_started`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_core::{
    admin::{AdminAct, AdminRead, HoldDecision, HoldRef, HoldVerdict, Viewer},
    id::*,
    port::Broker,
};
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

const ADMIN: &str = "local:admin";

fn operator() -> Viewer {
    Viewer {
        principal: PrincipalId(ADMIN.into()),
        operator: true,
    }
}

/// 보류 목록에서 이 run의 열린 호출 보류를 판정 대상으로 만든다.
async fn hold_of(w: &World, run: &RunId) -> HoldRef {
    let holds = AdminRead::holds(&*w.pg.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1, "{holds:?}");
    let hold = &holds[0];
    assert_eq!((&hold.run, hold.tool.0.as_str()), (run, "agenda.delete"));
    assert_eq!(hold.reason, "response_unknown");
    HoldRef {
        hold_id: hold.hold_id.clone().expect("호출 보류는 발생 ID가 있다"),
        profile: hold.profile.clone(),
        run: hold.run.clone(),
        call: hold.call.clone(),
        effect: None,
    }
}

async fn resolve(w: &World, run: &RunId, verdict: HoldVerdict) {
    let target = hold_of(w, run).await;
    let decided = AdminAct::resolve_hold(&*w.pg.store, PrincipalId(ADMIN.into()), target, verdict)
        .await
        .unwrap();
    assert_eq!(decided, HoldDecision::Applied);
    // 운영자 판정이 run을 깨운다
    until("운영자 판정이 run을 깨운다", async || {
        w.pg.state_of(run).await == "ready"
    })
    .await;
}

/// 한 워커를 새 "프로세스"로 세워 run 하나를 한 번 돌린다(멈추거나 끝날 때까지).
async fn run_on(s: &Stranded, worker: &str, tag: &str) -> Option<Outcome> {
    let host = host(
        &s.w,
        s.w.pg.peer(worker).await,
        s.upstream.clone(),
        s.sandbox.clone(),
        &workspace(tag),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("실행이 끝나거나 멈춰야 한다")
}

struct Stranded {
    w: World,
    run: RunId,
    upstream: Arc<FakeUpstream>,
    sandbox: Arc<LocalSandbox>,
}

/// 삭제 승인 → 둘째 워커가 `call_start`로 `Started`를 기록한 뒤 결과를 남기기 전에 사라짐 → 셋째 워커의 재개로 `Held`. 공통 사실을 확인하고 돌려준다.
async fn stranded() -> Option<Stranded> {
    let w = world().await?;
    let sandbox = LocalSandbox::new(claude_bin()?);
    let upstream = Arc::new(FakeUpstream::new());
    // 첫 응답은 되돌릴 수 없는 삭제를 부르고, 재개된 CLI의 다음 요청에는 마무리 글을 준다
    script(
        &upstream,
        tool_turn(
            "msg_s1",
            "toolu_del",
            "mcp__relay__agenda_delete",
            r#"{"event":"e1"}"#,
        ),
    );
    script(&upstream, final_turn("msg_s2", "삭제를 마쳤어요"));
    let run = w.accept("req-started", "산책 일정을 삭제해줘").await;
    let s = Stranded {
        w,
        run: run.clone(),
        upstream,
        sandbox,
    };

    // 1. 첫 워커: 삭제 승인을 기다리며 멈춘다. 소유자가 승인한다
    let outcome = run_on(&s, "worker-1", "st1").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    s.w.pg.assert_state(&run, "waiting").await;
    assert!(s.w.runner.jobs().is_empty(), "승인 전에는 도구 실행 0");
    let nonce =
        s.w.pg
            .scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    s.w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();

    // 2. 둘째 워커: 승인을 소비하고 실제 `call_start`로 PG에 `Started`를 기록한 뒤, 도구가 돌아오기 전(결과를 남기기 전)에 사라진다
    let _held_tool = s.w.runner.hold_next();
    let second = host(
        &s.w,
        s.w.pg.peer("worker-2").await,
        s.upstream.clone(),
        s.sandbox.clone(),
        &workspace("st2"),
    )
    .await;
    tokio::select! {
        outcome = second.worker.run_once() => panic!("도구가 돌아오지 않았는데 워커가 끝났다: {outcome:?}"),
        () = until("도구 실행이 시작된다", async || s.w.runner.jobs().len() == 1) => {}
    }
    drop(second);
    assert_eq!(
        s.w.pg.scalar::<String>("select state from bot_calls").await,
        "started"
    );
    assert_eq!(
        s.w.pg.scalar::<String>("select state from bot_runs").await,
        "leased"
    );
    assert_eq!(
        s.w.pg.scalar::<i32>("select attempts from bot_runs").await,
        0
    );
    assert!(marks(&s.w, &run).await.is_empty());

    // 3. 리스가 끝나 새 워커가 이어받는다: 시작은 했지만 결과를 모르는 쓰기라 다시 실행하지 않고 운영자 판정을 기다린다
    s.w.pg
        .sql("update bot_runs set not_before_ms = 0, lease_until_ms = case when state = 'leased' then 0 else lease_until_ms end")
        .await;
    let outcome = run_on(&s, "worker-3", "st3").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    // 시도로 세는 것은 리스를 잃은 옛 시도 한 번뿐이다(보류로 멈추는 것은 시도 횟수를 쓰지 않는다)
    assert_eq!(
        s.w.pg.scalar::<i32>("select attempts from bot_runs").await,
        1
    );
    Some(s)
}

#[tokio::test]
async fn a_write_that_started_but_never_reported_is_held_for_the_operator_and_never_rerun_or_marked()
 {
    let Some(s) = stranded().await else {
        return;
    };
    let (w, run) = (&s.w, &s.run);
    // 호출은 `Held`로, run은 보류 대기로 멈췄다. 시도 횟수는 쓰지 않고 시작한 세대·소유 표지는 남는다
    w.pg.assert_state(run, "waiting").await;
    assert_eq!(
        w.pg.scalar::<String>("select wait_kind from bot_runs")
            .await,
        "hold"
    );
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "held"
    );
    let owner = w.pg.scalar::<String>("select owner from bot_calls").await;
    assert!(owner.starts_with(&format!("{}.", run.0)), "{owner}");
    let started =
        w.pg.scalar::<i64>("select started_generation from bot_calls")
            .await;
    assert!(owner.contains(&format!(".{started}.")), "{owner}");
    // 표식 0, 재실행 0: 도구는 시작한 한 번뿐이고 옛 프로세스 종료만 확인했다
    assert!(
        marks(w, run).await.is_empty(),
        "시작한 호출은 거절로 닫지 않는다"
    );
    assert_eq!(w.runner.jobs().len(), 1, "도구는 다시 실행되지 않았다");
    assert_eq!(w.runner.reaped().len(), 1, "{:?}", w.runner.reaped());
    // 재개한 워커들은 CLI를 띄우기 전에 멈췄다: 모델 상류는 첫 응답 하나만 봤다
    assert_eq!(bodies(&s.upstream).len(), 1);
    // 운영자의 보류 목록에 그 호출이 한 건 올라 있다
    hold_of(w, run).await;
    // 이미 결정이 없는 보류는 다시 깨워도 다시 대기한다(자동으로 풀지 않는다)
    w.pg.sql("update bot_runs set state = 'ready', wait_kind = null, keep_generation = true")
        .await;
    let outcome = run_on(&s, "worker-4", "st4").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    w.pg.assert_state(run, "waiting").await;
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "held"
    );
    assert_eq!(
        w.pg.scalar::<i32>("select attempts from bot_runs").await,
        1,
        "다시 멈춰도 시도를 더 쓰지 않는다"
    );
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(marks(w, run).await.is_empty());
}

#[tokio::test]
async fn an_operator_verdict_of_not_executed_runs_the_call_exactly_once_more_after_a_fresh_approval()
 {
    let Some(s) = stranded().await else {
        return;
    };
    let (w, run) = (&s.w, &s.run);
    resolve(
        w,
        run,
        HoldVerdict::NotExecuted {
            evidence: "수신처에 삭제 흔적이 없음을 확인".into(),
        },
    )
    .await;
    // 재개: 같은 호출 ID를 현재 권한·승인으로 다시 판정한다. 처음 승인은 시작 때 소비됐으니 새 승인이 열리고 그 전에는 실행하지 않는다
    let outcome = run_on(&s, "worker-4", "st4").await;
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    w.pg.assert_state(run, "waiting").await;
    assert_eq!(
        w.pg.scalar::<String>("select wait_kind from bot_runs")
            .await,
        "approval"
    );
    assert_eq!(w.runner.jobs().len(), 1, "새 승인 전에는 실행하지 않는다");
    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    // 새 승인으로 한 번만 실행하고 CLI가 그 결과를 본다
    let outcome = run_on(&s, "worker-5", "st5").await;
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 2, "시작했던 한 번과 판정 뒤 한 번");
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "done"
    );
    assert!(marks(w, run).await.is_empty());
    let seen = bodies(&s.upstream);
    assert_eq!(seen.len(), 2);
    let deleted = result_text(&seen[1], "toolu_del").expect("삭제 결과");
    assert!(deleted.contains("산책"), "{deleted}");
    assert_eq!(result_error(&seen[1], "toolu_del"), Some(false));
}

#[tokio::test]
async fn an_operator_verdict_of_executed_completes_the_call_from_the_receipt_without_running_it_again()
 {
    let Some(s) = stranded().await else {
        return;
    };
    let (w, run) = (&s.w, &s.run);
    resolve(
        w,
        run,
        HoldVerdict::Confirmed {
            receipt: "evt-7".into(),
        },
    )
    .await;
    // 재개: 실행하지 않고 영수증으로 호출을 끝내 CLI가 그 결과를 본다
    let outcome = run_on(&s, "worker-4", "st4").await;
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 1, "다시 실행하지 않았다");
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_calls").await,
        "done"
    );
    assert!(marks(w, run).await.is_empty());
    let seen = bodies(&s.upstream);
    assert_eq!(seen.len(), 2);
    let receipt = result_text(&seen[1], "toolu_del").expect("영수증 결과");
    assert!(receipt.contains("evt-7"), "{receipt}");
    assert_eq!(result_error(&seen[1], "toolu_del"), Some(false));
}
