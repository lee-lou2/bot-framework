//! Built-in tool visibility tests: reserved-name deduplication, ceilings, and super-admin tools.

use super::*;

#[test]
fn only_builtin_tools_inside_the_agent_ceiling_are_listed_once() {
    let mut snap = snapshot();
    snap.policy.scopes.insert("schedule.list".into());
    snap.policy.scopes.insert("schedule.deliver".into());
    let agent = snap.agents.get_mut(&AgentId("main".into())).unwrap();
    agent.scopes.insert("schedule.list".into());
    agent.scopes.insert("schedule.deliver".into());
    // Even if a malformed snapshot registers a reserved name as a skill tool, only the built-in spec is exposed.
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

    let narrow_run = build(
        &snap,
        &run_context("main", &["run", "calendar.read"], "x"),
        &[],
        &[],
    );
    assert!(!ids(&narrow_run).contains(&"schedule.list".to_owned()));
    assert!(!ids(&narrow_run).contains(&"memory.share".to_owned()));

    // policy.agents ceilings cap both skill and built-in tools and are inherited by child agents.
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

    let regular = build(
        &snap,
        &run_context("main", &manage_scopes, "Create a skill"),
        &[],
        &[],
    );
    let regular_ids: Vec<&str> = regular.tools.iter().map(|t| t.id.0.as_str()).collect();
    for id in [SKILL_INSPECT_TOOL, SKILL_CREATE_TOOL, MCP_ADD_TOOL] {
        assert!(
            !regular_ids.contains(&id),
            "regular user must not see `{id}`: {regular_ids:?}"
        );
    }

    snap.super_admins.insert(PrincipalId("local:owner".into()));
    let admin = build(
        &snap,
        &run_context("main", &manage_scopes, "Create a skill"),
        &[],
        &[],
    );
    let admin_ids: Vec<&str> = admin.tools.iter().map(|t| t.id.0.as_str()).collect();
    for id in [SKILL_INSPECT_TOOL, SKILL_CREATE_TOOL, MCP_ADD_TOOL] {
        assert!(
            admin_ids.contains(&id),
            "super-admin should see `{id}`: {admin_ids:?}"
        );
    }
    let mcp_add = admin.tools.iter().find(|t| t.id.0 == MCP_ADD_TOOL).unwrap();
    assert_eq!(mcp_add.class, Risk::Write);

    let narrow = build(
        &snap,
        &run_context("main", &["run", SKILL_INSPECT_TOOL], "Inspect"),
        &[],
        &[],
    );
    let narrow_ids: Vec<&str> = narrow.tools.iter().map(|t| t.id.0.as_str()).collect();
    assert!(narrow_ids.contains(&SKILL_INSPECT_TOOL));
    assert!(!narrow_ids.contains(&SKILL_CREATE_TOOL));
    assert!(!narrow_ids.contains(&MCP_ADD_TOOL));

    snap.policy
        .agents
        .insert(AgentId("main".into()), scopes(&["run", SKILL_INSPECT_TOOL]));
    let capped = build(
        &snap,
        &run_context("main", &manage_scopes, "Create a skill"),
        &[],
        &[],
    );
    let capped_ids: Vec<&str> = capped.tools.iter().map(|t| t.id.0.as_str()).collect();
    assert!(capped_ids.contains(&SKILL_INSPECT_TOOL));
    assert!(!capped_ids.contains(&SKILL_CREATE_TOOL));
    assert!(!capped_ids.contains(&MCP_ADD_TOOL));
}
