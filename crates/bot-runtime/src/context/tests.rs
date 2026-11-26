//! 맥락 조립 시험: 한도 · 출처 · 스코프 필터 · 직계 위임 · 기억 · 순서 고정.

use super::*;
use bot_core::{
    command::Step,
    id::{BindingId, Generation, MemoryId, PrincipalId, ProfileId, Revision, RunId},
    profile::{Policy, Skill, Tool},
    state::{Conversation, ConversationKind, Visibility},
};
use serde_json::json;
use std::collections::BTreeMap;

fn scopes(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn tool(class: Risk, needs: &[&str], input: &str) -> Tool {
    Tool {
        server: "demo-agenda".into(),
        revision: Revision("t1".into()),
        class,
        scopes: scopes(needs),
        input: JsonPayload(input.into()),
        description: None,
        env: Default::default(),
        secrets: BTreeMap::new(),
        egress: BTreeSet::new(),
        effects: false,
        external: false,
        destination: None,
        definition: None,
        mcp_name: None,
        timeout_secs: None,
    }
}

fn agenda() -> Skill {
    Skill {
        description: None,
        instructions: Document {
            relative_path: "skills/agenda/SKILL.md".into(),
            text:
                "일정 목록은 list로 조회한다. delete는 사용자가 확인한 일정 ID 하나에만 사용한다.\n"
                    .into(),
        },
        tools: BTreeMap::from([
            (
                ToolId("agenda.delete".into()),
                tool(
                    Risk::Irreversible,
                    &["calendar.delete"],
                    r#"{"type":"object","required":["event"],"properties":{"event":{"type":"string"}}}"#,
                ),
            ),
            (
                ToolId("agenda.list".into()),
                tool(
                    Risk::Read,
                    &["calendar.read"],
                    r#"{"type":"object","description":"일정 목록을 조회한다"}"#,
                ),
            ),
        ]),
    }
}

fn agent(parent: Option<&str>, instructions: &str) -> Agent {
    Agent {
        description: String::new(),
        parent: parent.map(|p| AgentId(p.into())),
        model: "assistant".into(),
        instructions: instructions.into(),
        env: BTreeMap::new(),
        scopes: scopes(&["run", "calendar.read", "calendar.delete"]),
        builtin: BTreeSet::new(),
        skills: BTreeMap::new(),
        knowledge: vec![],
        memory_seeds: vec![],
        triggers: vec![],
    }
}

fn doc(path: &str, text: &str) -> Document {
    Document {
        relative_path: path.into(),
        text: text.into(),
    }
}

fn snapshot() -> Snapshot {
    let mut main = agent(None, "사용자의 오늘 할 일을 한국어로 짧게 정리한다.");
    main.skills.insert("agenda".into(), agenda());
    let research = agent(
        Some("main"),
        "사용자의 오늘 할 일을 한국어로 짧게 정리한다.\n\n부모가 맡긴 일정 조회를 수행한다.",
    );
    Snapshot {
        id: ProfileId("personal".into()),
        revision: Revision("r1".into()),
        policy_revision: Revision("p1".into()),
        timezone: "Asia/Seoul".into(),
        model: "assistant".into(),
        root: AgentId("main".into()),
        agents: BTreeMap::from([
            (AgentId("main".into()), main),
            (AgentId("main/research".into()), research),
        ]),
        bindings: BTreeMap::new(),
        roles: BTreeMap::new(),
        members: BTreeMap::new(),
        policy: Policy {
            scopes: scopes(&["run", "calendar.read", "calendar.delete"]),
            agents: BTreeMap::new(),
            approve: BTreeSet::new(),
        },
        secrets: vec![],
    }
}

fn run_context(agent: &str, scope_list: &[&str], text: &str) -> RunContext {
    RunContext {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        root: RunId("r1".into()),
        principal: PrincipalId("local:owner".into()),
        agent: AgentId(agent.into()),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "owner".into(),
            thread: None,
        },
        binding: BindingId("desk".into()),
        api: None,
        scopes: scopes(scope_list),
        private_memory_read: false,
        work: Work {
            revision: Revision("r1".into()),
            target: Target::Agent {
                agent: AgentId(agent.into()),
                instruction: text.into(),
            },
            input: JsonPayload(json!({"text": text}).to_string()),
            attachments: vec![],
            steps: Vec::<Step>::new(),
            pass_pos: None,
        },
        approver: None,
        trigger: None,
        origin: bot_core::state::RunOrigin::Message,
    }
}

/// 2026-11-24 11:30 KST = 02:30 UTC (화요일)
fn now() -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(1_795_487_400)
}

fn build<'a>(
    snapshot: &'a Snapshot,
    run: &'a RunContext,
    history: &'a [Turn],
    memory: &'a [MemoryHit],
) -> Assembled {
    assemble(&ContextInput {
        snapshot,
        run,
        history,
        memory,
        now: now(),
    })
    .unwrap()
}

const FULL: [&str; 3] = ["run", "calendar.read", "calendar.delete"];

mod chat;

#[test]
fn system_has_a_fixed_shape_and_the_conversation_ends_with_the_input() {
    let snap = snapshot();
    let run = run_context("main", &FULL, "오늘 일정 알려줘");
    let out = build(&snap, &run, &[], &[]);
    assert_eq!(out.model, "assistant");
    assert_eq!(
        out.system,
        "사용자의 오늘 할 일을 한국어로 짧게 정리한다.\n\n\
         ## 현재 시각\n2026-11-24(화) 11:30 (Asia/Seoul)\n\n\
         ## 스킬\n### agenda\n일정 목록은 list로 조회한다. delete는 사용자가 확인한 일정 ID 하나에만 사용한다.\n\n\
         ## 위임\n직계 하위 에이전트에게만 `delegate` 도구로 일을 맡길 수 있어요.\n- main/research: 부모가 맡긴 일정 조회를 수행한다."
    );
    assert_eq!(out.conversation.len(), 1);
    assert_eq!(out.conversation[0].role, Role::User);
    assert_eq!(
        out.conversation[0].speaker,
        Some(PrincipalId("local:owner".into()))
    );
    assert_eq!(out.conversation[0].text, "오늘 일정 알려줘");
    assert!(out.knowledge.is_empty());
    // 같은 입력이면 같은 출력
    let again = build(&snap, &run, &[], &[]);
    assert_eq!(
        (out.system.as_str(), &out.conversation),
        (again.system.as_str(), &again.conversation)
    );
    assert_eq!(out.tools, again.tools);
}

#[test]
fn tools_are_filtered_by_request_agent_and_policy_scopes() {
    let snap = snapshot();
    let ids = |out: &Assembled| out.tools.iter().map(|t| t.id.0.clone()).collect::<Vec<_>>();
    let full = build(&snap, &run_context("main", &FULL, "x"), &[], &[]);
    assert_eq!(
        ids(&full),
        ["agenda.delete", "agenda.list", "delegate"],
        "ID 순서 고정, 위임 도구는 맨 뒤"
    );
    let read = full.tools.iter().find(|t| t.id.0 == "agenda.list").unwrap();
    assert_eq!(
        (read.kind, read.class, read.description.as_str()),
        (ToolKind::Skill, Risk::Read, "일정 목록을 조회한다")
    );
    let delete = full
        .tools
        .iter()
        .find(|t| t.id.0 == "agenda.delete")
        .unwrap();
    assert_eq!(
        (delete.class, delete.description.as_str()),
        (Risk::Irreversible, "agenda.delete")
    );

    // 요청자에게 읽기만 있으면 삭제는 보이지 않는다
    let reader = build(
        &snap,
        &run_context("main", &["run", "calendar.read"], "x"),
        &[],
        &[],
    );
    assert_eq!(ids(&reader), ["agenda.list", "delegate"]);

    // 에이전트 상한이 좁으면 요청 범위가 넓어도 보이지 않는다
    let mut narrow = snapshot();
    narrow
        .agents
        .get_mut(&AgentId("main".into()))
        .unwrap()
        .scopes = scopes(&["run", "calendar.read"]);
    assert_eq!(
        ids(&build(&narrow, &run_context("main", &FULL, "x"), &[], &[])),
        ["agenda.list", "delegate"]
    );

    // 정책 상한도 마찬가지
    let mut capped = snapshot();
    capped.policy.scopes = scopes(&["run"]);
    let out = build(&capped, &run_context("main", &FULL, "x"), &[], &[]);
    assert_eq!(ids(&out), ["delegate"]);
    assert!(
        !out.system.contains("## 스킬"),
        "쓸 수 있는 도구가 하나도 없는 스킬은 지침에도 싣지 않는다"
    );

    // 도구 스코프는 하나라도 모자라면 숨긴다(모두 덮어야 한다)
    let mut both = snapshot();
    both.agents
        .get_mut(&AgentId("main".into()))
        .unwrap()
        .skills
        .get_mut("agenda")
        .unwrap()
        .tools
        .insert(
            ToolId("agenda.move".into()),
            tool(Risk::Write, &["calendar.read", "calendar.write"], "{}"),
        );
    assert!(
        !ids(&build(&both, &run_context("main", &FULL, "x"), &[], &[]))
            .contains(&"agenda.move".to_owned())
    );
}

#[test]
fn delegation_targets_are_direct_children_only() {
    let mut snap = snapshot();
    snap.agents.insert(
        AgentId("main/research/deep".into()),
        agent(Some("main/research"), "깊이 조사한다."),
    );
    snap.agents.insert(
        AgentId("main/other".into()),
        agent(Some("main"), "기타 일을 한다."),
    );
    let out = build(&snap, &run_context("main", &FULL, "x"), &[], &[]);
    assert_eq!(
        out.delegates,
        [
            AgentId("main/other".into()),
            AgentId("main/research".into())
        ],
        "손자(main/research/deep)는 없다"
    );
    let spec = out.tools.iter().find(|t| t.id.0 == DELEGATE_TOOL).unwrap();
    assert_eq!(spec.kind, ToolKind::Delegate);
    let schema: Value = serde_json::from_str(&spec.input.0).unwrap();
    assert_eq!(
        schema["properties"]["agent"]["enum"],
        json!(["main/other", "main/research"])
    );
    assert_eq!(schema["required"], json!(["agent", "instruction"]));
    assert_eq!(schema["additionalProperties"], false);

    let child = build(&snap, &run_context("main/research", &FULL, "x"), &[], &[]);
    assert_eq!(child.delegates, [AgentId("main/research/deep".into())]);
    let leaf = build(
        &snap,
        &run_context("main/research/deep", &FULL, "x"),
        &[],
        &[],
    );
    assert!(leaf.delegates.is_empty() && !leaf.tools.iter().any(|t| t.id.0 == DELEGATE_TOOL));
    assert!(!leaf.system.contains("## 위임"));
}

#[cfg(test)]
mod knowledge;

#[cfg(test)]
mod memory;

#[test]
fn history_becomes_messages_and_old_turns_are_dropped_first() {
    let snap = snapshot();
    let run = run_context("main", &FULL, "지금 질문");
    let turn = |from: Option<&str>, text: &str| Turn {
        from: from.map(|p| PrincipalId(p.into())),
        text: text.into(),
    };
    let history = [
        turn(Some("local:owner"), "안녕"),
        turn(None, "네, 안녕하세요"),
        turn(Some("local:owner"), "어제 일정은?"),
    ];
    let out = build(&snap, &run, &history, &[]);
    let roles: Vec<Role> = out.conversation.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::User, Role::User]);
    assert_eq!(out.conversation[1].speaker, None);
    assert_eq!(out.conversation.last().unwrap().text, "지금 질문");

    // 한도를 넘으면 오래된 차례부터 버린다(최근 것은 연속으로 남는다)
    let big = "가".repeat(30_000);
    let long = [
        turn(Some("a"), &big),
        turn(None, &big),
        turn(Some("a"), &big),
        turn(None, "마지막 답"),
    ];
    let out = build(&snap, &run, &long, &[]);
    let total: usize = out.conversation.iter().map(|m| m.text.len()).sum();
    assert!(total <= MAX_HISTORY_BYTES + "지금 질문".len(), "{total}");
    assert_eq!(
        out.conversation[out.conversation.len() - 2].text,
        "마지막 답"
    );
    assert!(out.conversation.len() < 5);
}

#[test]
fn trigger_instruction_and_event_body_are_combined_sensibly() {
    let snap = snapshot();
    let mut run = run_context("main", &FULL, "");
    let cases = [
        // (instruction, body, 기대하는 입력)
        ("아침 요약을 만든다", "{}", "아침 요약을 만든다"),
        ("", r#"{"text":"안녕"}"#, "안녕"),
        ("같은 말", r#"{"text":"같은 말"}"#, "같은 말"),
        ("요약한다", r#"{"text":"본문"}"#, "요약한다\n\n입력:\n본문"),
        (
            "요약한다",
            r#"{"channel":"c1","n":2}"#,
            "요약한다\n\n입력:\n{\"channel\":\"c1\",\"n\":2}",
        ),
        ("", "null", ""),
    ];
    for (instruction, body, want) in cases {
        run.work.target = Target::Agent {
            agent: AgentId("main".into()),
            instruction: instruction.into(),
        };
        run.work.input = JsonPayload(body.into());
        assert_eq!(
            build(&snap, &run, &[], &[])
                .conversation
                .last()
                .unwrap()
                .text,
            want,
            "{instruction:?} {body}"
        );
    }
}

#[test]
fn clock_uses_the_profile_timezone_and_falls_back_to_utc() {
    assert_eq!(
        clock(now(), "Asia/Seoul"),
        "2026-11-24(화) 11:30 (Asia/Seoul)"
    );
    assert_eq!(
        clock(now(), "America/New_York"),
        "2026-11-23(월) 21:30 (America/New_York)"
    );
    assert_eq!(clock(now(), "Not/AZone"), "2026-11-24(화) 02:30 (UTC)");
    assert_eq!(clock(UNIX_EPOCH, "UTC"), "1970-01-01(목) 00:00 (UTC)");
}

#[test]
fn bad_runs_are_rejected_and_into_request_wires_every_field() {
    let snap = snapshot();
    let mut run = run_context("nobody", &FULL, "x");
    let input = ContextInput {
        snapshot: &snap,
        run: &run,
        history: &[],
        memory: &[],
        now: now(),
    };
    assert_eq!(
        assemble(&input).unwrap_err(),
        ContextError::UnknownAgent(AgentId("nobody".into()))
    );
    run.agent = AgentId("main".into());
    run.work.target = Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId("agenda.list".into()),
        args: JsonPayload("{}".into()),
    };
    let input = ContextInput {
        snapshot: &snap,
        run: &run,
        history: &[],
        memory: &[],
        now: now(),
    };
    assert_eq!(assemble(&input).unwrap_err(), ContextError::NotAnAgentRun);

    let run = run_context("main", &FULL, "안녕");
    let out = build(&snap, &run, &[], &[]);
    let (tx, rx) = watch::channel(false);
    let request = out.clone().into_request(
        run.attempt.clone(),
        vec![],
        Limits::default(),
        rx,
        None,
        None,
    );
    assert_eq!(
        (request.attempt.clone(), request.model.as_str()),
        (run.attempt, "assistant")
    );
    assert_eq!(
        (request.system, request.tools, request.conversation.len()),
        (out.system, out.tools, 1)
    );
    assert!(request.exchanges.is_empty() && request.relay.is_none() && !*request.cancel.borrow());
    drop(tx);
}

#[test]
fn only_builtin_tools_inside_the_agent_ceiling_are_listed_once() {
    let mut snap = snapshot();
    let agent = snap.agents.get_mut(&AgentId("main".into())).unwrap();
    agent.scopes.insert("schedule.list".into());
    agent.scopes.insert("schedule.deliver".into());
    // 호스트에서 잘못 만든 판본이 예약 이름을 스킬 도구로 넣어도 내장 선언 하나만 쓴다.
    let skill = agent.skills.get_mut("agenda").unwrap();
    for id in ["schedule.list", "schedule.deliver"] {
        skill
            .tools
            .insert(ToolId(id.into()), tool(Risk::Read, &["run"], "{}"));
    }
    let input = build(&snap, &run_context("main", &FULL, "x"), &[], &[]);
    let builtins: Vec<_> = input
        .tools
        .iter()
        .filter(|t| t.id.0.starts_with("schedule."))
        .collect();
    assert_eq!(builtins.len(), 1);
    assert_eq!(builtins[0].id.0, "schedule.list");
    assert_eq!(builtins[0].input.0, bot_core::builtin::LIST.schema);
}
