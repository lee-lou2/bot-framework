//! 실제 `claude` CLI(가짜 상류)+실제 PG+실제 브로커 프런트(PlannedCalls·CheckpointSink)로 승인·위임 뒤 세션을 이어 가는 수락 시험.
//! 한 시나리오는 두 "프로세스"(저장소 핸들·프런트·세션 토큰·워커가 모두 새것)에 걸친다. 모델 호출은 0이다.
//! 실행: `BOT_CLAUDE_BIN=<claude 경로> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test cli_resume`

mod support;

use bot_broker::front::park::ParkReason;
use bot_core::{id::*, port::Broker};
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

#[tokio::test]
async fn an_approval_pause_resumes_the_real_cli_session_and_runs_the_tool_exactly_once() {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    // 첫 모델 응답: 되돌릴 수 없는 삭제 도구(승인이 필요하다). 재개된 CLI의 첫 요청에는 마무리 글을 준다
    script(
        &upstream,
        tool_turn(
            "msg_a1",
            "toolu_del1",
            "mcp__relay__agenda_delete",
            r#"{"event":"e1"}"#,
        ),
    );
    script(&upstream, final_turn("msg_a2", "삭제를 마쳤어요"));

    let run = w.accept("req-approve", "산책 일정을 삭제해줘").await;
    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("a1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("첫 실행이 끝나야 한다");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "승인을 기다리며 멈춘다: {outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    // 정확히 이 시도 하나만 Approval 대기로 통지됐다
    let parked = first.park.seen();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(
        (parked[0].0.run.clone(), parked[0].1),
        (run.clone(), ParkReason::Approval)
    );
    assert_eq!(parked[0].0.generation, Generation(1));
    assert!(w.runner.jobs().is_empty(), "승인 전에는 도구 실행 0");
    // 프런트가 확인한 응답과 세션 줄이 한 묶음으로 저장돼 있다
    let keys = step_keys(&w, &run).await;
    for expected in ["cli-resp-msg_a1", "cli-use-toolu_del1"] {
        assert!(keys.iter().any(|k| k == expected), "{expected} in {keys:?}");
    }
    assert!(keys.iter().any(|k| k.starts_with("cli-rec-")), "{keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("cli-group-")), "{keys:?}");
    assert_eq!(
        upstream.requests().len(),
        1,
        "승인 대기 뒤 CLI는 모델에 더 닿지 못했다"
    );

    // 소유자가 승인한다. 그다음 프로세스를 통째로 바꾼다(새 저장소 핸들·프런트·토큰·워커)
    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let old_tokens = first.tokens();
    assert!(!old_tokens.is_empty());
    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace("a2"),
    )
    .await;
    // 새 시도가 자기 토큰을 발급해 살아 있는 동안에 확인한다: 그 토큰은 통하고, 이전 프로세스의 토큰은 같은 프런트에서도 통하지 않으며
    // 두 토큰은 서로 다르다(토큰을 다시 쓰는 회귀를 잡는다)
    let live_check = async {
        let mut live = None;
        for _ in 0..1500 {
            if let Some(token) = second.tokens().first() {
                live = Some(token.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
        let live = live.expect("재개한 시도가 토큰을 발급했다");
        // 새 토큰은 유효한 MCP 요청에 정상 결과(200과 도구 목록)를 받는다
        let (status, body) = list_tools_with(second.front.addr(), &live).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(r#""tools":["#) && body.contains("agenda_delete"),
            "새 토큰은 살아 있는 동안 정상 결과를 받는다: {body}"
        );
        for token in &old_tokens {
            assert_ne!(token, &live, "이전 시도의 토큰을 다시 쓰지 않는다");
            // 같은 유효한 요청이라도 이전 프로세스의 토큰은 401이고 결과를 받지 못한다
            let (status, body) = list_tools_with(second.front.addr(), token).await;
            assert_eq!(status, 401, "이전 프로세스의 토큰은 통하지 않는다: {body}");
            assert!(!body.contains("agenda_delete"), "{body}");
        }
    };
    let (outcome, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(150), second.worker.run_once()),
        live_check
    );
    let outcome = outcome.expect("재개한 실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert!(second.park.seen().is_empty(), "재개한 시도는 멈추지 않았다");
    assert_eq!(w.pg.state_of(&run).await, "succeeded");

    // 도구는 정확히 한 번(승인 뒤에) 실행됐다
    let jobs = w.runner.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    // 재개한 CLI의 모델 요청은 원래 도구 사용에 붙은 **실제** 결과를 본다
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    let text = result_text(&seen[1], "toolu_del1").expect("둘째 요청에 도구 결과가 있다");
    assert!(
        text.contains("산책"),
        "실제 결과(자리표시가 아니다): {text}"
    );
    assert!(
        !text.contains("approval_pending") && !text.contains("stale"),
        "{text}"
    );
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"삭제를 마쳤어요"}"#
    );
}

#[tokio::test]
async fn a_child_pause_resumes_the_real_cli_session_with_the_childs_result_and_delegates_once() {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    // 부모의 첫 응답: 자식에게 위임. 자식의 첫 요청: 마무리 글. 재개한 부모의 첫 요청: 마무리 글
    script(
        &upstream,
        tool_turn(
            "msg_d1",
            "toolu_del2",
            "mcp__relay__delegate",
            r#"{"agent":"main/research","instruction":"일정을 조사해"}"#,
        ),
    );
    script(&upstream, final_turn("msg_c1", "조사 결과: 맑음"));
    script(&upstream, final_turn("msg_d2", "자식이 알려 줬어요"));

    let run = w.accept("req-delegate", "연구 에이전트에게 맡겨줘").await;
    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("d1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("부모의 첫 실행이 끝나야 한다");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    // 정확히 이 시도 하나만 Child 대기로 통지됐다
    let parked = first.park.seen();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(
        (parked[0].0.run.clone(), parked[0].1),
        (run.clone(), ParkReason::Child)
    );
    assert_eq!(parked[0].0.generation, Generation(1));
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        1,
        "자식은 한 번만 만들어진다"
    );

    // 자식을 돌린다(같은 프로세스). 끝나면 부모가 깨어난다
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("자식 실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "ready");

    // 프로세스 교체 뒤 부모가 이어진다
    drop(first);
    let second = host(
        &w,
        w.pg.peer("worker-2").await,
        upstream.clone(),
        sandbox,
        &workspace("d2"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("재개한 부모가 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert!(second.park.seen().is_empty(), "재개한 시도는 멈추지 않았다");
    assert_eq!(w.pg.state_of(&run).await, "succeeded");
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        1,
        "재개가 자식을 또 만들지 않았다"
    );
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 3);
    let text = result_text(&seen[2], "toolu_del2").expect("재개한 부모의 요청에 자식 결과가 있다");
    assert!(text.contains("맑음"), "{text}");
}

#[tokio::test]
async fn two_tools_of_one_response_resume_without_rerunning_the_finished_one_and_both_results_are_real()
 {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(bot_broker::testing::FakeUpstream::new());
    // 한 응답이 읽기 도구와 승인이 필요한 삭제 도구를 함께 부른다
    script(
        &upstream,
        tools_turn(
            "msg_t1",
            &[
                ("toolu_list1", "mcp__relay__agenda_list", "{}"),
                (
                    "toolu_del3",
                    "mcp__relay__agenda_delete",
                    r#"{"event":"e1"}"#,
                ),
            ],
        ),
    );
    script(&upstream, final_turn("msg_t2", "조회하고 삭제했어요"));

    let run = w.accept("req-two", "일정을 보고 산책을 지워줘").await;
    let first = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("t1"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), first.worker.run_once())
        .await
        .expect("첫 실행이 끝나야 한다");
    assert!(
        matches!(outcome, Some(Outcome::Lost | Outcome::Parked)),
        "{outcome:?}"
    );
    assert_eq!(w.pg.state_of(&run).await, "waiting");
    // 정확히 이 시도 하나만 Approval 대기로 통지됐다
    let parked = first.park.seen();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(
        (parked[0].0.run.clone(), parked[0].1),
        (run.clone(), ParkReason::Approval)
    );
    assert_eq!(parked[0].0.generation, Generation(1));
    let tools: Vec<String> = w
        .runner
        .jobs()
        .iter()
        .map(|j| format!("{:?}", j.tool))
        .collect();
    assert_eq!(tools.len(), 1, "읽기 도구만 실행됐다: {tools:?}");
    assert!(tools[0].contains("agenda.list"), "{tools:?}");

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
        &workspace("t2"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), second.worker.run_once())
        .await
        .expect("재개한 실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert!(second.park.seen().is_empty(), "재개한 시도는 멈추지 않았다");

    // 읽기 도구는 다시 실행하지 않고(끝난 호출의 값), 삭제는 승인 뒤 한 번만
    let jobs: Vec<String> = w
        .runner
        .jobs()
        .iter()
        .map(|j| format!("{:?}", j.tool))
        .collect();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(jobs[1].contains("agenda.delete"), "{jobs:?}");
    // 재개한 CLI의 요청은 두 도구 결과를 모두 실제 값으로 본다
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    for id in ["toolu_list1", "toolu_del3"] {
        let text = result_text(&seen[1], id).unwrap_or_else(|| panic!("{id}의 결과가 없다"));
        assert!(text.contains("산책"), "{id}: {text}");
        assert!(
            !text.contains("approval_pending") && !text.contains("aborted"),
            "{id}: {text}"
        );
    }
}
