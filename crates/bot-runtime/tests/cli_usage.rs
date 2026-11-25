//! 실제 `claude` CLI(가짜 상류)+실제 PG로 확인하는 «실행 계량은 모든 모델 요청이 실제로 보고됐을 때만 저장한다».
//! 한 CLI 실행이 도구 한 번과 최종 답(상류 요청 둘)을 돌 때: 둘 다 보고되면 합(5/4 + 9/4 → 14/8), 하나라도 보고가 없으면 CLI의 총합이 나머지의 합(양수)이라도
//! 비워 둔다. 끊긴 답을 CLI가 이어 쓰면 요청이 늘어도 모두 보고됐으니 그 합이다(최종 글은 이어 쓴 글이다).
//!
//! 실행: `BOT_CLAUDE_BIN=<claude 경로> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test cli_usage`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

/// 한 CLI 실행(도구 한 번 + 최종 답)을 돌려 저장된 (입력/출력/잘림)을 읽는다.
async fn stored(first: Vec<Vec<u8>>, last: Vec<Vec<u8>>, tag: &str) -> Option<(String, String)> {
    let w = world().await?;
    let claude = claude_bin()?;
    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, first);
    script(&upstream, last);
    let run = w.accept(&format!("req-{tag}"), "일정 목록을 알려줘").await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        LocalSandbox::new(claude),
        &workspace(tag),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded), "{tag}");
    assert_eq!(upstream.requests().len(), 2, "{tag}: 도구 한 번과 최종 답");
    let usage = w
        .pg
        .scalar::<String>(&format!(
            "select coalesce(usage_prompt::text, '-') || '/' || coalesce(usage_completion::text, '-') || '/' || coalesce(truncated::text, '-') from bot_runs where id = '{}'",
            run.0
        ))
        .await;
    let text =
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await;
    Some((usage, text))
}

fn tool() -> Vec<Vec<u8>> {
    tool_turn("msg_first", "toolu_first", "mcp__relay__agenda_list", "{}")
}

#[tokio::test]
async fn usage_is_stored_only_when_every_model_request_of_the_run_was_reported() {
    // 두 요청 모두 보고(5/4 + 9/4): 합
    let Some((usage, _)) = stored(tool(), final_turn("msg_last", "마지막 답"), "u-all").await
    else {
        return;
    };
    assert_eq!(usage, "14/8/false");
    // 첫 요청만 미보고: CLI의 총합은 9/4(나머지)지만 전체 합이 아니다 → 비움
    let (usage, _) = stored(
        unreported(tool()),
        final_turn("msg_last", "마지막 답"),
        "u-first",
    )
    .await
    .unwrap();
    assert_eq!(usage, "-/-/false");
    // 마지막 요청만 미보고 → 비움
    let (usage, _) = stored(
        tool(),
        unreported(final_turn("msg_last", "마지막 답")),
        "u-last",
    )
    .await
    .unwrap();
    assert_eq!(usage, "-/-/false");
    // 모두 미보고 → 비움
    let (usage, _) = stored(
        unreported(tool()),
        unreported(final_turn("msg_last", "마지막 답")),
        "u-none",
    )
    .await
    .unwrap();
    assert_eq!(usage, "-/-/false");
}

#[tokio::test]
async fn a_cut_answer_the_cli_continues_is_measured_over_all_its_requests_and_is_not_truncated() {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let upstream = Arc::new(FakeUpstream::new());
    // 첫 응답은 출력 한도에서 끊기고, CLI가 «이어서 쓰라»고 다시 요청해 둘째 응답이 마무리한다
    script(&upstream, cut_turn("msg_cut", "끊긴 앞부분"));
    script(&upstream, final_turn("msg_end", "이어 쓴 마무리"));
    let run = w.accept("req-cut-continued", "긴 답을 써줘").await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        LocalSandbox::new(claude),
        &workspace("u-cut"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(upstream.requests().len(), 2);
    // 두 요청 모두 보고됐다(9/4 + 9/4). 잘린 답이 최종 답이 된 것이 아니라 이어 쓴 글이 최종 답이다
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select usage_prompt::text || '/' || usage_completion::text || '/' || truncated::text from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        "18/8/false"
    );
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"이어 쓴 마무리"}"#
    );
}
