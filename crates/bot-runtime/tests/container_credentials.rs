//! 진짜 모델 이미지의 진짜 `claude`(내장 Read를 켠 구성)를 VM에서 돌려, 세션 토큰이 모델이 읽을 수 있는 어떤 파일에도 없음을 확인한다(모델 호출 0).
//! 이 시도의 Bearer는 MCP 설정 파일이 아니라 환경변수(`RELAY_TOKEN`)로만 간다. VM 안의 실제 파일시스템·`/proc`·마운트에서:
//! - 실제 프런트가 환경에서 채워진 Bearer를 받아들인다(같은 응답에서 부른 릴레이 도구가 진짜 결과를 돌려준다).
//! - 모델이 Read로 읽을 수 있는 것은 입력 폴더(`/bot`)뿐이다: 입력 파일은 읽히고(대조), MCP 설정(`/work`)·CLI 설정·`/proc/self/environ`(토큰이 든 환경)·
//!   호스트 파일은 권한 거절이다.
//! - 모델이 받은 어떤 요청에도, VM이 끝난 직후 호스트에서 본 두 마운트 폴더(`/bot`·`/work`) 전체에도 토큰이 0건이다.
//!
//! 실행: `BOT_CONTAINER_CANARY=required BOT_CLAUDE_CANARY=required BOT_MODEL_IMAGE=… BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test container_credentials`

mod support;
mod vm;

use bot_broker::testing::FakeUpstream;
use bot_core::{id::*, profile::Builtin, sandbox::ModelSandbox};
use bot_runtime::{Outcome, model::container::ContainerSandbox};
use std::{sync::Arc, time::Duration};
use support::{cli::*, sweep::Sweeping, view::*, *};

/// 모델이 한 응답에서 부르는 도구들. 상대 경로는 작업 폴더(`/bot`) 기준이다.
const READS: [(&str, &str, &str); 8] = [
    ("toolu_in", "Read", r#"{"file_path":"AGENTS.md"}"#),
    (
        "toolu_cfg",
        "Read",
        r#"{"file_path":"/work/mcp.claude.json"}"#,
    ),
    (
        "toolu_cfg2",
        "Read",
        r#"{"file_path":"../work/mcp.claude.json"}"#,
    ),
    (
        "toolu_cli",
        "Read",
        r#"{"file_path":"/work/claude/.claude.json"}"#,
    ),
    ("toolu_env", "Read", r#"{"file_path":"/proc/self/environ"}"#),
    ("toolu_env1", "Read", r#"{"file_path":"/proc/1/environ"}"#),
    ("toolu_pass", "Read", r#"{"file_path":"/etc/passwd"}"#),
    ("toolu_mcp", "mcp__relay__agenda_list", "{}"),
];

#[tokio::test(flavor = "multi_thread")]
async fn the_real_claude_in_the_vm_cannot_read_the_session_token_from_any_file() {
    let Some(w) = world().await else {
        return;
    };
    let Some(program) = vm::available() else {
        return;
    };
    let _serial = vm::serial().await;
    let Some(image) = vm::model_image(&program) else {
        return;
    };
    // 내장 Read를 켠 프로필
    let mut snapshot = (*profile()).clone();
    snapshot
        .agents
        .get_mut(&AgentId("main".into()))
        .expect("루트 에이전트")
        .builtin = [Builtin::Read].into();
    w.catalog.publish(Arc::new(snapshot));

    let env = vm::Env::new(program.clone(), "cred");
    let mut config = env.firewalled(&image);
    config.cpus = 2;
    config.memory_mib = 2048;
    let anchored = ContainerSandbox::start(config).await.expect("시작");

    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, tools_turn("msg_vc1", &READS));
    script(&upstream, final_turn("msg_vc2", "읽었어요"));
    let run = w.accept("req-vm-credentials", "파일을 읽어줘").await;

    let sandbox = Arc::new(std::sync::Mutex::new(None));
    let slot = sandbox.clone();
    let bind = anchored.bind_addr(0);
    let host = host_at(
        &w,
        w.pg.peer("worker-cred").await,
        upstream.clone(),
        bind,
        async move |front| {
            let relay = anchored.relay(front.port()).await.expect("릴레이");
            let sweeping = Arc::new(Sweeping::new(relay));
            *slot.lock().unwrap() = Some(sweeping.clone());
            sweeping as Arc<ModelSandbox>
        },
        &env.root.join("ws"),
        w.broker.clone(),
    )
    .await;
    let sandbox = sandbox.lock().unwrap().take().expect("샌드박스");

    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    w.pg.assert_state(&run, "succeeded").await;

    let seen = bodies(&upstream);
    assert_eq!(
        seen.len(),
        2,
        "응답 하나에 도구 여덟, 그 결과로 마무리 하나"
    );
    let result = |id: &str| result_text(&seen[1], id).unwrap_or_else(|| panic!("{id} 결과 없음"));
    // 실제 프런트가 환경에서 채워진 Bearer를 받아들였다
    let mcp = result("toolu_mcp");
    assert!(mcp.contains("산책"), "릴레이 도구가 실제로 실행됐다: {mcp}");
    assert_eq!(w.runner.jobs().len(), 1);
    // 대조: 입력 폴더의 파일은 읽힌다
    let agents = result("toolu_in");
    assert_eq!(result_error(&seen[1], "toolu_in"), Some(false), "{agents}");
    assert!(agents.starts_with("1\t"), "{agents}");
    // 입력 폴더 밖은 모두 거절이다: 파일은 권한 거절(MCP 설정·CLI 설정·시스템 파일), 토큰이 든 프로세스 환경(`/proc`)은 도구가 아예 읽지 않는 경로다
    for (id, why) in [
        ("toolu_cfg", "requested permissions to read"),
        ("toolu_cfg2", "requested permissions to read"),
        ("toolu_cli", "requested permissions to read"),
        ("toolu_pass", "requested permissions to read"),
        ("toolu_env", "device or process file"),
        ("toolu_env1", "device or process file"),
    ] {
        assert_eq!(result_error(&seen[1], id), Some(true), "{id}");
        assert!(result(id).contains(why), "{id}: {}", result(id));
    }
    // 모델이 받은 어떤 요청에도 토큰이 없다
    let tokens = host.tokens();
    assert!(!tokens.is_empty());
    for body in &seen {
        let text = body.to_string();
        for token in &tokens {
            assert!(!text.contains(token.as_str()), "모델 요청에 토큰이 있다");
        }
    }
    // VM이 끝난 직후 호스트에서 본 두 마운트 폴더 전체(CLI가 쓴 파일 포함)에 토큰이 없다
    let swept = sandbox.swept();
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert!(swept[0].token_len >= 16, "{swept:?}");
    assert!(swept[0].files >= 3, "{swept:?}");
    assert!(
        swept[0].hits.is_empty(),
        "모델이 읽을 수 있는 폴더에 세션 토큰이 있다: {:?}",
        swept[0].hits
    );

    env.until(20, "닻만 남아야 한다", |e| e.ids().len() == 1)
        .await;
    sandbox.inner().shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
}
