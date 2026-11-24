//! 실제 `claude` CLI(내장 Read를 켠 구성)+PG+실제 프런트·브로커로 확인하는 «세션 토큰은 모델이 읽는 어떤 파일에도 없다».
//! 이 시도의 Bearer는 MCP 설정 파일이 아니라 환경변수(`RELAY_TOKEN`)로만 간다: 설정에는 `${RELAY_TOKEN}` 자리표시만 있고 실제 CLI가 환경에서 채운다.
//! - 실제 프런트가 그 Bearer를 받아들인다(같은 응답에서 부른 릴레이 도구가 진짜 결과를 돌려준다).
//! - 모델이 Read로 읽을 수 있는 것은 입력 폴더(`/bot`)뿐이다: 입력 파일은 읽히고(대조), MCP 설정·CLI 설정 폴더·홈·호스트 파일은 권한 거절이다.
//! - 모델이 받은 어떤 요청에도, CLI가 끝난 뒤 두 폴더(`/bot`·`/work`) 전체에도 토큰이 0건이다.
//!
//! 실행: `BOT_CLAUDE_BIN=<claude 경로> BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test cli_credentials`

mod support;

use bot_broker::testing::FakeUpstream;
use bot_core::{id::*, profile::Builtin};
use bot_runtime::Outcome;
use std::{sync::Arc, time::Duration};
use support::{cli::*, view::*, *};

/// 모델이 한 응답에서 부르는 도구들: 상대 경로는 CLI의 작업 폴더(`/bot`)를 기준으로 한다(`..`은 그 바깥 `/work` 쪽이다).
const READS: [(&str, &str, &str); 6] = [
    ("toolu_in", "Read", r#"{"file_path":"AGENTS.md"}"#),
    (
        "toolu_cfg",
        "Read",
        r#"{"file_path":"../work/mcp.claude.json"}"#,
    ),
    (
        "toolu_cli",
        "Read",
        r#"{"file_path":"../work/claude/.claude.json"}"#,
    ),
    (
        "toolu_home",
        "Read",
        r#"{"file_path":"../work/home/.claude.json"}"#,
    ),
    ("toolu_host", "Read", r#"{"file_path":"/etc/hosts"}"#),
    ("toolu_mcp", "mcp__relay__agenda_list", "{}"),
];

#[tokio::test]
async fn the_real_front_accepts_the_token_from_the_environment_and_no_readable_file_or_model_request_holds_it()
 {
    let Some(w) = world().await else {
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    // 내장 Read를 켠 프로필: 모델이 입력 폴더를 읽을 수 있다
    let mut snapshot = (*profile()).clone();
    snapshot
        .agents
        .get_mut(&AgentId("main".into()))
        .expect("루트 에이전트")
        .builtin = [Builtin::Read].into();
    w.catalog.publish(Arc::new(snapshot));

    let sandbox = LocalSandbox::new(claude);
    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, tools_turn("msg_c1", &READS));
    script(&upstream, final_turn("msg_c2", "읽었어요"));

    let run = w.accept("req-credentials", "파일을 읽어줘").await;
    let host = host(
        &w,
        w.pg.store.clone(),
        upstream.clone(),
        sandbox.clone(),
        &workspace("cred"),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");

    // 실제 프런트가 환경에서 채워진 Bearer를 받아들였다: 릴레이 도구가 진짜 결과를 돌려줬다
    let seen = bodies(&upstream);
    assert_eq!(
        seen.len(),
        2,
        "응답 하나에 도구 여섯, 그 결과로 마무리 하나"
    );
    let result = |id: &str| result_text(&seen[1], id).unwrap_or_else(|| panic!("{id} 결과 없음"));
    let mcp = result("toolu_mcp");
    assert!(mcp.contains("산책"), "릴레이 도구가 실제로 실행됐다: {mcp}");
    assert_eq!(result_error(&seen[1], "toolu_mcp"), Some(false));
    assert_eq!(w.runner.jobs().len(), 1, "릴레이 도구는 한 번 실행됐다");

    // 대조: 모델은 입력 폴더의 파일은 읽는다(그래서 아래 거절이 «읽기가 꺼져서»가 아니다)
    let agents = result("toolu_in");
    assert_eq!(result_error(&seen[1], "toolu_in"), Some(false), "{agents}");
    assert!(
        agents.starts_with("1\t"),
        "줄 번호가 붙은 파일 내용: {agents}"
    );
    // 입력 폴더 밖(쓰기 공간·CLI 설정·홈·호스트 파일)은 권한 거절이다. MCP 설정도 모델이 읽을 수 없다
    for id in ["toolu_cfg", "toolu_cli", "toolu_home", "toolu_host"] {
        assert_eq!(result_error(&seen[1], id), Some(true), "{id}");
        assert!(
            result(id).contains("requested permissions to read"),
            "{id}: {}",
            result(id)
        );
    }

    // 모델이 받은 어떤 요청에도 토큰이 없다. 토큰은 이 호스트가 발급한 것 전부로 본다
    let tokens = host.tokens();
    assert!(!tokens.is_empty());
    for body in &seen {
        let text = body.to_string();
        for token in &tokens {
            assert!(!text.contains(token.as_str()), "모델 요청에 토큰이 있다");
        }
    }

    // CLI가 끝난 직후 두 폴더 전체(CLI가 쓴 파일 포함)에도 토큰이 없다
    let swept = sandbox.swept();
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert!(swept[0].token_len >= 16, "토큰을 훑었다: {swept:?}");
    assert!(
        swept[0].files >= 3,
        "AGENTS.md·MCP 설정·CLI가 쓴 파일까지 훑었다: {swept:?}"
    );
    assert!(
        swept[0].hits.is_empty(),
        "모델이 읽을 수 있는 폴더에 세션 토큰이 있다: {:?}",
        swept[0].hits
    );
}
