//! 호스트 조립이 ②·③의 입구를 실제로 연결했는지 `serve` 경로에서 확인한다(실제 `claude` CLI·가짜 상류·실제 PG·합성 샌드박스 주입):
//! CLI 확정 거절 표식(`PlannedCalls::with_preflight`): 도구 입력 검사에서 확정적으로 거절되는 호출은 run 체크포인트에 표식을 남긴다.
//! 다른 수신자로 가는 예약 회신(`Worker::with_delivery`·`ScheduleRoutes`)은 SQL 없이 가짜 Slack에서 시작하는 종단 시험 `claude_route.rs`가 본다.
//!
//! 실행: `BOT_CLAUDE_CANARY=required BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test claude_wiring` (claude CLI·`srt`가 없으면 건너뛴다)
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use claude_support::*;
use process_support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cli_call_the_broker_confirms_as_refused_leaves_the_refusal_marker_in_the_run_checkpoint()
{
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let Some(claude) = claude_bin() else {
        return;
    };
    if !have_srt() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("w")
        .tempdir_in("/tmp")
        .unwrap();
    // 도구 서버는 실행되지 않는다(입력 검사에서 거절된다): 설치할 패키지만 있으면 된다
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let profile = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run, calendar.read]
  approve: []
"#;
    let tools = "version: 1\nserver: agenda\ntools:\n  - id: list\n    class: read\n    scopes: [calendar.read]\n    secrets: {}\n    egress: []\n    input: {type: object, properties: {}, additionalProperties: false}\n";
    let rig = Rig::start_hosted(
        schema,
        claude,
        profile,
        &[
            ("agents/main/skills/agenda/SKILL.md", "일정을 조회한다."),
            ("agents/main/skills/agenda/tools.yaml", tools),
        ],
        &cli_gateway(),
        &format!(
            r#", "servers": {{"agenda": {{"root": "{}", "command": ["bin/server"]}}}}"#,
            pkg.display()
        ),
        &[Role::Gateway, Role::Worker],
    )
    .await;
    // 모델이 선언한 도구를 입력 스키마에 어긋난 인자(`bogus`는 선언에 없다)로 부른다 → 브로커가 `Invalid`로 거절하고, 사전 판정이 «실행 전에 확정된 거절»임을 확인해
    // 프런트가 표식을 남긴다. 거절을 본 모델은 마무리 답을 낸다.
    rig.script(tool_turn(
        "msg_w1",
        "toolu_w1",
        "mcp__relay__agenda_list",
        r#"{"bogus":1}"#,
    ));
    rig.script(final_turn("msg_w2", "도구 인자가 잘못돼 조회하지 못했어요"));
    let out = rig.chat("w1", "일정을 알려 줘").await;
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("조회하지 못했어요"), "{}", said(&out));
    assert_eq!(
        rig.query("select count(*) from {s}.bot_steps where key like 'cli-refusal-%'"),
        "1",
        "확정된 거절의 표식이 run 체크포인트에 남아야 해요"
    );
    assert_eq!(
        rig.query("select body from {s}.bot_steps where key like 'cli-refusal-%'"),
        "\"invalid\""
    );
    // 도구 서버는 한 번도 실행되지 않았다(거절은 실행 전이다)
    assert_eq!(
        rig.query("select count(*) from {s}.bot_calls where state = 'started'"),
        "0"
    );
}
