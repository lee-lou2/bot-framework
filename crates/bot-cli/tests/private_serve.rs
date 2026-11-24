//! 민감 DM(`PrivateIntake`)의 호스트 조립 수락 시험: 가짜 Slack DM → 게이트웨이의 `PrivateGate`(전용 원장·지문 키·도구 실행기) → 이 프로세스의 srt 샌드박스에서
//! 지정한 도구 실행. 본문은 도구에만 간다(도구는 기대한 원문일 때만 성공으로 답한다). 일반 큐·모델은 쓰지 않고 저장소의 어떤 표에도 원문이 없다.
//! 실제 PG·실제 `srt`·`python3`가 있어야 하고 없으면 건너뛴다. 모델 호출 0.
mod claude_support;
mod process_support;

use bot_broker::testing::FakeSecrets;
use bot_cli::cli::Role;
use bot_gateway::testing::slack::{FakeSlack, dm_frame};
use claude_support::*;
use process_support::{Schema, have_srt_and_python, package};
use std::{
    fs::{File, TryLockError},
    path::PathBuf,
    sync::Arc,
};

/// 이 원문일 때만 도구가 성공으로 답한다
const SECRET_TEXT: &str = "TOP-SECRET-DM-TEXT-7731";

const SERVER: &str = r#"#!/usr/bin/env python3
import json, sys

EXPECTED = "TOP-SECRET-DM-TEXT-7731"
TOOLS = [{"name": "receive", "description": "받은 글을 전달한다",
          "inputSchema": {"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}, "additionalProperties": False}}]

def reply(message_id, **body):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message_id, **body}) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    if "id" not in message:
        continue
    method, identity = message.get("method"), message["id"]
    if method == "initialize":
        reply(identity, result={"protocolVersion": message["params"].get("protocolVersion", "2025-06-18"),
                                "capabilities": {"tools": {"listChanged": False}}, "serverInfo": {"name": "inbox", "version": "0"}})
    elif method == "ping":
        reply(identity, result={})
    elif method == "tools/list":
        reply(identity, result={"tools": TOOLS})
    elif method == "tools/call":
        text = (message["params"].get("arguments") or {}).get("text")
        ok = text == EXPECTED
        reply(identity, result={"content": [{"type": "text", "text": "ok" if ok else "wrong"}], "isError": not ok})
    else:
        reply(identity, error={"code": -32601, "message": "method not found"})
"#;

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run, inbox.write]
members:
  - {identity: "slack:home/U1", roles: [owner]}
bindings:
  - {id: dm, adapter: slack, workspace: home, dm: true, agent: main}
secrets: []
policy:
  scopes: [run, inbox.write]
  approve: []
"#;

const TOOLS: &str = "version: 1\nserver: inbox\ntools:\n  - id: receive\n    class: write\n    scopes: [inbox.write]\n    secrets: {}\n    egress: []\n    input: {type: object, required: [text], properties: {text: {type: string}}, additionalProperties: false}\n";
const TRIGGERS: &str = "version: 1\ntriggers:\n  - id: secure\n    \"on\": {event: message, binding: dm}\n    private: true\n    target: {tool: inbox.receive}\n    input: {text: body.text}\n";

fn secrets() -> Arc<FakeSecrets> {
    Arc::new(FakeSecrets::new(&[
        ("personal", "model-key", "provider-key-for-personal"),
        ("personal", "bot-token", "xoxb-test-token"),
        ("personal", "app-token", "xapp-test-token"),
        // 프로필별 지문 키: 없으면 민감 접수를 열지 않는다
        ("personal", "private-intake", "profile-intake-key-bytes"),
    ]))
}

/// 스키마의 모든 표를 글로 펼쳐 이 표지가 있는 행 수를 센다.
fn rows_containing(rig: &Rig, needle: &str) -> usize {
    let tables = rig.query(
        "select string_agg(table_name::text, ',') from information_schema.tables \
         where table_schema = '{s}' and table_type = 'BASE TABLE'",
    );
    tables
        .split(',')
        .map(|table| {
            rig.query(&format!(
                "select count(*) from {{s}}.\"{table}\" t where t::text like '%{needle}%'"
            ))
            .parse::<usize>()
            .unwrap()
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_dm_reaches_only_the_designated_tool_in_the_srt_sandbox_and_leaves_no_plaintext()
{
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    if !have_srt_and_python() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("p")
        .tempdir_in("/tmp")
        .unwrap();
    let package = package(dir.path(), SERVER);
    let slack = FakeSlack::start().await;
    let rig = Rig::launch(
        schema,
        PathBuf::from("/nonexistent/claude"),
        PROFILE,
        &[
            ("agents/main/skills/inbox/SKILL.md", "받은 글을 전달한다."),
            ("agents/main/skills/inbox/tools.yaml", TOOLS),
            ("agents/main/triggers.yaml", TRIGGERS),
        ],
        &route_gateway(),
        &[Role::Gateway],
        secrets(),
        &format!(
            r#", "servers": {{"inbox": {{"root": "{}", "command": ["bin/server", "--stdio"]}}}}"#,
            package.display()
        ),
        Some(&slack.base),
    )
    .await;
    let mut rig = rig;
    rig.until(
        "게이트웨이가 활성 판본을 게시하지 않았어요",
        60,
        |rig| !rig.schema.active().is_empty(),
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");

    // 원문이 도구가 기대한 글이면 도구가 성공으로 답해 접수가 끝난다(Slack에도 ack)
    let first = ts(1);
    slack
        .push(dm_frame("E1", "T1", "D1", "U1", &first, SECRET_TEXT))
        .await;
    wait(&rig, "민감 접수가 끝나지 않았어요", |rig| {
        rig.query("select count(*) from {s}.bot_runs where private and state = 'succeeded'") == "1"
    })
    .await;
    assert!(
        slack
            .until(|slack| slack.acks().iter().any(|id| id == "E1"))
            .await,
        "접수를 끝낸 뒤에 ack해야 해요"
    );
    // 민감 접수를 받는 동안 도구 설치본의 사용 잠금을 쥐고 있다: 같은 설치본을 복구하려는 다른 기동이 배타 잠금을 잡을 수 없다
    let lock = std::fs::read_dir(rig.dir.path().join("installs"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "lock"))
        .expect("설치본 사용 잠금 파일");
    assert!(
        matches!(
            File::open(&lock).unwrap().try_lock(),
            Err(TryLockError::WouldBlock)
        ),
        "접수가 도는 동안에는 설치본의 배타 잠금을 잡을 수 없어야 해요"
    );
    // 일반 run은 만들어지지 않았고 저장소의 어떤 표에도 원문이 없다
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where not private"),
        "0"
    );
    assert_eq!(
        rows_containing(&rig, SECRET_TEXT),
        0,
        "원문이 저장소에 남았어요"
    );
    // 도구는 이 글 한 번만 받았다: 같은 Slack 이벤트의 재전달은 호출을 늘리지 않는다
    slack
        .push(dm_frame("E1b", "T1", "D1", "U1", &first, SECRET_TEXT))
        .await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where private"),
        "1",
        "재전달은 접수를 늘리지 않아요"
    );

    // 도구가 기대하지 않은 글은 도구가 실패로 답한다. 쓰기 도구의 실패 답은 «실행되지 않았다»를 증명하지 못하므로 접수는 성공으로 끝나지도 일반 대화로
    // 넘어가지도 않고 운영자가 볼 수 있는 보류로 남는다
    slack
        .push(dm_frame("E2", "T1", "D1", "U1", &ts(2), "다른 글이에요"))
        .await;
    wait(
        &rig,
        "두 번째 접수가 보류되지 않았어요",
        |rig| {
            rig.query(
                "select count(*) from {s}.bot_holds where kind = 'private' and closed_ms is null",
            ) == "1"
        },
    )
    .await;
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where private and state = 'succeeded'"),
        "1",
        "도구가 실패로 답한 접수는 성공이 아니에요"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs where not private"),
        "0",
        "일반 대화로 넘기지 않아요"
    );
    assert_eq!(rows_containing(&rig, "다른 글이에요"), 0);
    rig.stop().await;
    // 끝난 뒤에는 잠금이 풀린다
    assert!(
        File::open(&lock).unwrap().try_lock().is_ok(),
        "종료 뒤에는 설치본의 배타 잠금을 잡을 수 있어야 해요"
    );
}
