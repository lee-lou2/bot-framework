//! 실제 `claude` CLI(가짜 상류)+실제 PG+실제 프런트·브로커로 확인하는 «프로필 지침이 모델 요청의 system에 실제로 닿는다».
//! `--bare`는 작업 폴더의 `AGENTS.md`·`CLAUDE.md`를 자동으로 읽지 않는다(실측, claude 2.1.331): 지침을 `AGENTS.md`에만 두면 모델이 받은 요청에는 지침이 없다.
//! 그래서 실행기는 `--append-system-prompt-file`로 그 파일을 시스템 지침에 싣는다. 이 시험은 상류가 받은 **실제 요청 본문**을 본다: 그 인자를 빼면 실패한다.
//!
//! 실행: `BOT_CLAUDE_BIN=<claude 경로> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test cli_instructions`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_core::id::*;
use bot_runtime::Outcome;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

const TRUSTED: &str = "TRUSTED-INSTRUCTION-4410 한국어로 짧게 답한다";
const QUESTION: &str = "USER-QUESTION-8821 오늘 일정 알려줘";

/// 요청 본문의 `system`·메시지 `content`(글 하나이거나 블록 목록)의 글.
fn texts(blocks: &Value) -> String {
    match blocks {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[tokio::test]
async fn the_profiles_instructions_reach_the_models_system_prompt_and_the_users_words_stay_in_the_user_message()
 {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut snapshot = (*profile()).clone();
    snapshot
        .agents
        .get_mut(&AgentId("main".into()))
        .expect("루트 에이전트")
        .instructions = TRUSTED.into();
    w.catalog.publish(Arc::new(snapshot));
    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, final_turn("msg_i1", "답이에요"));

    let run = w.accept("req-instructions", QUESTION).await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        LocalSandbox::new(claude),
        &workspace("instructions"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");

    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 1, "모델 요청은 한 번");
    // 지침은 실제로 모델에 닿았다: 조립한 시스템 지침(프로필 지침과 고정 머리글)이 요청의 system에 있다
    let system = texts(&seen[0]["system"]);
    assert!(
        system.contains(TRUSTED),
        "프로필 지침이 요청의 system에 없다: {system}"
    );
    assert!(system.contains("## 현재 시각"), "{system}");
    // 사용자가 쓴 글은 사용자 메시지에만 있다
    assert!(!system.contains("USER-QUESTION-8821"), "{system}");
    let messages = seen[0]["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["role"], "user");
    assert!(texts(&messages[0]["content"]).contains("USER-QUESTION-8821"));
    assert!(!texts(&messages[0]["content"]).contains(TRUSTED));
}
