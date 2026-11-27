//! Tests for MCP relay negotiation, tool listing, calls, and fixed refusal words.

use super::*;

#[tokio::test]
async fn mcp_lists_tool_descriptions_from_the_snapshot() {
    let r = plain().await;
    let mut source = (*world()).clone();
    let main = source.agents.get_mut(&AgentId("main".into())).unwrap();
    let tools = &mut main.skills.get_mut("echo").unwrap().tools;
    let say = tools.get_mut(&ToolId("echo.say".into())).unwrap();
    let locked_description = "Forwards the message \"verbatim\".\nEnter your text.";
    say.description = Some(locked_description.into());
    say.input = JsonPayload(r#"{"type":"object","description":"Schema description"}"#.into());
    let drop = tools.get_mut(&ToolId("echo.drop".into())).unwrap();
    drop.input = JsonPayload(r#"{"type":"object","description":"Deletes the message."}"#.into());
    let locked = tools.get_mut(&ToolId("echo.locked".into())).unwrap();
    locked.input = JsonPayload(r#"{"type":"object","description":""}"#.into());
    locked.scopes = ["echo.use".into()].into();
    r.catalog.publish(Arc::new(source));

    let reply = rpc(&r, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
    let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    let tools = value["result"]["tools"].as_array().unwrap();
    for (name, description) in [
        ("echo_say", locked_description),
        ("echo_drop", "Deletes the message."),
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
    assert_eq!(names, ["echo_drop", "echo_say"]);
    assert!(names.iter().all(|n| {
        n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }));
    assert_eq!(
        value["result"]["tools"][1]["inputSchema"],
        serde_json::json!({"type": "object"})
    );
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
        "duplicate key"
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
    rpc(&r, call).await;
    assert_eq!(r.runner.jobs().len(), 1);
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
    assert!(!pending.body.contains("01010101"), "approval nonce leaked");
    // Entering approval wait revokes the session so subsequent calls fail auth.
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
    r.ledger.bump_claim("r1");
    let stale = rpc(&r, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
    assert!(
        stale.body.contains("-32001") && !stale.body.contains("echo_say"),
        "{}",
        stale.body
    );
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
    assert!(
        r.runner.jobs().is_empty(),
        "must not execute without toolUseId"
    );
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo_say","arguments":{"a":1},"_meta":{"claudecode/toolUseId":"toolu_x"}}}"#;
    rpc(&r, call).await;
    rpc(&r, call).await;
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn the_builtin_memory_share_tool_is_listed_only_when_effective_scopes_include_it() {
    let r = plain().await;
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    assert!(!rpc(&r, list).await.body.contains("memory_share"));
    // Present in snapshot policy, absent from the run's pinned `ctx.scopes`.
    let scopes = ["run", "echo.use", "echo.drop", "memory.share"];
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &scopes)
            .member("local:owner", &["owner"])
            .policy_scopes(&scopes)
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            // Built-in name stays reserved even if a skill declares the same ID.
            .tool("main", "memory.share", tool(Risk::Read, &["echo.use"]))
            .build(),
    );
    assert!(
        !tool_names(&rpc(&r, list).await.body).contains(&"memory_share".to_owned()),
        "builtin must not be listed when absent from pinned run scopes"
    );
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_share","arguments":{"id":"m1"},"_meta":{"claudecode/toolUseId":"toolu_share1"}}}"#;
    let reply = rpc(&r, call).await;
    assert!(
        reply.body.contains(r#"\"text\":\"denied\""#) || reply.body.contains(r#""text":"denied""#),
        "{}",
        reply.body
    );
    assert!(r.runner.jobs().is_empty());
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &scopes,
    ));
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
}

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
    rpc(&r, &call("toolu_a")).await;
    assert_eq!(r.runner.jobs().len(), 1);
    rpc(&r, &call("toolu_b")).await;
    assert_eq!(r.runner.jobs()[1].call.0, "c-1");
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
    assert_eq!(r.runner.jobs().len(), 1, "execution started");
    r.sessions.revoke(&r.token);
    gate.add_permits(1);
    let reply = waiting.await.unwrap();
    assert!(
        reply.body.contains("stale") && !reply.body.contains(r#"{\"ok\":true}"#),
        "{}",
        reply.body
    );
    assert!(matches!(
        r.ledger.phase("r1", "m-toolu_wait"),
        bot_core::ledger::CallPhase::Done(_)
    ));
}

#[tokio::test]
async fn a_revoked_session_starts_nothing_even_if_the_request_got_in() {
    let r = plain().await;
    let (_gone_tx, gone) = tokio::sync::watch::channel(true);
    let session = Session {
        attempt: r.attempt.clone(),
        agent: AgentId("main".into()),
        alias: "assistant".into(),
        protocol: Protocol::Anthropic,
    };
    let call = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo_say","arguments":{"text":"hi"},"_meta":{"claudecode/toolUseId":"toolu_late"}}}"#;
    let (status, body) =
        super::super::mcp::handle(&r.front.shared, &session, &gone, call.as_bytes()).await;
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("stale"));
    assert!(r.runner.jobs().is_empty());
    assert!(
        !r.ledger.has_call("r1", "m-toolu_late"),
        "must not record a call for a revoked session"
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
    let scopes = ["run", "echo.use", "schedule.create", "schedule.list"];
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &scopes)
            .member("local:owner", &["owner"])
            .policy_scopes(&scopes)
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            // Built-in takes precedence over a skill declaring the same ID.
            .tool("main", "schedule.create", tool(Risk::Read, &["echo.use"]))
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
    let cancel = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"schedule_cancel","arguments":{"id":"sched-001"},"_meta":{"claudecode/toolUseId":"toolu_sc3"}}}"#;
    assert!(rpc(&r, cancel).await.body.contains("denied"));
    assert!(
        r.runner.jobs().is_empty(),
        "must not invoke any tool server"
    );
}

#[tokio::test]
async fn tools_list_filters_skills_and_builtins_by_effective_scopes_and_agent_ceiling() {
    let r = plain().await;
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let all = ["run", "echo.use", "echo.drop", "memory.share"];
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &all)
            .member("local:owner", &["owner"])
            .policy_scopes(&all)
            .agent_limit("main", &["run", "echo.use"])
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            .tool(
                "main",
                "echo.drop",
                tool(Risk::Irreversible, &["echo.drop"]),
            )
            .build(),
    );
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &all,
    ));
    assert_eq!(tool_names(&rpc(&r, list).await.body), ["echo_say"]);
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &all)
            .member("local:owner", &["owner"])
            .policy_scopes(&all)
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            .tool(
                "main",
                "echo.drop",
                tool(Risk::Irreversible, &["echo.drop"]),
            )
            .build(),
    );
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &["run", "echo.use"],
    ));
    assert_eq!(tool_names(&rpc(&r, list).await.body), ["echo_say"]);
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &all)
            .policy_scopes(&all)
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            .build(),
    );
    assert!(tool_names(&rpc(&r, list).await.body).is_empty());
    let manage = ["run", "echo.use", "skill.inspect", "skill.create"];
    let with_manage = |admin: bool| {
        let mut b = snapshot("personal", "r1")
            .role("owner", &manage)
            .member("local:owner", &["owner"])
            .policy_scopes(&manage)
            .agent_limit("main", &["run", "echo.use", "skill.inspect"])
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]));
        if admin {
            b = b.super_admin("local:owner");
        }
        b.build()
    };
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &manage,
    ));
    r.catalog.publish(with_manage(false));
    assert_eq!(tool_names(&rpc(&r, list).await.body), ["echo_say"]);
    r.catalog.publish(with_manage(true));
    assert_eq!(
        tool_names(&rpc(&r, list).await.body),
        ["echo_say", "skill_inspect"]
    );
}
