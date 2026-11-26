//! MCP 중계: 협상·도구 목록·호출·거절 표현.

use super::*;

#[tokio::test]
async fn mcp_lists_tool_descriptions_from_the_snapshot() {
    let r = plain().await;
    let mut source = (*world()).clone();
    let main = source.agents.get_mut(&AgentId("main".into())).unwrap();
    let tools = &mut main.skills.get_mut("echo").unwrap().tools;
    let say = tools.get_mut(&ToolId("echo.say".into())).unwrap();
    let locked_description = "메시지를 \"그대로\" 전달합니다.\n내용을 입력하세요.";
    say.description = Some(locked_description.into());
    say.input = JsonPayload(r#"{"type":"object","description":"스키마 설명"}"#.into());
    let drop = tools.get_mut(&ToolId("echo.drop".into())).unwrap();
    drop.input = JsonPayload(r#"{"type":"object","description":"메시지를 지웁니다."}"#.into());
    let locked = tools.get_mut(&ToolId("echo.locked".into())).unwrap();
    locked.input = JsonPayload(r#"{"type":"object","description":""}"#.into());
    r.catalog.publish(Arc::new(source));

    let reply = rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
    let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    let tools = value["result"]["tools"].as_array().unwrap();
    for (name, description) in [
        ("echo_say", locked_description),
        ("echo_drop", "메시지를 지웁니다."),
        ("echo_locked", "echo.locked"),
    ] {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        assert_eq!(tool["description"], description, "{name}");
    }
}

#[tokio::test]
async fn mcp_negotiates_and_lists_only_the_agents_declared_tools_with_safe_names() {
    let r = plain().await;
    let init = rpc(&r, r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"claude-code","version":"2"}}}"#).await;
    assert_eq!(init.status, 200);
    assert!(
        init.body.contains(r#""protocolVersion":"2025-11-25""#)
            && init.body.contains(r#""tools":{}"#),
        "{}",
        init.body
    );
    let odd = rpc(
        &r,
        r#"{"jsonrpc":"2.0","id":"x","method":"initialize","params":{"protocolVersion":"latest"}}"#,
    )
    .await;
    assert!(
        odd.body.contains(r#""protocolVersion":"2025-11-25""#) && odd.body.contains(r#""id":"x""#)
    );
    let discover = rpc(&r, r#"{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{}}"#).await;
    assert!(discover.body.contains("-32601"), "{}", discover.body);
    let list = rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
    let value: serde_json::Value = serde_json::from_str(&list.body).unwrap();
    let names: Vec<&str> = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo_drop", "echo_locked", "echo_say"]);
    assert!(names.iter().all(|n| {
        n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }));
    assert_eq!(
        value["result"]["tools"][2]["inputSchema"],
        serde_json::json!({"type": "object"})
    );
    // 알림은 202, 모르는 메서드는 -32601, 깨진 본문은 400
    assert_eq!(
        rpc(
            &r,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .await
        .status,
        202
    );
    assert!(
        rpc(&r, r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#)
            .await
            .body
            .contains("-32601")
    );
    assert_eq!(rpc(&r, "not json").await.status, 400);
    assert_eq!(
        rpc(
            &r,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","id":2}"#
        )
        .await
        .status,
        400,
        "중복 키"
    );
}

#[tokio::test]
async fn a_tool_call_goes_through_the_broker_with_a_call_id_from_the_tool_use_id() {
    let r = plain().await;
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo_say","arguments":{"text":"hello","n":1.0},"_meta":{"claudecode/toolUseId":"toolu_01AbC","progressToken":2}}}"#;
    let reply = rpc(&r, call).await;
    assert_eq!(reply.status, 200);
    let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(value["id"], 2);
    assert_eq!(value["result"]["content"][0]["text"], "{\"ok\":true}");
    let jobs = r.runner.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (
            jobs[0].call.0.as_str(),
            jobs[0].tool.id.0.as_str(),
            jobs[0].args.as_str()
        ),
        ("m-toolu_01AbC", "echo.say", r#"{"n":1,"text":"hello"}"#)
    );
    // 같은 도구 사용을 다시 보내면 같은 호출이라 다시 실행하지 않는다
    rpc(&r, call).await;
    assert_eq!(r.runner.jobs().len(), 1);
    // 도구가 MCP 모양 결과를 주면 그대로 전달한다
    r.runner.script(Ran::Done(JsonPayload(
        r#"{"content":[{"text":"hi","type":"text"}],"isError":false}"#.into(),
    )));
    let second = rpc(&r, &call.replace("toolu_01AbC", "toolu_02")).await;
    assert!(second.body.contains(r#""text":"hi""#));
}

#[tokio::test]
async fn refusals_reach_the_model_as_single_words_and_never_carry_secrets_or_nonces() {
    let r = plain().await;
    let counter = std::cell::Cell::new(0u32);
    let call = |name: &str, extra: &str| {
        counter.set(counter.get() + 1);
        format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"{name}","arguments":{{}}{extra},"_meta":{{"claudecode/toolUseId":"toolu_r{}"}}}}}}"#,
            counter.get()
        )
    };
    let word = |body: &str| -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["result"]["isError"], true, "{body}");
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // 스코프가 없는 도구·없는 도구·잘못된 이름·잘못된 인자
    assert_eq!(
        word(&rpc(&r, &call("echo_locked", "")).await.body),
        "denied"
    );
    assert_eq!(
        word(&rpc(&r, &call("nope_tool", "")).await.body),
        "unknown_tool"
    );
    for bad in ["echo", "a_b_c", "echo.say", ""] {
        assert_eq!(
            word(&rpc(&r, &call(bad, "")).await.body),
            "invalid",
            "{bad}"
        );
    }
    let not_object = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"echo_say","arguments":[1],"_meta":{"claudecode/toolUseId":"toolu_n1"}}}"#;
    assert_eq!(word(&rpc(&r, not_object).await.body), "invalid");
    let pending = rpc(&r, &call("echo_drop", "")).await;
    assert_eq!(word(&pending.body), "approval_pending");
    assert!(!pending.body.contains("01010101"), "승인 nonce 노출");
    // 승인 대기에 들어간 시도의 세션으로 건넨 다음 호출은 인증 단계에서 닫힌다.
    let replay = rpc(&r, &call("echo_say", "")).await;
    assert_eq!(replay.status, 401);
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn the_tool_list_needs_a_current_claim_too_and_the_protocol_version_is_one_we_support() {
    let r = plain().await;
    assert!(
        rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .body
            .contains("echo_say")
    );
    r.ledger.bump_claim("r1"); // 같은 세대에서 새 리스: 이 세션은 옛 claim이다
    let stale = rpc(&r, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
    assert!(
        stale.body.contains("-32001") && !stale.body.contains("echo_say"),
        "{}",
        stale.body
    );
    // 날짜 모양이면 무엇이든 되돌리지 않는다: 지원 판본만 협상하고 아니면 가장 새 지원 판본으로 답한다
    for (asked, expect) in [
        ("2025-11-25", "2025-11-25"),
        ("2024-11-05", "2024-11-05"),
        ("2026-07-28", "2025-11-25"),
        ("1999-01-01", "2025-11-25"),
        ("latest", "2025-11-25"),
    ] {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"protocolVersion":"{asked}"}}}}"#
        );
        assert!(
            rpc(&r, &body)
                .await
                .body
                .contains(&format!("\"protocolVersion\":\"{expect}\"")),
            "{asked}"
        );
    }
}

#[tokio::test]
async fn a_call_without_a_stable_tool_use_id_is_refused_before_anything_runs() {
    let r = plain().await;
    for params in [
        r#"{"name":"echo_say","arguments":{}}"#,
        r#"{"name":"echo_say","arguments":{},"_meta":{"progressToken":3}}"#,
        r#"{"name":"echo_say","arguments":{},"_meta":{"claudecode/toolUseId":"bad id"}}"#,
        r#"{"name":"echo_say","arguments":{},"_meta":{"claudecode/toolUseId":""}}"#,
    ] {
        let body = format!(r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{params}}}"#);
        let reply = rpc(&r, &body).await;
        assert!(
            reply.body.contains(r#""text":"invalid""#) && reply.body.contains("isError\":true"),
            "{params}: {}",
            reply.body
        );
    }
    assert!(r.runner.jobs().is_empty(), "식별자가 없으면 실행 0");
    // 같은 식별자의 재전달은 같은 호출로 처리돼 다시 실행하지 않는다
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo_say","arguments":{"a":1},"_meta":{"claudecode/toolUseId":"toolu_x"}}}"#;
    rpc(&r, call).await;
    rpc(&r, call).await;
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn the_builtin_memory_share_tool_is_listed_only_when_the_agent_scopes_include_it() {
    let r = plain().await;
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    assert!(!rpc(&r, list).await.body.contains("memory_share"));
    // 에이전트 상한에 `memory.share`가 들어오면 보인다(스키마는 기억 ID 하나)
    let scopes = ["run", "echo.use", "echo.drop", "memory.share"];
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &scopes)
            .member("local:owner", &["owner"])
            .policy_scopes(&scopes)
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            // 같은 이름을 스킬이 선언해도 내장 도구의 이름은 예약돼 있다
            .tool("main", "memory.share", tool(Risk::Read, &["echo.use"]))
            .build(),
    );
    let reply = rpc(&r, list).await;
    let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    let tools = value["result"]["tools"].as_array().unwrap();
    assert_eq!(
        tools.iter().filter(|t| t["name"] == "memory_share").count(),
        1,
        "{}",
        reply.body
    );
    let share = tools
        .iter()
        .find(|t| t["name"] == "memory_share")
        .expect("memory_share");
    assert_eq!(share["inputSchema"]["required"], serde_json::json!(["id"]));
    assert_eq!(share["inputSchema"]["additionalProperties"], false);
    // 호출은 브로커의 기억 공유 경로로 간다: 이 실행의 고정 권한에는 스코프가 없어 거절되고, 도구 서버는 부르지 않는다
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_share","arguments":{"id":"m1"},"_meta":{"claudecode/toolUseId":"toolu_share1"}}}"#;
    let reply = rpc(&r, call).await;
    assert!(
        reply.body.contains(r#"\"text\":\"denied\""#) || reply.body.contains(r#""text":"denied""#),
        "{}",
        reply.body
    );
    assert!(r.runner.jobs().is_empty());
}

/// 도구 사용마다 처음 보는 순서대로 `c-<번호>`를 주는 원천(런타임이 체크포인트에 남기는 매핑을 흉내 낸다).
#[derive(Default)]
struct Ordinals {
    seen: std::sync::Mutex<Vec<String>>,
    down: std::sync::atomic::AtomicBool,
}

impl CallBook for Ordinals {
    fn call_id(
        &self,
        _: Attempt,
        tool_use: String,
        _: String,
        _: bot_core::command::JsonPayload,
    ) -> bot_core::port::Reply<'_, CallId> {
        Box::pin(async move {
            if self.down.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(bot_core::port::Error::Unavailable);
            }
            let mut seen = self.seen.lock().unwrap();
            let at = seen.iter().position(|t| *t == tool_use).unwrap_or_else(|| {
                seen.push(tool_use.clone());
                seen.len() - 1
            });
            Ok(CallId(format!("c-{at}")))
        })
    }
}

#[tokio::test]
async fn the_call_id_comes_from_the_hosts_call_book_and_an_unmapped_call_does_not_run() {
    let book = Arc::new(Ordinals::default());
    let shared = book.clone();
    let r = rig_with(|_| {}, |_| {}, Some(Box::new(move |_, _| shared))).await;
    let call = |tool_use: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"echo_say","arguments":{{"text":"hi"}},"_meta":{{"claudecode/toolUseId":"{tool_use}"}}}}}}"#
        )
    };
    rpc(&r, &call("toolu_a")).await;
    assert_eq!(r.runner.jobs()[0].call.0, "c-0");
    // 같은 도구 사용을 다시 보내면 같은 호출이고, 새 도구 사용은 새 호출이다
    rpc(&r, &call("toolu_a")).await;
    assert_eq!(r.runner.jobs().len(), 1);
    rpc(&r, &call("toolu_b")).await;
    assert_eq!(r.runner.jobs()[1].call.0, "c-1");
    // 원천이 호출 ID를 못 주면 실행하지 않고 고정 단어만 돌려준다
    book.down.store(true, std::sync::atomic::Ordering::SeqCst);
    let reply = rpc(&r, &call("toolu_c")).await;
    assert!(reply.body.contains("unavailable"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 2);
}

#[tokio::test]
async fn a_session_revoked_while_a_call_runs_gets_no_result_but_the_receipt_is_kept() {
    let r = plain().await;
    let gate = r.runner.hold_next();
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo_say","arguments":{"text":"hi"},"_meta":{"claudecode/toolUseId":"toolu_wait"}}}"#;
    let request = post(
        "/mcp",
        Some(&format!("Bearer {}", r.token)),
        "authorization",
        call,
    );
    let addr = r.front.addr();
    let waiting = tokio::spawn(async move { raw(addr, &request).await });
    for _ in 0..100 {
        if !r.runner.jobs().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(r.runner.jobs().len(), 1, "실행이 시작됐다");
    r.sessions.revoke(&r.token);
    gate.add_permits(1);
    let reply = waiting.await.unwrap();
    assert!(
        reply.body.contains("stale") && !reply.body.contains(r#"{\"ok\":true}"#),
        "{}",
        reply.body
    );
    // 이미 시작한 효과의 영수증은 원장에 남는다(새 claim이 같은 호출 ID로 다시 들어오면 그 결과를 본다)
    assert!(matches!(
        r.ledger.phase("r1", "m-toolu_wait"),
        bot_core::ledger::CallPhase::Done(_)
    ));
}

#[tokio::test]
async fn a_revoked_session_starts_nothing_even_if_the_request_got_in() {
    let r = plain().await;
    let (_gone_tx, gone) = tokio::sync::watch::channel(true); // 이미 회수됐다
    let session = Session {
        attempt: r.attempt.clone(),
        agent: AgentId("main".into()),
        alias: "assistant".into(),
    };
    let call = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo_say","arguments":{"text":"hi"},"_meta":{"claudecode/toolUseId":"toolu_late"}}}"#;
    let (status, body) =
        super::super::mcp::handle(&r.front.shared, &session, &gone, call.as_bytes()).await;
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("stale"));
    assert!(r.runner.jobs().is_empty());
    assert!(
        !r.ledger.has_call("r1", "m-toolu_late"),
        "호출 기록도 만들지 않는다"
    );
}

fn tool_names(body: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_schedule_tools_are_listed_only_for_the_scopes_the_agent_holds_and_the_host_id_never() {
    let r = plain().await;
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    assert!(
        !tool_names(&rpc(&r, list).await.body)
            .iter()
            .any(|n| n.starts_with("schedule_"))
    );
    // 에이전트 스코프에 create·list만 있다: 그 둘만 보이고 cancel·deliver는 보이지 않는다
    let scopes = ["run", "echo.use", "schedule.create", "schedule.list"];
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &scopes)
            .member("local:owner", &["owner"])
            .policy_scopes(&scopes)
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            // 같은 이름을 스킬이 선언해도 예약된 ID는 내장이 우선한다(목록에 한 번만)
            .tool("main", "schedule.create", tool(Risk::Read, &["echo.use"]))
            .build(),
    );
    let names = tool_names(&rpc(&r, list).await.body);
    assert_eq!(
        names.iter().filter(|n| n.starts_with("schedule_")).count(),
        2,
        "{names:?}"
    );
    assert!(names.contains(&"schedule_create".into()) && names.contains(&"schedule_list".into()));
    assert!(
        !names.contains(&"schedule_cancel".into()) && !names.contains(&"schedule_deliver".into())
    );
    // 스키마: 필수는 instruction뿐이고 모르는 키는 거절한다
    let value: serde_json::Value = serde_json::from_str(&rpc(&r, list).await.body).unwrap();
    let create = value["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "schedule_create")
        .unwrap();
    assert_eq!(
        create["inputSchema"]["required"],
        serde_json::json!(["instruction"])
    );
    assert_eq!(create["inputSchema"]["additionalProperties"], false);
    // 호스트 전용 ID는 호출해도 모델에게는 없는 도구다(계획을 기다리지도, 브로커를 부르지도 않는다)
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"schedule_deliver","arguments":{},"_meta":{"claudecode/toolUseId":"toolu_dl"}}}"#;
    let reply = rpc(&r, call).await;
    assert!(reply.body.contains("unknown_tool"), "{}", reply.body);
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn a_schedule_created_through_the_relay_goes_through_the_broker() {
    let r = plain().await;
    let scopes = ["run", "echo.use", "schedule.create", "schedule.list"];
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &scopes)
            .member("local:owner", &["owner"])
            .policy_scopes(&scopes)
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            .build(),
    );
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &scopes,
    ));
    let create = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"schedule_create","arguments":{"at":"1970-01-02T12:00:00Z","instruction":"check the mail"},"_meta":{"claudecode/toolUseId":"toolu_sc1"}}}"#;
    let reply = rpc(&r, create).await;
    assert!(reply.body.contains("sched-001"), "{}", reply.body);
    let list = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"schedule_list","arguments":{},"_meta":{"claudecode/toolUseId":"toolu_sc2"}}}"#;
    let reply = rpc(&r, list).await;
    assert!(
        reply.body.contains("sched-001") && !reply.body.contains("check the mail"),
        "{}",
        reply.body
    );
    // 스코프가 없는 취소는 거절된다
    let cancel = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"schedule_cancel","arguments":{"id":"sched-001"},"_meta":{"claudecode/toolUseId":"toolu_sc3"}}}"#;
    assert!(rpc(&r, cancel).await.body.contains("denied"));
    assert!(r.runner.jobs().is_empty(), "도구 서버를 거치지 않는다");
}
