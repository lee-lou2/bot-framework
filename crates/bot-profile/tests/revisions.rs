//! 판본: 바이트·정책·도구 선언에 따른 판본, 번들 복원과 변조 거절, 입력 검증, 실효 등급.
mod support;

use bot_core::{
    id::{ProfileId, ToolId},
    profile::{InputValidator, Snapshot},
};
use bot_profile::{Limits, SchemaValidator, from_bundle, revision_of};
use std::collections::BTreeSet;
use support::{TOOLS_PATH, agent, base, codes, edit, ok, raw_bundle, snapshot};

#[test]
fn revisions_follow_bytes_policy_and_declarations() {
    let a = snapshot(&base());
    let b = snapshot(&base());
    assert_eq!(
        (&a.revision, &a.policy_revision),
        (&b.revision, &b.policy_revision),
        "같은 바이트는 같은 판본"
    );

    let knowledge = snapshot(&edit(base(), "knowledge/guide.md", "안내", "안내 2"));
    assert_ne!(
        knowledge.revision, a.revision,
        "파일 한 바이트도 판본이 바뀐다"
    );
    assert_eq!(
        knowledge.policy_revision, a.policy_revision,
        "지식은 정책 판본과 무관"
    );

    let approve = snapshot(&edit(
        base(),
        "profile.yaml",
        "approve: []",
        "approve: [calendar.read]",
    ));
    assert_ne!(approve.policy_revision, a.policy_revision);
    let role = snapshot(&edit(
        base(),
        "profile.yaml",
        "reader: [calendar.read]",
        "reader: [calendar.read, run]",
    ));
    assert_ne!(role.policy_revision, a.policy_revision);
    let approver = {
        let files = edit(
            base(),
            "profile.yaml",
            "{identity: \"service:morning\", roles: [reader]}",
            "{identity: \"service:morning\", roles: [reader]}\n  - {identity: \"local:boss\", roles: [reader]}",
        );
        snapshot(&edit(
            files,
            "agents/main/triggers.yaml",
            "principal: \"service:morning\"",
            "principal: \"service:morning\"\n    approver: \"local:boss\"",
        ))
    };
    assert_ne!(
        approver.policy_revision, a.policy_revision,
        "승인자 매핑도 정책 판본에 묶인다"
    );

    let tool = |s: &Snapshot, name: &str| {
        s.agents[&agent("main")].skills["agenda"].tools[&ToolId(format!("agenda.{name}"))].clone()
    };
    let scopes = snapshot(&edit(
        base(),
        TOOLS_PATH,
        "scopes: [calendar.delete]",
        "scopes: [calendar.delete, run]",
    ));
    assert_ne!(
        tool(&scopes, "delete").revision,
        tool(&a, "delete").revision
    );
    assert_eq!(
        tool(&scopes, "list").revision,
        tool(&a, "list").revision,
        "다른 도구의 판본은 그대로"
    );
    let server = {
        let mut host_files = edit(
            base(),
            TOOLS_PATH,
            "server: demo-agenda",
            "server: demo-notes",
        );
        host_files = edit(
            host_files,
            "profile.yaml",
            "model: assistant",
            "model: assistant",
        );
        snapshot(&host_files)
    };
    assert_ne!(
        tool(&server, "list").revision,
        tool(&a, "list").revision,
        "서버 별칭이 도구 판본에 들어간다"
    );
}

#[test]
fn bundle_rebuilds_the_same_snapshot_and_rejects_tampering() {
    let files = base();
    let set = ok(&files);
    let id = ProfileId("personal".into());
    let original = set.profile(&id).unwrap();
    let bundle = set.bundle(&id).unwrap().to_vec();
    let limits = Limits::default();
    let again = from_bundle(&bundle, &id, &original.revision, &limits).unwrap();
    assert_eq!(format!("{again:?}"), format!("{:?}", **original));
    assert!(
        again.agents.values().all(|a| a.builtin.is_empty()),
        "builtin이 없던 번들은 빈 집합"
    );

    let wrong_revision = bot_core::id::Revision("0".repeat(64));
    let codes = |r: Result<Snapshot, Vec<bot_profile::Issue>>| {
        r.unwrap_err().iter().map(|i| i.code).collect::<Vec<_>>()
    };
    assert_eq!(
        codes(from_bundle(&bundle, &id, &wrong_revision, &limits)),
        ["revision-mismatch"]
    );
    let mut flipped = bundle.clone();
    *flipped.last_mut().unwrap() ^= 1;
    assert_eq!(
        codes(from_bundle(&flipped, &id, &original.revision, &limits)),
        ["revision-mismatch"]
    );
    let other = ProfileId("second".into());
    assert!(
        codes(from_bundle(&bundle, &other, &original.revision, &limits))
            .contains(&"profile-id-mismatch")
    );
    // 기록한 판본과 해시가 다르면 구조를 읽기 전에 거절한다
    assert_eq!(
        codes(from_bundle(b"garbage", &id, &original.revision, &limits)),
        ["revision-mismatch"]
    );
    let half = &bundle[..bundle.len() / 2];
    assert_eq!(
        codes(from_bundle(half, &id, &original.revision, &limits)),
        ["revision-mismatch"]
    );
    // 해시가 맞아도 구조가 틀린 묶음은 거절한다
    for broken in [&b"garbage"[..], half] {
        let own = bot_profile::revision_of(broken);
        assert_eq!(
            codes(from_bundle(broken, &id, &own, &limits)),
            ["bundle-invalid"]
        );
    }
}

#[test]
fn bundle_restore_does_not_need_the_host_registry() {
    // 활성화 뒤 서버가 철회돼도 옛 판본의 구조는 다시 만들 수 있다(현재 권한 검사는 호출 때 따로 한다)
    let set = ok(&base());
    let id = ProfileId("personal".into());
    let snap = set.profile(&id).unwrap();
    let restored = from_bundle(
        set.bundle(&id).unwrap(),
        &id,
        &snap.revision,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        restored.agents[&agent("main")].skills["agenda"]
            .tools
            .values()
            .next()
            .unwrap()
            .server,
        "demo-agenda"
    );
}

#[test]
fn input_validator_checks_declared_tool_input() {
    let snap = snapshot(&base());
    let delete =
        &snap.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.delete".into())];
    let v = SchemaValidator::new();
    let args = |s: &str| bot_core::command::JsonPayload(s.to_owned());
    assert!(
        v.validate(&delete.input, &args(r#"{"event":"walk-1"}"#))
            .is_ok()
    );
    for bad in [
        r#"{}"#,
        r#"{"event":5}"#,
        r#"{"event":"x","extra":1}"#,
        "[]",
        "nope",
    ] {
        assert!(v.validate(&delete.input, &args(bad)).is_err(), "{bad}");
    }
    let find = &snap.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.find".into())];
    assert!(
        v.validate(&find.input, &args(r#"{"query":"산책"}"#))
            .is_ok()
    );
    assert!(
        v.validate(&find.input, &args(r#"{"query":""}"#)).is_err(),
        "minLength 1"
    );
    let kinds: BTreeSet<_> = snap.agents[&agent("main")].skills["agenda"]
        .tools
        .values()
        .map(|t| t.class)
        .collect();
    assert_eq!(kinds.len(), 2);
}

#[test]
fn explicit_destination_raises_the_effective_class() {
    let tools = "version: 1\nserver: demo-notes\ntools:\n  - id: send\n    class: write\n    scopes: [calendar.read]\n    destination: {recipient: args.to, content: args.text}\n    input: {type: object, required: [to, text], properties: {to: {type: string}, text: {type: string}}, additionalProperties: false}\n  - id: reply\n    class: write\n    scopes: [calendar.read]\n    destination: reply\n    input: {type: object, properties: {text: {type: string}}, additionalProperties: false}\n";
    let files = edit(
        edit(
            base(),
            "agents/main/skills/mail/SKILL.md",
            "",
            "메일 안내\n",
        ),
        "agents/main/skills/mail/tools.yaml",
        "",
        tools,
    );
    let snap = snapshot(&files);
    let mail = &snap.agents[&agent("main")].skills["mail"];
    let class = |name: &str| mail.tools[&ToolId(format!("mail.{name}"))].class;
    assert_eq!(
        class("send"),
        bot_core::state::Risk::Irreversible,
        "다른 사람에게 보내는 도구"
    );
    assert_eq!(
        class("reply"),
        bot_core::state::Risk::Write,
        "원래 대화에 답하는 도구는 선언 그대로"
    );
}

/// 스킬 `skill`에 도구 하나(`tool`, write 등급)만 선언한 파일 지도
fn with_tool(skill: &str, tool: &str) -> support::Files {
    let tools = format!(
        "version: 1\nserver: demo-notes\ntools:\n  - id: {tool}\n    class: write\n    scopes: [calendar.read]\n    input: {{type: object, properties: {{}}, additionalProperties: false}}\n"
    );
    let files = edit(
        base(),
        &format!("agents/main/skills/{skill}/SKILL.md"),
        "",
        "스킬 안내\n",
    );
    edit(
        files,
        &format!("agents/main/skills/{skill}/tools.yaml"),
        "",
        &tools,
    )
}

#[test]
fn only_the_builtin_tool_ids_are_reserved_for_new_and_restored_profiles() {
    // 새 프로필: 정확한 전체 ID `memory.share`만 거절하고 나머지는 허용
    assert_eq!(codes(&with_tool("memory", "share")), ["reserved-tool"]);
    ok(&with_tool("memory", "recall"));
    ok(&with_tool("mail", "share"));
    ok(&with_tool("memory-notes", "share"));
    // 대화 중 예약·후속 확인·자동 회신 승인의 내장 도구: schedule.create·list·cancel·deliver만 예약한다
    for tool in ["create", "list", "cancel", "deliver"] {
        assert_eq!(
            codes(&with_tool("schedule", tool)),
            ["reserved-tool"],
            "schedule.{tool}"
        );
    }
    for tool in ["get", "update", "delete"] {
        ok(&with_tool("schedule", tool));
    }
    ok(&with_tool("calendar", "create"));
    ok(&with_tool("schedule-notes", "create"));

    // 복원한 프로필: 같은 검증이 돈다(해시가 맞는 묶음이어도 거절)
    let (id, limits) = (ProfileId("personal".into()), Limits::default());
    let restore = |files: &support::Files| {
        let bundle = raw_bundle(files);
        from_bundle(&bundle, &id, &revision_of(&bundle), &limits)
    };
    let found = restore(&with_tool("memory", "share")).unwrap_err();
    assert!(found.iter().any(|i| i.code == "reserved-tool"), "{found:?}");
    assert!(restore(&with_tool("memory", "recall")).is_ok());
    assert!(restore(&with_tool("mail", "share")).is_ok());
    for tool in ["create", "list", "cancel", "deliver"] {
        let found = restore(&with_tool("schedule", tool)).unwrap_err();
        assert!(
            found.iter().any(|i| i.code == "reserved-tool"),
            "{tool}: {found:?}"
        );
    }
    assert!(restore(&with_tool("schedule", "get")).is_ok());
}
