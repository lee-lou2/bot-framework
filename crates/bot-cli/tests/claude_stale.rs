//! 호스트 설정(모델 상류 목적지)이 바뀌면 이미 집은 시도의 옛 프런트도 새 모델 요청을 옛 목적지로 보내지 않는다(실제 `claude` CLI·가짜 상류·
//! 실제 PG·실제 `serve --activate`). 프로필 판본은 그대로이고 호스트 설정만 바뀌는 활성화다 — 활성화 판본의 권한 지문이 상류 목적지를 묶는다.
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_stale` (claude CLI가 없으면 건너뛴다)
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

fn leased(rig: &Rig) -> String {
    rig.query("select count(*) from {s}.bot_runs where state = 'leased'")
}

/// 프로필 판본(스냅샷 판본)
fn profile_revision(rig: &Rig) -> String {
    rig.query("select revision from {s}.bot_active_profiles where profile = 'personal'")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_attempt_sends_no_new_model_request_after_the_host_destination_changed_and_the_new_worker_finishes_it()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut rig = Rig::start(
        schema,
        claude,
        PROFILE,
        &[],
        &cli_gateway(),
        &[Role::Gateway, Role::Scheduler, Role::Worker],
    )
    .await;
    // 대조: 호스트 설정이 그대로인 동안 정상 대화는 상류까지 간다
    rig.script(final_turn("msg_ok", "안녕하세요"));
    let ok = rig.chat("s0", "안녕").await;
    assert!(ok.status.success(), "{}", said(&ok));
    assert!(said(&ok).contains("안녕하세요"), "{}", said(&ok));
    assert_eq!(rig.upstream.requests().len(), 1);

    // 두 번째 대화: worker가 시도를 집었지만 모델 CLI가 뜨기 전에 멈춘다(모델 요청은 아직 나가지 않았다)
    let (active, profile) = (rig.schema.active(), profile_revision(&rig));
    rig.gate.hold();
    rig.script(final_turn("msg_next", "새 목적지에서 이어서 답했어요"));
    let mut chat = rig.chat_in_background("s1", "한 번 더 안녕");
    rig.until("worker가 시도를 집지 않았어요", 30, |rig| {
        leased(rig) == "1"
    })
    .await;

    // 같은 프로필 판본에서 호스트 설정(모델 상류 목적지)만 바꿔 활성화한다 → 이 프로세스는 구판이다
    let _moved = rig.move_upstream().await;
    assert_ne!(rig.schema.active(), active, "활성화 판본이 바뀌었어야 해요");
    assert_eq!(profile_revision(&rig), profile, "프로필 판본은 그대로예요");

    // 모델 CLI가 이제 요청을 낸다: 구판 프런트는 상류로 보내지 않고 거절하고, 시도는 **시도 횟수를 쓰지 않고** 대기열로 돌아간다
    rig.gate.release();
    rig.until("거절된 시도가 끝나지 않았어요", 90, |rig| {
        leased(rig) == "0"
    })
    .await;
    assert_eq!(
        rig.upstream.requests().len(),
        1,
        "구판 프런트가 옛 목적지로 새 모델 요청을 보냈어요"
    );
    let waiting = "select state || ':' || attempts from {s}.bot_runs where conv_id = 'owner' order by created_ms desc limit 1";
    assert_eq!(
        rig.query(waiting),
        "ready:0",
        "새 판본의 worker가 이어받도록 대기열로 돌아가고 시도 횟수는 그대로여야 해요"
    );

    // 새 판본의 worker가 집어 **새 목적지**로 보내고 끝낸다. 기다리던 `bot chat`도 그 답을 받는다
    let worker = rig.start_worker_on_moved_host();
    rig.until(
        "새 판본의 worker가 이어받지 못했어요",
        90,
        |rig| rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'") == "2",
    )
    .await;
    let seen = rig.upstream.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].0, "api.main.example:443",
        "대조 대화는 옛 목적지로 갔다"
    );
    assert_eq!(
        seen[1].0, "api.moved.example:443",
        "이어받은 요청은 새 목적지로만 나간다"
    );
    assert_eq!(rig.query(waiting), "succeeded:0");
    // 구판 gateway는 결과를 돌려주지 않는다(일시 실패로 닫는다): 처음 붙었던 `bot chat`은 끝나고, 사용자는 새 판본 gateway에 같은 요청 ID로 다시 붙어
    // 새 목적지에서 이어서 마친 답을 받는다
    tokio::time::timeout(Duration::from_secs(60), chat.wait())
        .await
        .expect("처음 붙었던 bot chat이 끝나지 않았어요")
        .unwrap();
    let attached = rig.chat_on_moved_gateway("s1", "한 번 더 안녕").await;
    assert!(attached.status.success(), "{}", said(&attached));
    assert!(
        said(&attached).contains("새 목적지에서 이어서 답했어요"),
        "{}",
        said(&attached)
    );
    assert_eq!(
        rig.upstream.requests().len(),
        2,
        "다시 붙는다고 모델을 또 부르지 않아요"
    );
    worker.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_model_stream_is_cut_when_the_host_destination_changes() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let mut rig = Rig::start(
        schema,
        claude,
        PROFILE,
        &[],
        &cli_gateway(),
        &[Role::Gateway, Role::Scheduler, Role::Worker],
    )
    .await;
    // 응답을 조각마다 2초씩 흘린다(여섯 조각, 끝까지 가면 십여 초) — 열려 있는 동안 호스트 설정을 바꾼다
    rig.script_slowly(
        final_turn("msg_slow", "스트림이 끝까지 가면 안 돼요"),
        Duration::from_secs(2),
    );
    let _chat = rig.chat_in_background("s2", "느린 답을 해 줘");
    rig.until("모델 요청이 상류에 닿지 않았어요", 30, |rig| {
        rig.upstream.requests().len() == 1
    })
    .await;
    let _moved = rig.move_upstream().await;

    // 구판 프런트가 열린 스트림을 끊고, CLI가 다시 보내는 요청은 상류에 닿지 않는다
    rig.until("끊긴 시도가 끝나지 않았어요", 90, |rig| {
        leased(rig) == "0"
    })
    .await;
    assert_eq!(
        rig.upstream.requests().len(),
        1,
        "끊긴 뒤 구판 프런트가 새 요청을 상류로 보냈어요"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'"),
        "0",
        "끊긴 스트림의 응답으로 run이 성공하면 안 돼요"
    );
    assert!(
        !rig.query("select coalesce(string_agg(result, ''), '') from {s}.bot_runs")
            .contains("스트림이 끝까지 가면 안 돼요")
    );
}
