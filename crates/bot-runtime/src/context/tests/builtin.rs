//! 내장 도구 노출 규칙 시험: 예약 이름 중복 제거 · 요청/정책 상한 · 슈퍼 관리자 전용 관리 도구.

use super::*;

#[test]
fn only_builtin_tools_inside_the_agent_ceiling_are_listed_once() {
    let mut snap = snapshot();
    snap.policy.scopes.insert("schedule.list".into());
    snap.policy.scopes.insert("schedule.deliver".into());
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
    let with_schedule = [
        "run",
        "calendar.read",
        "calendar.delete",
        "schedule.list",
        "schedule.deliver",
    ];
    let input = build(&snap, &run_context("main", &with_schedule, "x"), &[], &[]);
    let builtins: Vec<_> = input
        .tools
        .iter()
        .filter(|t| t.id.0.starts_with("schedule."))
        .collect();
    assert_eq!(builtins.len(), 1);
    assert_eq!(builtins[0].id.0, "schedule.list");
    assert_eq!(builtins[0].input.0, bot_core::builtin::LIST.schema);
}

#[test]
fn builtin_tools_and_skill_tools_respect_policy_agents_ceiling_and_run_scopes() {
    let mut snap = snapshot();
    for scope in ["schedule.list", "memory.share"] {
        snap.policy.scopes.insert(scope.into());
        snap.agents
            .get_mut(&AgentId("main".into()))
            .unwrap()
            .scopes
            .insert(scope.into());
        snap.agents
            .get_mut(&AgentId("main/research".into()))
            .unwrap()
            .scopes
            .insert(scope.into());
    }
    let all = [
        "run",
        "calendar.read",
        "calendar.delete",
        "schedule.list",
        "memory.share",
    ];
    let ids = |out: &Assembled| out.tools.iter().map(|t| t.id.0.clone()).collect::<Vec<_>>();

    // 1) 요청 범위(run.scopes)에 없는 내장 도구는 에이전트·정책에 있어도 보이지 않는다
    let narrow_run = build(
        &snap,
        &run_context("main", &["run", "calendar.read"], "x"),
        &[],
        &[],
    );
    assert!(!ids(&narrow_run).contains(&"schedule.list".to_owned()));
    assert!(!ids(&narrow_run).contains(&"memory.share".to_owned()));

    // 2) policy.agents 에이전트 상한이 좁히면 스킬 도구와 내장 도구 모두 가려진다(자식도 부모 상한을 상속한다)
    snap.policy.agents.insert(
        AgentId("main".into()),
        scopes(&["run", "calendar.read", "schedule.list"]),
    );
    let capped_main = build(&snap, &run_context("main", &all, "x"), &[], &[]);
    assert!(ids(&capped_main).contains(&"schedule.list".to_owned()));
    assert!(!ids(&capped_main).contains(&"memory.share".to_owned()));
    assert!(!ids(&capped_main).contains(&"agenda.delete".to_owned()));

    let capped_child = build(&snap, &run_context("main/research", &all, "x"), &[], &[]);
    assert!(ids(&capped_child).contains(&"schedule.list".to_owned()));
    assert!(!ids(&capped_child).contains(&"memory.share".to_owned()));
}

#[test]
fn super_admin_manage_tools_are_only_exposed_to_super_admins_with_scopes() {
    use bot_core::builtin::{MCP_ADD_TOOL, SKILL_CREATE_TOOL, SKILL_INSPECT_TOOL};

    let mut snap = snapshot();
    let manage_scopes = [
        "run",
        "calendar.read",
        SKILL_INSPECT_TOOL,
        SKILL_CREATE_TOOL,
        MCP_ADD_TOOL,
    ];
    snap.policy.scopes = scopes(&manage_scopes);
    snap.agents.get_mut(&AgentId("main".into())).unwrap().scopes = scopes(&manage_scopes);

    // 일반 멤버는 스코프가 있어도 슈퍼 관리자 전용 도구를 보지 못한다
    let regular = build(
        &snap,
        &run_context("main", &manage_scopes, "스킬 만들어줘"),
        &[],
        &[],
    );
    let regular_ids: Vec<&str> = regular.tools.iter().map(|t| t.id.0.as_str()).collect();
    for id in [SKILL_INSPECT_TOOL, SKILL_CREATE_TOOL, MCP_ADD_TOOL] {
        assert!(
            !regular_ids.contains(&id),
            "일반 사용자에게 `{id}`가 노출되면 안 돼요: {regular_ids:?}"
        );
    }

    // 슈퍼 관리자는 허용된 관리 도구를 본다
    snap.super_admins.insert(PrincipalId("local:owner".into()));
    let admin = build(
        &snap,
        &run_context("main", &manage_scopes, "스킬 만들어줘"),
        &[],
        &[],
    );
    let admin_ids: Vec<&str> = admin.tools.iter().map(|t| t.id.0.as_str()).collect();
    for id in [SKILL_INSPECT_TOOL, SKILL_CREATE_TOOL, MCP_ADD_TOOL] {
        assert!(
            admin_ids.contains(&id),
            "슈퍼 관리자에게 `{id}`가 보여야 해요: {admin_ids:?}"
        );
    }
    let mcp_add = admin.tools.iter().find(|t| t.id.0 == MCP_ADD_TOOL).unwrap();
    assert_eq!(mcp_add.class, Risk::Write);

    // 스코프 상한에 없는 관리 도구는 슈퍼 관리자여도 보이지 않는다
    let narrow = build(
        &snap,
        &run_context("main", &["run", SKILL_INSPECT_TOOL], "조회"),
        &[],
        &[],
    );
    let narrow_ids: Vec<&str> = narrow.tools.iter().map(|t| t.id.0.as_str()).collect();
    assert!(narrow_ids.contains(&SKILL_INSPECT_TOOL));
    assert!(!narrow_ids.contains(&SKILL_CREATE_TOOL));
    assert!(!narrow_ids.contains(&MCP_ADD_TOOL));

    // policy.agents 상한이 관리 도구를 막으면 슈퍼 관리자여도 보이지 않는다
    snap.policy
        .agents
        .insert(AgentId("main".into()), scopes(&["run", SKILL_INSPECT_TOOL]));
    let capped = build(
        &snap,
        &run_context("main", &manage_scopes, "스킬 만들어줘"),
        &[],
        &[],
    );
    let capped_ids: Vec<&str> = capped.tools.iter().map(|t| t.id.0.as_str()).collect();
    assert!(capped_ids.contains(&SKILL_INSPECT_TOOL));
    assert!(!capped_ids.contains(&SKILL_CREATE_TOOL));
    assert!(!capped_ids.contains(&MCP_ADD_TOOL));
}
