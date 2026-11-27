use super::*;

fn parse_text(doc: Value) -> (Option<Vec<Arc<Tool>>>, Vec<&'static str>) {
    let mut issues = Issues::new(50);
    let parsed = parse(
        &doc,
        "agenda",
        "tools.yaml",
        &Limits::default(),
        None,
        &mut issues,
    );
    (parsed, issues.items().iter().map(|i| i.code).collect())
}

fn doc(tool: Value) -> Value {
    json!({"version": 1, "server": "demo", "tools": [tool]})
}

fn tool(extra: Value) -> Value {
    let mut base = json!({
        "id": "send", "class": "write", "scopes": ["mail.send"],
        "input": {"type": "object", "properties": {"to": {"type": "string"}, "message": {"type": "string"}}, "additionalProperties": false}
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

#[test]
fn builds_tools_with_revision_and_schema() {
    let (parsed, codes) = parse_text(doc(tool(
        json!({"destination": {"recipient": "args.to", "content": "args.message"}, "egress": ["mail.test:443"]}),
    )));
    assert!(codes.is_empty(), "{codes:?}");
    let tools = parsed.unwrap();
    let t = &tools[0];
    assert_eq!(t.id.0, "agenda.send");
    assert_eq!(t.risk(), Risk::Irreversible);
    assert_eq!(t.revision.0.len(), 64);
    assert!(
        t.schema
            .validate(&json!({"to": "a", "message": "b"}))
            .is_empty()
    );
    assert!(!t.schema.validate(&json!({"to": 1})).is_empty());
    let (other, _) = parse_text(doc(tool(json!({"scopes": ["mail.other"]}))));
    assert_ne!(other.unwrap()[0].revision, t.revision);
}

#[test]
fn rejects_bad_declarations() {
    assert_eq!(
        parse_text(doc(tool(
            json!({"destination": {"recipient": "args.nope", "content": "args.message"}})
        )))
        .1,
        ["bad-destination"]
    );
    assert_eq!(
        parse_text(doc(tool(json!({"class": "read", "destination": "reply"})))).1,
        ["destination-on-read"]
    );
    assert_eq!(
        parse_text(doc(tool(json!({"egress": ["a.test:0"]})))).1,
        ["bad-egress"]
    );
    assert_eq!(
        parse_text(doc(tool(json!({"egress": ["-a.test:443"]})))).1,
        ["bad-egress"]
    );
    assert_eq!(
        parse_text(doc(tool(json!({"egress": ["a..test:443"]})))).1,
        ["bad-egress"]
    );
    assert_eq!(
        parse_text(doc(tool(json!({"input": {"properties": {}}})))).1,
        ["bad-input-schema"]
    );
    assert_eq!(
        parse_text(doc(tool(
            json!({"input": {"type": "object", "patternProperties": {}}})
        )))
        .1,
        ["bad-input-schema"]
    );
    let dup = json!({"version": 1, "server": "s", "tools": [tool(json!({})), tool(json!({}))]});
    assert_eq!(parse_text(dup).1, ["duplicate-tool"]);
}

#[test]
fn reply_destination_is_allowed_on_write_tools() {
    let (parsed, codes) = parse_text(doc(tool(json!({"destination": "reply"}))));
    assert!(codes.is_empty());
    assert!(matches!(
        parsed.unwrap()[0].destination,
        Some(Destination::Reply)
    ));
}

#[test]
fn only_the_exact_builtin_tool_id_is_reserved() {
    let named = |skill: &str, tool: &str| {
        let doc = doc(tool_named(tool));
        let mut issues = Issues::new(10);
        let parsed = parse(
            &doc,
            skill,
            "tools.yaml",
            &Limits::default(),
            None,
            &mut issues,
        );
        (
            parsed.is_some(),
            issues.items().iter().map(|i| i.code).collect::<Vec<_>>(),
        )
    };
    assert_eq!(named("memory", "share"), (false, vec!["reserved-tool"]));
    for tool in ["create", "list", "cancel", "deliver"] {
        assert_eq!(
            named("schedule", tool),
            (false, vec!["reserved-tool"]),
            "schedule.{tool}"
        );
    }
    for tool in ["get", "update", "delete"] {
        assert_eq!(named("schedule", tool), (true, vec![]), "schedule.{tool}");
    }
    assert_eq!(named("calendar", "create"), (true, vec![]));
    assert_eq!(named("schedule-notes", "create"), (true, vec![]));
    assert_eq!(
        named("memory", "recall"),
        (true, vec![]),
        "memory 스킬 자체는 허용"
    );
    assert_eq!(
        named("mail", "share"),
        (true, vec![]),
        "같은 도구 이름은 허용"
    );
    assert_eq!(named("memory-notes", "share"), (true, vec![]));
}

fn tool_named(name: &str) -> Value {
    json!({"id": name, "class": "read", "scopes": ["x.read"],
           "input": {"type": "object", "properties": {}, "additionalProperties": false}})
}

fn locked(server: &str, id: &str, description: &str, schema: Value) -> Lock {
    locked_as(server, id, id, description, schema)
}

/// 서버가 `name`으로 내세우는 도구를 `id`로 묶은 잠금.
fn locked_as(server: &str, id: &str, name: &str, description: &str, schema: Value) -> Lock {
    let sha = bot_core::mcp::definition_sha256(name, description, &schema).unwrap();
    Lock {
        path: "skills/agenda/tools.lock.yaml".into(),
        server: server.into(),
        tools: [(
            id.to_owned(),
            crate::lock::Entry {
                name: name.into(),
                description: description.into(),
                input_schema: schema,
                sha256: sha,
            },
        )]
        .into(),
    }
}

fn parse_locked(doc: Value, lock: Option<&Lock>) -> (Option<Vec<Arc<Tool>>>, Vec<&'static str>) {
    let mut issues = Issues::new(50);
    let parsed = parse(
        &doc,
        "agenda",
        "tools.yaml",
        &Limits::default(),
        lock,
        &mut issues,
    );
    (parsed, issues.items().iter().map(|i| i.code).collect())
}

fn bare() -> Value {
    json!({"id": "send", "class": "read", "scopes": ["mail.read"]})
}

fn remote_schema() -> Value {
    json!({"type": "object", "properties": {"q": {"type": "string"}}, "additionalProperties": false})
}

#[test]
fn an_omitted_input_comes_from_the_lock_and_its_definition_binds_into_the_revision() {
    let lock = locked("demo", "send", "보낸다", remote_schema());
    let (parsed, codes) = parse_locked(doc(bare()), Some(&lock));
    assert!(codes.is_empty(), "{codes:?}");
    let tools = parsed.unwrap();
    let tool = &tools[0];
    assert_eq!(
        tool.definition.as_deref(),
        Some(lock.tools["send"].sha256.as_str())
    );
    assert!(tool.schema.validate(&json!({"q": "a"})).is_empty());
    assert!(!tool.schema.validate(&json!({"q": 1})).is_empty());
    // 같은 스키마라도 설명이 바뀌면(정의 해시가 바뀌면) 도구 판본이 달라진다
    let described = locked("demo", "send", "다른 설명", remote_schema());
    let changed = parse_locked(doc(bare()), Some(&described)).0.unwrap();
    assert_ne!(changed[0].revision, tool.revision);
    // 같은 스키마를 인라인으로 적은 도구와도 판본이 다르다(해시가 고정돼 있다)
    let mut inline = bare();
    inline["input"] = remote_schema();
    let inline = parse_locked(doc(inline), None).0.unwrap();
    assert_eq!(inline[0].definition, None);
    assert_ne!(inline[0].revision, tool.revision);
}

#[test]
fn an_inline_input_wins_and_omitting_both_is_an_error() {
    let other = json!({"type": "object", "properties": {"zzz": {"type": "string"}}});
    let lock = locked("demo", "send", "d", other);
    let mut inline = bare();
    inline["input"] = remote_schema();
    let (parsed, codes) = parse_locked(doc(inline), Some(&lock));
    assert!(codes.is_empty(), "{codes:?}");
    let tools = parsed.unwrap();
    assert!(tools[0].definition.is_none());
    assert!(tools[0].schema.validate(&json!({"q": "a"})).is_empty());
    assert!(!tools[0].schema.validate(&json!({"zzz": "a"})).is_empty());
    // 잠금도 input도 없다
    assert_eq!(parse_locked(doc(bare()), None).1, ["missing-input"]);
    // 잠금에 이 도구가 없다(다른 항목은 쓰이지 않는다고 알린다)
    let elsewhere = locked("demo", "other", "d", remote_schema());
    let (parsed, codes) = parse_locked(doc(bare()), Some(&elsewhere));
    assert!(parsed.is_none());
    assert_eq!(codes, ["missing-input", "lock-unused"]);
}

#[test]
fn a_lock_of_another_server_is_refused_and_an_unused_entry_only_warns() {
    let foreign = locked("elsewhere", "send", "d", remote_schema());
    let (parsed, codes) = parse_locked(doc(bare()), Some(&foreign));
    assert!(parsed.is_none());
    assert_eq!(codes, ["lock-server-mismatch"]);
    let mut lock = locked("demo", "send", "d", remote_schema());
    lock.tools
        .extend(locked("demo", "gone", "d", remote_schema()).tools);
    let (parsed, codes) = parse_locked(doc(bare()), Some(&lock));
    assert!(parsed.is_some());
    assert_eq!(codes, ["lock-unused"]);
}

#[test]
fn a_lock_schema_outside_the_vocabulary_is_an_error_like_an_inline_one() {
    let lock = locked(
        "demo",
        "send",
        "d",
        json!({"type": "object", "patternProperties": {}}),
    );
    let (parsed, codes) = parse_locked(doc(bare()), Some(&lock));
    assert!(parsed.is_none());
    assert_eq!(codes, ["bad-input-schema"]);
}

fn revision_of(parsed: (Option<Vec<Arc<Tool>>>, Vec<&'static str>)) -> bot_core::id::Revision {
    parsed.0.expect("받아들여야 해요")[0].revision.clone()
}

#[test]
fn a_remote_name_is_optional_binds_the_revision_and_must_match_the_lock() {
    // 이름을 id와 같게 적은 것은 쓰지 않은 것과 같다(판본도 그대로)
    let lock = locked("demo", "send", "d", remote_schema());
    let mut same = bare();
    same["name"] = json!("send");
    let (parsed, _) = parse_locked(doc(same), Some(&lock));
    assert_eq!(parsed.as_ref().unwrap()[0].mcp_name, None);
    assert_eq!(
        parsed.unwrap()[0].revision,
        revision_of(parse_locked(doc(bare()), Some(&lock)))
    );
    // 서버 이름이 다르면 그 이름이 도구에 실리고, 해시는 그 이름으로 계산한다
    let mut named = bare();
    named["name"] = json!("send_mail");
    let locked_mail = locked_as("demo", "send", "send_mail", "d", remote_schema());
    let (parsed, codes) = parse_locked(doc(named.clone()), Some(&locked_mail));
    assert!(codes.is_empty(), "{codes:?}");
    let tool = parsed.unwrap().remove(0);
    assert_eq!(tool.mcp_name.as_deref(), Some("send_mail"));
    assert_eq!(tool.id.0, "agenda.send");
    assert_eq!(
        tool.definition.as_deref(),
        bot_core::mcp::definition_sha256("send_mail", "d", &remote_schema()).as_deref()
    );
    // 다른 서버 이름은 다른 도구다(판본이 다르다)
    let mut renamed = bare();
    renamed["name"] = json!("send_mail_v2");
    let locked_v2 = locked_as("demo", "send", "send_mail_v2", "d", remote_schema());
    assert_ne!(
        revision_of(parse_locked(doc(renamed), Some(&locked_v2))),
        tool.revision
    );
    // 같은 이름을 인라인 input으로 쓴 도구도 이름이 판본에 든다
    let mut inline = named.clone();
    inline["input"] = remote_schema();
    let inline_named = revision_of(parse_locked(doc(inline.clone()), None));
    inline.as_object_mut().unwrap().remove("name");
    inline["input"] = remote_schema();
    assert_ne!(inline_named, revision_of(parse_locked(doc(inline), None)));
    // 잠금과 tools.yaml이 정한 서버 이름이 다르면 거절한다(어느 쪽이 낡았는지 다시 동기화하라고 알린다)
    let (parsed, codes) = parse_locked(doc(bare()), Some(&locked_mail));
    assert!(parsed.is_none());
    assert_eq!(codes, ["lock-name-mismatch"]);
    let (parsed, codes) = parse_locked(doc(named), Some(&lock));
    assert!(parsed.is_none());
    assert_eq!(codes, ["lock-name-mismatch"]);
}

#[test]
fn an_input_whose_top_level_is_a_reference_is_checked_after_expansion() {
    let input = json!({
        "$ref": "#/$defs/Args",
        "$defs": {"Args": {"type": "object", "properties": {"to": {"type": "string"}, "message": {"type": "string"}}}}
    });
    let mut write =
        tool(json!({"destination": {"recipient": "args.to", "content": "args.message"}}));
    write["input"] = input;
    let (parsed, codes) = parse_text(doc(write));
    assert!(codes.is_empty(), "{codes:?}");
    let tools = parsed.unwrap();
    // 컴파일한 스키마는 펼친 것이고, 도구가 고정한 입력(실행할 때 서버와 견주는 값)은 받은 그대로다
    assert!(
        tools[0]
            .schema
            .validate(&json!({"to": "a", "message": "b"}))
            .is_empty()
    );
    assert!(!tools[0].schema.validate(&json!({"to": 1})).is_empty());
    assert!(tools[0].input.0.contains("$defs"));
}

#[test]
fn float_integer_timeout_secs_is_preserved_not_silently_dropped() {
    let (int_parsed, codes) = parse_text(doc(tool(json!({"timeout_secs": 300}))));
    assert!(codes.is_empty(), "{codes:?}");
    let (float_parsed, codes) = parse_text(doc(tool(json!({"timeout_secs": 300.0}))));
    assert!(codes.is_empty(), "{codes:?}");
    let int_tool = &int_parsed.unwrap()[0];
    let float_tool = &float_parsed.unwrap()[0];
    assert_eq!(float_tool.timeout_secs, Some(300));
    assert_eq!(float_tool.revision, int_tool.revision);
}
