//! 호출 원장에 남지 않는 CLI 거절(예: CLI 대화가 명시 목적지를 준 `schedule.create`)도 «모델에 돌려준 단어»를 그 호출 자리의 **표식**(`cli-refusal-<자리>`)으로 먼저
//! 기록한 뒤 돌려준다. 그래서 첫 시도가 `session_unsaved`로 물러나지 않고 시도를 쓰지 않는다: 저장소의 시도 한도가 1이어도 run이 정상으로 끝나고, 같은 슬롯의
//! 표식은 하나, 모델 요청은 필요한 두 번(도구 호출 → 거절 단어를 본 마무리)뿐이며 예약·승인 카드·호출 행은 0이다(총괄 결정 «모든 CLI 거절은 표식 먼저»).
//!
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_attempts` (claude CLI가 없으면 건너뛴다)
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_store::Config;
use claude_support::*;
use process_support::Schema;

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, schedule.create, schedule.list, schedule.cancel]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run, schedule.create, schedule.list, schedule.cancel]
  approve: []
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_refused_before_it_is_recorded_does_not_spend_an_attempt() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    let rig = Rig::start_with_store(
        schema,
        claude,
        PROFILE,
        &cli_gateway(),
        &[Role::Gateway, Role::Worker],
        Config {
            max_attempts: 1,
            ..Config::default()
        },
    )
    .await;
    // CLI 대화는 원래 대화에만 답한다: 목적지를 주면 호출을 기록하기 전에 거절된다(`destination_unsupported`)
    let destination =
        serde_json::json!({"binding": "desk", "conversation": {"kind": "channel", "id": "C1"}});
    rig.script(tool_turn(
        "msg_a1",
        "toolu_a1",
        "mcp__relay__schedule_create",
        &create_with(destination, &in_secs(3600)),
    ));
    rig.script(final_turn("msg_a2", "그 목적지는 쓸 수 없대요"));
    let chat = rig.chat("a1", "팀 채널에 알려 줘").await;
    let rows = rig.query(
        "select string_agg(state || ':' || attempts || ':' || coalesce(error, ''), ' / ') from {s}.bot_runs",
    );
    assert!(chat.status.success(), "{} / runs: {rows}", said(&chat));
    // 시도 한도 1에서도 run이 정상으로 끝나고 시도를 쓰지 않았다
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where state = 'succeeded'"),
        "1",
        "시도 한도 1에서도 run이 정상으로 끝나야 해요: {rows}"
    );
    assert_eq!(
        rig.query("select attempts from {s}.bot_runs"),
        "0",
        "거절이 시도를 쓰지 않아요: {rows}"
    );
    // 같은 슬롯의 표식이 하나이고 모델에는 `denied`가 돌아갔다
    assert_eq!(
        rig.query("select count(*) from {s}.bot_steps where key like 'cli-refusal-%'"),
        "1"
    );
    assert_eq!(
        rig.query("select body from {s}.bot_steps where key like 'cli-refusal-%'"),
        "\"denied\""
    );
    assert_eq!(
        tool_result(&rig.upstream, "toolu_a1").as_deref(),
        Some("denied")
    );
    // 불필요한 모델 요청이 없다(도구 호출 한 번과 거절을 본 마무리 한 번)
    assert_eq!(rig.upstream.requests().len(), 2, "{rows}");
    // 예약·승인 카드·호출 행은 만들어지지 않았다
    for table in ["bot_user_schedules", "bot_approvals", "bot_calls"] {
        assert_eq!(
            rig.query(&format!("select count(*) from {{s}}.{table}")),
            "0",
            "{table}"
        );
    }
    rig.stop().await;
}
