//! 실제 `claude` CLI+PG+실제 프런트(PlannedCalls·사전 판정·CheckpointSink)로 확인하는 CLI 확정 거절.
//! - 확정: 고정 판본에 없는 도구(`unknown_tool`)와 고정 스키마가 거절하는 입력(`invalid`)은 PG 표식을 쓴 **뒤에만** 그 단어(오류 표시)를 돌려주고,
//!   재개는 브로커에 다시 묻지 않고 같은 단어로 그 자리를 닫으며 어떤 도구도 실행하지 않는다.
//! - 미확정: 고정 판본을 확인하지 못함·claim 상실은 표식을 남기지 않고, 그 시도는 성공이 아니라 재시도이며 재개가 다시 판단한다.
//! - 시작 뒤 실행 여부 불명(`call_start`로 `Started`를 기록한 뒤 결과를 남기기 전에 워커가 사라진 쓰기)은 실제 PG 행으로 `cli_started.rs`가 확인한다:
//!   표식 0·재실행 0이고 그 호출은 `Held`(운영자 판정 대기)로 run이 멈춘다.
//!
//! 입력 검사는 실제 `JcsSealer`+`SchemaValidator`(시험용 봉인기가 아니다)가 한다.
//!
//! 실행: `BOT_CLAUDE_BIN=<claude 경로> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test cli_refusal`

mod support;

use bot_broker::{
    seal::JcsSealer,
    service::{BrokerService, Config as BrokerConfig, Parts, preflight::Preflight},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer},
};
use bot_core::{
    cli::CliRefusal,
    command::JsonPayload,
    id::*,
    port::{Broker, Reply},
    profile::ActiveRevisions,
};
use bot_profile::SchemaValidator;
use bot_runtime::Outcome;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

/// 실제 스키마 검증을 하는 세계.
async fn strict_world() -> Option<World> {
    world_with(Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new())))).await
}

/// 접수한 뒤(r1에 고정) 프로필이 r2로 올라갔다: 목록은 현재 판본(r2)이 정하고, 이 run은 r1로 판정한다.
async fn promote(w: &World) {
    w.catalog.publish(profile_next());
    w.pg.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            BTreeMap::from([(ProfileId("personal".into()), Revision("r2".into()))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
}

async fn audit_count(w: &World, action: &str) -> i64 {
    w.pg.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = '{action}'"
    ))
    .await
}

fn json_word(refusal: CliRefusal) -> String {
    serde_json::to_string(refusal.word()).unwrap()
}

/// 그 `tool_use`의 결과가 정확히 이 단어이고 오류 표시다.
fn assert_word(body: &serde_json::Value, id: &str, word: &str) {
    assert_eq!(
        (result_text(body, id), result_error(body, id)),
        (Some(word.to_owned()), Some(true)),
        "{id}"
    );
}

/// 판본을 확인하지 못하는 판정자: 실행이 고정한 판본(r1)이 이 카탈로그에 없다.
fn blind_judge(w: &World) -> Arc<dyn Preflight> {
    let store = w.pg.store.clone();
    Arc::new(BrokerService::new(
        Parts {
            catalog: Arc::new(FakeCatalog::new()),
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store,
            runner: Arc::new(FakeRunner::new()),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        BrokerConfig::default(),
    ))
}

/// 판정하는 사이에 다른 워커가 이 run을 이어받는다(claim 상실): 판정은 `Stale`이다.
struct LosesTheClaim {
    inner: Arc<dyn Preflight>,
    pool: sqlx::PgPool,
}

impl Preflight for LosesTheClaim {
    fn refusal(
        &self,
        attempt: Attempt,
        tool: ToolId,
        args: JsonPayload,
    ) -> Reply<'_, Option<CliRefusal>> {
        Box::pin(async move {
            sqlx::query("update bot_runs set claim_epoch = claim_epoch + 1 where id = $1")
                .bind(&attempt.run.0)
                .execute(&self.pool)
                .await
                .unwrap();
            self.inner.refusal(attempt, tool, args).await
        })
    }
}

#[tokio::test]
async fn a_tool_missing_from_the_pinned_revision_and_a_schema_violation_are_stored_then_read_back_on_resume_without_running_anything()
 {
    let Some(w) = strict_world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    // 한 응답이 (고정 판본에 없는 도구, 고정 스키마가 거절하는 입력, 승인이 필요한 삭제)를 함께 부른다
    script(
        &upstream,
        tools_turn(
            "msg_r1",
            &[
                ("toolu_extra", "mcp__relay__agenda_extra", "{}"),
                (
                    "toolu_move",
                    "mcp__relay__agenda_move",
                    r#"{"event":"Bad Id"}"#,
                ),
                (
                    "toolu_del",
                    "mcp__relay__agenda_delete",
                    r#"{"event":"e1"}"#,
                ),
            ],
        ),
    );
    script(
        &upstream,
        final_turn("msg_r2", "거절된 것은 건너뛰고 삭제했어요"),
    );
    let run = w.accept("req-refusal", "일정을 정리해줘").await;
    promote(&w).await;

    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("r1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("첫 실행이 끝나야 한다");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "삭제 승인을 기다리며 멈춘다: {outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    // 확정 거절 둘은 호출 ID별 표식(고정 낱말 JSON 문자열)으로 저장돼 있고, 어떤 도구도 실행되지 않았다
    let stored = marks(&w, &run).await;
    assert_eq!(stored.len(), 2, "{stored:?}");
    let mut words: Vec<&str> = stored.iter().map(|(_, body)| body.as_str()).collect();
    words.sort();
    let mut want = vec![
        json_word(CliRefusal::UnknownTool),
        json_word(CliRefusal::Invalid),
    ];
    want.sort();
    assert_eq!(words, want, "{stored:?}");
    assert!(
        stored
            .iter()
            .all(|(key, _)| key.starts_with("cli-refusal-cli-")),
        "{stored:?}"
    );
    assert!(
        w.runner.jobs().is_empty(),
        "거절된 호출도 승인 대기 중인 삭제도 실행되지 않았다"
    );
    // 브로커가 입력 검사 실패를 감사한 횟수: 재개가 표식으로 닫으면 다시 판단하지 않아 늘지 않는다
    let judged = audit_count(&w, "call.invalid").await;
    assert_eq!(judged, 1);

    // 소유자가 삭제를 승인하고 프로세스를 통째로 바꾼다
    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace("r2"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("재개한 실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(
        audit_count(&w, "call.invalid").await,
        judged,
        "재개는 거절된 자리를 브로커에 다시 묻지 않고 표식으로 닫는다"
    );

    // 재개한 CLI는 거절된 두 자리를 정확히 같은 단어(오류 표시)로, 삭제는 실제 결과로 본다. 실행은 삭제 한 번뿐이다
    let jobs: Vec<String> = w
        .runner
        .jobs()
        .iter()
        .map(|j| format!("{:?}", j.tool))
        .collect();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert!(jobs[0].contains("agenda.delete"), "{jobs:?}");
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    assert_word(&seen[1], "toolu_extra", "unknown_tool");
    assert_word(&seen[1], "toolu_move", "invalid");
    let deleted = result_text(&seen[1], "toolu_del").expect("삭제 결과");
    assert!(deleted.contains("산책"), "{deleted}");
    assert_eq!(result_error(&seen[1], "toolu_del"), Some(false));
    // 표식은 그대로이고 새로 만들어지지 않았다
    assert_eq!(marks(&w, &run).await, stored);
}

/// 표식을 남기지 못하는 이유.
#[derive(Clone, Copy, Debug)]
enum Unmarked {
    /// 판정자가 고정 판본을 확인하지 못한다
    Blind,
    /// 판정하는 사이 다른 워커가 run을 이어받았다
    LostClaim,
}

async fn unmarked(cause: Unmarked) {
    let Some(w) = strict_world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    script(
        &upstream,
        tool_turn("msg_u1", "toolu_extra2", "mcp__relay__agenda_extra", "{}"),
    );
    for n in 2..6 {
        script(&upstream, final_turn(&format!("msg_u{n}"), "마무리했어요"));
    }
    let run = w.accept("req-unmarked", "일정을 정리해줘").await;
    promote(&w).await;

    let tag = format!("{cause:?}");
    let judge: Arc<dyn Preflight> = match cause {
        Unmarked::Blind => blind_judge(&w),
        Unmarked::LostClaim => Arc::new(LosesTheClaim {
            inner: w.broker.clone(),
            pool: w.pg.store.pool().clone(),
        }),
    };
    let first = host_judged(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace(&format!("{tag}-1")),
        judge,
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("첫 실행이 끝나야 한다");
    match cause {
        // 답을 낸 CLI가 있어도 실행되지 못한 호출이 남았으니 성공이 아니라 재시도다
        Unmarked::Blind => assert!(
            matches!(outcome, Some(Outcome::Failed(_))),
            "{cause:?}: 성공으로 끝나면 안 된다: {outcome:?}"
        ),
        // 이어받은 쪽이 있으니 이 시도는 아무것도 쓰지 않고 물러난다
        Unmarked::LostClaim => assert_eq!(outcome, Some(Outcome::Lost), "{cause:?}"),
    }
    assert!(
        marks(&w, &run).await.is_empty(),
        "{cause:?}: 확정하지 못한 거절에는 표식이 없다"
    );
    assert!(w.runner.jobs().is_empty(), "{cause:?}");
    assert_ne!(w.pg.state_of(&run).await, "succeeded", "{cause:?}");
    let first_seen = bodies(&upstream);
    match cause {
        Unmarked::Blind => assert_word(&first_seen[1], "toolu_extra2", "unavailable"),
        Unmarked::LostClaim => {}
    }

    // 재개는 다시 판단한다: 같은 호출이 이번에는 확정 거절로 닫히고 도구 실행은 여전히 0이다
    w.pg.sql(
        "update bot_runs set not_before_ms = 0, lease_until_ms = case when state = 'leased' then 0 else lease_until_ms end",
    )
    .await;
    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace(&format!("{tag}-2")),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("재개한 실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded), "{cause:?}");
    assert!(w.runner.jobs().is_empty(), "{cause:?}: 추가 도구 실행 0");
    let seen = bodies(&upstream);
    assert_word(
        seen.last().expect("재개한 요청"),
        "toolu_extra2",
        "unknown_tool",
    );
}

#[tokio::test]
async fn a_verdict_that_could_not_confirm_the_pinned_revision_leaves_no_mark_and_the_resume_judges_again()
 {
    unmarked(Unmarked::Blind).await;
}

#[tokio::test]
async fn a_lost_claim_during_the_verdict_leaves_no_mark_and_the_new_owner_judges_again() {
    unmarked(Unmarked::LostClaim).await;
}

#[tokio::test]
async fn a_marked_refusal_lets_the_run_finish_in_one_attempt_without_running_anything() {
    let Some(w) = strict_world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    script(
        &upstream,
        tool_turn("msg_m1", "toolu_nope", "mcp__relay__agenda_extra", "{}"),
    );
    script(&upstream, final_turn("msg_m2", "그 도구는 쓸 수 없어요"));
    let run = w.accept("req-marked", "일정을 정리해줘").await;
    promote(&w).await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox,
        &workspace("marked"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("실행이 끝나야 한다");
    // 확정 거절은 끝난 호출이다: 모델이 그 단어를 보고 답했으면 재시도 없이 성공이다
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");
    let stored = marks(&w, &run).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].1, json_word(CliRefusal::UnknownTool));
    assert!(w.runner.jobs().is_empty());
    let seen = bodies(&upstream);
    assert_word(&seen[1], "toolu_nope", "unknown_tool");
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"그 도구는 쓸 수 없어요"}"#
    );
}
