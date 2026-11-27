//! 내장 `delegate`: 직계 자식에게만, 현재·고정 판본의 범위 안에서, 같은 단계는 같은 자식으로. 대기에 들어가면 그 시도의 세션만 거둔다.

use super::*;
use crate::front::{cli::PlannedCalls, park::ParkReason};

const SCOPES: [&str; 3] = ["run", "echo.use", "echo.drop"];

/// `main` 아래에 직계 자식 `main/research`와 손자 `main/research/deep`, 형제 `main/ops`가 있는 판본.
pub(super) fn family(revision: &str) -> crate::testing::SnapshotBuilder {
    snapshot("personal", revision)
        .role("owner", &SCOPES)
        .member("local:owner", &["owner"])
        .policy_scopes(&SCOPES)
        .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
        .tool(
            "main",
            "echo.drop",
            tool(Risk::Irreversible, &["echo.drop"]),
        )
        .agent("main/research")
        .agent("main/research/deep")
        .agent("main/ops")
}

pub(super) async fn with_family() -> Rig {
    let r = rig_with(
        |_| {},
        |_| {},
        Some(Box::new(|ledger, _| {
            Arc::new(
                PlannedCalls::new(ledger.clone(), Duration::from_millis(200))
                    .with_poll(Duration::from_millis(10)),
            )
        })),
    )
    .await;
    r.catalog.publish(family("r1").build());
    r
}

pub(super) fn delegate(id: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"delegate","arguments":{args},"_meta":{{"claudecode/toolUseId":"{id}"}}}}}}"#
    )
}

/// ②가 남기는 기록을 흉내 낸다(위임도 같은 계획 대조를 거친다).
pub(super) fn plan(r: &Rig, turn: u64, id: &str, input: &str) {
    r.ledger.add_step(
        "r1",
        &format!("cli-use-{id}"),
        &format!(
            r#"{{"seq":{turn},"turn":{turn},"pos":0,"name":"mcp__relay__delegate","input":{input}}}"#
        ),
    );
    r.ledger.add_step(
        "r1",
        &format!("cli-group-{turn}"),
        &format!(r#"{{"seq":{turn},"uses":["{id}"]}}"#),
    );
}

#[tokio::test]
async fn the_tool_list_names_exactly_the_direct_children() {
    let r = with_family().await;
    let list = rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
    let value: serde_json::Value = serde_json::from_str(&list.body).unwrap();
    let tool = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "delegate")
        .expect("delegate");
    assert_eq!(
        tool["inputSchema"]["properties"]["agent"]["enum"],
        serde_json::json!(["main/ops", "main/research"]),
        "손자는 보이지 않는다"
    );
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    // 자식이 없으면 도구도 없다
    let r = plain().await;
    assert!(
        !rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .body
            .contains("delegate")
    );
}

#[tokio::test]
async fn a_delegation_starts_the_child_once_parks_the_attempt_and_retires_only_its_session() {
    let r = with_family().await;
    let input = r#"{"agent":"main/research","instruction":"summarize the week"}"#;
    plan(&r, 4, "toolu_d", input);
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(reply.body.contains("child_pending"), "{}", reply.body);
    let commands = r.delegation.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        (
            commands[0].step.as_str(),
            commands[0].child.0.as_str(),
            commands[0].instruction.as_str()
        ),
        ("cli-4-0", "main/research", "summarize the week"),
        "단계 키는 논리 호출 ID 그대로"
    );
    assert_eq!(commands[0].parent, r.attempt);
    assert!(
        commands[0]
            .scopes
            .iter()
            .all(|s| SCOPES.contains(&s.as_str()))
    );
    assert_eq!(r.park.seen(), [(r.attempt.clone(), ParkReason::Child)]);
    // 대기에 들어간 시도의 세션은 거둬졌다(CLI가 모델을 더 부를 수 없다)
    assert_eq!(rpc(&r, &delegate("toolu_x", input)).await.status, 401);
    // 같은 단계의 재전달(재개 뒤)은 새 자식을 만들지 않는다: 새 claim의 세션으로 같은 단계를 다시 보내도 저장소가 같은 자식으로 본다
    assert_eq!(r.delegation.commands().len(), 1);
}

#[tokio::test]
async fn only_a_direct_child_in_both_revisions_may_be_delegated_to() {
    let r = with_family().await;
    let mut n = 0;
    for (agent, word) in [
        ("main/research/deep", "denied"), // 손자
        ("main/ops/none", "denied"),      // 없는 에이전트
        ("main", "denied"),               // 자기 자신
        ("other", "denied"),
    ] {
        n += 1;
        let input = format!(r#"{{"agent":"{agent}","instruction":"x"}}"#);
        plan(&r, n, &format!("toolu_{n}"), &input);
        let reply = rpc(&r, &delegate(&format!("toolu_{n}"), &input)).await;
        assert!(reply.body.contains(word), "{agent}: {}", reply.body);
    }
    // 형식이 틀린 인자
    for (i, input) in [
        r#"{"agent":"main/research"}"#,
        r#"{"agent":"main/research","instruction":""}"#,
        r#"{"agent":"main/research","instruction":"x","extra":1}"#,
        r#"{"agent":5,"instruction":"x"}"#,
    ]
    .iter()
    .enumerate()
    {
        let id = format!("toolu_f{i}");
        plan(&r, 20 + i as u64, &id, input);
        let reply = rpc(&r, &delegate(&id, input)).await;
        assert!(reply.body.contains("invalid"), "{input}: {}", reply.body);
    }
    assert!(r.delegation.commands().is_empty());
    assert!(r.park.seen().is_empty());
    // 실행이 고정한 판본에 그 자식이 없었다면(현재 판본에서 새로 생긴 에이전트) 거절한다
    let pinned = snapshot("personal", "r1")
        .role("owner", &SCOPES)
        .member("local:owner", &["owner"])
        .policy_scopes(&SCOPES)
        .agent("main/ops")
        .build();
    r.catalog.publish(pinned);
    r.catalog.publish(family("r2").build());
    r.ledger.set_active_revision("personal", "r2");
    let input = r#"{"agent":"main/research","instruction":"x"}"#;
    plan(&r, 40, "toolu_p", input);
    assert!(
        rpc(&r, &delegate("toolu_p", input))
            .await
            .body
            .contains("denied")
    );
    assert!(r.delegation.commands().is_empty());
}

#[tokio::test]
async fn the_child_gets_only_the_scopes_the_requester_holds_in_both_revisions() {
    let r = with_family().await;
    // 요청자 권한은 `echo.use`만이다(접수 때 고정): 자식에게 `echo.drop`을 넘기지 못한다
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run", "echo.use"],
    ));
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    plan(&r, 5, "toolu_s", input);
    rpc(&r, &delegate("toolu_s", input)).await;
    let scopes: Vec<String> = r.delegation.commands()[0].scopes.iter().cloned().collect();
    assert_eq!(scopes, ["echo.use", "run"]);
    // 지금 판본이 요청자의 역할에서 스코프를 거두면 그만큼 줄어든다
    let r = with_family().await;
    r.catalog
        .publish(family("r1").role("owner", &["run"]).build());
    plan(&r, 5, "toolu_s", input);
    rpc(&r, &delegate("toolu_s", input)).await;
    assert_eq!(
        r.delegation.commands()[0]
            .scopes
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["run"]
    );
}

/// 이전 시도가 이미 자식을 시작해 두었고(저장소에 기록) 자식이 끝나 결과가 부모 체크포인트에 들어온 상태.
/// `scopes`는 그때 자식에게 넘긴 범위다.
async fn finished_child(r: &Rig, instruction: &str, scopes: &[&str], body: &str) {
    let command = bot_core::command::Delegate {
        parent: r.attempt.clone(),
        checked_revision: Revision("r1".into()),
        step: "cli-4-0".into(),
        child: AgentId("main/research".into()),
        instruction: instruction.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
    };
    use bot_core::port::DelegationStore;
    r.delegation.delegate(command).await.unwrap();
    r.delegation.finish_child("personal", "r1", "cli-4-0", body);
    r.ledger.add_step("r1", "cli-4-0", body);
}

#[tokio::test]
async fn a_finished_child_returns_its_result_to_the_same_tool_use() {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "go",
        &SCOPES,
        r#"{"state":"succeeded","result":"the week was calm"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("the week was calm") && reply.body.contains(r#""isError":false"#),
        "{}",
        reply.body
    );
    assert_eq!(r.delegation.commands().len(), 1, "다시 시작하지 않는다");
    assert!(r.park.seen().is_empty());
    // 실패·취소한 자식은 고정 오류 단어만
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "go",
        &SCOPES,
        r#"{"state":"dead","error":"secret-internal-detail"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("child_failed") && !reply.body.contains("secret-internal-detail"),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_finished_childs_result_is_hidden_when_the_revision_moved_in_the_store() {
    let r = with_family().await;
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    plan(&r, 4, "toolu_d", input);
    finished_child(&r, "go", &SCOPES, r#"{"state":"succeeded","result":"x"}"#).await;
    r.delegation.set_active("r2"); // 다른 프로세스가 새 판본을 게시했다
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("stale") && !reply.body.contains("\"x\""),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_stored_child_result_needs_the_original_scopes_inside_the_current_ones_and_the_same_target()
 {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    // 처음 넘긴 범위가 지금 계산한 범위보다 넓다(권한이 줄었다): 결과를 내주지 않는다
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "go",
        &["run", "echo.use", "echo.drop", "calendar.admin"],
        r#"{"state":"succeeded","result":"secret result"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("denied") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
    // 같은 단계인데 지시가 달라졌다
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    finished_child(
        &r,
        "something else",
        &SCOPES,
        r#"{"state":"succeeded","result":"secret result"}"#,
    )
    .await;
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("conflict") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
    // 체크포인트에는 있는데 저장소는 아직 끝나지 않았다고 한다: 결과를 지어내지 않는다
    let r = with_family().await;
    plan(&r, 4, "toolu_d", input);
    r.ledger
        .add_step("r1", "cli-4-0", r#"{"state":"succeeded","result":"early"}"#);
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(!reply.body.contains("early"), "{}", reply.body);
}

#[tokio::test]
async fn delegation_needs_the_planned_call_and_a_current_session() {
    let r = with_family().await;
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    // 계획이 없다
    assert!(
        rpc(&r, &delegate("toolu_d", input))
            .await
            .body
            .contains("unavailable")
    );
    // 계획의 인자와 다르다
    plan(
        &r,
        4,
        "toolu_e",
        r#"{"agent":"main/ops","instruction":"go"}"#,
    );
    assert!(
        rpc(&r, &delegate("toolu_e", input))
            .await
            .body
            .contains("conflict")
    );
    assert!(r.delegation.commands().is_empty());
    // claim이 넘어간 옛 시도
    plan(&r, 5, "toolu_f", input);
    r.ledger.bump_claim("r1");
    assert!(
        rpc(&r, &delegate("toolu_f", input))
            .await
            .body
            .contains("stale")
    );
    assert!(r.delegation.commands().is_empty());
}

#[tokio::test]
async fn an_approval_or_a_hold_parks_the_attempt_and_retires_its_session_but_not_another_claims() {
    let r = with_family().await;
    let drop = r#"{"agent":"x"}"#;
    let _ = drop;
    // 승인이 필요한 도구 → 승인 대기
    let call = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo_drop","arguments":{},"_meta":{"claudecode/toolUseId":"toolu_g"}}}"#;
    r.ledger.add_step(
        "r1",
        "cli-use-toolu_g",
        r#"{"seq":2,"turn":2,"pos":0,"name":"mcp__relay__echo_drop","input":{}}"#,
    );
    r.ledger
        .add_step("r1", "cli-group-2", r#"{"seq":2,"uses":["toolu_g"]}"#);
    // 같은 run의 다른 claim이 이미 발급받은 세션(새 워커)은 거두지 않는다
    let mut newer = r.attempt.clone();
    newer.claim = Some(bot_core::id::ClaimEpoch(2));
    let other = r
        .sessions
        .issue(Session {
            attempt: newer.clone(),
            agent: AgentId("main".into()),
            alias: "assistant".into(),
        })
        .unwrap();
    let reply = rpc(&r, call).await;
    assert!(reply.body.contains("approval_pending"), "{}", reply.body);
    assert_eq!(r.park.seen(), [(r.attempt.clone(), ParkReason::Approval)]);
    assert_eq!(rpc(&r, call).await.status, 401, "이 시도의 세션은 거둬졌다");
    let token = format!("Bearer {other}");
    let still = raw(
        r.front.addr(),
        &post(
            "/mcp",
            Some(&token),
            "authorization",
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        ),
    )
    .await;
    assert_eq!(still.status, 200, "새 claim의 세션은 그대로다");
}

/// 위임이 거절됐고(고정 단어) 자식도 대기도 없었다.
fn refused(r: &Rig, body: &str, word: &str) {
    assert!(body.contains(word), "{body}");
    assert!(r.delegation.commands().is_empty());
    assert!(r.park.seen().is_empty());
}

#[tokio::test]
async fn a_requester_without_run_in_either_revision_cannot_start_or_resume_a_delegation() {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    // 지금 판본에서 요청자의 역할이 `run`을 주지 않는다: 새 자식도, 저장된 자식 결과도 나가지 않는다
    let r = with_family().await;
    r.catalog.publish(
        family("r1")
            .role("owner", &["echo.use", "echo.drop"])
            .build(),
    );
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
    r.ledger.add_step(
        "r1",
        "cli-4-0",
        r#"{"state":"succeeded","result":"secret result"}"#,
    );
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("denied") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
    // 실행이 접수 때 고정한 범위에 `run`이 없다
    let r = with_family().await;
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["echo.use", "echo.drop"],
    ));
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
    // 실행이 고정한 판본에서는 `run`이 없었다(지금 판본에는 있다)
    let r = with_family().await;
    r.catalog.publish(
        family("r1")
            .role("owner", &["echo.use", "echo.drop"])
            .build(),
    );
    r.catalog.publish(family("r2").build());
    r.ledger.set_active_revision("personal", "r2");
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
}

#[tokio::test]
async fn a_requester_who_is_no_longer_a_member_cannot_delegate_or_read_a_stored_child_result() {
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    let r = with_family().await;
    // 지금 판본에서 요청자가 멤버가 아니다
    let without_member = snapshot("personal", "r1")
        .role("owner", &SCOPES)
        .member("local:someone-else", &["owner"])
        .policy_scopes(&SCOPES)
        .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
        .agent("main/research")
        .build();
    r.catalog.publish(without_member);
    plan(&r, 4, "toolu_d", input);
    refused(
        &r,
        &rpc(&r, &delegate("toolu_d", input)).await.body,
        "denied",
    );
    r.ledger.add_step(
        "r1",
        "cli-4-0",
        r#"{"state":"succeeded","result":"secret result"}"#,
    );
    let reply = rpc(&r, &delegate("toolu_d", input)).await;
    assert!(
        reply.body.contains("denied") && !reply.body.contains("secret result"),
        "{}",
        reply.body
    );
}

#[test]
fn delegation_scopes_is_one_function_for_every_entry_point() {
    use bot_core::scope::{Denial, delegation_scopes};
    let full = family("r1").build();
    let ctx = run_context("personal", "r1", 1, "local:owner", "main", &SCOPES);
    let child = AgentId("main/research".into());
    // 성공: 실행의 범위 ∩ 두 판본의 허용 범위, `run` 포함
    let scopes = delegation_scopes(&ctx, &full, &full, &child).unwrap();
    assert!(scopes.contains("run") && scopes.contains("echo.use"));
    // 손자·없는 에이전트·자기 자신
    for other in ["main/research/deep", "main/ghost", "main"] {
        assert_eq!(
            delegation_scopes(&ctx, &full, &full, &AgentId(other.into())),
            Err(Denial::Agent),
            "{other}"
        );
    }
    // 멤버가 아니다
    let outsider = run_context("personal", "r1", 1, "local:nobody", "main", &SCOPES);
    assert_eq!(
        delegation_scopes(&outsider, &full, &full, &child),
        Err(Denial::Member)
    );
    // `run`이 없다
    let no_run = family("r1").role("owner", &["echo.use"]).build();
    assert_eq!(
        delegation_scopes(&ctx, &no_run, &full, &child),
        Err(Denial::Missing(vec!["run".into()]))
    );
    assert_eq!(
        delegation_scopes(&ctx, &full, &no_run, &child),
        Err(Denial::Missing(vec!["run".into()]))
    );
}
