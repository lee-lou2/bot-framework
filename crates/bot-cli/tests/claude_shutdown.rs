//! 종료 신호(SIGTERM·SIGINT와 같은 경로의 `stop` 토큰)를 받은 worker는 유예(8초) 뒤에도 남은 run의 모델 CLI를 끝내고 세션을 거둔 뒤,
//! run을 시도 횟수를 쓰지 않고 대기열로 돌려준다. 새 serve가 같은 run을 이어서 마친다(실제 `claude` CLI·가짜 상류·실제 PG).
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_shutdown` (claude CLI가 없으면 건너뛴다)
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use claude_support::*;
use process_support::Schema;
use std::time::Duration;

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run]
  approve: []
"#;

const ROLES: [Role; 3] = [Role::Gateway, Role::Scheduler, Role::Worker];

fn state(rig: &Rig) -> String {
    rig.query("select state from {s}.bot_runs where parent is null")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_signal_during_a_model_request_returns_the_run_without_spending_the_attempt_and_a_new_serve_finishes_it()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut rig = Rig::start(schema, claude, PROFILE, &[], &cli_gateway(), &ROLES).await;

    // 모델 응답이 천천히 흐르는 동안(스트림이 열려 있다) 종료 신호가 온다
    rig.script_slowly(
        final_turn("msg_slow", "오래 걸려요"),
        Duration::from_secs(60),
    );
    let _chat = rig.chat_in_background("s1", "오래 걸리는 일");
    rig.until("worker가 시도를 집지 않았어요", 30, |rig| {
        state(rig) == "leased"
    })
    .await;
    rig.until("모델 요청이 나가지 않았어요", 30, |rig| {
        rig.upstream.requests().len() == 1
    })
    .await;

    // 종료: 유예(8초)를 넘겨도 끝나지 않는 run은 모델 CLI를 끝내고 세션을 거둔 뒤 돌려준다
    let ended = rig.signal_stop().await;
    assert_eq!(ended, Ok(()), "serve가 오류로 끝났어요");
    assert_eq!(state(&rig), "ready", "대기열로 돌아갔다");
    assert_eq!(
        rig.query("select attempts from {s}.bot_runs where parent is null"),
        "0",
        "시도 횟수를 쓰지 않았다"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action = 'run.release'"),
        "1"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_audit where action = 'run.reclaim'"),
        "0",
        "리스가 끝나 인계된 것이 아니다"
    );

    // 새 serve가 이어받아 같은 run을 마친다: 같은 요청 ID·같은 말로 다시 붙으면 그 run의 답을 받는다
    rig.script(final_turn("msg_next", "이어서 답했어요"));
    rig.restart_same(&ROLES).await;
    rig.until("새 serve가 run을 끝내지 않았어요", 90, |rig| {
        state(rig) == "succeeded"
    })
    .await;
    let answer = rig.chat("s1", "오래 걸리는 일").await;
    assert!(
        said(&answer).contains("이어서 답했어요"),
        "{}",
        said(&answer)
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where parent is null"),
        "1",
        "같은 run이다"
    );
    rig.stop().await;
}
